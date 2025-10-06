use anyhow::Result;
use nostr_sdk::filter::MatchEventOptions;
use nostr_sdk::prelude::{Events, Nip19};
use nostr_sdk::{Client, Event, EventId, Filter, Kind, serde_json};
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
}

impl FetchQueue {
    pub fn new(client: Client) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            client,
        }
    }

    pub fn client(&self) -> Client {
        self.client.clone()
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
