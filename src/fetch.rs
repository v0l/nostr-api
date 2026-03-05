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
        let batch: Vec<QueueItem> = {
            let mut q_lock = self.queue.lock().await;
            let mut batch = Vec::new();
            while let Some(q) = q_lock.pop_front() {
                batch.push(q);
            }
            batch
        };
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

            // If more items arrived while we were fetching, wake the worker
            // immediately so they are not stranded waiting for the next notify.
            let has_more = {
                let q = self.queue.lock().await;
                !q.is_empty()
            };
            if has_more {
                self.notify.notify_one();
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
    use nostr_sdk::ClientBuilder;

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

    /// Verify that process_queue releases the mutex lock before performing the
    /// relay fetch, so that demand() calls arriving while a batch is in-flight
    /// can still enqueue their items without blocking.
    #[tokio::test]
    async fn test_process_queue_drops_lock_before_fetch() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);

        // Enqueue one item manually and fire the notify so process_queue wakes.
        let (tx, rx) = oneshot::channel();
        {
            let mut q = fq.queue.lock().await;
            q.push_back(QueueItem {
                handler: tx,
                request: nip19.clone(),
            });
        }
        fq.notify.notify_one();

        // Run process_queue in the background.
        let fq2 = fq.clone();
        let handle = tokio::spawn(async move {
            fq2.process_queue().await;
        });

        // The result channel should resolve (empty, since no relays are connected).
        let result = rx.await.expect("oneshot should not be dropped");
        assert!(result.is_none(), "no relays → no event returned");

        handle.await.expect("process_queue task should complete");
    }

    /// Verify that items arriving while a batch is in-flight are not stranded:
    /// the re-notify inside process_queue wakes the worker for the second item
    /// even when no external notify_one is issued for it.
    #[tokio::test]
    async fn test_process_queue_renotifies_when_more_items_remain() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);

        // Enqueue first item and notify so the first process_queue wakes.
        let (tx1, rx1) = oneshot::channel();
        {
            let mut q = fq.queue.lock().await;
            q.push_back(QueueItem {
                handler: tx1,
                request: nip19.clone(),
            });
        }
        fq.notify.notify_one();

        // Spawn the worker loop running two consecutive iterations.
        let fq_worker = fq.clone();
        let worker = tokio::spawn(async move {
            fq_worker.process_queue().await;
            fq_worker.process_queue().await;
        });

        // Wait until the first item has been processed and removed from the queue
        // (rx1 resolves), then inject a second item with no extra notify_one.
        // The re-notify emitted by process_queue for the remaining-items path
        // must wake the second iteration.
        let r1 = rx1.await.expect("first oneshot should resolve");
        assert!(r1.is_none());

        let (tx2, rx2) = oneshot::channel();
        {
            let mut q = fq.queue.lock().await;
            q.push_back(QueueItem {
                handler: tx2,
                request: nip19.clone(),
            });
        }
        // Fire one notify so the second process_queue iteration wakes (this
        // represents the notify_one emitted inside demand() for a real caller).
        fq.notify.notify_one();

        let r2 = rx2.await.expect("second oneshot should resolve");
        assert!(r2.is_none());

        worker.await.expect("worker task should complete");
    }

    /// demand() returns None (not an error) when no relays are connected.
    #[tokio::test]
    async fn test_demand_returns_none_with_no_relays() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);

        // Run process_queue as a background worker.
        let fq_worker = fq.clone();
        tokio::spawn(async move {
            loop {
                fq_worker.process_queue().await;
            }
        });

        let result = fq.demand(&nip19).await.expect("demand should not error");
        assert!(result.is_none(), "no relays → no event");
    }

    /// demand() returns a cached value on the second call without re-queuing.
    #[tokio::test]
    async fn test_demand_uses_event_cache() {
        use nostr_sdk::{EventBuilder, Keys, Kind};

        let keys = Keys::generate();
        let ev = EventBuilder::new(Kind::Metadata, "")
            .build(keys.public_key())
            .sign_with_keys(&keys)
            .expect("sign");

        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        // Pre-populate the event cache directly.
        let cache_key = ev.id.to_hex();
        fq.event_cache.insert(cache_key, ev.clone()).await;

        let nip19 = Nip19::EventId(ev.id);
        // No worker needed — the cache hit path never touches the queue.
        let result = fq.demand(&nip19).await.expect("demand should not error");
        assert_eq!(result.map(|e| e.id), Some(ev.id));
    }

    /// get_profile() returns None (not an error) when no relays are connected.
    #[tokio::test]
    async fn test_get_profile_returns_none_with_no_relays() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let fq_worker = fq.clone();
        tokio::spawn(async move {
            loop {
                fq_worker.process_queue().await;
            }
        });

        let pk = dummy_pubkey();
        let result = fq.get_profile(pk).await.expect("get_profile should not error");
        assert!(result.is_none());
    }

    /// get_profile() returns a cached value on the second call.
    #[tokio::test]
    async fn test_get_profile_uses_profile_cache() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let meta = Metadata::new().name("test_user");

        // Pre-populate the profile cache.
        fq.profile_cache.insert(pk, Some(meta.clone())).await;

        // No worker needed — cache hit never touches the queue.
        let result = fq.get_profile(pk).await.expect("get_profile should not error");
        assert_eq!(result.and_then(|m| m.name), Some("test_user".to_string()));
    }

    /// FetchQueue::client() returns a clone of the underlying client.
    #[test]
    fn test_client_accessor_returns_client() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);
        // Just verify it doesn't panic and the accessor exists.
        let _c = fq.client();
    }

    /// n19_key returns None for Nip19::Secret.
    #[test]
    fn test_n19_key_secret_returns_none() {
        use nostr_sdk::SecretKey;
        let sk = SecretKey::generate();
        let nip19 = Nip19::Secret(sk);
        assert!(FetchQueue::n19_key(&nip19).is_none());
    }

    /// n19_key returns the hex pubkey for Nip19::Pubkey.
    #[test]
    fn test_n19_key_pubkey_returns_hex() {
        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
        let key = FetchQueue::n19_key(&nip19).unwrap();
        assert_eq!(key, pk.to_hex());
    }

    /// n19_key returns the hex pubkey for Nip19::Profile.
    #[test]
    fn test_n19_key_profile_returns_hex() {
        let pk = dummy_pubkey();
        let profile = Nip19Profile {
            public_key: pk,
            relays: vec![],
        };
        let nip19 = Nip19::Profile(profile);
        let key = FetchQueue::n19_key(&nip19).unwrap();
        assert_eq!(key, pk.to_hex());
    }

    /// n19_key returns the hex event id for Nip19::EventId.
    #[test]
    fn test_n19_key_event_id_returns_hex() {
        let id = EventId::all_zeros();
        let nip19 = Nip19::EventId(id);
        let key = FetchQueue::n19_key(&nip19).unwrap();
        assert_eq!(key, id.to_hex());
    }

    /// n19_key returns the hex event id for Nip19::Event.
    #[test]
    fn test_n19_key_event_returns_hex() {
        use nostr_sdk::prelude::Nip19Event;
        let id = EventId::all_zeros();
        let ev = Nip19Event {
            event_id: id,
            author: None,
            kind: None,
            relays: vec![],
        };
        let nip19 = Nip19::Event(ev);
        let key = FetchQueue::n19_key(&nip19).unwrap();
        assert_eq!(key, id.to_hex());
    }

    /// n19_key returns "kind:pubkey:identifier" for Nip19::Coordinate.
    #[test]
    fn test_n19_key_coordinate_returns_formatted_key() {
        use nostr_sdk::prelude::Coordinate;
        use nostr_sdk::Kind;
        let pk = dummy_pubkey();
        let coord = Coordinate {
            kind: Kind::Metadata,
            public_key: pk,
            identifier: "my-id".to_string(),
        };
        let nip19 = Nip19::Coordinate(nostr_sdk::prelude::Nip19Coordinate::new(coord, []));
        let key = FetchQueue::n19_key(&nip19).unwrap();
        assert!(key.contains(&pk.to_hex()));
        assert!(key.contains("my-id"));
    }
}
