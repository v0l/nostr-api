use anyhow::Error;
use nostr::{Event, EventId, JsonUtil, Kind};
use nostr_database::{DatabaseError, DynNostrDatabase, NostrDatabase};
use rocket::{Data, Route, State};
use rocket::http::Status;
use rocket::serde::json::Json;
use crate::store::SledDatabase;

pub fn routes() -> Vec<Route> {
    routes![import_event, get_event]
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
    id: &str,
) -> Option<Json<Event>> {
    let id = match EventId::parse(id) {
        Ok(i) => i,
        _ => return None
    };
    match db.event_by_id(id).await {
        Ok(ev) => Some(Json::from(ev)),
        _ => None
    }
}