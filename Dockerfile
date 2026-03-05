FROM rust:trixie AS build
WORKDIR /app/src
COPY . .
RUN cargo install --path . --root /app/build

FROM debian:trixie-slim AS runner
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /app/build/bin/nostr_services_rs ./nostr_services_rs
COPY avatars avatars
ENTRYPOINT ["./nostr_services_rs"]