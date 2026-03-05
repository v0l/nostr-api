# nostr-services-rs

A lightweight HTTP API for serving Nostr-powered web applications. It fetches events and profiles from configured relays on demand and exposes them via a simple REST interface, with built-in support for OpenGraph tag injection, link previews, and deterministic avatars.

## Features

- **OpenGraph injection** — POST any HTML page with a Nostr identifier to get `og:*` and `twitter:*` meta tags injected. Handles profiles, notes, live streams, and stream clips.
- **Event API** — fetch and store Nostr events by NIP-19 identifier or by kind + pubkey.
- **Link previews** — scrape OpenGraph metadata from arbitrary URLs with caching.
- **Deterministic avatars** — hash-based avatar selection from bundled image sets (cyberpunks, robots, zombies).
- **NIP-05 resolution** — profile identifiers like `name@domain.tld` are resolved automatically.

## Running

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

Create a `config.yaml` in the working directory (or set `APP_` prefixed environment variables):

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

Interactive docs are served at `/` when the server is running. The raw OpenAPI spec is at `/openapi.yaml`. Production instance: `https://nostr-rs-api.v0l.io`.

### OpenGraph injection

Inject `og:*` / `twitter:*` meta tags into an HTML document for a given Nostr identifier. Intended to be called by an SSR proxy or edge function before returning a page to a crawler.

```
POST /opengraph/{id}
Content-Type: text/html

<!DOCTYPE html><html><head>...</head><body>...</body></html>
```

`{id}` can be any NIP-19 bech32 identifier (`npub1…`, `nprofile1…`, `note1…`, `nevent1…`, `naddr1…`) or a NIP-05 address (`name@domain.tld`).

Optional query parameter `canonical` — a URL template with `%s` as a placeholder for the bech32 event ID. When provided, a `<link rel="canonical">` tag is injected.

```
POST /opengraph/kieran@snort.social
POST /opengraph/npub1xtscya34g58tk0z605fvr788k263gsu6cy9x0mhnm87echrgufzsevkk5s
POST /opengraph/note1qqqqqq...?canonical=https://snort.social/%s
```

The endpoint returns the original HTML unchanged if the identifier cannot be resolved.

### Events

```
POST /event                        Import a signed Nostr event
GET  /event/{id}                   Fetch event by NIP-19 ID (note1…/nevent1…/naddr1…)
GET  /event/{kind}/{pubkey}        Fetch latest replaceable event by kind + hex pubkey
```

### Link previews

```
GET /preview?url={url}             Scrape OpenGraph metadata from a URL
```

Returns title, description, image, and raw `og:*` tag pairs. Results are cached for 24 hours; failed fetches are cached for 10 minutes.

### Avatars

```
GET /avatar/{set}/{value}          Deterministic avatar image (webp)
```

Available sets: `cyberpunks`, `robots`, `zombies`. The `value` is hashed to select a stable image from the set.

## Architecture

Relay fetches are batched through a single `FetchQueue`. Requests that arrive at the same time are grouped into one filter and sent to all configured relays concurrently, reducing relay load. Results are cached in-memory (profiles for 24 hours, events for 10 minutes).
