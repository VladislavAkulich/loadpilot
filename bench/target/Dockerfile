FROM rust:1.85-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml .
COPY src/ src/
RUN cargo build --release

FROM alpine:latest
RUN apk add --no-cache wget
COPY --from=builder /app/target/release/bench-target /usr/local/bin/bench-target
EXPOSE 8000
CMD ["bench-target"]
