use anyhow::Result;
use moka::future::Cache;
use nostr_sdk::filter::MatchEventOptions;
use nostr_sdk::prelude::{Events, Nip19};
use nostr_sdk::{Client, Event, EventId, Filter, JsonUtil, Kind, Metadata, PublicKey, serde_json};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinSet;

struct QueueItem {
    pub handler: oneshot::Sender<Option<Event>>,
    pub request: Nip19,
}

#[derive(Clone)]
pub struct FetchQueue {
    queue: Arc<Mutex<VecDeque<QueueItem>>>,
    notify: Arc<Notify>,
    client: Client,
    profile_cache: Cache<PublicKey, Option<Metadata>>,
    event_cache: Cache<String, Event>,
}

impl FetchQueue {
    pub fn new(client: Client) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            client,
            profile_cache: Cache::builder()
                .time_to_live(Duration::from_secs(24 * 60 * 60)) // 1 day
                .build(),
            event_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 10)) // 10 mins
                .build(),
        }
    }

    pub fn client(&self) -> Client {
        self.client.clone()
    }

    fn n19_key(n19: &Nip19) -> Option<String> {
        match n19 {
            Nip19::Pubkey(p) => Some(p.to_hex()),
            Nip19::Profile(p) => Some(p.public_key.to_hex()),
            Nip19::EventId(i) => Some(i.to_hex()),
            Nip19::Event(i) => Some(i.event_id.to_hex()),
            Nip19::Coordinate(c) => Some(format!(
                "{}:{}:{}",
                c.kind,
                c.public_key.to_hex(),
                c.coordinate
            )),
            _ => None,
        }
    }

    pub async fn get_profile(&self, pubkey: PublicKey) -> Result<Option<Metadata>> {
        if let Some(r) = self.profile_cache.get(&pubkey).await {
            Ok(r)
        } else {
            let p = self.demand(&Nip19::Pubkey(pubkey)).await?;
            let p = p.and_then(|x| Metadata::from_json(x.content).ok());
            self.profile_cache.insert(pubkey, p.clone()).await;
            Ok(p)
        }
    }

    pub async fn demand(&self, ent: &Nip19) -> Result<Option<Event>> {
        let cache_key = Self::n19_key(ent);
        if let Some(cc) = &cache_key
            && let Some(cached) = self.event_cache.get(cc).await
        {
            return Ok(Some(cached.clone()));
        }

        let (tx, rx) = oneshot::channel();

        {
            let mut q_lock = self.queue.lock().await;
            q_lock.push_back(QueueItem {
                handler: tx,
                request: ent.clone(),
            });
        }
        self.notify.notify_one();
        let res = rx
            .await
            .map_err(|e| anyhow::anyhow!("Failed to demand {}", e))?;

        if let Some(r) = &res
            && let Some(cc) = cache_key
        {
            self.event_cache.insert(cc, r.clone()).await;
        }
        Ok(res)
    }

    pub async fn process_queue(&self) {
        self.notify.notified().await;
        let mut q_lock = self.queue.lock().await;
        let mut batch = Vec::new();
        while let Some(q) = q_lock.pop_front() {
            batch.push(q);
        }
        if !batch.is_empty() {
            let filters: Vec<Filter> = batch
                .iter()
                .map(move |x| Self::nip19_to_filter(&x.request).unwrap())
                .collect();

            info!(
                "Sending filters: {}",
                serde_json::to_string(&filters).unwrap()
            );
            let mut join_set = JoinSet::new();
            for filter in filters {
                let client = self.client.clone();
                join_set.spawn(async move {
                    client.fetch_events(filter, Duration::from_secs(2)).await
                });
            }
            let mut all_events = Events::default();
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(events)) => {
                        events.into_iter().for_each(|e| {
                            all_events.insert(e);
                        });
                    }
                    Ok(Err(e)) => warn!("Failed to fetch events: {}", e),
                    Err(e) => warn!("Fetch task panicked: {}", e),
                }
            }
            for b in batch {
                let f = Self::nip19_to_filter(&b.request).unwrap();
                let ev = all_events
                    .iter()
                    .find(|e| f.match_event(e, MatchEventOptions::new()));
                if b.handler.send(ev.cloned()).is_err() {
                    warn!("process_queue: receiver dropped before response could be sent");
                }
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
            Nip19::Profile(p) => Some(
                Filter::new()
                    .author(p.public_key)
                    .kind(Kind::Metadata),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::prelude::Nip19Profile;

    fn dummy_pubkey() -> PublicKey {
        PublicKey::from_hex(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap()
    }

    #[test]
    fn test_nip19_to_filter_pubkey_returns_metadata_filter() {
        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
        let filter = FetchQueue::nip19_to_filter(&nip19).unwrap();
        // Should be a metadata filter for this author
        let json = serde_json::to_value(&filter).unwrap();
        let kinds = json["kinds"].as_array().unwrap();
        assert!(kinds.iter().any(|k| k.as_u64() == Some(0)));
    }

    #[test]
    fn test_nip19_to_filter_profile_returns_metadata_filter() {
        let pk = dummy_pubkey();
        let profile = Nip19Profile {
            public_key: pk,
            relays: vec![],
        };
        let nip19 = Nip19::Profile(profile);
        let filter = FetchQueue::nip19_to_filter(&nip19).unwrap();
        let json = serde_json::to_value(&filter).unwrap();
        let kinds = json["kinds"].as_array().unwrap();
        assert!(kinds.iter().any(|k| k.as_u64() == Some(0)));
    }

    #[test]
    fn test_nip19_to_filter_event_id() {
        let event_id = EventId::all_zeros();
        let nip19 = Nip19::EventId(event_id);
        let filter = FetchQueue::nip19_to_filter(&nip19);
        assert!(filter.is_some());
    }

    #[test]
    fn test_nip19_to_filter_unknown_returns_none() {
        // Nip19::Secret is the remaining catch-all
        use nostr_sdk::SecretKey;
        let sk = SecretKey::generate();
        let nip19 = Nip19::Secret(sk);
        let filter = FetchQueue::nip19_to_filter(&nip19);
        assert!(filter.is_none());
    }
}
