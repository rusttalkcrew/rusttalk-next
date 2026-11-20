FROM rust:1.99-trixie AS build
WORKDIR /src
COPY Cargo.* ./
COPY crates/ crates/
COPY services/ services/
ARG SERVICE=api
# The cache is shared across checkouts. Force Cargo to recheck workspace sources
# with older mtimes than artifacts left by another checkout.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=rusttalk-services-target,target=/src/target,sharing=locked \
    find crates services -type f -exec touch {} + && \
    cargo build --release --locked -p "rusttalk-${SERVICE}" && \
    install "target/release/rusttalk-${SERVICE}" "/usr/local/bin/rusttalk-${SERVICE}"

FROM debian:trixie-slim AS runtime
ARG SERVICE=api
COPY --from=build /usr/local/bin/rusttalk-${SERVICE} /usr/local/bin/
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
RUN install -d -o 65532 -g 65532 -m 0700 /data
USER 65532:65532

FROM runtime AS index
ENTRYPOINT ["rusttalk-index"]

FROM runtime AS api
ENTRYPOINT ["rusttalk-api"]
