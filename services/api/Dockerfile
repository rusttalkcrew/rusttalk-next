FROM rust:1.99-trixie AS build
WORKDIR /src
COPY Cargo.* ./
COPY crates/ crates/
COPY services/ services/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p rusttalk-api && \
    install target/release/rusttalk-api /usr/local/bin/rusttalk-api

FROM debian:trixie-slim
COPY --from=build /usr/local/bin/rusttalk-api /usr/local/bin/
RUN install -d -o 65532 -g 65532 -m 0700 /data
USER 65532:65532
ENTRYPOINT ["rusttalk-api"]
