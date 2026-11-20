FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
WORKDIR /build
# Keep dependency compilation in a layer keyed by the lockfile and toolchain.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && \
    printf 'fn main() {}\n' > src/main.rs && \
    cargo build --locked --release --bin solo-tracker && \
    cargo clean --release --package solo-tracker && \
    rm -r src
COPY . .
RUN cargo build --locked --offline --release --bin solo-tracker

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc AS runtime
WORKDIR /app
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /build/target/release/solo-tracker /usr/local/bin/solo-tracker
COPY --from=build /build/assets ./assets
USER 65532:65532
CMD ["solo-tracker", "serve"]
