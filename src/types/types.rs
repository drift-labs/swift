use drift_rs::types::SdkError;
use std::time::UNIX_EPOCH;
use thiserror::Error;

#[derive(PartialEq, Debug, strum_macros::AsRefStr)]
pub enum WsError {
    /// Client tried to do a protected action while unauthenticated
    Unauthenticated,
    /// Client failed to authenticate its challenge
    FailedChallenge,
    /// Unknown drift market with name
    UnknownMarket(String),
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

#[derive(Error, Debug)]
pub enum TxError {
    #[error("internal error: {0}")]
    Sdk(#[from] SdkError),
    #[error("tx failed ({code}): {reason}")]
    TxFailed { reason: String, code: u32 },
    #[error("tx not found: {tx_sig}")]
    TxNotFound { tx_sig: String },
}

/// Current unix timestamp (ms)
pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
