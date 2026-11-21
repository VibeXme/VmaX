FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY relay-server/Cargo.toml relay-server/Cargo.lock relay-server/
COPY relay-server/src/ relay-server/src/
COPY src-tauri/src/signaling.rs src-tauri/src/stun.rs src-tauri/src/vmax_relay_ice.rs src-tauri/src/
COPY src-tauri/src/net/regional_wire.rs src-tauri/src/net/regional_wire.rs
RUN cargo build --locked --release --manifest-path relay-server/Cargo.toml

FROM debian:bookworm-slim
COPY --from=build /src/relay-server/target/release/vmax-relay /usr/local/bin/vmax-relay
ENTRYPOINT ["/usr/local/bin/vmax-relay"]
