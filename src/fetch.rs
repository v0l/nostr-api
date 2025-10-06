use anyhow::Result;
use moka::future::Cache;
use nostr_sdk::filter::MatchEventOptions;
use nostr_sdk::prelude::{Events, Nip19};
use nostr_sdk::{Client, Event, EventId, Filter, JsonUtil, Kind, Metadata, PublicKey, serde_json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, oneshot};

struct QueueItem {
    pub handler: oneshot::Sender<Option<Event>>,
    pub request: Nip19,
}

#[derive(Clone)]
pub struct FetchQueue {
    queue: Arc<Mutex<VecDeque<QueueItem>>>,
    client: Client,
    profile_cache: Cache<PublicKey, Option<Metadata>>,
}

impl FetchQueue {
    pub fn new(client: Client) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            client,
            profile_cache: Cache::builder()
                .time_to_live(Duration::from_secs(24 * 60 * 60)) // 1 day
                .build(),
        }
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    pub async fn get_profile(&self, pubkey: PublicKey) -> Result<Option<Metadata>> {
        if let Some(r) = self.profile_cache.get(&pubkey).await {
            Ok(r)
        } else {
            let p = self
                .demand(&Nip19::Pubkey(pubkey))
                .await?;
            let p = p.and_then(|x| Metadata::from_json(x.content).ok());
            self.profile_cache.insert(pubkey, p.clone()).await;
            Ok(p)
        }
    }

    pub async fn demand(&self, ent: &Nip19) -> Result<Option<Event>> {
        let (tx, rx) = oneshot::channel();

        {
            let mut q_lock = self.queue.lock().await;
            q_lock.push_back(QueueItem {
                handler: tx,
                request: ent.clone(),
            });
        }
        rx.await
            .map_err(|e| anyhow::anyhow!("Failed to demand {}", e))
    }

    pub async fn process_queue(&self) {
        let mut q_lock = self.queue.lock().await;
        let mut batch = Vec::new();
        while let Some(q) = q_lock.pop_front() {
            batch.push(q);
        }
        if batch.len() > 0 {
            let filters: Vec<Filter> = batch
                .iter()
                .map(move |x| Self::nip19_to_filter(&x.request).unwrap())
                .collect();

            info!(
                "Sending filters: {}",
                serde_json::to_string(&filters).unwrap()
            );
            let mut all_events = Events::default();
            for filter in filters {
                match self
                    .client
                    .fetch_events(filter, Duration::from_secs(2))
                    .await
                {
                    Ok(events) => {
                        events.into_iter().for_each(|e| {
                            all_events.insert(e);
                        });
                    }
                    Err(e) => {
                        warn!("Failed to fetch events: {}", e);
                    }
                }
            }
            for b in batch {
                let f = Self::nip19_to_filter(&b.request).unwrap();
                let ev = all_events
                    .iter()
                    .find(|e| f.match_event(e, MatchEventOptions::new()));
                b.handler.send(ev.cloned()).unwrap()
            }
        }
    }

    fn nip19_to_filter(nip19: &Nip19) -> Option<Filter> {
        match nip19.clone() {
            Nip19::Coordinate(c) => Some(
                Filter::new()
                    .author(c.public_key)
                    .kind(c.kind)
                    .identifier(&c.identifier),
            ),
            Nip19::Event(e) => {
                let mut f = Filter::new();
                if e.event_id.ne(&EventId::all_zeros()) {
                    f = f.id(e.event_id);
                }
                if let Some(a) = e.author {
                    f = f.author(a);
                }
                if let Some(k) = e.kind {
                    f = f.kind(k);
                }
                Some(f)
            }
            Nip19::EventId(e) => Some(Filter::new().id(e)),
            Nip19::Pubkey(pk) => Some(Filter::new().author(pk).kind(Kind::Metadata)),
            _ => None,
        }
    }
}
