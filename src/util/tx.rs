use std::time::SystemTime;

use crate::types::types::TxError;
use drift_rs::types::RpcSendTransactionConfig;
use drift_rs::types::SdkError;
use drift_rs::types::VersionedMessage;
use drift_rs::DriftClient;
use log::{debug, info, warn};
use prometheus::core::AtomicF64;
use prometheus::core::GenericCounter;
use prometheus::IntCounter;
use serde::{Deserialize, Serialize};
use solana_sdk::commitment_config::CommitmentLevel;
use tokio::time::Duration;

#[derive(Serialize, Deserialize, Debug)]
pub struct TxResponse {
    tx: String,
}

impl TxResponse {
    pub fn new(tx_signature: String) -> Self {
        Self { tx: tx_signature }
    }
}

const LOG_TARGET: &str = "tx";
const DEFAULT_TX_TTL: u16 = 30;

pub async fn send_tx(
    client: &DriftClient,
    tx: VersionedMessage,
    reason: &'static str,
    ttl: Option<u16>,
    log_prefix: String,
    confirmed_counter: IntCounter,
) -> Result<TxResponse, TxError> {
    let recent_block_hash = client.get_latest_blockhash().await?;
    let tx = client.wallet().sign_tx(tx, recent_block_hash)?;
    let tx_config = RpcSendTransactionConfig {
        max_retries: Some(0),
        preflight_commitment: Some(CommitmentLevel::Confirmed),
        skip_preflight: true,
        ..Default::default()
    };

    // submit to primary RPC first,
    let sig = client
        .rpc()
        .send_transaction_with_config(&tx, tx_config)
        .await
        .inspect(|s| {
            log::debug!(target: LOG_TARGET, "{log_prefix} sent tx ({reason}): {s}");
        })
        .map_err(|err| {
            warn!(target: LOG_TARGET, "sending tx ({reason}) failed: {err:?}");
            // tx has some program/logic error, retry won't fix
            handle_tx_err(err.into())
        })?;

    // start a dedicated tx sending task
    // - tx is broadcast to all available RPCs
    // - retried at set intervals
    // - retried upto some given deadline
    // client should poll for the tx to confirm success
    let tx_signature = sig;
    let rpc = client.clone().rpc();
    tokio::spawn(async move {
        let start = SystemTime::now();
        let ttl = Duration::from_secs(ttl.unwrap_or(DEFAULT_TX_TTL) as u64);
        let mut confirmed = false;
        while SystemTime::now()
            .duration_since(start)
            .is_ok_and(|x| x < ttl)
        {
            let res = rpc.send_transaction_with_config(&tx, tx_config).await;
            match res {
                Ok(sig) => {
                    debug!(target: LOG_TARGET, "sent tx ({reason}): {sig}");
                }
                Err(err) => {
                    warn!(target: LOG_TARGET, "sending tx ({reason}) failed: {err:?}");
                }
            };

            tokio::time::sleep(Duration::from_secs(2)).await;

            if let Ok(Some(Ok(()))) = rpc.get_signature_status(&tx_signature).await {
                confirmed = true;
                confirmed_counter.inc();
                info!(target: LOG_TARGET, "tx confirmed onchain: {tx_signature:?}");
                break;
            }
        }
        if !confirmed {
            warn!(target: LOG_TARGET, "tx was not confirmed: {tx_signature:?}");
        }
    });

    Ok(TxResponse::new(sig.to_string()))
}

fn handle_tx_err(err: SdkError) -> TxError {
    if let Some(err) = err.to_anchor_error_code() {
        match err {
            drift_rs::types::ProgramError::Drift(err) => TxError::TxFailed {
                reason: err.to_string(),
                code: err.into(),
            },
            drift_rs::types::ProgramError::Other { ix_idx, code } => TxError::TxFailed {
                reason: format!("tx failed. ix index: {ix_idx}"),
                code: code.into(),
            },
        }
    } else {
        TxError::Sdk(err)
    }
}
