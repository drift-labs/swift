FROM rust:1.84.1 AS builder

RUN apt update -y && \
  apt install -y git libssl-dev libsasl2-dev cmake jq

ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup component add rustfmt
RUN SO_URL=$(curl -s https://api.github.com/repos/drift-labs/drift-ffi-sys/releases/latest | jq -r '.assets[] | select(.name=="libdrift_ffi_sys.so") | .browser_download_url') &&\
  curl -L -o libdrift_ffi_sys.so "$SO_URL" &&\
  cp libdrift_ffi_sys.so /usr/local/lib

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN CARGO_DRIFT_FFI_PATH="/usr/local/lib" cargo build --release

FROM amazonlinux:2023

RUN yum install -y openssl cyrus-sasl cyrus-sasl-devel

COPY --from=builder /usr/local/lib/libdrift_ffi_sys.so /usr/lib/
COPY --from=builder /app/target/release/server-server /usr/local/bin/server-server
COPY --from=builder /usr/lib/x86_64-linux-gnu/libsasl2.so.2 /usr/lib/

RUN ldconfig

ENTRYPOINT ["/usr/local/bin/swift-server"]
