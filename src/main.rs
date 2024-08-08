#[macro_use]
extern crate rocket;

use anyhow::Error;
use nostr::Event;
use nostr_database::{DynNostrDatabase, NostrDatabase};
use rocket::shield::Shield;

use crate::store::SledDatabase;

mod events;
mod store;

#[rocket::main]
async fn main() -> Result<(), Error> {
    pretty_env_logger::init();

    let db = SledDatabase::new("nostr.db");

    let rocket = rocket::Rocket::build()
        .manage(db)
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