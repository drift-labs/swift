use std::time::SystemTime;

use crate::types::types::TxError;
use drift_rs::types::RpcSendTransactionConfig;
use drift_rs::types::SdkError;
use drift_rs::types::VersionedMessage;
use drift_rs::DriftClient;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use solana_rpc_client_api::config::RpcSimulateTransactionConfig;
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
) -> Result<TxResponse, TxError> {
    let recent_block_hash = client.get_latest_blockhash().await?;
    let tx = client.wallet().sign_tx(tx, recent_block_hash)?;
    let tx_config = RpcSendTransactionConfig {
        max_retries: Some(0),
        preflight_commitment: Some(CommitmentLevel::Confirmed),
        skip_preflight: true,
        ..Default::default()
    };

    // simulate tx first
    let sim_result = client
        .rpc()
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                ..Default::default()
            },
        )
        .await
        .map_err(|err| {
            warn!(target: LOG_TARGET, "sending tx ({reason}) failed: {err:?}");
            // tx has some program/logic error, retry won't fix
            handle_tx_err(err.into())
        })?;

    if let Some(err) = sim_result.value.err {
        if let Some(sim_logs) = sim_result.value.logs {
            warn!(target: LOG_TARGET, "{log_prefix} tx simulation logs: {sim_logs:?}");
        }
        warn!(target: LOG_TARGET, "{log_prefix} tx simulation failed: {err:?}");
        return Err(TxError::SimError {
            tx_sig: tx.signatures[0].to_string(),
        });
    }

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
    if let Some(code) = err.to_anchor_error_code() {
        TxError::TxFailed {
            reason: code.name(),
            code: code.into(),
        }
    } else {
        TxError::Sdk(err)
    }
}
