use std::{
    collections::HashSet,
    env,
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, SystemTime},
};

use crate::{
    connection::kafka_connect::KafkaClientBuilder,
    super_slot_subscriber::SuperSlotSubscriber,
    types::{
        messages::{
            IncomingSignedMessage, OrderMetadataAndMessage, ProcessOrderResponse,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
            PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY, PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
        },
        types::unix_now_ms,
    },
    user_account_fetcher::UserAccountFetcher,
    util::{
        headers::XSwiftClientConsumer,
        metrics::{metrics_handler, MetricsServerParams, SwiftServerMetrics},
    },
};
use axum::{
    extract::State,
    http::{self, Method},
    routing::{get, post},
    Json, Router,
};
use dotenv::dotenv;
use drift_rs::{
    constants::high_leverage_mode_account,
    event_subscriber::PubsubClient,
    math::account_list_builder::AccountsListBuilder,
    swift_order_subscriber::{SignedMessageInfo, SignedOrderType},
    types::{
        accounts::{HighLeverageModeConfig, PerpMarket},
        errors::ErrorCode,
        CommitmentConfig, MarketId, MarketType, OrderParams, OrderType, ProgramError, SdkError,
        VersionedMessage, VersionedTransaction,
    },
    utils::load_keypair_multi_format,
    Context, DriftClient, RpcClient, TransactionBuilder, Wallet,
};
use log::warn;
use prometheus::Registry;
use rdkafka::{
    producer::{FutureProducer, FutureRecord, Producer},
    util::Timeout,
};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use solana_rpc_client_api::{client_error, config::RpcSimulateTransactionConfig};
use solana_sdk::{
    clock::Slot,
    hash::Hash,
    message::v0::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
};
use tower_http::cors::{Any, CorsLayer};

/// Accept orders under-collaterized upto this ratio.
const COLLATERAL_BUFFER: f64 = 1.01;

struct Config {
    /// RPC tx simulation on/off
    disable_rpc_sim: AtomicBool,
    /// RPC tx simulation timeout
    simulation_timeout: Duration,
}

impl Config {
    fn from_env() -> Self {
        Self {
            disable_rpc_sim: AtomicBool::new(
                std::env::var("DISABLE_RPC_SIM").unwrap_or("false".to_string()) == "true",
            ),
            simulation_timeout: Duration::from_millis(300),
        }
    }
}

#[derive(Clone)]
pub struct ServerParams {
    drift: drift_rs::DriftClient,
    slot_subscriber: Arc<SuperSlotSubscriber>,
    kafka_producer: Option<FutureProducer>,
    metrics: SwiftServerMetrics,
    redis_pool: Option<MultiplexedConnection>,
    user_account_fetcher: UserAccountFetcher,
    config: Arc<Config>,
    farmer_pubkeys: HashSet<Pubkey>,
}

pub async fn fallback(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::NOT_FOUND, format!("No route {uri}"))
}

#[inline]
fn extract_uuid(msg: &SignedOrderType) -> [u8; 8] {
    match msg {
        SignedOrderType::Authority(x) => x.uuid,
        SignedOrderType::Delegated(x) => x.uuid,
    }
}

pub async fn process_order_wrapper(
    x_swift_client_header: Option<axum_extra::TypedHeader<XSwiftClientConsumer>>,
    State(server_params): State<&'static ServerParams>,
    Json(incoming_message): Json<IncomingSignedMessage>,
) -> impl axum::response::IntoResponse {
    let uuid_raw = extract_uuid(&incoming_message.message);
    let uuid = core::str::from_utf8(&uuid_raw).unwrap_or("00000000");
    let (status, resp) = process_order(server_params, incoming_message).await;
    log::info!(
        target: "server", "{status}|{uuid}|{:?}|ui={}",
        resp.error.as_deref().unwrap_or(""),
        x_swift_client_header.is_some_and(|x| x.is_app_order())
    );
    (status, Json(resp))
}

pub async fn process_order(
    server_params: &'static ServerParams,
    incoming_message: IncomingSignedMessage,
) -> (http::StatusCode, ProcessOrderResponse) {
    let process_order_time = unix_now_ms();
    let IncomingSignedMessage {
        taker_pubkey,
        signature: taker_signature,
        message: _,
        signing_authority,
        taker_authority,
    } = incoming_message;

    let taker_authority = if taker_authority == Pubkey::default() {
        taker_pubkey
    } else {
        taker_authority
    };

    if server_params.farmer_pubkeys.contains(&taker_authority) {
        log::debug!(
            target: "server",
            "Ignoring order from farmer pubkey: {taker_authority}"
        );
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY,
                error: None,
            },
        );
    }

    server_params.metrics.taker_orders_counter.inc();

    let signing_pubkey = if signing_authority == Pubkey::default() {
        taker_authority
    } else {
        signing_authority
    };

    let log_prefix = format!("[process_order {taker_authority}: {process_order_time}]");
    log::trace!(
        target: "server",
        "{log_prefix}: Received order with signing pubkey: {signing_pubkey}"
    );

    let signed_msg = match incoming_message.verify_and_get_signed_message() {
        Ok(m) => m,
        Err(e) => {
            log::warn!("{log_prefix}: Error verifying signed message: {e:?}",);
            return (
                axum::http::StatusCode::BAD_REQUEST,
                ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
                    error: Some(e.to_string()),
                },
            );
        }
    };
    let is_delegated = signed_msg.is_delegated();
    let SignedMessageInfo {
        slot: taker_slot,
        taker_pubkey,
        uuid,
        order_params,
    } = signed_msg.info(&signing_pubkey);

    // check the order's slot is reasonable
    let current_slot = server_params.slot_subscriber.current_slot();
    if taker_slot < current_slot - 500 {
        log::warn!(
            target: "server",
            "{log_prefix}: Order slot too old: {taker_slot}, current slot: {current_slot}",
        );
        let err_str = PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD;
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: err_str,
                error: Some(err_str.to_string()),
            },
        );
    }

    // check the order is valid for execution by program
    let market = server_params
        .drift
        .try_get_perp_market_account(order_params.market_index)
        .ok();
    if let Err(err) = validate_signed_order_params(&order_params, &market) {
        log::warn!(
            target: "server",
            "{log_prefix}: Order did not validate: {err:?}, {order_params:?}",
        );
        return (
            axum::http::StatusCode::BAD_REQUEST,
            ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                error: Some(err.to_string()),
            },
        );
    }

    let delegate_signer = if is_delegated {
        Some(&signing_pubkey)
    } else {
        None
    };

    match server_params
        .simulate_taker_order_rpc(&taker_pubkey, &order_params, delegate_signer, current_slot)
        .await
    {
        Ok(sim_res) => {
            server_params
                .metrics
                .rpc_simulation_status
                .with_label_values(&[sim_res.as_str()])
                .inc();
        }
        Err((status, sim_err_str, logs)) => {
            server_params
                .metrics
                .rpc_simulation_status
                .with_label_values(&["invalid"])
                .inc();
            log::warn!(
                target: "server",
                "{log_prefix}: Order sim failed (taker: {taker_pubkey:?}, delegate: {delegate_signer:?}, market: {:?}-{}): {sim_err_str}. Logs: {logs:?}",
                order_params.market_type,
                order_params.market_index,
            );
            log::warn!(
                target: "server",
                "{log_prefix}: failed order params: {order_params:?}"
            );
            return (
                status,
                ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                    error: Some(sim_err_str),
                },
            );
        }
    };

    // If fat fingered order that requires sanitization, then just send the order
    let will_sanitize = server_params.simulate_will_auction_params_sanitize(&order_params);
    let order_metadata = OrderMetadataAndMessage {
        signing_authority: signing_pubkey,
        taker_authority,
        order_message: signed_msg.clone(),
        order_signature: taker_signature.into(),
        ts: process_order_time,
        uuid,
        will_sanitize,
    };
    let encoded = order_metadata.encode();
    let market_index = order_params.market_index;
    let market_type = order_params.market_type;

    let topic = format!("swift_orders_{}_{market_index}", market_type.as_str());

    if let Some(kafka_producer) = &server_params.kafka_producer {
        let enqueue_result = match kafka_producer
            .send_result(FutureRecord::<String, String>::to(&topic).payload(&encoded))
        {
            Ok(fut) => fut.await,
            Err((err, _)) => {
                log::error!(
                    target: "kafka",
                    "{log_prefix}: Failed to queue order: {order_metadata:?}, error: {err:?}"
                );
                server_params.metrics.kafka_forward_fail_counter.inc();
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                        error: Some(format!("kafka publish error: {err:?}")),
                    },
                );
            }
        };

        match enqueue_result {
            Ok(Ok(_delivery_result)) => {
                log::trace!(target: "kafka", "{log_prefix}: Sent message for order: {order_metadata:?}");
                server_params
                    .metrics
                    .current_slot_gauge
                    .set(current_slot as f64);

                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&[
                        market_type.as_str(),
                        &market_index.to_string(),
                        match will_sanitize {
                            true => "true",
                            false => "false",
                        },
                    ])
                    .inc();

                server_params
                    .metrics
                    .kafka_inflight_count
                    .set(kafka_producer.in_flight_count() as i64);

                server_params
                    .metrics
                    .response_time_histogram
                    .observe((unix_now_ms() - process_order_time) as f64);

                log::info!(target: "kafka", "published to kafka: {}", order_metadata.uuid());
                (
                    axum::http::StatusCode::OK,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
                        error: None,
                    },
                )
            }
            Ok(Err((e, _))) => {
                log::error!(
                    target: "kafka",
                    "{log_prefix}: Failed to deliver order: {order_metadata:?}, error: {e:?}"
                );
                server_params.metrics.kafka_forward_fail_counter.inc();
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                        error: Some(format!("kafka publish error: {e:?}")),
                    },
                )
            }
            Err(_) => {
                log::error!(
                    target: "kafka",
                    "{log_prefix}: Failed to queue order: {order_metadata:?}"
                );
                server_params.metrics.kafka_forward_fail_counter.inc();
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                        error: Some("kafka publish error".into()),
                    },
                )
            }
        }
    } else {
        let mut conn = server_params.redis_pool.clone().unwrap();
        match conn.publish::<String, String, i64>(topic, encoded).await {
            Ok(_) => {
                log::trace!(target: "redis", "{log_prefix}: Sent redis message for order: {order_metadata:?}");
                server_params
                    .metrics
                    .current_slot_gauge
                    .set(current_slot as f64);
                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&[market_type.as_str(), &market_index.to_string(), "false"])
                    .inc();

                server_params
                    .metrics
                    .response_time_histogram
                    .observe((unix_now_ms() - process_order_time) as f64);

                (
                    axum::http::StatusCode::OK,
                    ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
                        error: None,
                    },
                )
            }
            Err(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                    error: Some(format!("redis publish error: {e:?}")),
                },
            ),
        }
    }
}

pub async fn send_heartbeat(server_params: &'static ServerParams) {
    let hearbeat_time = unix_now_ms();
    let log_prefix = format!("[hearbeat: {hearbeat_time}]");

    if let Some(kafka_producer) = &server_params.kafka_producer {
        match kafka_producer
            .send(
                FutureRecord::<String, String>::to("hearbeat").payload(&"love you".to_string()),
                Timeout::After(Duration::ZERO),
            )
            .await
        {
            Ok(_) => {
                log::trace!(target: "kafka", "{log_prefix}: Sent heartbeat");
                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&["_", "heartbeat", "_"])
                    .inc();
            }
            Err((e, _)) => {
                log::error!(
                    target: "kafka",
                    "{log_prefix}: Failed to deliver heartbeat, error: {e:?}"
                );
                server_params.metrics.kafka_forward_fail_counter.inc();
            }
        }
    } else {
        let conn = server_params.redis_pool.clone();
        match conn
            .unwrap()
            .publish::<String, String, i64>("heartbeat".to_string(), "love you".to_string())
            .await
        {
            Ok(_) => {
                log::trace!(target: "redis", "{log_prefix}: Sent redis heartbeat");
                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&["_", "heartbeat", "_"])
                    .inc();
            }
            Err(e) => {
                log::error!(
                    target: "redis",
                    "{log_prefix}: Failed to deliver heartbeat, error: {e:?}"
                );
            }
        }
    }
}

pub async fn health_check(
    State(server_params): State<&'static ServerParams>,
) -> impl axum::response::IntoResponse {
    let ws_healthy = server_params.drift.ws().is_running();
    let slot_sub_healthy = !server_params.slot_subscriber.is_stale();

    // Check if optional accounts are healthy
    let user_account_fetcher_redis_health = if server_params.user_account_fetcher.redis.is_some() {
        server_params
            .user_account_fetcher
            .check_redis_health()
            .await
    } else {
        true
    };

    // Check if optional accounts are healthy
    let redis_health = if server_params.redis_pool.is_some() {
        if let Some(mut conn) = server_params.redis_pool.clone() {
            let ping_result: redis::RedisResult<String> = conn.ping().await;
            ping_result.is_ok()
        } else {
            false
        }
    } else {
        true
    };

    // Check if rpc is healthy
    let rpc_healthy = server_params.drift.rpc().get_health().await.is_ok();

    if ws_healthy
        && slot_sub_healthy
        && user_account_fetcher_redis_health
        && redis_health
        && rpc_healthy
    {
        (axum::http::StatusCode::OK, "ok".into())
    } else {
        let msg = format!(
            "slot_sub_healthy={slot_sub_healthy} | ws_sub_healthy={ws_healthy} 
            | user_account_fetcher_healthy={user_account_fetcher_redis_health} |
            redis_healthy={redis_health}|rpc_healthy={rpc_healthy}",
        );
        log::error!(target: "server", "Failed health check {}", &msg);
        (axum::http::StatusCode::PRECONDITION_FAILED, msg)
    }
}

pub async fn start_server() {
    // Start server

    dotenv().ok();

    let keypair =
        load_keypair_multi_format(env::var("PRIVATE_KEY").expect("PRIVATE_KEY set").as_str());
    if let Err(err) = keypair {
        log::error!(target: "server", "Failed to load swift private key: {err:?}");
        return;
    }

    let use_kafka: bool = env::var("USE_KAFKA").unwrap_or_else(|_| "false".to_string()) == "true";
    let running_local = env::var("RUNNING_LOCAL").unwrap_or("false".to_string()) == "true";
    let drift_env = env::var("ENV").unwrap_or("devnet".to_string());

    log::info!(target: "server", "USE_KAFKA: {use_kafka}, RUNNING_LOCAL: {running_local}, ENV: {drift_env}");

    let kafka_producer = if use_kafka {
        let producer = if running_local {
            log::info!(target: "kafka", "Starting local Kafka producer");
            KafkaClientBuilder::local().producer()
        } else {
            log::info!(target: "kafka", "Starting AWS Kafka producer");
            KafkaClientBuilder::aws_from_env().await.producer()
        };

        match producer {
            Ok(prod) => Some(prod),
            Err(e) => {
                log::error!(target: "kafka", "Failed to create Kafka producer: {e:?}");
                return;
            }
        }
    } else {
        None
    };

    let redis_pool = if !use_kafka {
        let elasticache_host =
            env::var("ELASTICACHE_HOST").unwrap_or_else(|_| "localhost".to_string());
        let elasticache_port = env::var("ELASTICACHE_PORT").unwrap_or_else(|_| "6379".to_string());
        let connection_string = if env::var("USE_SSL")
            .unwrap_or_else(|_| "false".to_string())
            .to_lowercase()
            == "true"
        {
            format!("rediss://{}:{}", elasticache_host, elasticache_port)
        } else {
            format!("redis://{}:{}", elasticache_host, elasticache_port)
        };
        let client = redis::Client::open(connection_string).expect("valid redis URL");
        Some(
            client
                .get_multiplexed_tokio_connection()
                .await
                .expect("redis connected"),
        )
    } else {
        None
    };

    let rpc_endpoint =
        drift_rs::utils::get_http_url(&env::var("ENDPOINT").expect("valid rpc endpoint"))
            .expect("valid RPC endpoint");

    // Registry for metrics
    let registry = Registry::new();
    let metrics = SwiftServerMetrics::new();
    metrics.register(&registry);

    let context = match drift_env.as_str() {
        "devnet" => Context::DevNet,
        "mainnet-beta" => Context::MainNet,
        _ => panic!("Invalid drift environment: {drift_env}"),
    };
    let wallet = Wallet::new(keypair.unwrap());
    let client = DriftClient::new(context, RpcClient::new(rpc_endpoint), wallet)
        .await
        .expect("initialized client");

    let user_account_fetcher = UserAccountFetcher::from_env(client.clone()).await;

    // Slot subscriber
    let mut ws_clients = vec![];
    for (_k, ws_endpoint) in std::env::vars().filter(|(k, _v)| k.starts_with("WS_ENDPOINT")) {
        ws_clients.push(Arc::new(PubsubClient::new(&ws_endpoint).await.unwrap()));
    }
    assert!(
        !ws_clients.is_empty(),
        "no slot subscribers provided: set WS_ENDPOINT_*"
    );
    let mut slot_subscriber = SuperSlotSubscriber::new(ws_clients, client.rpc());
    slot_subscriber.subscribe();

    // Set ignore pubkeys
    let ignore_pubkeys = env::var("IGNORE_PUBKEYS").unwrap_or_else(|_| "".to_string());
    let pubkeys = ignore_pubkeys
        .split(',')
        .map(|s| s.trim()) // remove extra whitespace
        .filter_map(|s| match s.parse::<Pubkey>() {
            Ok(key) => Some(key),
            Err(_) => {
                log::warn!(target: "server", "Warning: invalid pubkey skipped for ignore pubkeys: {s:?}");
                None
            }
        });

    let state: &'static ServerParams = Box::leak(Box::new(ServerParams {
        drift: client,
        slot_subscriber: Arc::new(slot_subscriber),
        kafka_producer,
        metrics,
        redis_pool,
        user_account_fetcher,
        config: Arc::new(Config::from_env()),
        farmer_pubkeys: HashSet::from_iter(pubkeys),
    }));

    // start oracle/market subscriptions (async)
    tokio::spawn(async move {
        let all_markets = state.drift.get_all_market_ids();
        log::info!("subscribing markets: {:?}", &all_markets);
        if let Err(err) = state.drift.subscribe_markets(&all_markets).await {
            log::error!("couldn't subscribe markets: {err:?}, RPC sim disabled!");
            state.disable_rpc_sim();
        }
        if let Err(err) = state.drift.subscribe_oracles(&all_markets).await {
            log::error!("couldn't subscribe oracles: {err:?}, RPC sim disabled!");
            state.disable_rpc_sim();
        }

        if let Err(err) = state.drift.subscribe_blockhashes().await {
            log::error!("couldn't subscribe to blockhashes: {err:?}, RPC sim disabled!");
            state.disable_rpc_sim();
        }
    });

    // App
    let host = env::var("HOST").unwrap_or("0.0.0.0".to_string());
    let port = env::var("PORT").unwrap_or("3000".to_string());
    let cors = CorsLayer::new()
        .allow_methods([Method::POST, Method::GET, Method::OPTIONS])
        .allow_headers(Any)
        .allow_origin(Any);
    let addr: SocketAddr = format!("{host}:{port}").parse().unwrap();
    let app = Router::new()
        .fallback(fallback)
        .route("/orders", post(process_order_wrapper))
        .route("/health", get(health_check))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    log::info!("Swift server on {}", listener.local_addr().unwrap());

    // Metrics
    let registry = Arc::new(registry);
    let server_metrics_state = MetricsServerParams { registry };
    let metrics_addr: SocketAddr = format!(
        "0.0.0.0:{}",
        env::var("METRICS_PORT").unwrap_or("9464".to_string())
    )
    .parse()
    .unwrap();
    let metrics_app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(server_metrics_state);

    let listener_metrics = tokio::net::TcpListener::bind(&metrics_addr).await.unwrap();
    log::info!(
        "Swift metrics server on {}",
        listener_metrics.local_addr().unwrap()
    );

    // RPC sim loop to avoid rpc cold starts when orders are infrequent
    // Build tx once and just resign with new blockhash
    let rpc_sim_loop = tokio::spawn(async {
        let sender = Keypair::new();
        let receiver = Keypair::new();
        let instruction = solana_sdk::system_instruction::transfer(
            &sender.pubkey(),
            &receiver.pubkey(),
            1_000_000_000u64,
        );

        let mut interval = tokio::time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            let message = Message::try_compile(
                &sender.pubkey(),
                &[instruction.clone()],
                &[],
                Hash::default(),
            )
            .unwrap();
            let versioned_message = VersionedMessage::V0(message);
            let _ = state
                .drift
                .rpc()
                .simulate_transaction_with_config(
                    &VersionedTransaction {
                        message: versioned_message,
                        // must provide a signature for the RPC call to work
                        signatures: vec![Signature::new_unique()],
                    },
                    RpcSimulateTransactionConfig {
                        sig_verify: false,
                        replace_recent_blockhash: true,
                        ..Default::default()
                    },
                )
                .await;
        }
    });

    let send_heartbeat_loop = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            send_heartbeat(state).await;
        }
    });

    let axum_server = tokio::spawn(async { axum::serve(listener, app).await });
    let metrics_server = tokio::spawn(async { axum::serve(listener_metrics, metrics_app).await });

    let _ = tokio::try_join!(
        rpc_sim_loop,
        axum_server,
        metrics_server,
        send_heartbeat_loop
    );
}

/// Simple validation from program's `handle_signed_order_ix`
fn validate_signed_order_params(
    taker_order_params: &OrderParams,
    market: &Option<PerpMarket>,
) -> Result<(), ErrorCode> {
    if !matches!(
        taker_order_params.order_type,
        OrderType::Market | OrderType::Oracle | OrderType::Limit
    ) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }

    if !matches!(taker_order_params.market_type, MarketType::Perp) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }

    match market {
        Some(market) => {
            if taker_order_params.base_asset_amount < market.amm.min_order_size {
                return Err(ErrorCode::InvalidOrderSizeTooSmall);
            }
        }
        None => {
            return Err(ErrorCode::PerpMarketNotFound);
        }
    }

    if taker_order_params.auction_duration.is_none()
        || taker_order_params.auction_start_price.is_none()
        || taker_order_params.auction_end_price.is_none()
    {
        return Err(ErrorCode::InvalidOrderAuction);
    }

    Ok(())
}

#[derive(Debug)]
pub enum SimulationStatus {
    /// Success sim'd locally
    Success,
    Degraded,
    Timeout,
    Disabled,
    /// Success but sim'd over RPC
    SuccessRpc,
    /// Given leniency for collateral error
    SuccessCollateralBuffer,
}

impl SimulationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Degraded => "degraded",
            Self::Timeout => "timeout",
            Self::Disabled => "disabled",
            Self::SuccessRpc => "successRpc",
            Self::SuccessCollateralBuffer => "successBuffer",
        }
    }
}

impl ServerParams {
    /// Toggle RPC simulation off
    pub fn disable_rpc_sim(&self) {
        self.config
            .disable_rpc_sim
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    /// True if RPC simulation is set disabled
    pub fn is_rpc_sim_disabled(&self) -> bool {
        self.config
            .disable_rpc_sim
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    fn simulate_taker_order_local(
        &self,
        order_params: &OrderParams,
        user: &drift_rs::types::accounts::User,
    ) -> bool {
        let state = match self.drift.state_account() {
            Ok(s) => s,
            Err(err) => {
                log::warn!(target: "sim", "state account fetch failed: {err:?}");
                return false;
            }
        };
        let mut hlm: HighLeverageModeConfig =
            match self.drift.try_get_account(high_leverage_mode_account()) {
                Ok(s) => s,
                Err(err) => {
                    log::warn!(target: "sim", "HLM config account fetch failed: {err:?}");
                    return false;
                }
            };
        let mut accounts_builder = AccountsListBuilder::default();
        let accounts = accounts_builder.try_build(
            &self.drift,
            user,
            &[MarketId::new(
                order_params.market_index,
                order_params.market_type,
            )],
        );
        if let Err(err) = accounts {
            log::warn!(target: "sim", "couldn't build accounts for sim: {err:?}");
            return false;
        }

        // simulation executed with error status
        match drift_rs::ffi::simulate_place_perp_order(
            user,
            &mut accounts.unwrap(),
            &state,
            order_params,
            Some(&mut hlm),
        ) {
            Ok(_) => true,
            Err(err) => {
                log::debug!(target: "sim", "local sim failed: {err:?}");
                false
            }
        }
    }
    /// Simulate the taker placing a perp order via RPC, tries local sim first
    async fn simulate_taker_order_rpc(
        &self,
        taker_subaccount_pubkey: &Pubkey,
        taker_order_params: &OrderParams,
        delegate_signer: Option<&Pubkey>,
        slot: Slot,
    ) -> Result<SimulationStatus, (axum::http::StatusCode, String, Option<Vec<String>>)> {
        let mut sim_result = SimulationStatus::Disabled;

        let t0 = SystemTime::now();

        let user_with_timeout = tokio::time::timeout(
            self.config.simulation_timeout,
            self.user_account_fetcher
                .get_user(taker_subaccount_pubkey, slot),
        )
        .await;

        if user_with_timeout.is_err() {
            sim_result = SimulationStatus::Timeout;
            warn!(target: "sim", "simulateTransaction degraded (timeout)");
            return Ok(sim_result);
        }

        let user_result = user_with_timeout.unwrap();
        let user = user_result.map_err(|err| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("unable to fetch user: {err:?}"),
                None,
            )
        })?;

        // check the account delegate matches the signer
        // if delegate_signer.is_some_and(|d| d != &user.delegate) {
        //     return Err((
        //         axum::http::StatusCode::BAD_REQUEST,
        //         "signer is not configured delegate".to_string(),
        //     ));
        // }

        log::info!(
            target: "server",
            "{:?}: max_leverage={},margin_mode={:?},activate_hlm={}",
            user.authority,
            taker_order_params.base_asset_amount == u64::MAX,
            user.margin_mode,
            taker_order_params.high_leverage_mode(),
        );

        if self.is_rpc_sim_disabled() {
            return Ok(sim_result);
        }

        let t1 = SystemTime::now();
        log::info!(target: "sim", "fetch user: {:?}", SystemTime::now().duration_since(t0));

        if self.simulate_taker_order_local(taker_order_params, &user) {
            sim_result = SimulationStatus::Success;
            log::info!(target: "sim", "simulate tx (local): {:?}", SystemTime::now().duration_since(t1));
            return Ok(sim_result);
        }

        // fallback to network sim
        let message = TransactionBuilder::new(
            self.drift.program_data(),
            *taker_subaccount_pubkey,
            std::borrow::Cow::Owned(user),
            false,
        )
        .with_priority_fee(5_000, Some(1_400_000))
        .place_orders(vec![*taker_order_params])
        .build();

        let simulate_result_with_timeout = tokio::time::timeout(
            self.config.simulation_timeout,
            self.drift.rpc().simulate_transaction_with_config(
                &VersionedTransaction {
                    message,
                    // must provide a signature for the RPC call to work
                    signatures: vec![Signature::new_unique()],
                },
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::confirmed()),
                    min_context_slot: Some(slot),
                    ..Default::default()
                },
            ),
        )
        .await;

        match simulate_result_with_timeout {
            Ok(Ok(res)) => {
                if let Some(simulate_err) = res.value.err {
                    log::warn!(target: "sim", "program sim error: {simulate_err:?}");
                    let err = SdkError::Rpc(client_error::Error {
                        request: None,
                        kind: client_error::ErrorKind::TransactionError(simulate_err.to_owned()),
                    });
                    match err.to_anchor_error_code() {
                        Some(code) => {
                            // insufficient collateral is prone to precision errors, allow the order through with some leniency
                            if code == ProgramError::Drift(ErrorCode::InsufficientCollateral) {
                                if let Some(ref logs) = res.value.logs {
                                    if let Some(collateral_ratio) = extract_collateral_ratio(logs) {
                                        if collateral_ratio <= COLLATERAL_BUFFER {
                                            log::info!(target: "sim", "accepting undercollateralized order: {collateral_ratio}");
                                            log::info!(target: "sim", "simulate tx (rpc): {:?}", SystemTime::now().duration_since(t1));
                                            return Ok(SimulationStatus::SuccessCollateralBuffer);
                                        }
                                    }
                                }
                            }
                            Err((
                                axum::http::StatusCode::BAD_REQUEST,
                                format!("invalid order. error code: {code:?}"),
                                res.value.logs,
                            ))
                        }
                        None => Err((
                            axum::http::StatusCode::BAD_REQUEST,
                            format!("invalid order: {simulate_err:?}"),
                            res.value.logs,
                        )),
                    }
                } else {
                    log::info!(target: "sim", "simulate tx (rpc): {:?}", SystemTime::now().duration_since(t1));
                    sim_result = SimulationStatus::SuccessRpc;
                    Ok(sim_result)
                }
            }
            Ok(Err(err)) => {
                log::warn!(target: "sim", "network sim error: {err:?}");
                sim_result = SimulationStatus::Degraded;
                Ok(sim_result)
            }
            Err(_) => {
                sim_result = SimulationStatus::Timeout;
                Ok(sim_result)
            }
        }
    }

    /// Simulate if auction params will be sanitized
    fn simulate_will_auction_params_sanitize(&self, order_params: &OrderParams) -> bool {
        let perp_market = match self
            .drift
            .try_get_perp_market_account(order_params.market_index)
        {
            Ok(m) => m,
            Err(err) => {
                log::debug!(target: "sim", "couldn't get perp market: {err:?}");
                return false;
            }
        };

        let market_id = MarketId::new(order_params.market_index, order_params.market_type);
        let oracle_data = match self.drift.try_get_oracle_price_data_and_slot(market_id) {
            Some(p) => p,
            None => {
                log::debug!(target: "sim", "oracle price is None");
                return false;
            }
        };

        match drift_rs::ffi::simulate_will_auction_params_sanitize(
            order_params,
            &perp_market,
            oracle_data.data.price,
            true,
        ) {
            Ok(result) => result,
            Err(err) => {
                log::debug!(target: "sim", "local sim failed: {err:?}");
                true
            }
        }
    }
}

/// extract collateral ratio from program sim logs
fn extract_collateral_ratio(logs: &[String]) -> Option<f64> {
    for line in logs {
        if line.contains("Program log: total_collateral=") {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 2 {
                // Extract total_collateral
                let total_collateral_part = parts[0];
                let margin_requirement_part = parts[1];

                let total_collateral = total_collateral_part
                    .split('=')
                    .nth(1)?
                    .trim()
                    .parse::<f64>()
                    .ok()?;

                let margin_requirement = margin_requirement_part
                    .split('=')
                    .nth(1)?
                    .trim()
                    .parse::<f64>()
                    .ok()?;

                if total_collateral != 0.0 {
                    return Some(margin_requirement / total_collateral);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![cfg(test)]
    use crate::{
        super_slot_subscriber::SuperSlotSubscriber, swift_server::ServerParams,
        user_account_fetcher::UserAccountFetcher, util::metrics::SwiftServerMetrics,
    };
    use drift_rs::{
        types::{accounts::User, MarketType, OrderParams, OrderType, PositionDirection},
        DriftClient, Pubkey, RpcClient,
    };
    use solana_sdk::{native_token::LAMPORTS_PER_SOL, signature::Keypair, signer::Signer};
    use std::{collections::HashMap, sync::Arc};

    #[tokio::test]
    async fn test_simulate_taker_order_rpc() {
        let _ = env_logger::try_init();
        // Create mock server params
        let drift = DriftClient::new(
            drift_rs::Context::DevNet,
            RpcClient::new("https://api.devnet.solana.com".to_string()),
            Keypair::new().into(),
        )
        .await
        .unwrap();

        let taker_pubkey = Keypair::new().pubkey();
        let taker_pubkey2 = Keypair::new().pubkey();
        let delegate_pubkey = Keypair::new().pubkey();
        let users: HashMap<Pubkey, User> = [
            (
                taker_pubkey,
                User {
                    authority: taker_pubkey,
                    delegate: Pubkey::default(),
                    ..Default::default()
                },
            ),
            (
                taker_pubkey2,
                User {
                    authority: taker_pubkey2,
                    delegate: delegate_pubkey,
                    ..Default::default()
                },
            ),
        ]
        .into();

        dbg!(users.contains_key(&taker_pubkey));
        dbg!(users.contains_key(&taker_pubkey2));

        let server_params = ServerParams {
            slot_subscriber: Arc::new(SuperSlotSubscriber::new(vec![], drift.rpc())),
            metrics: SwiftServerMetrics::new(),
            user_account_fetcher: UserAccountFetcher::mock(users),
            config: Arc::new(crate::swift_server::Config::from_env()),
            drift,
            farmer_pubkeys: Default::default(),
            kafka_producer: Default::default(),
            redis_pool: Default::default(),
        };

        // Create mock order params
        let order_params = OrderParams {
            market_index: 0,
            market_type: MarketType::Perp,
            order_type: OrderType::Market,
            base_asset_amount: 1 * LAMPORTS_PER_SOL,
            price: 1_000,
            direction: PositionDirection::Short,
            ..Default::default()
        };

        // Test
        let result = server_params
            .simulate_taker_order_rpc(&taker_pubkey, &order_params, Some(&delegate_pubkey), 1_000)
            .await;
        assert!(result.is_err_and(|(status, msg, _)| {
            dbg!(&msg);
            status == axum::http::StatusCode::BAD_REQUEST
                && msg.contains("signer is not configured delegate")
        }));

        let result = server_params
            .simulate_taker_order_rpc(&taker_pubkey2, &order_params, Some(&delegate_pubkey), 1_000)
            .await;
        // it fails later at remote sim since the account is not a real drift account
        assert!(result.is_err_and(|(status, msg, _)| {
            dbg!(&msg);
            status == axum::http::StatusCode::BAD_REQUEST
                && msg.contains("invalid order: AccountNotFound")
        }));
    }
}
