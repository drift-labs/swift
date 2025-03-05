use std::{collections::HashMap, env, net::SocketAddr};

use axum::{
    extract::{Query, State},
    http::Method,
    routing::get,
    Router,
};
use axum_prometheus::PrometheusMetricLayer;
use deadpool_redis::{redis::AsyncCommands, Config, Pool, Runtime};
use dotenv::dotenv;
use futures_util::StreamExt;
use redis::ScanOptions;
use tower_http::cors::{Any, CorsLayer};

#[derive(Clone)]
pub struct ServerParams {
    pub host: String,
    pub port: String,
    pub redis_pool: Pool,
}
#[derive(serde::Deserialize)]
pub struct HashQuery {
    pub hash: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct HashesResponse {
    #[serde(rename = "hashes")]
    pub hashes: HashMap<String, String>,
}

pub async fn fallback(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    (axum::http::StatusCode::NOT_FOUND, format!("No route {uri}"))
}

pub async fn health_check() -> impl axum::response::IntoResponse {
    axum::http::StatusCode::OK
}

pub async fn get_all_hashes(
    State(server_params): State<ServerParams>,
) -> impl axum::response::IntoResponse {
    let mut conn = match server_params.redis_pool.get().await {
        Ok(conn) => conn,
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Redis connection failed".to_string(),
            )
        }
    };

    let pattern = "swift-hashes::*";
    let scan_opts = ScanOptions::default().with_pattern(pattern).with_count(100);
    let keys = match conn.scan_options::<String>(scan_opts).await {
        Ok(it) => it.collect::<Vec<String>>().await,
        Err(e) => {
            log::error!("Error scanning keys: {:?}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Error getting keys".to_string(),
            );
        }
    };

    let mut pipe = redis::pipe();
    for key in &keys {
        pipe.get(key);
    }

    let values: Vec<Option<String>> = match pipe.query_async(&mut conn).await {
        Ok(values) => values,
        Err(e) => {
            log::error!("Error executing pipeline: {:?}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Error getting values".to_string(),
            );
        }
    };

    let hash_map: HashMap<String, String> = keys
        .into_iter()
        .zip(values.into_iter())
        .filter_map(|(key, value)| {
            value.map(|v| {
                let hash = key
                    .strip_prefix("swift-hashes::")
                    .unwrap_or(&key)
                    .to_string();
                (hash, v)
            })
        })
        .collect();

    match serde_json::to_string(&HashesResponse { hashes: hash_map }) {
        Ok(json) => (axum::http::StatusCode::OK, json),
        Err(_) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to serialize response".to_string(),
        ),
    }
}

pub async fn get_hash_status(
    State(server_params): State<ServerParams>,
    Query(query): Query<HashQuery>,
) -> impl axum::response::IntoResponse {
    let encoded_hash = query.hash;
    if encoded_hash.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Missing hash parameter".to_string(),
        );
    }
    let decoded_hash_result = urlencoding::decode(&encoded_hash);
    if decoded_hash_result.is_err() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid hash parameter".to_string(),
        );
    }

    let decoded_hash = decoded_hash_result.unwrap().to_string();

    let mut conn = match server_params.redis_pool.get().await {
        Ok(conn) => conn,
        Err(_) => {
            log::error!("Redis connection failed");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Redis connection failed".to_string(),
            );
        }
    };

    let redis_key = format!("swift-hashes::{}", decoded_hash);
    match conn.get::<_, Option<String>>(redis_key).await {
        Ok(Some(value)) => {
            println!("Value for decoded_hash {}: {}", decoded_hash, value);
            (axum::http::StatusCode::OK, value)
        }
        Ok(None) => {
            println!("No value found for key: {}", decoded_hash);
            (axum::http::StatusCode::NOT_FOUND, "not found".to_string())
        }
        Err(e) => {
            println!("Error: {:?}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "error finding".to_string(),
            )
        }
    }
}

pub async fn start_server() {
    dotenv().ok();

    let elasticache_host = env::var("ELASTICACHE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let elasticache_port = env::var("ELASTICACHE_PORT").unwrap_or_else(|_| "6379".to_string());
    let connection_string = if env::var("USE_SSL").unwrap_or_else(|_| "false".to_string()) == "true"
    {
        format!("rediss://{}:{}", elasticache_host, elasticache_port)
    } else {
        format!("redis://{}:{}", elasticache_host, elasticache_port)
    };

    let cfg = Config::from_url(connection_string);
    let redis_pool = cfg
        .create_pool(Some(Runtime::Tokio1))
        .expect("Failed to create Redis pool");

    println!("Connecting to Redis via pool");

    let state = ServerParams {
        redis_pool,
        host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
        port: env::var("PORT").unwrap_or_else(|_| "3000".to_string()),
    };

    println!("Established Redis pool connection");

    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();
    let addr: SocketAddr = format!("{}:{}", state.host, state.port).parse().unwrap();
    let cors = CorsLayer::new()
        .allow_methods([Method::POST, Method::GET, Method::OPTIONS])
        .allow_headers(Any)
        .allow_origin(Any);
    let app = Router::new()
        .fallback(fallback)
        .route("/confirmation/health", get(health_check))
        .route("/confirmation/hash-status", get(get_hash_status))
        .route("/confirmation/hashes", get(get_all_hashes))
        .with_state(state)
        .layer(cors)
        .layer(prometheus_layer);

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    log::info!(
        "Swift confirmation server on {}",
        listener.local_addr().unwrap()
    );

    // Metrics
    let metrics_addr: SocketAddr = format!(
        "0.0.0.0:{}",
        env::var("METRICS_PORT").unwrap_or("9464".to_string())
    )
    .parse()
    .unwrap();
    let metrics_app =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    let listener_metrics = tokio::net::TcpListener::bind(&metrics_addr).await.unwrap();
    log::info!(
        "Swift confirmation metrics server on {}",
        listener_metrics.local_addr().unwrap()
    );

    let _ = tokio::join!(
        axum::serve(listener, app),
        axum::serve(listener_metrics, metrics_app)
    );
}

#[cfg(test)]
mod tests {
    use std::str::from_utf8;

    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    use super::*;

    async fn setup_test_server() -> ServerParams {
        let cfg = Config::from_url("redis://localhost:6379");
        let redis_pool = cfg.create_pool(Some(Runtime::Tokio1)).unwrap();

        ServerParams {
            host: "127.0.0.1".to_string(),
            port: "3000".to_string(),
            redis_pool,
        }
    }

    #[tokio::test]
    async fn test_health_check() {
        let response = health_check().await;
        assert_eq!(response.into_response().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_fallback() {
        let uri = "/not-found".parse().unwrap();
        let response = fallback(uri).await;
        assert_eq!(response.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_hash_status_empty_hash() {
        let state = setup_test_server().await;
        let query = HashQuery {
            hash: "".to_string(),
        };

        let response = get_hash_status(State(state), Query(query)).await;
        assert_eq!(response.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[ignore]
    #[tokio::test]
    async fn test_get_hash_status_invalid_hash() {
        let state = setup_test_server().await;
        let query = HashQuery {
            hash: "%invalid%".to_string(),
        };

        let response = get_hash_status(State(state), Query(query)).await;
        assert_eq!(response.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_hash_status_not_found() {
        let state = setup_test_server().await;
        let query = HashQuery {
            hash: "nonexistent".to_string(),
        };

        let response = get_hash_status(State(state), Query(query)).await;
        assert_eq!(response.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_all_hashes_empty() {
        let state = setup_test_server().await;
        let response = get_all_hashes(State(state)).await;
        let response = response.into_response();

        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let body_str = from_utf8(&body_bytes).unwrap();
        let response: HashesResponse = serde_json::from_str(body_str).unwrap();

        assert!(response.hashes.len() > 0);
    }
}
