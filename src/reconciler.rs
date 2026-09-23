//! Periodic safety net for everything a lost webhook could leave behind. Webhooks are the fast
//! path; this loop guarantees that the outcome is eventually the same without them:
//!
//! * open deposits (`pending` / `partial_paid`) that have gone quiet are compared with the
//!   indexer's view of their watch — a lost `payment.confirmed` / `threshold.reached` /
//!   `watch.expired` is applied from there, and a watch the indexer no longer knows is re-registered;
//! * deposits stuck in `paid` are checked against the engine's job status — a lost
//!   `transaction.confirmed` / `.failed` is applied, and a job the engine no longer knows is resubmitted;
//! * open deposits well past their expiry are expired locally;
//! * idempotency keys and inbound event ids are pruned.
//!
//! Every transition it applies is the same guarded transition a webhook would apply, so running
//! this on several replicas at once is safe.

use std::time::Duration;

use alloy_primitives::U256;
use chrono::Utc;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::clients::UpstreamError;
use crate::deposit::{Deposit, store};
use crate::state::AppState;
use crate::webhooks::inbound::{SettlementOutcome, apply_settlement, resubmit};

/// Local expiry is a last resort: the indexer's `watch.expired` (webhook or polled below) is the
/// authoritative signal, and a deposit lingering in `pending` is harmless, whereas expiring one
/// that was actually paid is not. So only deposits a full day past expiry are expired blind.
const EXPIRY_GRACE_SECS: i64 = 86_400;
const BATCH: i64 = 100;
/// A due outbox job this old means the outbox is not draining. It normally runs within seconds.
pub const OUTBOX_STALL_SECS: f64 = 120.0;
/// A tick that takes longer than this is abandoned; the next one starts over.
const TICK_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn run(state: AppState, shutdown: CancellationToken) {
    let interval = Duration::from_secs(state.config.reconciler.interval_secs.max(5));
    tracing::info!(interval_secs = interval.as_secs(), "reconciler started");
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => break,
        }
        match tokio::time::timeout(TICK_TIMEOUT, tick(&state)).await {
            Ok(Ok(())) => metrics::counter!("gum_reconciler_ticks_total", "outcome" => "ok").increment(1),
            Ok(Err(err)) => {
                tracing::error!(error = %err, "reconciler tick failed");
                metrics::counter!("gum_reconciler_ticks_total", "outcome" => "error").increment(1);
            }
            Err(_) => {
                tracing::error!(timeout_secs = TICK_TIMEOUT.as_secs(), "reconciler tick timed out");
                metrics::counter!("gum_reconciler_ticks_total", "outcome" => "timeout").increment(1);
            }
        }
    }
    tracing::info!("reconciler stopped");
}

/// One pass. Public so tests can drive it without waiting for the interval.
pub async fn tick(state: &AppState) -> anyhow::Result<()> {
    check_outbox(state).await?;
    let expired = store::expire_overdue(&state.pool, EXPIRY_GRACE_SECS, 500).await?;
    if expired > 0 {
        tracing::warn!(expired, "expired overdue deposits without an indexer notice");
        state.outbox_wake.notify_one();
    }
    if state.indexer.is_configured() {
        reconcile_open(state).await?;
    }
    if state.engine.is_configured() {
        reconcile_paid(state).await?;
    }
    store::prune(&state.pool, state.config.reconciler.idempotency_ttl_secs).await?;
    Ok(())
}

/// Watches the outbox from outside it. The outbox cannot report its own stall (on 2026-09-23 it
/// hung for over ten minutes and logged nothing), so every tick measures how long the oldest due
/// job has been waiting. Alert on `gum_outbox_oldest_due_age_seconds`.
async fn check_outbox(state: &AppState) -> anyhow::Result<()> {
    let (due, oldest_secs) = store::outbox_due(&state.pool).await?;
    metrics::gauge!("gum_outbox_due").set(due as f64);
    metrics::gauge!("gum_outbox_oldest_due_age_seconds").set(oldest_secs);
    if oldest_secs > OUTBOX_STALL_SECS {
        tracing::error!(due, oldest_due_age_secs = oldest_secs as u64, "outbox is not draining");
    }
    Ok(())
}

fn recovered(deposit: &Deposit, what: &str) {
    tracing::warn!(deposit_id = %deposit.id, what, "state recovered by polling instead of a webhook");
    metrics::counter!("gum_deposit_transitions_total", "event" => "reconciled").increment(1);
}

async fn reconcile_open(state: &AppState) -> anyhow::Result<()> {
    let stale = store::stale_open(&state.pool, state.config.reconciler.open_stale_secs, BATCH).await?;
    for deposit in stale {
        let Some(watch_id) = deposit.watch_id else { continue };
        let watch = match state.indexer.get_watch(watch_id).await {
            Ok(Some(watch)) => watch,
            Ok(None) => {
                tracing::error!(deposit_id = %deposit.id, %watch_id, "indexer does not know our watch; re-registering");
                if store::reregister_watch(&state.pool, deposit.id).await? {
                    state.outbox_wake.notify_one();
                }
                continue;
            }
            Err(err @ UpstreamError::Rejected { .. }) => {
                tracing::error!(deposit_id = %deposit.id, error = %err, "indexer rejected the status request");
                continue;
            }
            Err(err) => {
                tracing::warn!(deposit_id = %deposit.id, error = %err, "indexer status unavailable");
                break;
            }
        };
        let confirmed = watch.confirmed();
        let ours = U256::from_str_radix(&deposit.confirmed_amount, 10).unwrap_or_default();
        let data = json!({
            "watch_id": watch_id, "polled_at": Utc::now(), "watch_status": watch.status,
            "confirmed_amount": confirmed.to_string(), "source": "reconciler",
        });
        let mut tx = state.pool.begin().await?;
        let changed = match watch.status.as_str() {
            "completed" => store::apply_threshold_reached(&mut tx, deposit.id, confirmed, data).await?.is_some(),
            "expired" | "cancelled" => store::apply_expired(&mut tx, deposit.id, data).await?.is_some(),
            _ if confirmed > ours => {
                store::apply_payment_confirmed(&mut tx, deposit.id, confirmed, data).await?.is_some()
            }
            _ => false,
        };
        tx.commit().await?;
        if changed {
            recovered(&deposit, &watch.status);
            state.outbox_wake.notify_one();
        } else {
            store::touch(&state.pool, deposit.id).await?;
        }
    }
    Ok(())
}

async fn reconcile_paid(state: &AppState) -> anyhow::Result<()> {
    // Alert on this: settlement takes seconds, so a `paid` deposit minutes old means the engine's
    // webhooks are not landing (as in the 2026-09-23 incident, when nothing else looked unhealthy).
    let (paid, oldest_secs) = store::paid_backlog(&state.pool).await?;
    metrics::gauge!("gum_deposits_paid").set(paid as f64);
    metrics::gauge!("gum_deposits_paid_oldest_age_seconds").set(oldest_secs);

    let cfg = &state.config.reconciler;
    let stale = store::stale_paid(&state.pool, cfg.paid_stale_secs, cfg.paid_repoll_secs, BATCH).await?;
    for deposit in stale {
        let Some(job_id) = deposit.engine_job_id else { continue };
        let job = match state.engine.get_job(job_id).await {
            Ok(Some(job)) => job,
            Ok(None) => {
                tracing::error!(deposit_id = %deposit.id, %job_id, "engine does not know our job; resubmitting");
                let mut tx = state.pool.begin().await?;
                let changed =
                    resubmit(&mut tx, &deposit, json!({ "automatic": true, "reason": "engine_job_unknown" })).await?;
                tx.commit().await?;
                if changed {
                    state.outbox_wake.notify_one();
                }
                continue;
            }
            Err(err @ UpstreamError::Rejected { .. }) => {
                tracing::error!(deposit_id = %deposit.id, error = %err, "engine rejected the status request");
                continue;
            }
            Err(err) => {
                tracing::warn!(deposit_id = %deposit.id, error = %err, "engine status unavailable");
                break;
            }
        };
        let tx_hash = job.tx_hash.map(|h| format!("{h:#x}"));
        let data = json!({
            "engine_job_id": job_id, "polled_at": Utc::now(), "status": job.status, "outcome": job.outcome,
            "tx_hash": tx_hash, "source": "reconciler",
        });
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
                // Still in flight; do not re-poll every tick.
                store::touch(&state.pool, deposit.id).await?;
                continue;
            }
        };
        let mut tx = state.pool.begin().await?;
        let changed = apply_settlement(&mut tx, &deposit, "reconciler", outcome).await?;
        tx.commit().await?;
        if changed {
            recovered(&deposit, &job.status);
            state.outbox_wake.notify_one();
        }
    }
    Ok(())
}
