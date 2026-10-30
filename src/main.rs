mod knihovny;
#[cfg(test)]
mod live_tests;
mod mapping;
mod marc;
mod server;

use std::sync::Arc;
use std::time::Duration;

pub const VERSION: &str = "0.1.0";

pub struct Config {
    pub auth_token: Option<String>,
    /// How long a search or lookup may take, covers included.
    pub deadline: Duration,
}

#[tokio::main]
async fn main() {
    let port = env("PORT", "8080");
    let site = env("KNIHOVNY_SITE", knihovny::SITE);
    let user_agent = env(
        "USER_AGENT",
        &format!("bookorbit-knihovnycz-provider/{VERSION}"),
    );
    let cfg = Config {
        auth_token: std::env::var("AUTH_TOKEN")
            .ok()
            .filter(|token| !token.is_empty()),
        deadline: Duration::from_millis(
            std::env::var("DEADLINE_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(8000),
        ),
    };
    // knihovny.cz is a public service of Czech libraries: two requests in flight and 250 ms
    // between starts, whatever the clients ask for. A search is one API request plus a cover
    // check per book.
    let client = knihovny::Client::new(&site, &user_agent, 2, Duration::from_millis(250));
    let app = server::routes(Arc::new(cfg), Arc::new(client));

    let port_number = port.parse::<u16>().expect("PORT must be a port number");
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port_number))
        .await
        .expect("could not bind the port");
    eprintln!("[provider] [start] port={port} site={site} version={VERSION} - listening");
    axum::serve(listener, app).await.expect("server stopped");
}

fn env(key: &str, fallback: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}
