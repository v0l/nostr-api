use nostr::{Event, FromBech32, PublicKey};
use nostr::prelude::Nip19;
use rocket::{Route, State};
use rocket::http::Status;
use rocket::serde::json::Json;

use crate::fetch::FetchQueue;
use crate::store::SledDatabase;

pub fn routes() -> Vec<Route> {
    routes![import_event, get_event, get_event_by_kind]
}

#[rocket::post("/event", data = "<data>")]
async fn import_event(
    db: &State<SledDatabase>,
    data: Json<Event>,
) -> Status {
    if data.verify().is_err() {
        return Status::InternalServerError;
    }
    if let Ok(v) = db.save_event(&data).await {
        match v {
            true => Status::Ok,
            false => Status::Conflict
        }
    } else {
        Status::InternalServerError
    }
}

#[rocket::get("/event/<id>")]
async fn get_event(
    db: &State<SledDatabase>,
    fetch: &State<FetchQueue>,
    id: &str,
) -> Option<Json<Event>> {
    let id = match Nip19::from_bech32(id) {
        Ok(i) => i,
        _ => return None
    };
    match db.event_by_id(&id) {
        Ok(ev) => Some(Json::from(ev)),
        _ => {
            let mut fetch = fetch.inner().clone();
            match fetch.demand(&id).await.await {
                Ok(Some(ev)) => Some(Json::from(ev)),
                _ => None
            }
        }
    }
}

#[rocket::get("/event/<kind>/<pubkey>")]
fn get_event_by_kind(
    db: &State<SledDatabase>,
    kind: u32,
    pubkey: &str,
) -> Option<Json<Event>> {
    let pk = match PublicKey::parse(pubkey) {
        Ok(i) => i,
        _ => return None
    };
    match db.event_by_kind_pubkey(kind, &pk) {
        Ok(ev) => Some(Json::from(ev)),
        _ => None
    }
}