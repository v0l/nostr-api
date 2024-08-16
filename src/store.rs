use std::fmt::Debug;
use std::sync::Arc;

use nostr::prelude::Nip19;
use nostr::util::hex;
use nostr::{Event, EventId, PublicKey};
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
        let rpk = Self::replaceable_index_key_of_event(event);

        match self.db.update_and_fetch(rpk, |prev| {
            if let Some(prev) = prev {
                let timestamp: u64 = u64::from_be_bytes(prev[..8].try_into().unwrap());
                if timestamp < event.created_at.as_u64() {
                    let new_val = Self::replaceable_index_value(event);
                    Some(IVec::from(new_val.as_slice()))
                } else {
                    Some(IVec::from(prev))
                }
            } else {
                let new_val = Self::replaceable_index_value(event);
                Some(IVec::from(new_val.as_slice()))
            }
        }) {
            Err(e) => Err(anyhow::Error::new(e)),
            Ok(v) => match v {
                Some(v) => {
                    info!(
                        "Wrote index {} = {}",
                        hex::encode(rpk),
                        hex::encode(v.as_ref())
                    );
                    Ok(true)
                }
                None => {
                    info!("Duplicate or older index {}", hex::encode(rpk));
                    Ok(false)
                }
            },
        }
    }

    fn replaceable_index_key_of_event(event: &Event) -> [u8; 36] {
        Self::replaceable_index_key(event.kind.as_u32(), &event.pubkey)
    }

    fn replaceable_index_key(kind: u32, pubkey: &PublicKey) -> [u8; 36] {
        let mut rpk = [0; 4 + 32]; // kind:pubkey
        rpk[..4].copy_from_slice(&kind.to_be_bytes());
        rpk[4..].copy_from_slice(&pubkey.to_bytes());
        rpk
    }

    fn replaceable_index_value(event: &Event) -> [u8; 40] {
        let mut new_val = [0; 8 + 32]; // timestamp:event_id
        new_val[..8].copy_from_slice(&event.created_at.as_u64().to_be_bytes());
        new_val[8..].copy_from_slice(event.id.as_bytes());
        new_val
    }

    pub fn event_by_id(&self, event_id: &Nip19) -> Result<Event, anyhow::Error> {
        let id_key = match event_id {
            Nip19::EventId(e) => e.as_bytes(),
            Nip19::Event(e) => e.event_id.as_bytes(),
            _ => return Err(anyhow::Error::msg("Not supported ID format")),
        };
        match self.db.get(id_key) {
            Ok(v) => match v {
                Some(v) => match Event::decode(&v) {
                    Ok(v) => Ok(v),
                    Err(e) => Err(anyhow::Error::new(e)),
                },
                None => Err(anyhow::Error::msg("Not Found")),
            },
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }

    pub fn event_by_kind_pubkey(
        &self,
        kind: u32,
        pubkey: &PublicKey,
    ) -> Result<Event, anyhow::Error> {
        let rpk = Self::replaceable_index_key(kind, pubkey);
        match self.db.get(rpk) {
            Ok(v) => match v {
                Some(v) => self.event_by_id(&Nip19::EventId(EventId::from_slice(v[8..].as_ref())?)),
                None => Err(anyhow::Error::msg("Not Found")),
            },
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
}
