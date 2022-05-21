FROM rust:alpine as planner
WORKDIR /app
# We only pay the installation cost once, 
# it will be cached from the second build onwards
# To ensure a reproducible build consider pinning 
# the cargo-chef version with `--version X.X.X`
RUN apk add --no-cache musl-dev
RUN cargo install cargo-chef 
COPY . .
RUN cargo chef prepare  --recipe-path recipe.json

FROM rust:alpine as cacher
WORKDIR /app
RUN apk add --no-cache musl-dev perl make
RUN cargo install cargo-chef
COPY --from=planner /app/recipe.json recipe.json

RUN rustup target add x86_64-unknown-linux-musl
RUN cargo chef cook --target x86_64-unknown-linux-musl --release --recipe-path recipe.json

FROM rust:alpine as builder
WORKDIR /app
COPY . .
# Copy over the cached dependencies
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo

RUN rustup target add x86_64-unknown-linux-musl
RUN cargo build --target x86_64-unknown-linux-musl --release --bin dgg-embeds

FROM alpine
WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/dgg-embeds .
CMD ["./dgg-embeds"]