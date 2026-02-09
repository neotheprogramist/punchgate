ARG APP_NAME=cli
ARG FEATURES=--no-default-features

FROM rust:1.92.0-alpine3.21 AS base
RUN apk add --no-cache build-base libressl-dev
RUN cargo install cargo-chef sccache --locked
ENV RUSTC_WRAPPER=sccache \
    SCCACHE_DIR=/sccache \
    CARGO_INCREMENTAL=0

FROM base AS planner
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM base AS builder
ARG APP_NAME
ARG FEATURES
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/sccache,sharing=locked \
    cargo chef cook --release ${FEATURES} --recipe-path recipe.json
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/sccache,sharing=locked \
    cargo build --release ${FEATURES} --bin "${APP_NAME}"

FROM alpine:3.21 AS runtime
ARG APP_NAME
RUN adduser -D punchgate
COPY --from=builder "/app/target/release/${APP_NAME}" /usr/local/bin/punchgate
USER punchgate
WORKDIR /home/punchgate
ENTRYPOINT [ "/usr/local/bin/punchgate" ]
