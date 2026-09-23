//! `POST /v1/webhooks/indexer` and `POST /v1/webhooks/engine`.
//!
//! Both senders deliver at-least-once with `X-Gum-Signature`. A delivery is verified, deduplicated
//! on its event id inside the same transaction as the state change it causes, and acknowledged
//! with `200` as soon as that transaction commits. Anything slow (submitting to the engine,
//! notifying the app) is queued to the outbox, never done on the sender's clock.
//!
//! `200` for an event we cannot map to a deposit (a watch we did not create, a job that is not
//! ours): retrying would not help. `503` on database trouble: retrying will. Also `503` for an
//! engine job we do not know *yet*: the engine can report a job before our outbox has recorded
//! its id, and acknowledging that event would lose it.
//!
//! A handler uses exactly one connection: its transaction. Every query runs on it, never on the
//! pool, or a burst of pool-size deliveries deadlocks waiting for second connections.

use alloy_primitives::{Address, B256, U256};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::sign;
use crate::chain::payment::{self, ExecutionOutcome, ReceiptLog};
use crate::deposit::{Deposit, store};
use crate::error::ApiError;
use crate::state::AppState;

/// Engine failure codes worth an automatic resubmission: the transaction never executed.
const RETRYABLE_ENGINE_ERRORS: &[&str] = &["expired", "stuck_cancelled", "internal"];
const MAX_AUTO_RETRIES: i64 = 3;
/// An engine event for a job we have no record of is retried (503) while the job is younger than
/// this: our outbox records the job id only after the engine accepts it, and on a fast chain the
/// engine can include the transaction and report it first. Older unknown jobs are not ours.
const UNKNOWN_JOB_GRACE_SECS: u64 = 600;

fn verify(
    state: &AppState,
    source: &'static str,
    secret: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), ApiError> {
    if secret.is_empty() {
        metrics::counter!("gum_inbound_webhooks_total", "source" => source, "outcome" => "unconfigured").increment(1);
        return Err(ApiError::unavailable("webhook_unconfigured", "inbound webhook secret is not configured"));
    }
    let header = headers.get("x-gum-signature").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !sign::verify(secret, header, body, Utc::now().timestamp(), state.config.webhooks.inbound_tolerance_secs) {
        metrics::counter!("gum_inbound_webhooks_total", "source" => source, "outcome" => "bad_signature").increment(1);
        return Err(ApiError::unauthorized("invalid webhook signature"));
    }
    Ok(())
}

fn parse<T: for<'de> Deserialize<'de>>(source: &'static str, body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| {
        metrics::counter!("gum_inbound_webhooks_total", "source" => source, "outcome" => "malformed").increment(1);
        ApiError::invalid(format!("malformed {source} event: {e}"))
    })
}

// ---------------------------------------------------------------------------------------------
// gum-indexer
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct IndexerEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub sequence: Option<i64>,
    pub watch: IndexerWatch,
    #[serde(default)]
    pub transfer: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct IndexerWatch {
    pub id: Uuid,
    pub chain_id: u64,
    pub payment_address: Address,
    #[serde(default)]
    pub confirmed_amount: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

impl IndexerWatch {
    fn confirmed(&self) -> U256 {
        self.confirmed_amount.as_deref().and_then(|s| U256::from_str_radix(s, 10).ok()).unwrap_or_default()
    }
}

pub async fn indexer(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Result<StatusCode, ApiError> {
    verify(&state, "indexer", &state.config.indexer.webhook_secret, &headers, &body)?;
    let event: IndexerEvent = parse("indexer", &body)?;
    let span = tracing::info_span!("indexer_event", event_id = %event.id, event_type = %event.event_type, watch_id = %event.watch.id);
    let _guard = span.enter();

    let mut tx = state.pool.begin().await?;
    if !store::claim_inbound(&mut tx, "indexer", &event.id).await? {
        metrics::counter!("gum_inbound_webhooks_total", "source" => "indexer", "outcome" => "duplicate").increment(1);
        return Ok(StatusCode::OK);
    }
    let deposit = match store::get_by_watch(&mut tx, event.watch.id).await? {
        Some(d) => Some(d),
        // The watch id is written by an outbox job; the first event can beat it.
        None => store::get_by_payment_address(&mut tx, event.watch.chain_id, event.watch.payment_address).await?,
    };
    let Some(deposit) = deposit else {
        tracing::warn!(payment_address = %event.watch.payment_address, chain_id = event.watch.chain_id, "indexer event for an unknown deposit");
        metrics::counter!("gum_inbound_webhooks_total", "source" => "indexer", "outcome" => "unknown_deposit")
            .increment(1);
        tx.commit().await?;
        return Ok(StatusCode::OK);
    };
    if deposit.watch_id.is_none() {
        sqlx::query("UPDATE deposits SET watch_id = $2, watch_registered_at = COALESCE(watch_registered_at, now()) WHERE id = $1 AND watch_id IS NULL")
            .bind(deposit.id)
            .bind(event.watch.id)
            .execute(&mut *tx)
            .await?;
    }

    let data = json!({
        "indexer_event_id": event.id,
        "transfer": event.transfer,
        "confirmed_amount": event.watch.confirmed().to_string(),
    });
    let applied = match event.event_type.as_str() {
        "payment.pending" => store::apply_payment_pending(&mut tx, deposit.id, data).await?,
        "payment.confirmed" => {
            store::apply_payment_confirmed(&mut tx, deposit.id, event.watch.confirmed(), data).await?
        }
        "payment.orphaned" => store::apply_payment_orphaned(&mut tx, deposit.id, data).await?,
        "threshold.reached" => {
            store::apply_threshold_reached(&mut tx, deposit.id, event.watch.confirmed(), data).await?
        }
        "watch.expired" => store::apply_expired(&mut tx, deposit.id, data).await?,
        other => {
            tracing::warn!(event_type = other, "unknown indexer event type");
            None
        }
    };
    tx.commit().await?;
    state.outbox_wake.notify_one();

    let outcome = if applied.is_some() { "applied" } else { "ignored" };
    metrics::counter!("gum_inbound_webhooks_total", "source" => "indexer", "outcome" => outcome, "type" => event.event_type.clone())
        .increment(1);
    tracing::info!(deposit_id = %deposit.id, outcome, "indexer event processed");
    Ok(StatusCode::OK)
}

// ---------------------------------------------------------------------------------------------
// gum-engine
// ---------------------------------------------------------------------------------------------

/// Whether an engine job id (UUIDv7, stamped with its creation time) is young enough that we may
/// simply not have recorded it yet.
fn job_is_recent(job_id: Uuid) -> bool {
    let Some(created) = job_id.get_timestamp() else { return false };
    let now = Utc::now().timestamp().max(0) as u64;
    now.saturating_sub(created.to_unix().0) < UNKNOWN_JOB_GRACE_SECS
}

#[derive(Debug, Deserialize)]
pub struct EngineEvent {
    pub event_id: String,
    pub event: String,
    pub job_id: Uuid,
    #[serde(default)]
    pub sequence: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub tx_hash: Option<B256>,
    #[serde(default)]
    pub block_number: Option<u64>,
    #[serde(default)]
    pub receipt: Option<Value>,
    #[serde(default)]
    pub reincluded: bool,
    #[serde(default)]
    pub error: Option<EngineErrorDetail>,
}

#[derive(Debug, Deserialize)]
pub struct EngineErrorDetail {
    pub code: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub revert_data: Option<String>,
}

pub async fn engine(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Result<StatusCode, ApiError> {
    verify(&state, "engine", &state.config.engine.webhook_secret, &headers, &body)?;
    let event: EngineEvent = parse("engine", &body)?;
    let span = tracing::info_span!("engine_event", event_id = %event.event_id, event_type = %event.event, job_id = %event.job_id);
    let _guard = span.enter();

    let mut tx = state.pool.begin().await?;
    if !store::claim_inbound(&mut tx, "engine", &event.event_id).await? {
        metrics::counter!("gum_inbound_webhooks_total", "source" => "engine", "outcome" => "duplicate").increment(1);
        return Ok(StatusCode::OK);
    }
    let Some(deposit) = store::get_by_engine_job(&mut tx, event.job_id).await? else {
        if job_is_recent(event.job_id) {
            // Returning drops the transaction, so the claim is rolled back and the engine's retry
            // is processed once the outbox has recorded the job id.
            tracing::warn!("engine event for a job not recorded yet; asking the engine to retry");
            metrics::counter!("gum_inbound_webhooks_total", "source" => "engine", "outcome" => "job_not_recorded_yet")
                .increment(1);
            return Err(ApiError::unavailable("job_not_recorded_yet", "job not recorded yet; retry"));
        }
        tracing::warn!("engine event for an unknown job");
        metrics::counter!("gum_inbound_webhooks_total", "source" => "engine", "outcome" => "unknown_deposit")
            .increment(1);
        tx.commit().await?;
        return Ok(StatusCode::OK);
    };

    let outcome = SettlementOutcome::from_event(&event, &deposit);
    let applied = apply_settlement(&mut tx, &deposit, &event.event, outcome).await?;
    tx.commit().await?;
    state.outbox_wake.notify_one();

    let label = if applied { "applied" } else { "ignored" };
    metrics::counter!("gum_inbound_webhooks_total", "source" => "engine", "outcome" => label, "type" => event.event.clone()).increment(1);
    tracing::info!(deposit_id = %deposit.id, outcome = label, "engine event processed");
    Ok(StatusCode::OK)
}

/// What an engine event (or a polled job) means for the deposit.
#[derive(Debug)]
pub enum SettlementOutcome {
    /// Mined; waiting for the confirmation re-check.
    Included {
        tx_hash: String,
        block_number: Option<i64>,
        data: Value,
    },
    Settled {
        tx_hash: String,
        block_number: Option<i64>,
        data: Value,
    },
    Failed {
        code: String,
        message: String,
        tx_hash: Option<String>,
        data: Value,
    },
    /// Nothing terminal yet.
    Pending,
}

impl SettlementOutcome {
    pub fn from_event(event: &EngineEvent, deposit: &Deposit) -> Self {
        let tx_hash = event.tx_hash.map(|h| format!("{h:#x}"));
        let block_number = event.block_number.map(|n| n as i64);
        let data = json!({
            "engine_event_id": event.event_id,
            "engine_job_id": event.job_id,
            "tx_hash": tx_hash,
            "block_number": event.block_number,
            "outcome": event.outcome,
            "reincluded": event.reincluded,
        });
        match event.event.as_str() {
            "transaction.included" => match tx_hash {
                Some(tx_hash) => Self::Included { tx_hash, block_number, data },
                None => Self::Pending,
            },
            "transaction.confirmed" => Self::from_receipt(
                event.outcome.as_deref(),
                tx_hash,
                block_number,
                event.receipt.as_ref(),
                event.error.as_ref().map(|e| (e.code.as_str(), e.message.as_str(), e.revert_data.as_deref())),
                deposit,
                data,
            ),
            "transaction.failed" => {
                let (code, message) =
                    event.error.as_ref().map(|e| (e.code.clone(), e.message.clone())).unwrap_or_else(|| {
                        ("engine_failed".to_owned(), "the engine could not execute the transaction".to_owned())
                    });
                Self::Failed { code: format!("engine_{code}"), message, tx_hash, data }
            }
            _ => Self::Pending,
        }
    }

    /// Reads the `Payment` constructor's events out of the receipt: the engine's `outcome` says
    /// whether the transaction reverted, the logs say where the money went.
    pub fn from_receipt(
        outcome: Option<&str>,
        tx_hash: Option<String>,
        block_number: Option<i64>,
        receipt: Option<&Value>,
        error: Option<(&str, &str, Option<&str>)>,
        deposit: &Deposit,
        data: Value,
    ) -> Self {
        let Some(tx_hash) = tx_hash else { return Self::Pending };
        // The node's receipt, exactly as gum-engine relayed it, travels with the terminal event so
        // the app gets the logs and gas figures too.
        let mut data = data;
        data["receipt"] = receipt.cloned().unwrap_or(Value::Null);
        match outcome {
            Some("success") => {
                let logs: Vec<ReceiptLog> = receipt
                    .and_then(|r| r.get("logs"))
                    .and_then(|l| serde_json::from_value(l.clone()).ok())
                    .unwrap_or_default();
                match payment::execution_outcome(&logs, deposit.payment_address()) {
                    ExecutionOutcome::Settled => Self::Settled { tx_hash, block_number, data },
                    ExecutionOutcome::RecoveredOnly => Self::Failed {
                        code: "expired_on_chain".into(),
                        message: "the payment expired before it was executed; the balance was forwarded to recovery"
                            .into(),
                        tx_hash: Some(tx_hash),
                        data,
                    },
                    ExecutionOutcome::WrongChain => Self::Failed {
                        code: "wrong_chain".into(),
                        message: "Payment was deployed on a chain other than the deposit's".into(),
                        tx_hash: Some(tx_hash),
                        data,
                    },
                    ExecutionOutcome::NoEvents => Self::Failed {
                        code: "unexpected_outcome".into(),
                        message: "execute succeeded but the payment address emitted no Settled event".into(),
                        tx_hash: Some(tx_hash),
                        data,
                    },
                }
            }
            Some("reverted") => {
                let (code, message, revert_data) =
                    error.unwrap_or(("execution_reverted", "PaymentFactory.execute reverted", None));
                data["revert_data"] = json!(revert_data);
                Self::Failed {
                    code: format!("engine_{code}"),
                    message: if message.is_empty() {
                        "PaymentFactory.execute reverted".into()
                    } else {
                        message.to_owned()
                    },
                    tx_hash: Some(tx_hash),
                    data,
                }
            }
            _ => Self::Pending,
        }
    }
}

/// Applies a settlement outcome inside `conn`; returns whether the deposit changed.
pub async fn apply_settlement(
    conn: &mut sqlx::PgConnection,
    deposit: &Deposit,
    source_event: &str,
    outcome: SettlementOutcome,
) -> Result<bool, sqlx::Error> {
    let changed = match outcome {
        SettlementOutcome::Included { tx_hash, block_number, data } => {
            store::apply_settlement_included(conn, deposit.id, &tx_hash, block_number, data).await?.is_some()
        }
        SettlementOutcome::Settled { tx_hash, block_number, data } => {
            store::apply_settled(conn, deposit.id, &tx_hash, block_number, data).await?.is_some()
        }
        SettlementOutcome::Failed { code, message, tx_hash, data } => {
            // A job the engine gave up on before it ever executed is resubmitted a few times
            // before the app is told about a failure.
            let engine_code = code.strip_prefix("engine_").unwrap_or(&code);
            if RETRYABLE_ENGINE_ERRORS.contains(&engine_code) && tx_hash.is_none() {
                let (retries,): (i64,) = sqlx::query_as(
                    "SELECT count(*) FROM deposit_events WHERE deposit_id = $1 AND type = 'deposit.settlement_retried'",
                )
                .bind(deposit.id)
                .fetch_one(&mut *conn)
                .await?;
                if retries < MAX_AUTO_RETRIES {
                    tracing::warn!(deposit_id = %deposit.id, code, retries, "settlement failed before execution; resubmitting");
                    return resubmit(conn, deposit, json!({ "automatic": true, "reason": code, "detail": message }))
                        .await;
                }
            }
            store::apply_failed(conn, deposit.id, &code, &message, tx_hash.as_deref(), data).await?.is_some()
        }
        SettlementOutcome::Pending => {
            tracing::debug!(deposit_id = %deposit.id, source_event, "non-terminal engine event");
            false
        }
    };
    Ok(changed)
}

/// Detaches a still-`paid` deposit from its engine job and queues a fresh `execute`.
pub async fn resubmit(conn: &mut sqlx::PgConnection, deposit: &Deposit, data: Value) -> Result<bool, sqlx::Error> {
    let detached: Option<Deposit> = sqlx::query_as(store::update_deposit(
        "engine_job_id = NULL, engine_submitted_at = NULL",
        "id = $1 AND status = 'paid'",
    ))
    .bind(deposit.id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(d) = detached else { return Ok(false) };
    let event = store::record_event(conn, &d, crate::deposit::events::SETTLEMENT_RETRIED, data).await?;
    store::enqueue_execute(conn, d.id, event.sequence).await?;
    Ok(true)
}
