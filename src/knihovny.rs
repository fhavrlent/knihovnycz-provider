use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use axum::http::HeaderMap;
use regex::Regex;
use reqwest::{Method, Url};
use serde_json::Value;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Where every link in an answer points, whatever `KNIHOVNY_SITE` fetches from.
pub const SITE: &str = "https://www.knihovny.cz";
const SEARCH_PATH: &str = "/api/v1/search";
const RECORD_PATH: &str = "/api/v1/record";
const COVER_PATH: &str = "/Cover/Show";
const MAX_BODY_BYTES: usize = 8 << 20;
const DEFAULT_RETRY_AFTER_SECONDS: u64 = 60;
/// The fields a search or lookup asks for. `fullRecord` is the MARC record, which alone carries
/// the language, the narrators and the parts of a title.
const FIELDS: &[&str] = &[
    "id",
    "title",
    "authors",
    "publishers",
    "publicationDates",
    "isbns",
    "summary",
    "formats",
    "playingTimes",
    "fullRecord",
];
/// Real covers come as JPEG; for a book without one the site answers a small PNG placeholder.
/// A PNG this large is a real cover after all.
const PLACEHOLDER_MAX_BYTES: usize = 4096;

#[derive(Debug)]
pub enum UpstreamError {
    NotFound,
    RateLimited { retry_after_seconds: u64 },
    Failed(String),
}

pub struct Client {
    base: String,
    http: reqwest::Client,
    slots: Semaphore,
    interval: Duration,
    next: Mutex<Instant>,
    /// Set when knihovny.cz answers 429; nothing is sent to it again before then.
    paused_until: Mutex<Option<Instant>>,
}

impl Client {
    pub fn new(base: &str, user_agent: &str, concurrency: usize, interval: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(3))
            .user_agent(user_agent)
            .build()
            .expect("could not build the HTTP client");
        Self {
            base: base.trim_end_matches('/').to_string(),
            http,
            slots: Semaphore::new(concurrency),
            interval,
            next: Mutex::new(Instant::now()),
            paused_until: Mutex::new(None),
        }
    }

    /// The records of a search, in the catalogue's order. An answer without a record count is
    /// not understood: an empty list is only ever the API saying it found nothing.
    pub async fn search(&self, lookfor: &str, limit: usize) -> Result<Vec<Value>, UpstreamError> {
        let mut url = self.url(SEARCH_PATH);
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("lookfor", lookfor)
                .append_pair("type", "AllFields")
                .append_pair("limit", &limit.to_string());
            for field in FIELDS {
                query.append_pair("field[]", field);
            }
        }
        let body = self.json(url).await?;
        if body["resultCount"].as_u64().is_none() {
            return Err(UpstreamError::Failed(
                "search answered without a record count".into(),
            ));
        }
        Ok(body["records"].as_array().cloned().unwrap_or_default())
    }

    /// One record by its id. The API answers an unknown id with 400 "Error loading record".
    pub async fn record(&self, id: &str) -> Result<Value, UpstreamError> {
        let mut url = self.url(RECORD_PATH);
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("id", id);
            for field in FIELDS {
                query.append_pair("field[]", field);
            }
        }
        let body = self.json(url).await?;
        body["records"]
            .as_array()
            .and_then(|records| records.first())
            .cloned()
            .ok_or(UpstreamError::NotFound)
    }

    /// Whether the site has a cover for an ISBN: a JPEG, or a PNG too large to be its
    /// placeholder.
    pub async fn has_cover(&self, isbn: &str) -> Result<bool, UpstreamError> {
        let url = self.url_for(&cover_url(isbn));
        let res = self.send(Method::HEAD, url.clone()).await?;
        match content_type(res.headers()).as_str() {
            "image/jpeg" => Ok(true),
            "image/png" => {
                let res = self.send(Method::GET, url).await?;
                let body = read_body(res).await?;
                Ok(body.len() > PLACEHOLDER_MAX_BYTES)
            }
            _ => Ok(false),
        }
    }

    async fn json(&self, url: Url) -> Result<Value, UpstreamError> {
        let res = self.send(Method::GET, url).await?;
        let body = read_body(res).await?;
        serde_json::from_slice(&body).map_err(|_| {
            UpstreamError::Failed("knihovny.cz answered something other than JSON".into())
        })
    }

    async fn send(&self, method: Method, url: Url) -> Result<reqwest::Response, UpstreamError> {
        let _permit = self.acquire().await?;
        // Errors drop the URL: it carries the title someone searched for.
        let res = self
            .http
            .request(method, url)
            .header(ACCEPT, "application/json, image/*;q=0.9, */*;q=0.8")
            .send()
            .await
            .map_err(|error| UpstreamError::Failed(error.without_url().to_string()))?;
        match res.status().as_u16() {
            200..=299 => Ok(res),
            400 => {
                // The API's answer for an id it does not know.
                let body = read_body(res).await.unwrap_or_default();
                if String::from_utf8_lossy(&body).contains("Error loading record") {
                    Err(UpstreamError::NotFound)
                } else {
                    Err(UpstreamError::Failed("knihovny.cz answered 400".into()))
                }
            }
            404 => Err(UpstreamError::NotFound),
            429 => {
                let retry_after_seconds = retry_after(res.headers());
                self.pause(Duration::from_secs(retry_after_seconds));
                Err(UpstreamError::RateLimited {
                    retry_after_seconds,
                })
            }
            status => Err(UpstreamError::Failed(format!(
                "knihovny.cz answered {status}"
            ))),
        }
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("{}{path}", self.base)).expect("valid URL")
    }

    /// A public knihovny.cz address, fetched from the configured site.
    fn url_for(&self, public: &str) -> Url {
        let local = match public.strip_prefix(SITE) {
            Some(rest) => format!("{}{rest}", self.base),
            None => public.to_string(),
        };
        Url::parse(&local).expect("valid URL")
    }

    /// At most `concurrency` requests in flight, each starting at least `interval` after the last.
    async fn acquire(&self) -> Result<SemaphorePermit<'_>, UpstreamError> {
        self.check_paused()?;
        let permit = self
            .slots
            .acquire()
            .await
            .map_err(|error| UpstreamError::Failed(error.to_string()))?;
        let start = {
            let mut next = self.next.lock().expect("limiter lock poisoned");
            let start = (*next).max(Instant::now());
            *next = start + self.interval;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        self.check_paused()?;
        Ok(permit)
    }

    fn pause(&self, by: Duration) {
        let until = Instant::now() + by;
        let mut paused = self.paused_until.lock().expect("pause lock poisoned");
        if paused.is_none_or(|current| current < until) {
            *paused = Some(until);
        }
    }

    fn check_paused(&self) -> Result<(), UpstreamError> {
        let paused = *self.paused_until.lock().expect("pause lock poisoned");
        match paused {
            Some(until) if until > Instant::now() => Err(UpstreamError::RateLimited {
                retry_after_seconds: (until - Instant::now()).as_secs_f64().ceil().max(1.0) as u64,
            }),
            _ => Ok(()),
        }
    }
}

async fn read_body(mut res: reqwest::Response) -> Result<Vec<u8>, UpstreamError> {
    let mut body = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|error| UpstreamError::Failed(error.without_url().to_string()))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(UpstreamError::Failed(format!(
                "answer over {MAX_BODY_BYTES} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_RETRY_AFTER_SECONDS)
}

/// The site's large cover for an ISBN, a public address BookOrbit downloads once.
pub fn cover_url(isbn: &str) -> String {
    let mut url = Url::parse(&format!("{SITE}{COVER_PATH}")).expect("valid cover URL");
    url.query_pairs_mut()
        .append_pair("isbn", isbn)
        .append_pair("size", "large");
    url.to_string()
}

static RECORD_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9]+\.[A-Za-z0-9_*.:-]{1,100}$").expect("valid pattern")
});
static RECORD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/Record/([A-Za-z0-9]+\.[A-Za-z0-9_*.:%-]{1,120})").expect("valid pattern")
});

/// A record id as the catalogue writes it, such as `nkp.NKC01-003423268`: a library's code, a
/// dot, and its own id. A knihovny.cz record address works too.
pub fn extract_id(value: &str) -> Option<String> {
    let value = value.trim();
    let id = match RECORD_LINK.captures(value) {
        Some(captures) => percent_decode(&captures[1]),
        None => value.to_string(),
    };
    RECORD_ID.is_match(&id).then_some(id)
}

fn percent_decode(value: &str) -> String {
    let hex = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(high * 16 + low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_ids() {
        for (input, want) in [
            ("nkp.NKC01-003423268", Some("nkp.NKC01-003423268")),
            ("mktnv.TynUsCat*0020201", Some("mktnv.TynUsCat*0020201")),
            (
                "kfbz.7efe9110-0a5a-42b1-88ae-18bf62e1ee5f",
                Some("kfbz.7efe9110-0a5a-42b1-88ae-18bf62e1ee5f"),
            ),
            (
                "https://www.knihovny.cz/Record/nkp.NKC01-003423268",
                Some("nkp.NKC01-003423268"),
            ),
            (
                "https://www.knihovny.cz/Record/upol.UpolUsCat%2A0393873",
                Some("upol.UpolUsCat*0393873"),
            ),
            ("not-an-id", None),
            ("..", None),
            ("conformance-unknown-id", None),
            ("nkp.NKC01 003423268", None),
            ("nkp./etc/passwd", None),
        ] {
            assert_eq!(extract_id(input).as_deref(), want, "{input}");
        }
    }

    #[test]
    fn cover_urls_ask_for_the_large_cover() {
        assert_eq!(
            cover_url("9788074770791"),
            "https://www.knihovny.cz/Cover/Show?isbn=9788074770791&size=large"
        );
    }

    #[tokio::test]
    async fn limiter_spaces_request_starts() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::from_millis(100));
        let started = Instant::now();
        for _ in 0..3 {
            drop(client.acquire().await.unwrap());
        }
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn a_pause_refuses_requests_until_it_ends() {
        let client = Client::new("http://127.0.0.1:9", "test", 2, Duration::ZERO);
        client.pause(Duration::from_secs(30));
        match client.acquire().await {
            Err(UpstreamError::RateLimited {
                retry_after_seconds,
            }) => assert!((29..=30).contains(&retry_after_seconds)),
            other => panic!("expected a pause, got {:?}", other.map(|_| ())),
        };
    }
}
