FROM rust:1.99-bookworm AS build
WORKDIR /vmax
COPY signaling-server/Cargo.toml signaling-server/Cargo.lock signaling-server/
COPY signaling-server/src/ signaling-server/src/
COPY src-tauri/src/signaling.rs src-tauri/src/signaling.rs
COPY src-tauri/src/stun.rs src-tauri/src/stun.rs
COPY src-tauri/src/relay_probe.rs src-tauri/src/relay_probe.rs
COPY src-tauri/src/vmax_relay_ice.rs src-tauri/src/vmax_relay_ice.rs
RUN cargo build --release --locked --manifest-path signaling-server/Cargo.toml

FROM debian:bookworm-slim
COPY --from=build /vmax/signaling-server/target/release/vmax-signaling /usr/local/bin/
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/vmax-signaling"]
CMD ["0.0.0.0:8787"]
