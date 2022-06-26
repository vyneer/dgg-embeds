FROM lukemathwalker/cargo-chef:latest-rust-slim-bullseye as planner
LABEL builder=true multistage_tag="embeds-planner"
WORKDIR /app
COPY . .
RUN cargo chef prepare  --recipe-path recipe.json

FROM lukemathwalker/cargo-chef:latest-rust-slim-bullseye as builder
LABEL builder=true multistage_tag="embeds-builder"
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin dgg-embeds

FROM debian:bullseye-slim
WORKDIR /app
COPY --from=builder /app/target/release/dgg-embeds .
CMD ["./dgg-embeds"]