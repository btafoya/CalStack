FROM rust:1-bookworm AS build
WORKDIR /build
COPY . .
RUN cargo build --release --bin calendar-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/calendar-server /usr/local/bin/calendar-server
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/calendar-server"]
