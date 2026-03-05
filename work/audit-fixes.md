# Audit Fixes

**Status:** complete (round 2)
**Started:** 2026-03-05
**Last updated:** 2026-03-05

## Goal

Address all open findings from `work/audit-report.md`. Several issues were already fixed in the codebase before this session started; this file tracks what remains.

## Findings

Already resolved (confirmed by reading source):
- #1 CORS credentials: `CorsLayer::very_permissive()` no longer sets credentials header — resolved.
- #3 Avatar path traversal: allowlist check at `avatar.rs:11-13` — resolved.
- #5 HTML injection: `html_escape` function + used in `Display for HeadElement` — resolved.
- #9 Head injection offset bug: `inject_tags` rewritten using scraper DOM traversal — resolved.
- #10 Wrong HTTP status codes: `events.rs` already returns `BAD_REQUEST` / `UNPROCESSABLE_ENTITY` — resolved.
- #12 Kind cast truncation: explicit range check + `u16::MAX` guard — resolved.
- #20 Shield comment: no Rocket, now using `tower-http` CORS — N/A.
- #21 Listen address parse: uses `?` propagation — resolved.
- #23 OG selector substring: already uses `^=` prefix selector — resolved.

Still open:
- #2 SSRF in `/preview`: no scheme/IP validation before `self.client.get(url)`.
- #4/#14 NIP-05 creates new `reqwest::Client` per call; no domain blocklist.
- #8 `b.handler.send(...).unwrap()` panics on dropped receiver.
- #11 `Nip19::Profile` not handled in `nip19_to_filter` → `None` returned → queue item never resolved.
- #15 `std::fs::read_dir` called synchronously in async handler on every request.
- #22 CSS selectors (`title`, `meta[name='description']`) parsed on every request rather than lazily.

## Tasks

- [x] Create this work file
- [x] #2 SSRF: validate URL scheme (https-only) and block private/loopback IPs in `link_preview.rs`
- [x] #4/#14 NIP-05: pass shared `reqwest::Client` through instead of constructing one per call; add domain validation (no private IPs)
- [x] #8 Replace `.unwrap()` with `.ok()` (log warning) on `handler.send` in `fetch.rs`
- [x] #11 Add `Nip19::Profile` branch to `nip19_to_filter`
- [x] #15 Cache avatar file listing in `AppState` at startup; use it in handler
- [x] #22 Lazy-init static CSS selectors (`title`, `meta[name='description']`) in `link_preview.rs`
- [x] Tests + `cargo llvm-cov --summary-only` — 63 tests pass; all pure functions in touched files covered (async handlers requiring live relay are excluded)

### Round 2 (remaining open items)

- [x] #4 (partial): `parse_nip05` now rejects `.local`, `.internal`, `.localhost` TLDs and bare IPv4 addresses
- [x] #13: replaced 100ms poll loop with `tokio::sync::Notify`; `demand` calls `notify_one()`, worker calls `notified().await`
- [x] #16: `process_queue` now spawns all filter fetches concurrently via `tokio::task::JoinSet` instead of sequential `await`
- [x] #17: Dockerfile runner stage changed from `rust:trixie` (~1.5 GB) to `debian:trixie-slim`
- [x] #18: `config.yaml` no longer copied into image; `config::File` marked `.required(false)` so env vars (`APP_*`) fully override
- [x] #19: `/health` endpoint added returning `200 OK`
- [x] Tests + `cargo llvm-cov --summary-only` — 76 tests pass

## Notes

- Cargo.toml does not include `once_cell`; use `std::sync::LazyLock` (stable since Rust 1.80) for lazy selectors.
- For SSRF IP blocking, parse the URL host and reject RFC-1918, loopback, link-local addresses.
