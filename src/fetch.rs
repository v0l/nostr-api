use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use nostr::prelude::Nip19;
use nostr::{Event, EventId, Filter, Kind, Url};
use nostr_sdk::{FilterOptions, RelayOptions, RelayPool};
use tokio::sync::{oneshot, Mutex};

struct QueueItem {
    pub handler: oneshot::Sender<Option<Event>>,
    pub request: Nip19,
}

#[derive(Clone)]
pub struct FetchQueue {
    queue: Arc<Mutex<VecDeque<QueueItem>>>,
    pool: Arc<Mutex<RelayPool>>,
}

impl FetchQueue {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            pool: Default::default(),
        }
    }

    pub async fn add_relay(&mut self, relay: Url) -> Result<bool, anyhow::Error> {
        let pool_lock = self.pool.lock().await;
        pool_lock
            .add_relay(relay.clone(), RelayOptions::default())
            .await?;
        if let Err(e) = pool_lock.connect_relay(relay, None).await {
            Err(anyhow::Error::new(e))
        } else {
            Ok(true)
        }
    }

    pub async fn demand(&mut self, ent: &Nip19) -> oneshot::Receiver<Option<Event>> {
        let (tx, rx) = oneshot::channel();

        let mut q_lock = self.queue.lock().await;
        q_lock.push_back(QueueItem {
            handler: tx,
            request: ent.clone(),
        });
        rx
    }

    pub async fn process_queue(&self) {
        let mut q_lock = self.queue.lock().await;
        let mut batch = Vec::new();
        while let Some(q) = q_lock.pop_front() {
            batch.push(q);
        }
        let filters: Vec<Filter> =
            batch.iter().map(move |x| Self::nip19_to_filter(&x.request).unwrap()).collect();

        let pool_lock = self.pool.lock().await;
        info!("Sending filters: {:?}", &filters);
        if let Ok(evs) = pool_lock
            .get_events_of(filters, Duration::from_secs(2), FilterOptions::ExitOnEOSE)
            .await
        {
            for b in batch {
                let f = Self::nip19_to_filter(&b.request).unwrap();
                let ev = evs.iter().find(|e| f.match_event(e));
                b.handler.send(ev.cloned()).unwrap()
            }
        }
    }

    fn nip19_to_filter(filter: &Nip19) -> Option<Filter> {
        match filter {
            Nip19::Coordinate(c) => Some(Filter::from(c)),
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
            Nip19::EventId(e) => Some(Filter::new().id(*e)),
            Nip19::Pubkey(pk) => Some(Filter::new().author(*pk).kind(Kind::Metadata)),
            _ => None,
        }
    }
}
