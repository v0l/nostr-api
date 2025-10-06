use anyhow::Result;
use nostr_sdk::prelude::{Nip19, Nip19Event, RejectedReason, SaveEventStatus};
use nostr_sdk::{Event, EventId, FromBech32, Kind, PublicKey};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::{Route, State};

use crate::fetch::FetchQueue;

pub fn routes() -> Vec<Route> {
    routes![import_event, get_event, get_event_by_kind]
}

#[rocket::post("/event", data = "<data>")]
async fn import_event(fetch: &State<FetchQueue>, data: Json<Event>) -> Status {
    if data.verify().is_err() {
        return Status::InternalServerError;
    }
    let client = fetch.client();
    let db = client.database();
    if let Ok(v) = db.save_event(&data).await {
        match v {
            SaveEventStatus::Success => Status::Ok,
            SaveEventStatus::Rejected(r) => match r {
                RejectedReason::Duplicate => Status::Conflict,
                _ => Status::InternalServerError,
            },
        }
    } else {
        Status::InternalServerError
    }
}

#[rocket::get("/event/<id>")]
async fn get_event(fetch: &State<FetchQueue>, id: &str) -> Result<Option<Json<Event>>, Status> {
    let id = Nip19::from_bech32(id).map_err(|_| Status::InternalServerError)?;
    Ok(fetch
        .demand(&id)
        .await
        .map_err(|e| {
            error!("Failed get_event {}", e);
            Status::InternalServerError
        })?
        .map(|r| Json::from(r)))
}

#[rocket::get("/event/<kind>/<pubkey>")]
async fn get_event_by_kind(
    fetch: &State<FetchQueue>,
    kind: u32,
    pubkey: &str,
) -> Result<Option<Json<Event>>, Status> {
    let pk = PublicKey::parse(pubkey).map_err(|_| Status::InternalServerError)?;
    let kind = Kind::from(kind as u16);
    if !kind.is_replaceable() {
        return Ok(None);
    }

    let id = Nip19::Event(Nip19Event {
        event_id: EventId::all_zeros(),
        kind: Some(kind),
        author: Some(pk),
        relays: vec![],
    });
    Ok(fetch
        .demand(&id)
        .await
        .map_err(|e| {
            error!("Failed get_event_by_kind {}", e);
            Status::InternalServerError
        })?
        .map(|r| Json::from(r)))
}
