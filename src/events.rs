use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use nostr_sdk::prelude::{Nip19, Nip19Event, RejectedReason, SaveEventStatus};
use nostr_sdk::{Event, EventId, FromBech32, Kind, PublicKey};

use crate::fetch::FetchQueue;

pub async fn import_event(
    State(fetch): State<FetchQueue>,
    Json(data): Json<Event>,
) -> Response {
    if data.verify().is_err() {
        return StatusCode::UNPROCESSABLE_ENTITY.into_response();
    }
    let client = fetch.client();
    let db = client.database();
    match db.save_event(&data).await {
        Ok(v) => match v {
            SaveEventStatus::Success => StatusCode::OK.into_response(),
            SaveEventStatus::Rejected(r) => match r {
                RejectedReason::Duplicate => StatusCode::CONFLICT.into_response(),
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            },
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn get_event(
    State(fetch): State<FetchQueue>,
    Path(id): Path<String>,
) -> Response {
    let id = match Nip19::from_bech32(&id) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match fetch.demand(&id).await {
        Ok(Some(ev)) => Json(ev).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            error!("Failed get_event {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn get_event_by_kind(
    State(fetch): State<FetchQueue>,
    Path((kind, pubkey)): Path<(u32, String)>,
) -> Response {
    let pk = match PublicKey::parse(&pubkey) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if kind > u16::MAX as u32 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let kind = Kind::from(kind as u16);
    if !kind.is_replaceable() {
        return StatusCode::NO_CONTENT.into_response();
    }

    let id = Nip19::Event(Nip19Event {
        event_id: EventId::all_zeros(),
        kind: Some(kind),
        author: Some(pk),
        relays: vec![],
    });
    match fetch.demand(&id).await {
        Ok(Some(ev)) => Json(ev).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            error!("Failed get_event_by_kind {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
