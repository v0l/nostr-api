#[macro_use]
extern crate rocket;

use std::time::Duration;

use anyhow::Error;
use config::Config;
use rocket::shield::Shield;

use crate::fetch::FetchQueue;
use crate::settings::Settings;
use crate::store::SledDatabase;

mod events;
mod fetch;
mod store;
mod settings;

#[rocket::main]
async fn main() -> Result<(), Error> {
    pretty_env_logger::init();

    let builder = Config::builder()
        .add_source(config::File::with_name("config.toml"))
        .add_source(config::Environment::with_prefix("APP"))
        .build()?;

    let settings: Settings = builder.try_deserialize()?;

    let db = SledDatabase::new("nostr.db");

    let mut fetch = FetchQueue::new();
    for x in settings.relays
    {
        fetch.add_relay(x).await.unwrap();
    }


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
