# Static (musl) build of the exporter and the replay tool, shipped in an empty image.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/lgh-exporter /lgh-exporter
COPY --from=build /src/target/release/lgh-replay /lgh-replay
ENTRYPOINT ["/lgh-exporter"]
