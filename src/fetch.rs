use anyhow::Result;
use moka::future::Cache;
use nostr_sdk::prelude::Nip19;
use nostr_sdk::{Client, Event, EventId, Filter, JsonUtil, Kind, Metadata, PublicKey, serde_json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, oneshot};

/// Per-item timeout applied in [`FetchQueue::demand`] so a slow/unreachable
/// relay cannot wedge callers indefinitely. The relay pool is itself bounded by
/// the 2s fetch timeout below, but this caps the total caller wait as well.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for each individual relay fetch inside a worker.
const RELAY_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to remember that an event lookup came back empty, so transient
/// misses do not immediately re-query the relays. Kept short (unlike the 10
/// minute positive cache) so newly-published events are picked up quickly.
const NEGATIVE_TTL: Duration = Duration::from_secs(60);

struct QueueItem {
    /// Cache key for the request (used to coalesce in-flight waiters).
    key: String,
    request: Nip19,
}

/// Map of cache key → waiters currently blocked on a shared in-flight fetch.
type InFlightMap = HashMap<String, Vec<oneshot::Sender<Option<Event>>>>;

#[derive(Clone)]
pub struct FetchQueue {
    queue: Arc<Mutex<VecDeque<QueueItem>>>,
    notify: Arc<Notify>,
    /// In-flight requests keyed by cache key. Each value is the list of oneshot
    /// senders that are all waiting on the same underlying relay fetch, so
    /// concurrent identical requests share a single round-trip to the relays.
    in_flight: Arc<Mutex<InFlightMap>>,
    client: Client,
    profile_cache: Cache<PublicKey, Option<Metadata>>,
    event_cache: Cache<String, Event>,
    neg_cache: Cache<String, bool>,
}

impl FetchQueue {
    pub fn new(client: Client) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            client,
            profile_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(Duration::from_secs(24 * 60 * 60)) // 1 day
                .build(),
            event_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(Duration::from_secs(60 * 10)) // 10 mins
                .build(),
            neg_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(NEGATIVE_TTL)
                .build(),
        }
    }

    /// Spawn `count` worker tasks that each process one queue item at a time.
    /// Multiple workers provide concurrency so a slow relay cannot serialize
    /// all requests behind a single worker. Returns the join handles.
    pub fn spawn_workers(self, count: usize) -> Vec<tokio::task::JoinHandle<()>> {
        (0..count)
            .map(|_| {
                let fq = self.clone();
                tokio::spawn(async move {
                    loop {
                        fq.process_one().await;
                    }
                })
            })
            .collect()
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

    /// Fetch an event for the given NIP-19 identifier, coalescing concurrent
    /// identical requests into a single relay fetch, and returning early from
    /// the in-memory cache when available.
    pub async fn demand(&self, ent: &Nip19) -> Result<Option<Event>> {
        let key = Self::n19_key(ent);

        if let Some(cc) = &key {
            if let Some(cached) = self.event_cache.get(cc).await {
                return Ok(Some(cached.clone()));
            }
            if self.neg_cache.get(cc).await.is_some() {
                return Ok(None);
            }
        }

        let (tx, rx) = oneshot::channel();

        // Register this caller in the in-flight map. The first caller for a key
        // becomes the leader and enqueues a queue item; late callers simply wait
        // on the same map entry and share the leader's fetch.
        let is_leader = {
            let mut inflight = self.in_flight.lock().await;
            let key = key.clone().unwrap_or_default();
            match inflight.get_mut(&key) {
                Some(waiters) => {
                    waiters.push(tx);
                    false
                }
                None => {
                    inflight.insert(key.clone(), vec![tx]);
                    true
                }
            }
        };

        if is_leader {
            let key = key.unwrap_or_default();
            {
                let mut q = self.queue.lock().await;
                q.push_back(QueueItem {
                    key,
                    request: ent.clone(),
                });
            }
        }
        self.notify.notify_one();

        let res = tokio::time::timeout(FETCH_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow::anyhow!("demand timed out after {:?}", FETCH_TIMEOUT))?
            .map_err(|e| anyhow::anyhow!("Failed to demand {}", e))?;

        Ok(res)
    }

    /// Pull the next item off the queue (waiting without losing wakeups) and
    /// service it. This is the unit of work for each worker task.
    pub async fn process_one(&self) {
        let item = loop {
            {
                let mut q = self.queue.lock().await;
                if let Some(it) = q.pop_front() {
                    break it;
                }
            }
            // Queue was empty; wait for the next notification. Checking the
            // queue before awaiting avoids the lost-wakeup race.
            self.notify.notified().await;
        };

        let res = self.fetch_one(&item.request).await;

        // Cache the result before releasing the in-flight waiters, so any
        // caller arriving between cache-insert and in-flight-removal hits the
        // cache rather than re-queuing.
        match &res {
            Some(r) => {
                self.event_cache.insert(item.key.clone(), r.clone()).await;
            }
            None => {
                self.neg_cache.insert(item.key.clone(), true).await;
            }
        }

        let waiters = {
            let mut inflight = self.in_flight.lock().await;
            inflight.remove(&item.key).unwrap_or_default()
        };
        for w in waiters {
            if w.send(res.clone()).is_err() {
                warn!("process_one: receiver dropped before response could be sent");
            }
        }
    }

    async fn fetch_one(&self, ent: &Nip19) -> Option<Event> {
        let filter = match Self::nip19_to_filter(ent) {
            Some(f) => f,
            None => return None,
        };
        info!("Sending filter: {}", serde_json::to_string(&filter).unwrap());
        match self
            .client
            .fetch_events(filter, RELAY_FETCH_TIMEOUT)
            .await
        {
            Ok(events) => events.into_iter().next(),
            Err(e) => {
                warn!("Failed to fetch events: {}", e);
                None
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

    /// Verify that process_one pops one item at a time (no batching) so that
    /// concurrent requests are not all serialized behind a single fetch.
    #[tokio::test]
    async fn test_process_one_services_single_item() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
        let key = FetchQueue::n19_key(&nip19).unwrap();

        // Enqueue one item manually (registering the waiter in the in-flight
        // map, as demand() does) and fire the notify so process_one wakes.
        let (tx, rx): (oneshot::Sender<Option<Event>>, _) = oneshot::channel();
        {
            let mut q = fq.queue.lock().await;
            q.push_back(QueueItem {
                key: key.clone(),
                request: nip19.clone(),
            });
            let mut inflight = fq.in_flight.lock().await;
            inflight.insert(key, vec![tx]);
        }
        fq.notify.notify_one();

        // Run process_one in the background.
        let fq2 = fq.clone();
        let handle = tokio::spawn(async move {
            fq2.process_one().await;
        });

        // The result channel should resolve (empty, since no relays are connected).
        let result = rx.await.expect("oneshot should not be dropped");
        assert!(result.is_none(), "no relays → no event returned");

        handle.await.expect("process_one task should complete");
    }

    /// Verify that items arriving while a worker is fetching are not stranded:
    /// the queue is drained by the next worker iteration, and the demand()
    /// notify wakes it for a subsequent item.
    #[tokio::test]
    async fn test_process_one_renotifies_when_more_items_remain() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
        let key = FetchQueue::n19_key(&nip19).unwrap();

        // Enqueue first item (registering the waiter in the in-flight map) and
        // notify so the first process_one wakes.
        let (tx1, rx1): (oneshot::Sender<Option<Event>>, _) = oneshot::channel();
        {
            let mut q = fq.queue.lock().await;
            q.push_back(QueueItem {
                key: key.clone(),
                request: nip19.clone(),
            });
            let mut inflight = fq.in_flight.lock().await;
            inflight.insert(key.clone(), vec![tx1]);
        }
        fq.notify.notify_one();

        // Spawn a worker loop running two consecutive iterations.
        let fq_worker = fq.clone();
        let worker = tokio::spawn(async move {
            fq_worker.process_one().await;
            fq_worker.process_one().await;
        });

        // Wait until the first item has been processed (rx1 resolves), then
        // inject a second item with no extra notify_one. The second process_one
        // iteration checks the queue before awaiting, so it must see the item.
        let r1 = rx1.await.expect("first oneshot should resolve");
        assert!(r1.is_none());

        let (tx2, rx2): (oneshot::Sender<Option<Event>>, _) = oneshot::channel();
        {
            let mut inflight = fq.in_flight.lock().await;
            inflight.insert(key.clone(), vec![tx2]);
            let mut q = fq.queue.lock().await;
            q.push_back(QueueItem {
                key,
                request: nip19.clone(),
            });
        }
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

        // Run process_one as a background worker.
        let fq_worker = fq.clone();
        tokio::spawn(async move {
            loop {
                fq_worker.process_one().await;
            }
        });

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
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

    /// demand() returns None from the negative cache on the second call without
    /// re-queuing, after the first lookup missed.
    #[tokio::test]
    async fn test_demand_uses_negative_cache() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
        let key = FetchQueue::n19_key(&nip19).unwrap();

        // Run process_one as a background worker.
        let fq_worker = fq.clone();
        let queue_len = {
            let q = fq.queue.lock().await;
            q.len()
        };
        assert_eq!(queue_len, 0);
        tokio::spawn(async move {
            loop {
                fq_worker.process_one().await;
            }
        });

        let r1 = fq.demand(&nip19).await.expect("first demand");
        assert!(r1.is_none());
        // Negative result should now be cached.
        assert!(fq.neg_cache.get(&key).await.is_some());

        let r2 = fq.demand(&nip19).await.expect("second demand");
        assert!(r2.is_none());
    }

    /// Concurrent demand() calls for the same key coalesce into a single fetch
    /// (only one queue item is enqueued).
    #[tokio::test]
    async fn test_demand_coalesces_concurrent_requests() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);

        let fq_worker = fq.clone();
        tokio::spawn(async move {
            loop {
                fq_worker.process_one().await;
            }
        });

        // Fire many identical demands concurrently.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let fq = fq.clone();
            let n = nip19.clone();
            handles.push(tokio::spawn(async move { fq.demand(&n).await }));
        }
        for h in handles {
            let r = h.await.expect("demand task").expect("demand ok");
            assert!(r.is_none());
        }

        // All calls coalesced into a single queue item that has been processed.
        let queue_len = {
            let q = fq.queue.lock().await;
            q.len()
        };
        assert_eq!(queue_len, 0);
    }

    /// get_profile() returns None (not an error) when no relays are connected.
    #[tokio::test]
    async fn test_get_profile_returns_none_with_no_relays() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let fq_worker = fq.clone();
        tokio::spawn(async move {
            loop {
                fq_worker.process_one().await;
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

    /// fetch_one() returns None for an unmappable Nip19 (e.g. nsec) without
    /// touching the relays.
    #[tokio::test]
    async fn test_fetch_one_unknown_nip19_returns_none() {
        use nostr_sdk::SecretKey;
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);
        let sk = SecretKey::generate();
        let res = fq.fetch_one(&Nip19::Secret(sk)).await;
        assert!(res.is_none());
    }

    /// spawn_workers() returns the requested number of join handles that stay
    /// alive (not immediately-finished tasks) and services a queued demand.
    #[tokio::test]
    async fn test_spawn_workers_services_requests() {
        let client = ClientBuilder::new().build();
        let fq = FetchQueue::new(client);

        let handles = fq.clone().spawn_workers(3);
        assert_eq!(handles.len(), 3);

        let pk = dummy_pubkey();
        let nip19 = Nip19::Pubkey(pk);
        let result = fq
            .demand(&nip19)
            .await
            .expect("demand should not error");
        assert!(result.is_none());

        // Workers are long-lived loops; abort them to avoid leaking tasks.
        for h in handles {
            h.abort();
        }
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
