//! Periodic safety net for the paths where a webhook could be lost for good (the sender gave up
//! after its retry window, or was misconfigured):
//!
//! * deposits stuck in `paid` are checked against the engine's status route;
//! * open deposits well past their expiry are expired locally;
//! * idempotency keys and inbound event ids are pruned.

use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::deposit::store;
use crate::state::AppState;
use crate::webhooks::inbound::{SettlementOutcome, apply_settlement};

const EXPIRY_GRACE_SECS: i64 = 600;

pub async fn run(state: AppState, shutdown: CancellationToken) {
    let interval = Duration::from_secs(state.config.reconciler.interval_secs.max(5));
    tracing::info!(interval_secs = interval.as_secs(), "reconciler started");
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
        if let Err(err) = tick(&state).await {
            tracing::error!(error = %err, "reconciler tick failed");
        }
    }
    tracing::info!("reconciler stopped");
}

async fn tick(state: &AppState) -> anyhow::Result<()> {
    let expired = store::expire_overdue(&state.pool, EXPIRY_GRACE_SECS, 500).await?;
    if expired > 0 {
        tracing::warn!(expired, "expired overdue deposits without an indexer notice");
        state.outbox_wake.notify_one();
    }

    if state.engine.is_configured() {
        let stale = store::stale_paid(&state.pool, state.config.reconciler.paid_stale_secs, 100).await?;
        for deposit in stale {
            let Some(job_id) = deposit.engine_job_id else { continue };
            let job = match state.engine.get_job(job_id).await {
                Ok(Some(job)) => job,
                Ok(None) => {
                    tracing::error!(deposit_id = %deposit.id, %job_id, "engine does not know our job; needs an operator");
                    continue;
                }
                Err(err) => {
                    tracing::warn!(deposit_id = %deposit.id, error = %err, "engine status unavailable");
                    break;
                }
            };
            let tx_hash = job.tx_hash.map(|h| format!("{h:#x}"));
            let data = json!({ "engine_job_id": job_id, "polled_at": Utc::now(), "status": job.status, "outcome": job.outcome, "tx_hash": tx_hash });
            let outcome = match job.status.as_str() {
                "confirmed" => SettlementOutcome::from_receipt(
                    job.outcome.as_deref(),
                    tx_hash,
                    job.block_number.map(|n| n as i64),
                    job.receipt.as_ref(),
                    job.error.as_ref().map(|e| (e.code.as_str(), e.message.as_str(), None)),
                    &deposit,
                    data,
                ),
                "failed" => {
                    let (code, message) =
                        job.error.as_ref().map(|e| (e.code.clone(), e.message.clone())).unwrap_or_else(|| {
                            ("engine_failed".to_owned(), "the engine could not execute the transaction".to_owned())
                        });
                    SettlementOutcome::Failed { code: format!("engine_{code}"), message, tx_hash, data }
                }
                _ => {
                    // Still in flight; touch the row so it is not re-polled every tick.
                    sqlx::query("UPDATE deposits SET updated_at = now() WHERE id = $1")
                        .bind(deposit.id)
                        .execute(&state.pool)
                        .await?;
                    continue;
                }
            };
            let mut tx = state.pool.begin().await?;
            let changed = apply_settlement(&mut tx, &deposit, "reconciler", outcome).await?;
            tx.commit().await?;
            if changed {
                tracing::warn!(deposit_id = %deposit.id, "settlement outcome recovered by polling the engine");
                metrics::counter!("gum_deposit_transitions_total", "event" => "reconciled").increment(1);
                state.outbox_wake.notify_one();
            }
        }
    }

    store::prune(&state.pool, state.config.reconciler.idempotency_ttl_secs).await?;
    Ok(())
}
