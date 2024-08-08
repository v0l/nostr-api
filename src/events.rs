use anyhow::Error;
use nostr::{Event, EventId, JsonUtil, Kind, PublicKey};
use nostr_database::{DatabaseError, DynNostrDatabase, NostrDatabase};
use rocket::{Route, State};
use rocket::http::Status;
use rocket::serde::json::Json;

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
fn get_event(
    db: &State<SledDatabase>,
    id: &str,
) -> Option<Json<Event>> {
    let id = match EventId::parse(id) {
        Ok(i) => i,
        _ => return None
    };
    match db.event_by_id(id) {
        Ok(ev) => Some(Json::from(ev)),
        _ => None
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