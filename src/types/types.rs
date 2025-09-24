use std::time::UNIX_EPOCH;

use drift_rs::types::MarketType;

use crate::types::messages::IncomingSignedMessage;

#[derive(PartialEq, Debug, strum_macros::AsRefStr)]
pub enum WsError {
    /// Client tried to do a protected action while unauthenticated
    Unauthenticated,
    /// Client failed to authenticate its challenge
    FailedChallenge,
    /// Unknown kafkas topic
    UnknownTopic(String),
    /// Server couldn't handle the message backpressure
    /// in a timely manner
    Backpressure,
    /// Sending Ws update to client failed
    SendFailed,
    /// Websocket closed unexpectedly
    SocketClosed,
    /// Closed for some Ws protocol reason
    Protocol,
    /// Internal channel closed
    ChannelClosed,
    /// Invalid client message
    BadMessage,
    /// Failed Ws handshake
    Handshake,
}

/// Current unix timestamp (ms)
pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[derive(Clone)]
pub struct RequestContext {
    pub recv_ts: u64,
    pub log_prefix: String,
    pub market_index: u16,
    pub market_type: &'static str,
}

impl RequestContext {
    pub fn from_incoming_message(msg: &IncomingSignedMessage) -> Self {
        let recv_ts = unix_now_ms();
        let info = msg.order().info(&msg.taker_authority);

        Self {
            market_index: info.order_params.market_index,
            market_type: match info.order_params.market_type {
                MarketType::Perp => "perp",
                MarketType::Spot => "spot",
            },
            recv_ts,
            log_prefix: format!("[process_order {}: {recv_ts}]", msg.taker_authority),
        }
    }
}
