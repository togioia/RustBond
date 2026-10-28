# syntax=docker/dockerfile:1
# Multi-arch friendly: the build stage uses the alpine Rust image, whose host
# target is already musl, so the resulting binary is fully static.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY shared ./shared
COPY server ./server
COPY client ./client
RUN cargo build --release --locked -p rnb-server

FROM scratch
COPY --from=build /src/target/release/rustbond-server /rustbond-server
USER 65534:65534
EXPOSE 5000
ENTRYPOINT ["/rustbond-server"]
