#[macro_use]
extern crate log;

use crate::fetch::FetchQueue;
use crate::settings::Settings;
use anyhow::Result;
use axum::http::HeaderValue;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Router, extract::FromRef};
use config::Config;
use nostr_sdk::ClientBuilder;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

mod avatar;
mod events;
mod fetch;
mod link_preview;
mod opengraph;
mod settings;

#[derive(Clone)]
pub struct AppState {
    pub fetch: FetchQueue,
    pub link_preview: Arc<link_preview::LinkPreviewCache>,
}

impl FromRef<AppState> for FetchQueue {
    fn from_ref(state: &AppState) -> Self {
        state.fetch.clone()
    }
}

impl FromRef<AppState> for Arc<link_preview::LinkPreviewCache> {
    fn from_ref(state: &AppState) -> Self {
        state.link_preview.clone()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let builder = Config::builder()
        .add_source(config::File::with_name("config.yaml"))
        .add_source(config::Environment::with_prefix("APP"))
        .build()?;

    let settings: Settings = builder.try_deserialize()?;

    let client = ClientBuilder::new().build();
    for x in settings.relays {
        client.add_relay(x).await?;
    }
    client.connect().await;

    let fetch = FetchQueue::new(client.clone());
    let fetch_worker = fetch.clone();
    tokio::spawn(async move {
        loop {
            fetch_worker.process_queue().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let state = AppState {
        fetch,
        link_preview: Arc::new(link_preview::LinkPreviewCache::new()),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/openapi.yaml", get(openapi))
        .route("/avatar/{set}/{value}", get(avatar::get_avatar))
        .route("/event", post(events::import_event))
        .route("/event/{id}", get(events::get_event))
        .route("/event/{kind}/{pubkey}", get(events::get_event_by_kind))
        .route("/preview", get(link_preview::get_preview))
        .route("/opengraph/{id}", post(opengraph::tag_page))
        .with_state(state)
        .layer(CorsLayer::very_permissive());

    let addr: SocketAddr = match &settings.listen {
        Some(i) => i.parse()?,
        None => SocketAddr::from(([0, 0, 0, 0], 8000)),
    };

    info!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

pub fn default_avatar(hash: &str) -> String {
    format!(
        "https://nostr-api.v0l.io/api/v1/avatar/cyberpunks/{}.webp",
        hash
    )
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../index.html"))
}

async fn openapi() -> impl IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/yaml"),
        )],
        include_str!("../openapi.yaml"),
    )
}
