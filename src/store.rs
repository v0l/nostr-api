use std::fmt::Debug;
use std::sync::Arc;

use nostr::{Event, EventId};
use nostr_database::{FlatBufferBuilder, FlatBufferDecode, FlatBufferEncode};
use sled::{Db, IVec};
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct SledDatabase {
    db: Db,
    fbb: Arc<Mutex<FlatBufferBuilder<'static>>>,
}

impl SledDatabase {
    pub fn new(path: &str) -> Self {
        Self {
            db: sled::open(path).unwrap(),
            fbb: Arc::new(Mutex::new(FlatBufferBuilder::with_capacity(70_000))),
        }
    }
}

impl SledDatabase {
    pub async fn save_event(&self, event: &Event) -> Result<bool, anyhow::Error> {
        let mut fbb = self.fbb.lock().await;
        if let Err(e) = self.db.insert(event.id.as_bytes(), event.encode(&mut fbb)) {
            return Err(anyhow::Error::new(e));
        }
        if event.kind.is_replaceable() {
            self.write_replaceable_index(event)?;
        }
        Ok(true)
    }

    fn write_replaceable_index(&self, event: &Event) -> Result<bool, anyhow::Error> {
        let rpk = Self::replaceable_index_key(event);

        if let Err(e) = self.db.update_and_fetch(rpk, |prev| {
            if let Some(prev) = prev {
                let timestamp: u64 = u64::from_be_bytes(prev[..8].try_into().unwrap());
                if timestamp < event.created_at.as_u64() {
                    let new_val = Self::replaceable_index_value(event);
                    Some(IVec::from(new_val.as_slice()))
                } else {
                    None
                }
            } else {
                let new_val = Self::replaceable_index_value(event);
                Some(IVec::from(new_val.as_slice()))
            }
        }) {
            return Err(anyhow::Error::new(e));
        }
        Ok(false)
    }

    fn replaceable_index_key(event: &Event) -> [u8; 36] {
        let mut rpk = [0; 4 + 32]; // kind:pubkey
        rpk[..4].copy_from_slice(&event.kind.as_u32().to_be_bytes());
        rpk[4..].copy_from_slice(&event.pubkey.to_bytes());
        rpk
    }

    fn replaceable_index_value(event: &Event) -> [u8; 40] {
        let mut new_val = [0; 8 + 32]; // timestamp:event_id
        new_val[..8].copy_from_slice(&event.created_at.as_u64().to_be_bytes());
        new_val[8..].copy_from_slice(event.id.as_bytes());
        new_val
    }

    pub async fn event_by_id(&self, event_id: EventId) -> Result<Event, anyhow::Error> {
        match self.db.get(event_id.as_bytes()) {
            Ok(v) => match v {
                Some(v) => match Event::decode(&v) {
                    Ok(v) => Ok(v),
                    Err(e) => Err(anyhow::Error::new(e))
                },
                None => Err(anyhow::Error::msg("Not Found"))
            }
            Err(e) => Err(anyhow::Error::new(e))
        }
    }
}