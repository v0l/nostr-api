#[macro_use]
extern crate rocket;

use crate::fetch::FetchQueue;
use crate::settings::Settings;
use anyhow::Result;
use config::Config;
use nostr_sdk::{ClientBuilder};
use rocket::http::ContentType;
use rocket::shield::Shield;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

mod avatar;
mod events;
mod fetch;
mod link_preview;
mod opengraph;
mod settings;

#[rocket::main]
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

    let link_preview_cache = Arc::new(link_preview::LinkPreviewCache::new());

    let mut config = rocket::Config::default();
    let ip: SocketAddr = match &settings.listen {
        Some(i) => i.parse().unwrap(),
        None => SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 8000),
    };
    config.address = ip.ip();
    config.port = ip.port();

    rocket::Rocket::custom(config)
        .manage(fetch)
        .manage(link_preview_cache)
        .attach(Shield::new()) // disable
        .mount("/", avatar::routes())
        .mount("/", events::routes())
        .mount("/", opengraph::routes())
        .mount("/", link_preview::routes())
        .mount("/", routes![index, openapi])
        .launch()
        .await?;

    Ok(())
}

pub fn default_avatar(hash: &str) -> String {
    format!(
        "https://nostr-api.v0l.io/api/v1/avatar/cyberpunks/{}.webp",
        hash
    )
}

#[get("/")]
pub fn index() -> (ContentType, &'static str) {
    (ContentType::HTML, include_str!("../index.html"))
}

#[get("/openapi.yaml")]
pub fn openapi() -> (ContentType, &'static str) {
    (
        ContentType::new("text", "yaml"),
        include_str!("../openapi.yaml"),
    )
}
