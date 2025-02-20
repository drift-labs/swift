FROM rust:1.81.0 AS builder

RUN apt update -y && \
  apt install -y git libssl-dev libsasl2-dev cmake

ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup component add rustfmt && rustup install 1.76.0-x86_64-unknown-linux-gnu
RUN curl -L https://github.com/drift-labs/drift-ffi-sys/releases/download/v2.109.0/libdrift_ffi_sys.so > libdrift_ffi_sys.so && cp libdrift_ffi_sys.so /usr/local/lib

RUN rustc --version && cargo --version && cmake --version

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN CARGO_DRIFT_FFI_PATH="/usr/local/lib" cargo build --release

FROM amazonlinux:2023

RUN yum install -y openssl cyrus-sasl cyrus-sasl-devel

COPY --from=builder /usr/local/lib/libdrift_ffi_sys.so /usr/lib/
COPY --from=builder /app/target/release/fastlane-server /usr/local/bin/fastlane-server
COPY --from=builder /usr/lib/x86_64-linux-gnu/libsasl2.so.2 /usr/lib/

RUN ldconfig

ENTRYPOINT ["/usr/local/bin/fastlane-server"]
