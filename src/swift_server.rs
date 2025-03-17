use std::{
    env,
    net::SocketAddr,
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, SystemTime},
};

use axum::{
    extract::State,
    http::Method,
    routing::{get, post},
    Json, Router,
};
use deadpool_redis::{Config as RedisConfig, Pool, Runtime};
use dotenv::dotenv;
use drift_rs::{
    event_subscriber::PubsubClient,
    math::account_list_builder::AccountsListBuilder,
    types::{
        errors::ErrorCode, Context, MarketId, MarketType, OrderParams, OrderType,
        SignedMsgOrderParamsMessage, VersionedMessage, VersionedTransaction,
    },
    DriftClient, RpcClient, Wallet,
};
use log::warn;
use prometheus::Registry;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    util::Timeout,
};
use redis::AsyncCommands;
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
use solana_sdk::{
    clock::Slot,
    hash::Hash,
    message::v0::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
};
use tower_http::cors::{Any, CorsLayer};

use crate::{
    connection::kafka_connect::KafkaClientBuilder,
    super_slot_subscriber::SuperSlotSubscriber,
    types::{
        messages::{
            IncomingSignedMessage, OrderMetadataAndMessage, ProcessOrderResponse,
            PROCESS_ORDER_RESPONSE_ERROR_INTERNAL_CONNECTION_ERROR,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD,
            PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
            PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
        },
        types::unix_now_ms,
    },
    user_account_fetcher::UserAccountFetcher,
    util::metrics::{metrics_handler, MetricsServerParams, SwiftServerMetrics},
};

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
    redis_pool: Option<Pool>,
    user_account_fetcher: UserAccountFetcher,
    config: Arc<Config>,
}

pub async fn fallback(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::NOT_FOUND, format!("No route {uri}"))
}

pub async fn process_order(
    State(server_params): State<&'static ServerParams>,
    Json(incoming_message): Json<IncomingSignedMessage>,
) -> impl axum::response::IntoResponse {
    let process_order_time = unix_now_ms();
    server_params.metrics.taker_orders_counter.inc();

    let IncomingSignedMessage {
        taker_pubkey,
        signature: taker_signature,
        message: _,
        signing_authority,
    } = incoming_message;

    let taker_pubkey = Pubkey::new_from_array(taker_pubkey);
    let signing_pubkey = if signing_authority == [0u8; 32] {
        taker_pubkey
    } else {
        Pubkey::new_from_array(signing_authority)
    };

    let log_prefix = format!("[process_order {taker_pubkey}: {process_order_time}]");

    let taker_message_and_prefix = match incoming_message.verify_and_get_signed_message() {
        Ok(taker_message_and_prefix) => taker_message_and_prefix,
        Err(e) => {
            log::error!("{log_prefix}: Error verifying signed message: {e:?}",);
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
                    error: Some(e.to_string()),
                }),
            );
        }
    };
    let taker_message = taker_message_and_prefix.message;

    // check the order's slot is reasonable
    if taker_message.slot < server_params.slot_subscriber.current_slot() - 500 {
        log::warn!(
            target: "server",
            "{log_prefix}: Order slot too old: {}, current slot: {}",
            taker_message.slot,
            server_params.slot_subscriber.current_slot(),
        );
        let err_str = PROCESS_ORDER_RESPONSE_ERROR_MSG_ORDER_SLOT_TOO_OLD;
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: err_str,
                error: Some(err_str.to_string()),
            }),
        );
    }

    // check the order is valid for execution by program
    let slot = server_params.slot_subscriber.current_slot();
    if let Err(err) = validate_signed_order_params(&taker_message.signed_msg_order_params) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                error: Some(err.to_string()),
            }),
        );
    }
    match server_params
        .simulate_taker_order_rpc(&taker_pubkey, &taker_message, slot)
        .await
    {
        Ok(sim_res) => {
            server_params
                .metrics
                .rpc_simulation_status
                .with_label_values(&[sim_res.as_str()])
                .inc();
        }
        Err((status, sim_err_str)) => {
            server_params
                .metrics
                .rpc_simulation_status
                .with_label_values(&["invalid"])
                .inc();
            return (
                status,
                Json(ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                    error: Some(sim_err_str),
                }),
            );
        }
    }

    let order_metadata = OrderMetadataAndMessage {
        signing_authority: signing_pubkey,
        taker_authority: taker_pubkey,
        order_message: taker_message_and_prefix,
        order_signature: taker_signature,
        ts: process_order_time,
        uuid: taker_message.uuid,
    };
    let encoded = order_metadata.encode();
    log::trace!(target: "server", "base64 encoded message: {encoded:?}");

    let market_index = taker_message.signed_msg_order_params.market_index;
    let market_type = taker_message.signed_msg_order_params.market_type;

    let topic = format!("swift_orders_{}_{market_index}", market_type.as_str());
    log::trace!(target: "server", "{log_prefix}: Topic: {topic}");

    if let Some(kafka_producer) = &server_params.kafka_producer {
        match kafka_producer
            .send(
                FutureRecord::<String, String>::to(&topic).payload(&encoded),
                Timeout::After(Duration::ZERO),
            )
            .await
        {
            Ok(_) => {
                log::trace!(target: "kafka", "{log_prefix}: Sent message for order: {order_metadata:?}");
                server_params.metrics.current_slot_gauge.add(slot as f64);
                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&[market_type.as_str(), &market_index.to_string()])
                    .inc();

                server_params
                    .metrics
                    .response_time_histogram
                    .observe((unix_now_ms() - process_order_time) as f64);
                (
                    axum::http::StatusCode::OK,
                    Json(ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
                        error: None,
                    }),
                )
            }
            Err((e, _)) => {
                log::error!(
                    target: "kafka",
                    "{log_prefix}: Failed to deliver for order: {order_metadata:?}, error: {e:?}"
                );
                server_params.metrics.kafka_forward_fail_counter.inc();
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                        error: Some(format!("kafka publish error: {e:?}")),
                    }),
                )
            }
        }
    } else {
        let mut conn = match server_params.redis_pool.as_ref().unwrap().get().await {
            Ok(conn) => conn,
            Err(e) => {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_INTERNAL_CONNECTION_ERROR,
                        error: Some(format!("redis connection error: {e:?}")),
                    }),
                )
            }
        };

        match conn.publish::<String, String, i64>(topic, encoded).await {
            Ok(_) => {
                log::trace!(target: "redis", "{log_prefix}: Sent redis message for order: {order_metadata:?}");
                server_params.metrics.current_slot_gauge.add(slot as f64);
                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&[market_type.as_str(), &market_index.to_string()])
                    .inc();

                server_params
                    .metrics
                    .response_time_histogram
                    .observe((unix_now_ms() - process_order_time) as f64);

                (
                    axum::http::StatusCode::OK,
                    Json(ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
                        error: None,
                    }),
                )
            }
            Err(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                    error: Some(format!("redis publish error: {e:?}")),
                }),
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
                    .with_label_values(&["_", "heartbeat"])
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
        let mut conn = match server_params.redis_pool.as_ref().unwrap().get().await {
            Ok(conn) => conn,
            Err(_) => {
                log::error!(target: "redis", "{log_prefix}: Obtaining redis connection failed");
                return;
            }
        };

        match conn
            .publish::<String, String, i64>("heartbeat".to_string(), "love you".to_string())
            .await
        {
            Ok(_) => {
                log::trace!(target: "redis", "{log_prefix}: Sent redis heartbeat");
                server_params
                    .metrics
                    .order_type_counter
                    .with_label_values(&["_", "heartbeat"])
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
    if ws_healthy && slot_sub_healthy {
        (axum::http::StatusCode::OK, "ok".into())
    } else {
        let msg = format!("slot_sub_healthy={slot_sub_healthy} | ws_sub_healthy={ws_healthy}");
        log::error!("{}", &msg);
        (axum::http::StatusCode::PRECONDITION_FAILED, msg)
    }
}

pub async fn start_server() {
    dotenv().ok();

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
        let cfg = RedisConfig::from_url(connection_string);
        Some(
            cfg.create_pool(Some(Runtime::Tokio1))
                .expect("Failed to create Redis pool"),
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
    let client = DriftClient::new(
        context,
        RpcClient::new(rpc_endpoint),
        Keypair::new().into(), // not sending txs
    )
    .await
    .expect("initialized client");

    let user_account_fetcher = UserAccountFetcher::from_env(client.clone());

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

    let state: &'static ServerParams = Box::leak(Box::new(ServerParams {
        drift: client,
        slot_subscriber: Arc::new(slot_subscriber),
        kafka_producer,
        metrics,
        redis_pool,
        user_account_fetcher,
        config: Arc::new(Config::from_env()),
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
        .route("/orders", post(process_order))
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
fn validate_signed_order_params(taker_order_params: &OrderParams) -> Result<(), ErrorCode> {
    if !matches!(
        taker_order_params.order_type,
        OrderType::Market | OrderType::Oracle | OrderType::Limit
    ) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }

    if !matches!(taker_order_params.market_type, MarketType::Perp) {
        return Err(ErrorCode::InvalidOrderMarketType);
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
enum SimulationStatus {
    Success,
    Degraded,
    Timeout,
    Disabled,
}

impl SimulationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Degraded => "degraded",
            Self::Timeout => "timeout",
            Self::Disabled => "disabled",
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
    /// Simulate the taker placing a perp order via RPC
    async fn simulate_taker_order_rpc(
        &self,
        taker_authority: &Pubkey,
        taker_message: &SignedMsgOrderParamsMessage,
        slot: Slot,
    ) -> Result<SimulationStatus, (axum::http::StatusCode, String)> {
        if self.is_rpc_sim_disabled() {
            return Ok(SimulationStatus::Disabled);
        }

        let taker_subaccount_pubkey =
            Wallet::derive_user_account(taker_authority, taker_message.sub_account_id);

        let t0 = SystemTime::now();

        let user_with_timeout = tokio::time::timeout(
            self.config.simulation_timeout,
            self.user_account_fetcher
                .get_user(&taker_subaccount_pubkey, slot),
        )
        .await;

        if user_with_timeout.is_err() {
            warn!("simulateTransaction degraded (timeout)");
            return Ok(SimulationStatus::Timeout);
        }

        let user_result = user_with_timeout.unwrap();
        let user = user_result.map_err(|err| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("unable to fetch user: {err:?}"),
            )
        })?;

        let t1 = SystemTime::now();
        log::info!("fetch user: {:?}", SystemTime::now().duration_since(t0));
        let state = match self.drift.state_account() {
            Ok(s) => s,
            Err(err) => {
                log::error!("state account fetch failed: {err:?}");
                return Ok(SimulationStatus::Degraded);
            }
        };

        let mut accounts_builder = AccountsListBuilder::default();
        let accounts = accounts_builder.try_build(
            &self.drift,
            &user,
            &[MarketId::perp(
                taker_message.signed_msg_order_params.market_index,
            )],
        );
        if let Err(err) = accounts {
            log::error!("couldn't build accounts for sim: {err:?}");
            return Ok(SimulationStatus::Degraded);
        }

        // simulation executed with error status
        if let Err(err) = drift_rs::ffi::simulate_place_perp_order(
            &user,
            &mut accounts.unwrap(),
            &state,
            &taker_message.signed_msg_order_params,
        ) {
            return Err((
                axum::http::StatusCode::BAD_REQUEST,
                format!("invalid order: {:?}", err.to_anchor_error_code(),),
            ));
        }
        log::info!("simulate tx: {:?}", SystemTime::now().duration_since(t1));

        Ok(SimulationStatus::Success)
    }
}
