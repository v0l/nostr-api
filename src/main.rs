#[macro_use]
extern crate rocket;

use std::time::Duration;

use anyhow::Error;
use rocket::shield::Shield;

use crate::fetch::FetchQueue;
use crate::store::SledDatabase;

mod events;
mod store;
mod fetch;

#[rocket::main]
async fn main() -> Result<(), Error> {
    pretty_env_logger::init();

    let db = SledDatabase::new("nostr.db");

    let mut fetch = FetchQueue::new();
    fetch.add_relay("wss://relay.snort.social".parse().unwrap()).await.unwrap();

    let fetch2 = fetch.clone();
    tokio::spawn(async move {
        loop {
            fetch2.process_queue().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let rocket = rocket::Rocket::build()
        .manage(db)
        .manage(fetch)
        .attach(Shield::new()) // disable
        .mount("/", events::routes())
        .launch()
        .await;

    if let Err(e) = rocket {
        error!("Rocker error {}", e);
        Err(Error::from(e))
    } else {
        Ok(())
    }
}