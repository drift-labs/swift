FROM rust:1.91.1-bookworm AS builder

RUN apt update -y && \
  apt install -y git libssl-dev

ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup component add rustfmt

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM amazonlinux:2023

RUN yum install -y openssl

COPY --from=builder /app/target/release/swift-server /usr/local/bin/swift-server

ENTRYPOINT ["/usr/local/bin/swift-server"]
