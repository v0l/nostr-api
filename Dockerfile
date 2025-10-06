ARG IMAGE=rust:trixie

FROM $IMAGE AS build
WORKDIR /app/src
COPY . .
RUN cargo install --path . --root /app/build

FROM $IMAGE AS runner
WORKDIR /app
COPY --from=build /app/build .
COPY avatars avatars
COPY config.yaml config.yaml
ENTRYPOINT ["./bin/nostr_services_rs"]