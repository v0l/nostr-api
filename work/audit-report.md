# Audit Report — nostr_services_rs

**Date:** 2026-03-05

---

## Summary

A Rust/Rocket web service providing Nostr utilities: event fetching, avatar generation, link previews, and OpenGraph tag injection. Codebase is relatively small (~900 lines) and functional, but has several notable issues across security, correctness, and maintainability.

---

## Security Issues

### 1. CORS wildcard with credentials (Critical)

`cors.rs:19-25` — `Access-Control-Allow-Origin: *` is set alongside `Access-Control-Allow-Credentials: true`. Browsers reject this combination per the CORS spec, making it both broken and misleading. Either use specific origins with credentials, or drop the credentials header.

### 2. Link preview SSRF — no URL validation (Critical)

`link_preview.rs:86` — The `/preview?<url>` endpoint fetches arbitrary user-supplied URLs with no validation. This enables:
- SSRF attacks against internal network services (e.g., `http://169.254.169.254/`, `http://localhost:8000/`)
- Fetching `file://` or other non-HTTP schemes (depending on reqwest defaults)
- Port scanning

No allowlist, blocklist, or scheme check is enforced before the request is made.

### 3. Avatar path traversal (High)

`avatar.rs:42` — `set` is a user-supplied path segment joined directly via `PathBuf::from("avatars").join(&set)`. A request to `/avatar/../etc/passwd/anything` could escape the `avatars/` directory. The `set` parameter should be validated against an allowlist (`["cyberpunks", "robots", "zombies"]`) before use.

### 4. NIP-05 resolution SSRF (High)

`opengraph.rs:158` — A new `reqwest::Client` is created per NIP-05 resolution request with no connection limits, and the domain comes from user input. Same SSRF concerns apply: internal network reachability, no scheme enforcement beyond the `https://` prefix, and no domain blocklist.

### 5. HTML injection in OpenGraph output (High)

`opengraph.rs:99-113` — Attribute values written into the injected `<meta>` tags are not HTML-escaped (see `Display for HeadElement`). If a Nostr profile name or event content contains `"`, `<`, or `>`, it will break the HTML structure or allow injection. Example: a profile name of `"><script>alert(1)</script>` would be written verbatim.

### 6. Secrets in config committed to repo (Medium)

`config.yaml` is committed and copied into the Docker image (`Dockerfile:12`). Any credentials added here would be baked into the image.

### 7. No request rate limiting (Medium)

The `/preview` and `/opengraph` endpoints make outbound HTTP requests with no per-IP or global rate limit. A single client could exhaust connections or relay bandwidth.

---

## Correctness / Logic Issues

### 8. `handler.send(...).unwrap()` will panic on dropped receivers (Medium)

`fetch.rs:136` — If the calling future is cancelled (e.g., the HTTP connection drops) after enqueueing but before the oneshot receiver is polled, `b.handler.send(...)` returns `Err` because the receiver is dropped. `.unwrap()` will panic, killing the worker loop iteration silently — the next 100ms iteration will restart it, but this is a latency cliff and potentially confusing in logs.

### 9. `inject_tags` — head replacement offset bug (High)

`opengraph.rs:511-518` — The range calculation is wrong:

```rust
let head_start = hs + head_start_end + 1;
let head_end = head_start_end + head_close;
```

`head_end` uses `head_start_end` (offset into the slice from `hs`) but `head_close` is found searching from `html_string[head_start_end..]`, not from the start of the string. The indices are inconsistent and will produce incorrect replacement ranges for anything other than trivially structured HTML. The `scraper` crate's parsed tree should be used to regenerate the head rather than doing string index arithmetic on the raw HTML.

### 10. Wrong HTTP status codes for client input errors (Low)

- `events.rs:36` — `Nip19::from_bech32(id)` failing on bad user input returns `Status::InternalServerError`. Should be `400 Bad Request`.
- `events.rs:53` — Invalid hex pubkey from the client returns `500` instead of `400`.
- `events.rs:16-18` — A bad signature from the client returns `500`. Should be `422 Unprocessable Entity`.

### 11. Missing filter for `Nip19::Profile` (High)

`fetch.rs:164` — `nip19_to_filter` returns `None` for `Nip19::Profile`, but `demand` is called with `Nip19::Profile` indirectly via `get_profile`. `n19_key` returns a key for it, but no filter is emitted, so the queue item will never find a matching event. The profile case should map to `Filter::new().author(p.public_key).kind(Kind::Metadata)`.

### 12. Event kind cast truncation (Low)

`events.rs:54` — `kind as u16` silently truncates if `kind > 65535`. Nostr kind values fit in u16 normally, but the input is `u32`, so an out-of-range value would silently wrap.

---

## Performance / Architecture

### 13. Poll loop instead of wake-on-demand (Low)

`fetch.rs` — The 100ms poll loop means requests can wait up to 100ms even with no queuing needed. A `Notify` or channel-based wake-up would eliminate the latency.

### 14. New `reqwest::Client` per NIP-05 request (Medium)

`opengraph.rs:160-163` — Each call to `resolve_nip05` creates a new `reqwest::Client`, which creates a new connection pool. This wastes connections and ignores Keep-Alive. A shared client (like `LinkPreviewCache::client`) should be used.

### 15. Avatar file listing on every request (Medium)

`avatar.rs:45-52` — `std::fs::read_dir` is called synchronously in an async handler on every single request. This should be cached at startup or use `tokio::fs`.

### 16. `process_queue` sends one relay request per filter (Medium)

`fetch.rs:115-130` — The comment implies batching, but the loop sends individual `fetch_events` calls sequentially. The filters should be sent as a single subscription to each relay if the client supports it.

---

## Docker / Ops

### 17. Full `rust:trixie` used as runner image (Medium)

`Dockerfile:8` — The runner stage is `FROM $IMAGE` (i.e., `rust:trixie`), which is ~1.5 GB. It should use `debian:trixie-slim` or `scratch` (for a fully static binary). This is a significant attack surface increase and image size bloat.

### 18. `config.yaml` baked into Docker image (Medium)

`Dockerfile:12` — The hardcoded relay list is in the image. Runtime configuration should come from environment variables or a mounted secret/configmap, not a committed file baked into the image. The `APP_` env prefix support exists (`main.rs:30`) but the file always overrides it.

### 19. No health check endpoint (Low)

There is no `/health` or `/status` route for liveness/readiness probes in Kubernetes.

---

## Minor / Code Quality

### 20. `Shield::new()` comment says "disable" but it is enabled (Low)

`main.rs:64` — The comment says "disable" but `Shield::new()` with no configuration still applies Rocket's default security headers (e.g., `X-Frame-Options`, `X-Content-Type-Options`). The intent is unclear — if it should be disabled entirely, use `Shield::default().enable(...)` explicitly, or remove it.

### 21. `unwrap()` on listen address parse (Low)

`main.rs:54` — `i.parse().unwrap()` will panic at startup if the `listen` value is malformed. Should return a proper error.

### 22. CSS selectors parsed on every request (Low)

`link_preview.rs:107`, `127`, `139` — Static selectors could be compiled once (e.g., `once_cell::sync::Lazy`) rather than parsed on every request.

### 23. OpenGraph selector uses substring match (Low)

`link_preview.rs:107` — `meta[property*='og']` matches any attribute containing "og" (e.g., `property="bog:title"`). The standard selector is `meta[property^='og:']` (prefix match).

---

## Summary Table

| # | File | Severity | Category |
|---|------|----------|----------|
| 1 | `cors.rs:19-25` | Critical | Security |
| 2 | `link_preview.rs:86` | Critical | Security (SSRF) |
| 3 | `avatar.rs:42` | High | Security (Path Traversal) |
| 4 | `opengraph.rs:158` | High | Security (SSRF) |
| 5 | `opengraph.rs:99-113` | High | Security (HTML Injection) |
| 6 | `config.yaml` + `Dockerfile` | Medium | Security |
| 7 | Multiple endpoints | Medium | Security (No Rate Limiting) |
| 8 | `fetch.rs:136` | Medium | Correctness (panic) |
| 9 | `opengraph.rs:511-518` | High | Correctness (offset bug) |
| 10 | `events.rs:16,36,53` | Low | Correctness (HTTP status) |
| 11 | `fetch.rs:164` | High | Correctness (missing filter) |
| 12 | `events.rs:54` | Low | Correctness (truncation) |
| 13 | `fetch.rs` | Low | Performance (poll loop) |
| 14 | `opengraph.rs:160` | Medium | Performance (client reuse) |
| 15 | `avatar.rs:45` | Medium | Performance (sync I/O) |
| 16 | `fetch.rs:115` | Medium | Performance (batching) |
| 17 | `Dockerfile:8` | Medium | Ops (image size) |
| 18 | `Dockerfile:12` | Medium | Ops (config in image) |
| 19 | — | Low | Ops (health check) |
| 20 | `main.rs:64` | Low | Code Quality |
| 21 | `main.rs:54` | Low | Code Quality |
| 22 | `link_preview.rs:107,127,139` | Low | Code Quality |
| 23 | `link_preview.rs:107` | Low | Code Quality |

### Priority Fixes

The highest priority items are:

1. **#2** — SSRF in `/preview` (validate URL scheme and block internal addresses)
2. **#3** — Path traversal in avatar endpoint (allowlist `set` parameter)
3. **#5** — HTML injection in OpenGraph output (HTML-escape all attribute values)
4. **#9** — Head injection offset bug (replace string indexing with proper tree manipulation)
5. **#11** — Missing `Nip19::Profile` filter causes silent fetch failure
