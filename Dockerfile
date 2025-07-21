FROM rust:1.88 AS chef 
RUN cargo install cargo-chef 
WORKDIR /usr/src/events/service

FROM chef AS planner
COPY ./service .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /usr/src/events/service/recipe.json recipe.json

RUN cargo chef cook --release --recipe-path recipe.json

COPY ./service .
RUN cargo build --release --bin events

FROM debian:bookworm-slim AS runtime

WORKDIR /usr/src/app/
COPY --from=builder /usr/src/events/service/target/release/events /usr/src/app/
ENTRYPOINT ["/usr/src/app/events"]