use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use crate::knihovny::{cover_url, extract_id, Client, UpstreamError};
use crate::mapping::{
    candidates, closeness, is_placeholder_isbn, isbn_digits, map_record, rank, same_isbn,
    BookMetadata, Candidate, Closeness,
};
use crate::{Config, VERSION};

const DEFAULT_LIMIT: usize = 5;
const MAX_LIMIT: usize = 10;
/// Records asked of the catalogue per search. It unites many libraries, so one edition can fill
/// several of them; ranking folds those copies into one.
const FETCH: usize = 30;
const MAX_ID_LENGTH: usize = 128;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    kc: Arc<Client>,
}

#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub matches: Vec<BookMetadata>,
    /// Proposed: something could not be fetched, so this answer must not be cached as complete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

pub fn routes(cfg: Arc<Config>, kc: Arc<Client>) -> Router {
    Router::new()
        .route("/provider/info", get(info))
        .route("/search", get(search))
        .route("/book/{*id}", get(lookup))
        .layer(middleware::from_fn(log_requests))
        .with_state(AppState { cfg, kc })
}

async fn info(State(state): State<AppState>) -> Response {
    Json(json!({
        "name": "Knihovny.cz",
        "version": VERSION,
        "avatarUrl": "https://www.knihovny.cz/themes/KnihovnyCz/images/favicons/apple-touch-icon.png",
        "specVersion": "1",
        "authenticationRequired": state.cfg.auth_token.is_some(),
        // Proposed extensions.
        "key": "knihovnycz",
        "mediaTypes": ["book", "audiobook", "comic"],
        "lookup": true,
        "supportedFields": [
            "title", "subtitle", "description", "cover", "authors", "publisher", "isbn",
            "publishedYear", "language", "pageCount", "seriesName", "seriesIndex", "genres",
            "narrators", "duration"
        ],
        "searchParams": ["isbn", "limit"],
        "idMaxLength": MAX_ID_LENGTH,
        "bookUrlTemplate": "https://www.knihovny.cz/Record/{id}",
        // No `coverShape`: print covers are portrait and audiobook covers square.
        // The site's primary colour (`--bs-primary` in its stylesheet).
        "color": "#e5004b",
        "rateLimit": { "maxConcurrency": 2, "minIntervalMs": 0 },
    }))
    .into_response()
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let param = |key: &str| q.get(key).map(|v| v.trim()).unwrap_or_default();
    let phrase = param("query");
    let given_isbn = normalize_isbn(param("isbn"));
    let isbn = given_isbn.clone().filter(|isbn| !is_placeholder_isbn(isbn));
    if phrase.is_empty() && isbn.is_none() {
        // A placeholder ISBN is a book without one: nothing to search, not a bad request.
        if given_isbn.is_some() {
            return Json(SearchResponse {
                matches: vec![],
                partial: false,
            })
            .into_response();
        }
        return error(StatusCode::BAD_REQUEST, "query is required");
    }
    let limit = param("limit")
        .parse::<usize>()
        .map_or(DEFAULT_LIMIT, |n| n.clamp(1, MAX_LIMIT));
    let audiobook = param("mediaType") == "audiobook";
    let deadline = tokio::time::Instant::now() + state.cfg.deadline;

    let found = tokio::time::timeout_at(
        deadline,
        find_books(
            &state,
            phrase,
            param("author"),
            isbn.as_deref(),
            audiobook,
            limit,
        ),
    )
    .await
    .unwrap_or_else(|_| Err(UpstreamError::Failed("search timed out".into())));
    match found {
        Ok(books) => {
            let (matches, partial) = with_covers(&state, books, deadline).await;
            Json(SearchResponse { matches, partial }).into_response()
        }
        Err(err) => upstream_error("search", err),
    }
}

async fn find_books(
    state: &AppState,
    phrase: &str,
    author: &str,
    isbn: Option<&str>,
    audiobook: bool,
    limit: usize,
) -> Result<Vec<BookMetadata>, UpstreamError> {
    // The ISBN first. A search for it matches other fields too, so only an edition that lists
    // the ISBN is its match.
    if let Some(isbn) = isbn {
        let records = state.kc.search(&isbn_digits(isbn), FETCH).await?;
        let listing: Vec<Candidate> = candidates(&records, Some(isbn))
            .into_iter()
            .filter(|candidate| candidate.isbns.iter().any(|listed| same_isbn(listed, isbn)))
            .collect();
        if !listing.is_empty() {
            return Ok(take(rank(listing, "", "", audiobook), limit));
        }
        if phrase.is_empty() {
            return Ok(vec![]);
        }
    }

    // The title and the author's surname together, which the catalogue matches precisely; the
    // title alone when that finds nothing by the author or with the title.
    let surname = author.split_whitespace().last().unwrap_or_default();
    let mut found = Vec::new();
    if !surname.is_empty() {
        let lookfor = format!(
            "title:({}) AND author:({})",
            escape(phrase),
            escape(surname)
        );
        found = candidates(&state.kc.search(&lookfor, FETCH).await?, None);
    }
    if found.is_empty() || closeness(&found, phrase, author) == Closeness::None {
        let lookfor = format!("title:({})", escape(phrase));
        let by_title = candidates(&state.kc.search(&lookfor, FETCH).await?, None);
        if closeness(&by_title, phrase, author) >= closeness(&found, phrase, author) {
            found = by_title;
        }
    }
    Ok(take(rank(found, phrase, author, audiobook), limit))
}

fn take(candidates: Vec<Candidate>, limit: usize) -> Vec<BookMetadata> {
    candidates
        .into_iter()
        .take(limit)
        .map(|candidate| candidate.book)
        .collect()
}

/// Each book's cover, when the site has one for its ISBN. A check that fails leaves the book
/// without a cover and marks the answer partial, so it is not cached as complete.
async fn with_covers(
    state: &AppState,
    mut books: Vec<BookMetadata>,
    deadline: tokio::time::Instant,
) -> (Vec<BookMetadata>, bool) {
    let mut tasks = JoinSet::new();
    for (index, book) in books.iter().enumerate() {
        let isbn = book.cover_code.clone();
        if isbn.is_empty() {
            continue;
        }
        let kc = state.kc.clone();
        tasks.spawn(async move { (index, isbn.clone(), kc.has_cover(&isbn).await) });
    }
    let mut partial = false;
    loop {
        match tokio::time::timeout_at(deadline, tasks.join_next()).await {
            Ok(None) => break,
            Ok(Some(Ok((index, isbn, Ok(true))))) => books[index].cover = cover_url(&isbn),
            Ok(Some(Ok((_, _, Ok(false))))) => {}
            Ok(Some(_)) => partial = true,
            Err(_) => {
                tasks.abort_all();
                partial = true;
                break;
            }
        }
    }
    (books, partial)
}

async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(raw): Path<String>,
) -> Response {
    if let Some(denied) = deny_unauthorized(&state, &headers) {
        return denied;
    }
    let id = match extract_id(&raw) {
        Some(id) if raw.len() <= MAX_ID_LENGTH * 3 && id.len() <= MAX_ID_LENGTH => id,
        _ => return error(StatusCode::NOT_FOUND, "Not found"),
    };
    let deadline = tokio::time::Instant::now() + state.cfg.deadline;
    let record = tokio::time::timeout_at(deadline, state.kc.record(&id))
        .await
        .unwrap_or_else(|_| Err(UpstreamError::Failed("record timed out".into())));
    match record {
        Ok(record) => match map_record(&record, None) {
            Some(candidate) => {
                let mut book = candidate.book;
                // The id that was asked for, as BookOrbit stored it.
                book.provider_id = id;
                let (mut books, _) = with_covers(&state, vec![book], deadline).await;
                Json(books.remove(0)).into_response()
            }
            None => {
                eprintln!("[provider.lookup] [fail] error=\"record has no title\" - knihovny.cz record unreadable");
                error(
                    StatusCode::BAD_GATEWAY,
                    "knihovny.cz record could not be read",
                )
            }
        },
        Err(UpstreamError::NotFound) => error(StatusCode::NOT_FOUND, "Not found"),
        Err(err) => upstream_error("lookup", err),
    }
}

/// A search term with the catalogue's query syntax characters escaped.
fn escape(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An ISBN of either length without hyphens or spaces, or nothing when it is not one.
fn normalize_isbn(raw: &str) -> Option<String> {
    let isbn: String = raw
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .collect::<String>()
        .to_uppercase();
    let valid = match isbn.len() {
        13 => isbn.bytes().all(|b| b.is_ascii_digit()),
        10 => {
            isbn[..9].bytes().all(|b| b.is_ascii_digit())
                && isbn[9..].bytes().all(|b| b.is_ascii_digit() || b == b'X')
        }
        _ => false,
    };
    valid.then_some(isbn)
}

/// Status codes are part of the contract: a client backs off on 429 and treats 5xx as a failure,
/// never as "no results".
fn upstream_error(op: &str, err: UpstreamError) -> Response {
    match err {
        UpstreamError::RateLimited {
            retry_after_seconds,
        } => {
            let mut res = error(
                StatusCode::TOO_MANY_REQUESTS,
                "knihovny.cz is rate limiting this provider",
            );
            res.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(retry_after_seconds));
            res
        }
        UpstreamError::NotFound => error(StatusCode::NOT_FOUND, "Not found"),
        UpstreamError::Failed(message) => {
            eprintln!("[provider.{op}] [fail] error={message:?} - knihovny.cz request failed");
            error(StatusCode::BAD_GATEWAY, "knihovny.cz is unavailable")
        }
    }
}

/// The refusal to send when a token is configured and the request does not carry it.
fn deny_unauthorized(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let token = state.cfg.auth_token.as_ref()?;
    let given = headers
        .get(header::AUTHORIZATION)
        .map(HeaderValue::as_bytes)
        .unwrap_or_default();
    (!constant_time_eq(given, token.as_bytes()))
        .then(|| error(StatusCode::UNAUTHORIZED, "Unauthorized"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn log_requests(req: Request, next: Next) -> Response {
    let (method, path, started) = (
        req.method().clone(),
        req.uri().path().to_string(),
        Instant::now(),
    );
    let res = next.run(req).await;
    eprintln!(
        "[request] [end] method={method} path={path} status={} durationMs={}",
        res.status().as_u16(),
        started.elapsed().as_millis()
    );
    res
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::http::Method;
    use axum::response::IntoResponse;

    use super::*;

    const SEARCH: &str = include_str!("../testdata/search-tichy-pristav.json");
    const EMPTY: &str = include_str!("../testdata/search-empty.json");
    const RECORD: &str = include_str!("../testdata/record-print.json");
    const THREE: &str = include_str!("../testdata/record-three-isbns.json");
    const UNKNOWN: &str = r#"{"status":"ERROR","statusMessage":"Error loading record"}"#;
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0 a cover";
    const PLACEHOLDER: &[u8] = b"\x89PNG a placeholder";

    /// A canned answer: status, content type, body.
    type Answer = (u16, &'static str, Vec<u8>);

    /// A stand-in for knihovny.cz. Answers are keyed by what a request asks for: `search:` and
    /// the query, `record:` and the id, `cover:` and the code; `*` answers anything of its kind.
    #[derive(Default)]
    struct Fake {
        answers: Mutex<HashMap<String, Answer>>,
        delays: Mutex<HashMap<String, Duration>>,
        requested: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            let fake = Arc::new(Fake::default());
            fake.json("search:*", 200, EMPTY);
            fake.answer("cover:*", 200, "image/png", PLACEHOLDER);
            fake
        }

        fn json(&self, key: &str, status: u16, body: &str) {
            self.answer(key, status, "application/json", body.as_bytes());
        }

        fn answer(&self, key: &str, status: u16, content_type: &'static str, body: &[u8]) {
            self.answers
                .lock()
                .unwrap()
                .insert(key.to_string(), (status, content_type, body.to_vec()));
        }

        fn requests(&self) -> Vec<String> {
            self.requested.lock().unwrap().clone()
        }
    }

    async fn fake_site(fake: Arc<Fake>) -> String {
        let app = Router::new().fallback(move |req: Request| {
            let fake = fake.clone();
            async move {
                let query: HashMap<String, String> = req
                    .uri()
                    .query()
                    .map(|q| {
                        reqwest::Url::parse(&format!("http://x/?{q}"))
                            .unwrap()
                            .query_pairs()
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect()
                    })
                    .unwrap_or_default();
                let (kind, value) = match req.uri().path() {
                    "/api/v1/search" => {
                        ("search", query.get("lookfor").cloned().unwrap_or_default())
                    }
                    "/api/v1/record" => ("record", query.get("id").cloned().unwrap_or_default()),
                    "/Cover/Show" => ("cover", query.get("isbn").cloned().unwrap_or_default()),
                    other => ("other", other.to_string()),
                };
                let key = format!("{kind}:{value}");
                let method = if req.method() == Method::HEAD {
                    "HEAD "
                } else {
                    ""
                };
                fake.requested
                    .lock()
                    .unwrap()
                    .push(format!("{method}{key}"));
                let delay = fake.delays.lock().unwrap().get(&key).copied();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let answer = {
                    let answers = fake.answers.lock().unwrap();
                    answers
                        .get(&key)
                        .or_else(|| answers.get(&format!("{kind}:*")))
                        .cloned()
                };
                match answer {
                    Some((429, _, _)) => (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "42")],
                        "",
                    )
                        .into_response(),
                    Some((status, content_type, body)) => (
                        StatusCode::from_u16(status).unwrap(),
                        [(header::CONTENT_TYPE, content_type)],
                        body,
                    )
                        .into_response(),
                    None => (StatusCode::BAD_REQUEST, UNKNOWN).into_response(),
                }
            }
        });
        serve(app).await
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn provider_at(upstream: &str, token: Option<&str>, deadline: Duration) -> String {
        let cfg = Config {
            auth_token: token.map(str::to_string),
            deadline,
        };
        let client = Client::new(upstream, "test", 2, Duration::ZERO);
        serve(routes(Arc::new(cfg), Arc::new(client))).await
    }

    async fn provider(fake: Arc<Fake>) -> String {
        provider_at(&fake_site(fake).await, None, Duration::from_secs(5)).await
    }

    async fn get(
        base: &str,
        path: &str,
        auth: Option<&str>,
    ) -> (u16, serde_json::Value, HeaderMap) {
        let mut req = reqwest::Client::new().get(format!("{base}{path}"));
        if let Some(auth) = auth {
            req = req.header("Authorization", auth);
        }
        let res = req.send().await.unwrap();
        let (status, headers) = (res.status().as_u16(), res.headers().clone());
        let body =
            serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(serde_json::Value::Null);
        (status, body, headers)
    }

    fn matches(body: &serde_json::Value) -> &Vec<serde_json::Value> {
        body["matches"].as_array().expect("a matches list")
    }

    const TITLE_AND_AUTHOR: &str = "search:title:(Tichý přístav) AND author:(Testová)";

    #[tokio::test]
    async fn info_declares_the_contract() {
        let base = provider(Fake::new()).await;
        let (status, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(status, 200);
        assert_eq!(
            (
                info["key"].as_str(),
                info["name"].as_str(),
                info["lookup"].as_bool()
            ),
            (Some("knihovnycz"), Some("Knihovny.cz"), Some(true))
        );
        assert_eq!(
            info["bookUrlTemplate"],
            "https://www.knihovny.cz/Record/{id}"
        );
        assert_eq!(info["color"], "#e5004b");
        assert_eq!(info["searchParams"], json!(["isbn", "limit"]));
        assert!(info.get("coverShape").is_none());
    }

    #[tokio::test]
    async fn search_by_title_and_author_ranks_and_finds_covers() {
        let fake = Fake::new();
        fake.json(TITLE_AND_AUTHOR, 200, SEARCH);
        fake.answer("cover:9788090000025", 200, "image/jpeg", JPEG);
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?query=Tich%C3%BD%20p%C5%99%C3%ADstav&author=Jana%20Testov%C3%A1",
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.get("partial").is_none());
        let first = &matches(&body)[0];
        assert_eq!(first["providerId"], "nkp.NKC01-900000001");
        assert_eq!(first["title"], "Tichý přístav");
        assert_eq!(
            first["series"],
            json!([{"series": "Kronika mlhy", "sequence": "1"}])
        );
        assert_eq!(
            first["cover"],
            "https://www.knihovny.cz/Cover/Show?isbn=9788090000025&size=large"
        );
        // A placeholder is no cover.
        for book in &matches(&body)[1..] {
            assert!(book.get("cover").is_none(), "{book}");
        }
        // One search, then a HEAD per book with a code; the placeholder PNG is fetched to be sure.
        assert_eq!(fake.requests()[0], TITLE_AND_AUTHOR);
        assert!(fake
            .requests()
            .contains(&"HEAD cover:9788090000025".to_string()));
    }

    #[tokio::test]
    async fn a_title_and_author_search_finding_nothing_falls_back_to_the_title() {
        let fake = Fake::new();
        fake.json("search:title:(Tichý přístav)", 200, SEARCH);
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(
            &base,
            "/search?query=Tich%C3%BD%20p%C5%99%C3%ADstav&author=Jana%20Testov%C3%A1",
            None,
        )
        .await;
        assert_eq!(
            (status, matches(&body)[0]["providerId"].as_str()),
            (200, Some("nkp.NKC01-900000001"))
        );
        assert_eq!(
            fake.requests()[..2],
            [TITLE_AND_AUTHOR, "search:title:(Tichý přístav)"]
        );
    }

    #[tokio::test]
    async fn query_syntax_in_a_title_is_escaped() {
        let fake = Fake::new();
        let base = provider(fake.clone()).await;
        get(&base, "/search?query=Kdo%3A%20(n%C4%9Bkdo)%3F", None).await;
        assert_eq!(fake.requests()[0], r"search:title:(Kdo\: \(někdo\)\?)");
    }

    #[tokio::test]
    async fn search_by_isbn_answers_only_editions_that_list_it() {
        let fake = Fake::new();
        // The API's search for an ISBN also matches other fields: only the edition listing it counts.
        fake.json("search:9788090000032", 200, SEARCH);
        fake.json("search:9788090001008", 200, THREE);
        let base = provider(fake.clone()).await;
        let (_, body, _) = get(&base, "/search?isbn=978-80-900000-3-2", None).await;
        let ids: Vec<&str> = matches(&body)
            .iter()
            .filter_map(|b| b["providerId"].as_str())
            .collect();
        assert_eq!(ids, ["mzk.MZK01-900000005"]);
        // The third ISBN of an edition with three is the one reported.
        let (_, body, _) = get(&base, "/search?isbn=9788090001008", None).await;
        assert_eq!(matches(&body)[0]["isbn13"], "9788090001008");
        // An ISBN no edition lists answers nothing, or the title when there is one.
        let (_, body, _) = get(&base, "/search?isbn=9788090009999", None).await;
        assert!(matches(&body).is_empty());
    }

    #[tokio::test]
    async fn a_placeholder_isbn_is_not_searched() {
        let fake = Fake::new();
        let base = provider(fake.clone()).await;
        let (status, body, _) = get(&base, "/search?isbn=0000000000", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    }

    #[tokio::test]
    async fn search_requires_a_query_or_an_isbn() {
        let fake = Fake::new();
        let base = provider(fake.clone()).await;
        assert_eq!(get(&base, "/search", None).await.0, 400);
        assert_eq!(get(&base, "/search?isbn=12", None).await.0, 400);
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn search_respects_the_limit() {
        let fake = Fake::new();
        fake.json(TITLE_AND_AUTHOR, 200, SEARCH);
        let base = provider(fake).await;
        let (_, body, _) = get(
            &base,
            "/search?query=Tich%C3%BD%20p%C5%99%C3%ADstav&author=Jana%20Testov%C3%A1&limit=2",
            None,
        )
        .await;
        assert_eq!(matches(&body).len(), 2);
    }

    #[tokio::test]
    async fn an_empty_result_is_an_empty_answer() {
        let base = provider(Fake::new()).await;
        let (status, body, _) = get(&base, "/search?query=qxzwv", None).await;
        assert_eq!((status, matches(&body).len()), (200, 0));
        assert!(body.get("partial").is_none());
    }

    #[tokio::test]
    async fn an_answer_that_is_not_understood_is_a_failure_not_an_empty_answer() {
        for body in [r#"{"records": []}"#, "<html>Údržba</html>"] {
            let fake = Fake::new();
            fake.json("search:*", 200, body);
            let base = provider(fake).await;
            assert_eq!(
                get(&base, "/search?query=Krakatit", None).await.0,
                502,
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn a_failing_or_rate_limited_api() {
        let fake = Fake::new();
        fake.json("search:*", 503, "");
        let base = provider(fake).await;
        assert_eq!(get(&base, "/search?query=Krakatit", None).await.0, 502);

        let fake = Fake::new();
        fake.json("search:*", 429, "");
        let base = provider(fake.clone()).await;
        let (status, _, headers) = get(&base, "/search?query=Krakatit", None).await;
        assert_eq!(
            (
                status,
                headers.get("retry-after").and_then(|v| v.to_str().ok())
            ),
            (429, Some("42"))
        );
        // The pause holds: a lookup is refused without asking the API again.
        let asked = fake.requests().len();
        assert_eq!(get(&base, "/book/nkp.NKC01-900000001", None).await.0, 429);
        assert_eq!(fake.requests().len(), asked);
    }

    #[tokio::test]
    async fn a_failing_cover_check_leaves_the_book_and_marks_the_answer_partial() {
        let fake = Fake::new();
        fake.json(TITLE_AND_AUTHOR, 200, SEARCH);
        fake.answer("cover:9788090000025", 503, "text/html", b"");
        let base = provider(fake).await;
        let (status, body, _) = get(
            &base,
            "/search?query=Tich%C3%BD%20p%C5%99%C3%ADstav&author=Jana%20Testov%C3%A1",
            None,
        )
        .await;
        assert_eq!((status, body["partial"].as_bool()), (200, Some(true)));
        assert_eq!(matches(&body)[0]["providerId"], "nkp.NKC01-900000001");
        assert!(matches(&body)[0].get("cover").is_none());
    }

    #[tokio::test]
    async fn a_slow_api_keeps_the_deadline() {
        let fake = Fake::new();
        fake.delays
            .lock()
            .unwrap()
            .insert("search:title:(Krakatit)".into(), Duration::from_secs(3));
        let base = provider_at(&fake_site(fake).await, None, Duration::from_millis(700)).await;
        let started = Instant::now();
        assert_eq!(get(&base, "/search?query=Krakatit", None).await.0, 502);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_slow_cover_is_left_out_and_marks_the_answer_partial() {
        let fake = Fake::new();
        fake.json(TITLE_AND_AUTHOR, 200, SEARCH);
        fake.answer("cover:9788090000025", 200, "image/jpeg", JPEG);
        fake.delays
            .lock()
            .unwrap()
            .insert("cover:9788090000025".into(), Duration::from_secs(3));
        let base = provider_at(&fake_site(fake).await, None, Duration::from_millis(900)).await;
        let started = Instant::now();
        let (status, body, _) = get(
            &base,
            "/search?query=Tich%C3%BD%20p%C5%99%C3%ADstav&author=Jana%20Testov%C3%A1",
            None,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!((status, body["partial"].as_bool()), (200, Some(true)));
        assert!(matches(&body)[0].get("cover").is_none());
    }

    #[tokio::test]
    async fn lookup_by_id_or_address_answers_with_the_asked_id() {
        let fake = Fake::new();
        fake.json("record:nkp.NKC01-900000001", 200, RECORD);
        fake.answer("cover:9788090000025", 200, "image/jpeg", JPEG);
        let base = provider(fake).await;
        for path in [
            "/book/nkp.NKC01-900000001",
            "/book/https%3A%2F%2Fwww.knihovny.cz%2FRecord%2Fnkp.NKC01-900000001",
        ] {
            let (status, book, _) = get(&base, path, None).await;
            assert_eq!(
                (status, book["providerId"].as_str(), book["title"].as_str()),
                (200, Some("nkp.NKC01-900000001"), Some("Tichý přístav")),
                "{path}"
            );
            assert!(book["cover"]
                .as_str()
                .is_some_and(|c| c.contains("isbn=9788090000025")));
        }
    }

    #[tokio::test]
    async fn lookup_of_unknown_and_invalid_ids() {
        let fake = Fake::new();
        let base = provider(fake.clone()).await;
        // The API answers an unknown id with 400 "Error loading record".
        assert_eq!(get(&base, "/book/nkp.NKC01-999999999", None).await.0, 404);
        assert_eq!(fake.requests(), ["record:nkp.NKC01-999999999"]);
        for id in ["not-an-id", "..", "conformance-unknown-id"] {
            assert_eq!(
                get(&base, &format!("/book/{id}"), None).await.0,
                404,
                "{id}"
            );
        }
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn lookup_errors_that_are_not_an_unknown_id_are_failures() {
        let fake = Fake::new();
        fake.json(
            "record:nkp.NKC01-1",
            400,
            r#"{"status":"ERROR","statusMessage":"Invalid request"}"#,
        );
        fake.json("record:nkp.NKC01-2", 500, "");
        fake.json(
            "record:nkp.NKC01-3",
            200,
            include_str!("../testdata/record-no-title.json"),
        );
        let base = provider(fake).await;
        for id in ["nkp.NKC01-1", "nkp.NKC01-2", "nkp.NKC01-3"] {
            assert_eq!(
                get(&base, &format!("/book/{id}"), None).await.0,
                502,
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_api_is_a_bad_gateway() {
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let base = provider_at(&closed, None, Duration::from_secs(5)).await;
        assert_eq!(get(&base, "/search?query=Krakatit", None).await.0, 502);
        assert_eq!(get(&base, "/book/nkp.NKC01-900000001", None).await.0, 502);
    }

    #[tokio::test]
    async fn auth_token() {
        let fake = Fake::new();
        fake.json("record:nkp.NKC01-900000001", 200, RECORD);
        let base = provider_at(
            &fake_site(fake).await,
            Some("Bearer s3cret"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(get(&base, "/book/nkp.NKC01-900000001", None).await.0, 401);
        assert_eq!(
            get(&base, "/search?query=x", Some("Bearer nope")).await.0,
            401
        );
        assert_eq!(
            get(&base, "/book/nkp.NKC01-900000001", Some("Bearer s3cret"))
                .await
                .0,
            200
        );
        let (_, info, _) = get(&base, "/provider/info", None).await;
        assert_eq!(info["authenticationRequired"], true);
    }

    #[test]
    fn isbns_are_normalized() {
        assert_eq!(
            normalize_isbn("978-80-7477-079-1").as_deref(),
            Some("9788074770791")
        );
        assert_eq!(
            normalize_isbn("80-85951-14-x").as_deref(),
            Some("808595114X")
        );
        assert_eq!(normalize_isbn("abc"), None);
    }
}
