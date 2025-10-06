# nostr-services-rs

Simple API for serving nostr applications

## API Documentation

Interactive API documentation is available at `/` when running the server.

OpenAPI specification: `/openapi.yaml`

## Endpoints

### Event Management

- `POST /event` - Import a Nostr event into the database
- `GET /event/{id}` - Get event by NIP-19 identifier (note1.../nevent1.../naddr1...)
- `GET /event/{kind}/{pubkey}` - Get replaceable event by kind and pubkey

### Link Previews

- `GET /preview?url={url}` - Fetch OpenGraph tags and metadata from a URL
- `POST /opengraph/{id}?canonical={template}` - Inject OpenGraph tags into HTML for Nostr events/profiles

### Avatars

- `GET /avatar/{set}/{value}` - Get deterministic avatar from set (cyberpunks/robots/zombies)