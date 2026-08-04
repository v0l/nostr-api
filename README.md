# nostr-services-rs

A lightweight HTTP API that powers Nostr web applications: serve events and profiles on demand from configured relays, and make pages look great when shared — link previews, deterministic avatars, and OpenGraph tag injection — all in one small service.

Built with Rust, [axum](https://github.com/tokio-rs/axum), and [nostr-sdk](https://github.com/rust-nostr/nostr).

## Features

- **Event API** — fetch Nostr events by NIP-19 identifier, or import/verify and serve them by kind + pubkey.
- **OpenGraph injection** — POST any HTML document with a Nostr identifier and get `og:*` and `twitter:*` meta tags (plus JSON-LD) injected for rich social-media previews. Handles profiles, notes, live streams, and stream clips.
- **Link previews** — scrape OpenGraph metadata from arbitrary URLs, with positive and negative caching.
- **Deterministic avatars** — hash-based avatar selection from bundled image sets (`cyberpunks`, `robots`, `zombies`), served straight from memory with `ETag`/`304` support.
- **NIP-05 resolution** — profile identifiers like `name@domain.tld` are resolved automatically, cached in-process.

## Quick start

### From source

```sh
cargo run --release
```

Listens on `0.0.0.0:8000` by default.

### Docker

```sh
docker build -t nostr-services-rs .
docker run -p 8000:8000 -v ./config.yaml:/app/config.yaml nostr-services-rs
```

## Configuration

Create a `config.yaml` in the working directory (or use `APP_`-prefixed environment variables):

```yaml
# Address to listen on (default: 0.0.0.0:8000)
listen: "0.0.0.0:8000"

# Nostr relays to fetch events and profiles from
relays:
  - "wss://relay.snort.social"
  - "wss://relay.damus.io"
  - "wss://nos.lol"
  - "wss://relay.primal.net"
```

## API

Interactive Swagger docs are served at `/` when the server is running. The raw OpenAPI spec is at `/openapi.yaml`. Production instance: `https://nostr-rs-api.v0l.io`.

### Events

```
POST /event                        Import a signed Nostr event (verified before storage)
GET  /event/{id}                   Fetch an event by NIP-19 id (note1…/nevent1…/naddr1…)
GET  /event/{kind}/{pubkey}        Fetch the latest replaceable event by kind + hex pubkey
```

`POST /event` returns `200` on success, `409` if the event already exists, and `500` if verification or storage fails. `GET /event/...` returns `204` (No Content) when the event is not found, and only works with replaceable kinds (e.g. kind 0, 1xxxxx, 3xxxxx) on the kind+pubkey route.

### OpenGraph injection

Inject `og:*` / `twitter:*` meta tags into an HTML document for a given Nostr identifier. Intended to be called by an SSR proxy or edge function before returning a page to a social-media crawler.

```
POST /opengraph/{id}
Content-Type: text/html

<!DOCTYPE html><html><head>...</head><body>...</body></html>
```

`{id}` can be any NIP-19 bech32 identifier (`npub1…`, `nprofile1…`, `note1…`, `nevent1…`, `naddr1…`) or a NIP-05 address (`name@domain.tld`).

Optional query parameter `canonical` — a URL template with `%s` as the placeholder for the bech32 ID. When provided, the corresponding `<link rel="canonical">` tag is injected (using `naddr` format for addressable events).

```
POST /opengraph/kieran@snort.social
POST /opengraph/npub1xtscya34g58tk0z605fvr788k263gsu6cy9x0mhnm87echrgufzsevkk5s
POST /opengraph/note1…?canonical=https://snort.social/%s
```

The endpoint returns the original HTML unchanged if the identifier cannot be resolved.

### Link previews

```
GET /preview?url={url}             Scrape OpenGraph metadata from a URL
```

Only `https` URLs with public hosts are accepted (private/loopback addresses are rejected). Returns `title`, `description`, `image`, and the raw `og:*` tag pairs as JSON. Successful results are cached for 24 hours; failed fetches are cached for 10 minutes so a bad URL isn't hammered.

### Avatars

```
GET /avatar/{set}/{value}          Deterministic avatar image (webp)
```

Available sets: `cyberpunks`, `robots`, `zombies`. The `value` (any string; a trailing file extension is ignored) is hashed with SHA-256 to deterministically pick an image from the set. Images are loaded into memory at startup and served with a content-derived `ETag`, so repeated requests are answered with `304 Not Modified` when the client already has the bytes.

## Architecture & performance

```
HTTP request ──► FetchQueue ──► workers ──► relay pool ──► relays
                     │                               │
                     └──▶ in-memory cache ◀───────────┘
```

- **Concurrent workers** — relay fetches are handled by 4 worker tasks, each servicing one request at a time, so a slow relay can't serialize all lookups behind a single worker.
- **Request coalescing** — concurrent requests for the same identifier share a single in-flight relay fetch, so a burst of `/avatar`, `/opengraph`, or `/event` hits for one id costs one round-trip instead of one per request.
- **Bounded in-memory caching** — profiles are cached for 24 hours, events for 10 minutes, and failed event lookups are negatively cached for 60 seconds. All caches are capped (100k entries) to keep memory growth bounded.
- **Timeout safety** — a request times out after 5 seconds total, and each relay fetch is capped at 2 seconds, so unreachable relays can't wedge callers. NIP-05 lookups are also cached (positive: 24h, negative: 10m) with a 5-second timeout per domain.

## Development

```sh
cargo run                  # run in debug
cargo test                 # run the test suite
cargo clippy --all-features # lint
```
