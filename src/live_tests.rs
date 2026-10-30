//! Checks against the real knihovny.cz API, so a change to it shows up as a failing test rather
//! than as a user's empty search. Ignored by `cargo test`; run them with
//! `cargo test live -- --ignored --test-threads=1`. They check facts about well-known books that
//! should not change (series, language, an edition's ISBN), not exact descriptions.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::knihovny::{Client, SITE};
use crate::server::routes;
use crate::{Config, VERSION};

async fn provider() -> String {
    let cfg = Config {
        auth_token: None,
        deadline: Duration::from_secs(20),
    };
    let user_agent = format!("bookorbit-knihovnycz-provider/{VERSION} (live check)");
    let client = Client::new(SITE, &user_agent, 2, Duration::from_millis(250));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = routes(Arc::new(cfg), Arc::new(client));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn get(base: &str, path: &str) -> (u16, Value) {
    let res = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = res.status().as_u16();
    let body = serde_json::from_slice(&res.bytes().await.unwrap()).unwrap_or(Value::Null);
    (status, body)
}

async fn ok(base: &str, path: &str) -> Value {
    let (status, body) = get(base, path).await;
    assert_eq!(status, 200, "{path}: {body}");
    body
}

fn matches(body: &Value) -> &Vec<Value> {
    body["matches"].as_array().expect("a matches list")
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_title_and_author() {
    let base = provider().await;
    let body = ok(
        &base,
        "/search?query=Posledn%C3%AD%20p%C5%99%C3%A1n%C3%AD&author=Andrzej%20Sapkowski",
    )
    .await;
    let first = &matches(&body)[0];
    assert_eq!(
        first["title"], "Poslední přání",
        "title from the title's parts"
    );
    assert_eq!(
        first["authors"][0], "Andrzej Sapkowski",
        "authors without life dates"
    );
    assert_eq!(
        first["series"][0]["series"], "Zaklínač",
        "series from the title's parts"
    );
    assert_eq!(first["series"][0]["sequence"], "1");
    assert_eq!(first["languageCode"], "cs", "language from 008");
    assert!(first["pages"].as_i64().unwrap_or(0) > 200, "pages: {first}");
    assert!(
        first["description"].as_str().is_some_and(|d| d.len() > 100),
        "description"
    );
    assert!(
        first["genres"]
            .as_array()
            .is_some_and(|g| g.iter().any(|g| g == "fantasy povídky")),
        "genres: {}",
        first["genres"]
    );
    // The cover exists and is an image.
    let cover = first["cover"].as_str().expect("a cover");
    let res = reqwest::get(cover).await.unwrap();
    let content_type = res.headers()["content-type"].to_str().unwrap().to_string();
    assert_eq!(
        (res.status().as_u16(), content_type.as_str()),
        (200, "image/jpeg"),
        "{cover}"
    );
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_isbn_finds_its_edition() {
    let base = provider().await;
    for (isbn, year) in [("978-80-7477-079-1", "2022"), ("8085951142", "1999")] {
        let body = ok(&base, &format!("/search?isbn={isbn}")).await;
        let first = &matches(&body)[0];
        assert_eq!(first["title"], "Poslední přání", "{isbn}");
        assert_eq!(first["publishedYear"], year, "{isbn}");
        let reported = first["isbn13"]
            .as_str()
            .or(first["isbn10"].as_str())
            .unwrap_or_default();
        assert!(
            crate::mapping::same_isbn(reported, isbn),
            "{isbn} reported {reported}"
        );
    }
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_isbns_that_no_edition_lists_find_nothing() {
    let base = provider().await;
    for isbn in ["9788025706305", "0000000000", "9780000000002"] {
        let body = ok(&base, &format!("/search?isbn={isbn}")).await;
        assert!(matches(&body).is_empty(), "{isbn}: {body}");
    }
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_audiobook() {
    let base = provider().await;
    let body = ok(
        &base,
        "/search?query=Hobit&author=J.%20R.%20R.%20Tolkien&mediaType=audiobook",
    )
    .await;
    let audiobooks: Vec<&Value> = matches(&body)
        .iter()
        .filter(|b| b["mediaType"] == "audiobook")
        .collect();
    assert!(!audiobooks.is_empty(), "no audiobook: {body}");
    assert_eq!(
        matches(&body)[0]["mediaType"],
        "audiobook",
        "an audiobook search puts audiobooks first"
    );
    assert!(
        audiobooks
            .iter()
            .any(|b| b["narrators"].as_array().is_some_and(|n| !n.is_empty())),
        "narrators: {body}"
    );
    assert!(
        audiobooks
            .iter()
            .any(|b| b["duration"].as_i64().unwrap_or(0) > 3600),
        "duration: {body}"
    );
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_slovak_edition() {
    let base = provider().await;
    let body = ok(
        &base,
        "/search?query=Posledn%C3%A9%20%C5%BEelanie&author=Andrzej%20Sapkowski",
    )
    .await;
    let first = &matches(&body)[0];
    assert_eq!(first["title"], "Posledné želanie");
    assert_eq!(first["languageCode"], "sk");
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_subtitles_are_split_off() {
    let base = provider().await;
    let body = ok(&base, "/search?query=Sapiens&author=Yuval%20Noah%20Harari").await;
    let first = &matches(&body)[0];
    assert_eq!(first["title"], "Sapiens");
    assert!(
        first["subtitle"].as_str().is_some_and(|s| !s.is_empty()),
        "{first}"
    );
    assert_eq!(first["mediaType"], "book");
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_lookup() {
    let base = provider().await;
    let book = ok(&base, "/book/nkp.NKC01-003423268").await;
    assert_eq!(book["providerId"], "nkp.NKC01-003423268");
    assert_eq!(book["isbn13"], "9788074770791");
    let (status, _) = get(&base, "/book/nkp.NKC01-999999999").await;
    assert_eq!(status, 404, "an unknown id");
}

#[tokio::test]
#[ignore = "hits knihovny.cz"]
async fn live_empty_search_and_limit() {
    let base = provider().await;
    let body = ok(&base, "/search?query=qxzwvqxzwv").await;
    assert!(matches(&body).is_empty());
    for (limit, expected) in [("1", 1), ("3", 3), ("50", 10)] {
        let body = ok(
            &base,
            &format!("/search?query=Harry%20Potter&limit={limit}"),
        )
        .await;
        assert_eq!(matches(&body).len(), expected, "limit {limit}");
    }
}
