use std::{
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
            PROCESS_ORDER_RESPONSE_ERROR_USER_NOT_FOUND, PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY,
            PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
        },
        types::unix_now_ms,
    },
    user_account_fetcher::UserAccountFetcher,
    util::{
        metrics::{metrics_handler, MetricsServerParams, SwiftServerMetrics},
        tx::send_tx,
    },
};
use axum::{
    extract::State,
    http::Method,
    routing::{get, post},
    Json, Router,
};
use dotenv::dotenv;
use drift_rs::{
    event_subscriber::PubsubClient,
    math::account_list_builder::AccountsListBuilder,
    priority_fee_subscriber::{PriorityFeeSubscriber, PriorityFeeSubscriberConfig},
    swift_order_subscriber::{SignedMessageInfo, SignedOrderInfo},
    types::{
        accounts::User, errors::ErrorCode, CommitmentConfig, MarketId, MarketType, OrderParams,
        OrderType, SdkError, VersionedMessage, VersionedTransaction,
    },
    utils::load_keypair_multi_format,
    Context, DriftClient, RpcClient, TransactionBuilder, Wallet,
};
use log::warn;
use prometheus::Registry;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
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

struct Config {
    /// RPC tx simulation on/off
    disable_rpc_sim: AtomicBool,
    /// RPC tx simulation timeout
    simulation_timeout: Duration,
    /// Send sanitized orders directly
    send_sanitized_orders: AtomicBool,
}

impl Config {
    fn from_env() -> Self {
        Self {
            disable_rpc_sim: AtomicBool::new(
                std::env::var("DISABLE_RPC_SIM").unwrap_or("false".to_string()) == "true",
            ),
            simulation_timeout: Duration::from_millis(300),
            send_sanitized_orders: AtomicBool::new(
                std::env::var("SEND_SANITIZED_ORDERS").unwrap_or("false".to_string()) == "true",
            ),
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
    priority_fee_subscriber: Arc<PriorityFeeSubscriber>,
    config: Arc<Config>,
    farmer_pubkeys: Vec<Pubkey>,
}

pub async fn fallback(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::NOT_FOUND, format!("No route {uri}"))
}

pub async fn process_order(
    State(server_params): State<&'static ServerParams>,
    Json(incoming_message): Json<IncomingSignedMessage>,
) -> impl axum::response::IntoResponse {
    let process_order_time = unix_now_ms();
    let IncomingSignedMessage {
        taker_pubkey,
        signature: taker_signature,
        message: _,
        signing_authority,
        taker_authority,
    } = incoming_message;

    if server_params.farmer_pubkeys.contains(&taker_authority) {
        log::debug!(
            target: "server",
            "Ignoring order from farmer pubkey: {taker_authority}"
        );
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_IGNORE_PUBKEY,
                error: None,
            }),
        );
    }

    server_params.metrics.taker_orders_counter.inc();

    let taker_authority = if taker_authority == Pubkey::default() {
        taker_pubkey
    } else {
        taker_authority
    };

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
                Json(ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_VERIFY_SIGNATURE,
                    error: Some(e.to_string()),
                }),
            );
        }
    };
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
            Json(ProcessOrderResponse {
                message: err_str,
                error: Some(err_str.to_string()),
            }),
        );
    }

    // check the order is valid for execution by program
    if let Err(err) = validate_signed_order_params(&order_params) {
        log::warn!(
            target: "server",
            "{log_prefix}: Order did not validate: {err:?}",
        );
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                error: Some(err.to_string()),
            }),
        );
    }

    let user: Option<User> = match server_params
        .simulate_taker_order_rpc(&taker_pubkey, &order_params, current_slot)
        .await
    {
        Ok(sim_res) => {
            server_params
                .metrics
                .rpc_simulation_status
                .with_label_values(&[sim_res.status.as_str()])
                .inc();
            sim_res.user
        }
        Err((status, sim_err_str)) => {
            server_params
                .metrics
                .rpc_simulation_status
                .with_label_values(&["invalid"])
                .inc();
            log::warn!(
                target: "server",
                "{log_prefix}: Order sim failed: {sim_err_str}",
            );
            return (
                status,
                Json(ProcessOrderResponse {
                    message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                    error: Some(sim_err_str),
                }),
            );
        }
    };

    if user.is_none() {
        log::warn!(
            target: "server",
            "{log_prefix}: Error simulating order, user not found"
        );
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(ProcessOrderResponse {
                message: PROCESS_ORDER_RESPONSE_ERROR_USER_NOT_FOUND,
                error: Some("user not found".to_string()),
            }),
        );
    }
    let user = user.unwrap();

    // If fat fingered order that requires sanitization, then just send the order
    if server_params.can_send_sanitized_orders() {
        let mut order_params = order_params.clone();
        if server_params.simulate_will_auction_params_sanitize(&mut order_params) {
            server_params
                .metrics
                .order_type_counter
                .with_label_values(&[
                    &order_params.market_type.as_str(),
                    &order_params.market_index.to_string(),
                    "true",
                ])
                .inc();

            if server_params.is_rpc_sim_disabled() {
                log::warn!(
                    target: "server",
                    "{log_prefix}: RPC disabled, not sending order sanitized order"
                );
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
                );

                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(ProcessOrderResponse {
                        message: PROCESS_ORDER_RESPONSE_ERROR_MSG_INVALID_ORDER,
                        error: Some("RPC offline and order would get sanitized".to_string()),
                    }),
                );
            }

            let swift_subaccount = server_params.drift.wallet().default_sub_account();
            let tx_builder = TransactionBuilder::new(
                server_params.drift.program_data(),
                swift_subaccount,
                std::borrow::Cow::Owned(User {
                    sub_account_id: 0,
                    authority: *server_params.drift.wallet().authority(),
                    ..Default::default()
                }),
                false,
            )
            .with_priority_fee(
                server_params.priority_fee_subscriber.priority_fee(),
                Some(1_400_000),
            );

            let uuid = std::str::from_utf8(&uuid)
                .expect("invalid utf8 uuid")
                .to_string();
            let signed_order_info = SignedOrderInfo::new(
                uuid,
                taker_authority,
                signing_pubkey,
                *signed_msg,
                Signature::from(taker_signature.to_bytes()),
            );
            let versioned_message = tx_builder
                .place_swift_order(&signed_order_info, &user)
                .build();

            let place_tx = send_tx(
                &server_params.drift,
                versioned_message,
                "place swift order",
                Some(30),
                log_prefix.clone(),
            );

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
            );

            match place_tx.await {
                Ok(tx_sig) => {
                    log::trace!(target: "server", "{log_prefix}: Sending sanitized order with order params: {order_params:?}, tx sig: {tx_sig:?}");
                    return (
                        axum::http::StatusCode::OK,
                        Json(ProcessOrderResponse {
                            message: PROCESS_ORDER_RESPONSE_MESSAGE_SUCCESS,
                            error: None,
                        }),
                    );
                }
                Err(err) => {
                    log::error!(
                        target: "server",
                        "{log_prefix}: Error sending order: {err:?}"
                    );
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(ProcessOrderResponse {
                            message: PROCESS_ORDER_RESPONSE_ERROR_MSG_DELIVERY_FAILED,
                            error: Some(format!("tx send error: {err:?}")),
                        }),
                    );
                }
            }
        }
    }

    let order_metadata = OrderMetadataAndMessage {
        signing_authority: signing_pubkey,
        taker_authority,
        order_message: signed_msg.clone(),
        order_signature: taker_signature.into(),
        ts: process_order_time,
        uuid,
    };
    let encoded = order_metadata.encode();
    let market_index = order_params.market_index;
    let market_type = order_params.market_type;

    let topic = format!("swift_orders_{}_{market_index}", market_type.as_str());

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
                server_params
                    .metrics
                    .current_slot_gauge
                    .add(current_slot as f64);
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
        let mut conn = server_params.redis_pool.clone().unwrap();
        match conn.publish::<String, String, i64>(topic, encoded).await {
            Ok(_) => {
                log::trace!(target: "redis", "{log_prefix}: Sent redis message for order: {order_metadata:?}");
                server_params
                    .metrics
                    .current_slot_gauge
                    .add(current_slot as f64);
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
        let redis_health = if let Some(mut conn) = server_params.redis_pool.clone() {
            let ping_result: redis::RedisResult<String> =
                redis::cmd("PING").query_async(&mut conn).await;
            ping_result.is_ok()
        } else {
            false
        };
        redis_health
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
    let rpc_endpoint_cloned = rpc_endpoint.clone(); // for the priority fee subscriber

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

    let pubkeys: Vec<Pubkey> = client
        .program_data()
        .perp_market_configs()
        .iter()
        .map(|config| config.pubkey)
        .collect();
    let priority_fee_subscriber = PriorityFeeSubscriber::with_config(
        RpcClient::new_with_commitment(rpc_endpoint_cloned.into(), CommitmentConfig::confirmed()),
        &pubkeys[..],
        PriorityFeeSubscriberConfig {
            refresh_frequency: Some(Duration::from_millis(400 * 10)),
            window: None,
        },
    );

    // Set ignore pubkeys
    let ignore_pubkeys = env::var("IGNORE_PUBKEYS").unwrap_or_else(|_| "".to_string());
    let pubkeys: Vec<Pubkey> = ignore_pubkeys
        .split(',')
        .map(|s| s.trim()) // remove extra whitespace
        .filter_map(|s| match s.parse::<Pubkey>() {
            Ok(key) => Some(key),
            Err(_) => {
                log::warn!(target: "server", "Warning: invalid pubkey skipped for ignore pubkeys: {s:?}");
                None
            }
        })
        .collect();

    let state: &'static ServerParams = Box::leak(Box::new(ServerParams {
        drift: client,
        slot_subscriber: Arc::new(slot_subscriber),
        kafka_producer,
        metrics,
        redis_pool,
        user_account_fetcher,
        priority_fee_subscriber: priority_fee_subscriber.subscribe(),
        config: Arc::new(Config::from_env()),
        farmer_pubkeys: pubkeys,
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
    /// Success sim'd locally
    Success,
    Degraded,
    Timeout,
    Disabled,
    /// Success but sim'd over RPC
    SuccessRpc,
}

impl SimulationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Degraded => "degraded",
            Self::Timeout => "timeout",
            Self::Disabled => "disabled",
            Self::SuccessRpc => "successRpc",
        }
    }
}

// Can we get compute units from local sim?
pub struct SimulationResult {
    pub status: SimulationStatus,
    pub user: Option<drift_rs::types::accounts::User>,
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
    /// True if we can send sanitized orders
    pub fn can_send_sanitized_orders(&self) -> bool {
        self.config
            .send_sanitized_orders
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
        slot: Slot,
    ) -> Result<SimulationResult, (axum::http::StatusCode, String)> {
        let mut sim_result = SimulationResult {
            status: SimulationStatus::Disabled,
            user: None,
        };

        let t0 = SystemTime::now();

        let user_with_timeout = tokio::time::timeout(
            self.config.simulation_timeout,
            self.user_account_fetcher
                .get_user(taker_subaccount_pubkey, slot),
        )
        .await;

        if user_with_timeout.is_err() {
            sim_result.status = SimulationStatus::Timeout;
            warn!(target: "sim", "simulateTransaction degraded (timeout)");
            return Ok(sim_result);
        }

        let user_result = user_with_timeout.unwrap();
        let user = user_result.map_err(|err| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("unable to fetch user: {err:?}"),
            )
        })?;

        sim_result.user = Some(user);

        if self.is_rpc_sim_disabled() {
            return Ok(sim_result);
        }

        let t1 = SystemTime::now();
        log::info!(target: "sim", "fetch user: {:?}", SystemTime::now().duration_since(t0));

        if self.simulate_taker_order_local(taker_order_params, &user) {
            sim_result.status = SimulationStatus::Success;
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
                        Some(code) => Err((
                            axum::http::StatusCode::BAD_REQUEST,
                            format!("invalid order. error code: {code:?}"),
                        )),
                        None => Err((
                            axum::http::StatusCode::BAD_REQUEST,
                            format!("invalid order: {simulate_err:?}"),
                        )),
                    }
                } else {
                    log::info!(target: "sim", "simulate tx (rpc): {:?}", SystemTime::now().duration_since(t1));
                    sim_result.status = SimulationStatus::SuccessRpc;
                    Ok(sim_result)
                }
            }
            Ok(Err(err)) => {
                log::warn!(target: "sim", "network sim error: {err:?}");
                sim_result.status = SimulationStatus::Degraded;
                Ok(sim_result)
            }
            Err(_) => {
                sim_result.status = SimulationStatus::Timeout;
                Ok(sim_result)
            }
        }
    }

    /// Simulate if auction params will be sanitized
    fn simulate_will_auction_params_sanitize(&self, order_params: &mut OrderParams) -> bool {
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
                log::debug!(target: "sim", "orace price is None");
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
