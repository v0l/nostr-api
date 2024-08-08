# nostr-services-rs

Simple API for serving nostr applications

## API

### `POST /event`
Save event in database

### `GET /event/<event-id>`
Get event by ID as `hex/nevent/naddr`

### `GET /event/<kind>/<pubkey>`
Get regular replaceable event for pubkey (non-parameterized)