# Fastlane Server
Infrastructure for the Fastlane order pipeline.

### Architecture
There are 3 server components:
- **Fastlane Server**: HTTP server for receiving signed order messages from takers e.g. via the UI
- **Ws Server**: Ws server for broadcasting taker orders to market makers
- **Confirmation Server**: Provides API for fastlane progress tracking

```mermaid
graph TD
    A[Taker]
    B[Fastlane Server]
    C[WebSocket Server]
    D[Market Makers]
    E[Kafka]
    F[Blockchain]
    G[Confirmation Server]
    H[Redis]
    I[Fillers]
    A -->|Post signed OrderParams| B
    B -->|Publish verified orders | E
    C -->|Broadcast new orders| D
    D -->|Send PlaceAndMake Tx| F
    E --> |Subscribe new orders| C
    A -->|Poll order status|G
    G -->|Fetch order hash|H
    C -->|Place taker + fill txs| I
```

## Build
ensure an x86_64 toolchain is configured for building `fastlane-server`
```shell
rustup install 1.83.0-x86_64-apple-darwin
# run inside fastlane-server directory
rusutp override set 1.83.0-x86_64-apple-darwin
```

```shell
cargo build --release
```

Run it
```shell
./target/release/fastlane-server --help
```

## Run
The fastlane stack uses kafka for sending messages between the `fastlane_server` and the `ws_server`.  
`docker-compose up` to run a local kafka instance.  
