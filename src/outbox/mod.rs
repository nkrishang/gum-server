//! Outbox workers: the only place this service talks to gum-indexer, gum-engine or an app's
//! webhook. Jobs are claimed with `FOR UPDATE SKIP LOCKED`, so any number of instances can run
//! workers; per (deposit, kind) they execute in creation order, so an app never sees
//! `deposit.settled` before `deposit.confirmed`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::FromRow;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::chain::payment::PaymentTerms;
use crate::clients::UpstreamError;
use crate::clients::engine::{SubmitTransaction, Webhook};
use crate::clients::indexer::CreateWatch;
use crate::deposit::{DepositStatus, events, store};
use crate::state::AppState;
use crate::webhooks::sign;

#[derive(Debug, Clone, FromRow)]
pub struct OutboxJob {
    pub id: Uuid,
    pub kind: String,
    pub deposit_id: Uuid,
    pub payload: Value,
    pub attempts: i32,
    pub created_at: DateTime<Utc>,
}

/// How a job attempt ended.
enum Attempt {
    Done,
    /// Try again later; the message is stored on the row.
    Retry(String),
    /// Never try again.
    Dead(String),
}

pub async fn run(state: AppState, shutdown: CancellationToken) {
    let cfg = state.config.outbox.clone();
    let poll = Duration::from_millis(cfg.poll_interval_ms);
    let permits = Arc::new(Semaphore::new(cfg.workers));
    tracing::info!(workers = cfg.workers, "outbox workers started");
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let jobs = match claim(&state, cfg.batch_size, cfg.lock_ttl_secs).await {
            Ok(jobs) => jobs,
            Err(err) => {
                tracing::error!(error = %err, "outbox claim failed");
                metrics::counter!("gum_db_errors_total", "kind" => "outbox_claim").increment(1);
                tokio::select! {
                    _ = tokio::time::sleep(poll) => continue,
                    _ = shutdown.cancelled() => break,
                }
            }
        };
        let full_batch = jobs.len() as i64 >= cfg.batch_size;
        let mut handles = Vec::with_capacity(jobs.len());
        for job in jobs {
            let permit = permits.clone().acquire_owned().await.expect("semaphore never closes");
            let state = state.clone();
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                execute(state, job).await;
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        if let Err(err) = gauge(&state).await {
            tracing::debug!(error = %err, "outbox gauge failed");
        }
        if full_batch {
            continue;
        }
        tokio::select! {
            _ = state.outbox_wake.notified() => {}
            _ = tokio::time::sleep(poll) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    // Let in-flight jobs finish so their rows are released cleanly.
    let _ = permits.acquire_many(cfg.workers as u32).await;
    tracing::info!("outbox workers stopped");
}

async fn claim(state: &AppState, batch: i64, lock_ttl_secs: i64) -> Result<Vec<OutboxJob>, sqlx::Error> {
    sqlx::query_as(
        "WITH due AS (
             SELECT o.id FROM outbox o
             WHERE o.dead_at IS NULL
               AND o.next_attempt_at <= now()
               AND (o.locked_until IS NULL OR o.locked_until < now())
               AND NOT EXISTS (
                   SELECT 1 FROM outbox p
                   WHERE p.deposit_id = o.deposit_id AND p.kind = o.kind AND p.id < o.id AND p.dead_at IS NULL
               )
             ORDER BY o.next_attempt_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE outbox SET locked_until = now() + make_interval(secs => $2), attempts = attempts + 1
         FROM due WHERE outbox.id = due.id
         RETURNING outbox.id, outbox.kind, outbox.deposit_id, outbox.payload, outbox.attempts, outbox.created_at",
    )
    .bind(batch)
    .bind(lock_ttl_secs as f64)
    .fetch_all(&state.pool)
    .await
}

async fn gauge(state: &AppState) -> Result<(), sqlx::Error> {
    let (pending, dead): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE dead_at IS NULL), count(*) FILTER (WHERE dead_at IS NOT NULL) FROM outbox",
    )
    .fetch_one(&state.pool)
    .await?;
    metrics::gauge!("gum_outbox_pending").set(pending as f64);
    metrics::gauge!("gum_outbox_dead").set(dead as f64);
    Ok(())
}

async fn execute(state: AppState, job: OutboxJob) {
    let span = tracing::info_span!("outbox_job", job_id = %job.id, kind = %job.kind, deposit_id = %job.deposit_id, attempt = job.attempts);
    let _guard = span.enter();
    let started = Instant::now();
    metrics::histogram!("gum_outbox_lag_seconds", "kind" => job.kind.clone())
        .record((Utc::now() - job.created_at).num_milliseconds().max(0) as f64 / 1000.0);

    let attempt = match job.kind.as_str() {
        "register_watch" => register_watch(&state, &job).await,
        "submit_execute" => submit_execute(&state, &job).await,
        "notify_app" => notify_app(&state, &job).await,
        other => Attempt::Dead(format!("unknown job kind {other}")),
    };
    let kind = job.kind.clone();
    let outcome = match &attempt {
        Attempt::Done => "done",
        Attempt::Retry(_) => "retry",
        Attempt::Dead(_) => "dead",
    };
    metrics::histogram!("gum_outbox_job_duration_seconds", "kind" => kind.clone(), "outcome" => outcome)
        .record(started.elapsed().as_secs_f64());
    metrics::counter!("gum_outbox_jobs_total", "kind" => kind, "outcome" => outcome).increment(1);

    let result = match attempt {
        Attempt::Done => {
            sqlx::query("DELETE FROM outbox WHERE id = $1").bind(job.id).execute(&state.pool).await.map(|_| ())
        }
        Attempt::Retry(error) => {
            let age = (Utc::now() - job.created_at).num_seconds();
            if age > state.config.webhooks.max_age_secs {
                tracing::error!(error, age_secs = age, "outbox job exhausted its retry window");
                mark_dead(&state, job.id, &error).await
            } else {
                let delay = backoff(&state, job.attempts);
                tracing::warn!(error, retry_in_ms = delay.as_millis() as u64, "outbox job failed; will retry");
                sqlx::query(
                    "UPDATE outbox SET next_attempt_at = now() + make_interval(secs => $2), locked_until = NULL, last_error = $3 WHERE id = $1",
                )
                .bind(job.id)
                .bind(delay.as_secs_f64())
                .bind(error)
                .execute(&state.pool)
                .await
                .map(|_| ())
            }
        }
        Attempt::Dead(error) => {
            tracing::error!(error, "outbox job dead");
            mark_dead(&state, job.id, &error).await
        }
    };
    if let Err(err) = result {
        tracing::error!(error = %err, "failed to finalise outbox job; the lock will expire and it will run again");
        metrics::counter!("gum_db_errors_total", "kind" => "outbox_finalise").increment(1);
    }
}

async fn mark_dead(state: &AppState, id: Uuid, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE outbox SET dead_at = now(), locked_until = NULL, last_error = $2 WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(&state.pool)
        .await
        .map(|_| ())
}

/// Exponential backoff with full jitter, capped.
fn backoff(state: &AppState, attempts: i32) -> Duration {
    let base = state.config.webhooks.retry_base_ms as f64;
    let cap = state.config.webhooks.retry_cap_ms as f64;
    let exp = base * 2f64.powi(attempts.saturating_sub(1).min(30));
    let upper = exp.min(cap);
    Duration::from_millis((base + rand::random::<f64>() * (upper - base).max(0.0)) as u64)
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

async fn register_watch(state: &AppState, job: &OutboxJob) -> Attempt {
    let deposit = match store::get(&state.pool, job.deposit_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return Attempt::Dead("deposit no longer exists".into()),
        Err(e) => return Attempt::Retry(format!("load deposit: {e}")),
    };
    if deposit.watch_id.is_some() || deposit.status.is_terminal() {
        return Attempt::Done;
    }
    if deposit.expires_at <= Utc::now() {
        // Registration was delayed past the deposit's own expiry (indexer outage); the indexer
        // would refuse it, and the payer can no longer settle either.
        let mut tx = match state.pool.begin().await {
            Ok(tx) => tx,
            Err(e) => return Attempt::Retry(format!("begin: {e}")),
        };
        let expired =
            store::apply_expired(&mut tx, deposit.id, json!({ "reason": "expired before the watch was registered" }))
                .await;
        return match expired.and(tx.commit().await) {
            Ok(()) => {
                state.outbox_wake.notify_one();
                Attempt::Done
            }
            Err(e) => Attempt::Retry(format!("record expiry: {e}")),
        };
    }
    if !state.indexer.is_configured() {
        return Attempt::Retry("indexer is not configured".into());
    }
    let endpoint = state.webhook_url("/v1/webhooks/indexer");
    let req = CreateWatch {
        payment_address: deposit.payment_address(),
        chain: deposit.chain_id.to_string(),
        token: deposit.token_address.parse().expect("stored addresses are valid"),
        balance_threshold: deposit.amount.clone(),
        webhook_endpoint: &endpoint,
        expires_at: deposit.expires_at,
    };
    match state.indexer.create_watch(&req).await {
        Ok(watch) => {
            if let Err(e) = store::mark_watch_registered(&state.pool, deposit.id, watch.id).await {
                return Attempt::Retry(format!("record watch: {e}"));
            }
            tracing::info!(watch_id = %watch.id, "watch registered");
            Attempt::Done
        }
        Err(err @ UpstreamError::Rejected { .. }) => {
            // The indexer will never accept this address; the deposit cannot be detected.
            let message = err.to_string();
            match fail_open(state, job.deposit_id, "watch_rejected", &message).await {
                Ok(()) => Attempt::Dead(message),
                Err(e) => Attempt::Retry(format!("record failure: {e}")),
            }
        }
        Err(err) => Attempt::Retry(err.to_string()),
    }
}

/// Fails a deposit that has not been paid yet (watch could not be registered).
async fn fail_open(state: &AppState, id: Uuid, code: &str, message: &str) -> Result<(), sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    let deposit: Option<crate::deposit::Deposit> = sqlx::query_as(store::update_deposit(
        "status = 'failed', failed_at = now(), failure_code = $2, failure_message = $3",
        "id = $1 AND status IN ('pending', 'partial_paid')",
    ))
    .bind(id)
    .bind(code)
    .bind(message)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(d) = &deposit {
        store::record_event(&mut tx, d, events::FAILED, json!({ "code": code, "message": message })).await?;
    }
    tx.commit().await?;
    state.outbox_wake.notify_one();
    Ok(())
}

async fn submit_execute(state: &AppState, job: &OutboxJob) -> Attempt {
    let deposit = match store::get(&state.pool, job.deposit_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return Attempt::Dead("deposit no longer exists".into()),
        Err(e) => return Attempt::Retry(format!("load deposit: {e}")),
    };
    if deposit.status != DepositStatus::Paid {
        return Attempt::Done;
    }
    if deposit.engine_job_id.is_some() {
        return Attempt::Done;
    }
    if !state.engine.is_configured() {
        return Attempt::Retry("engine is not configured".into());
    }
    let idempotency_key = job
        .payload
        .get("idempotency_key")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("deposit:{}:execute", deposit.id));
    let terms: PaymentTerms = deposit.terms();
    let webhook_url = state.webhook_url("/v1/webhooks/engine");
    let req = SubmitTransaction {
        chain_id: deposit.chain_id as u64,
        to: state.factory,
        data: terms.execute_calldata(),
        webhook: Webhook { url: &webhook_url },
    };
    match state.engine.submit(&req, &idempotency_key).await {
        Ok(submitted) => {
            if let Err(e) =
                store::mark_engine_submitted(&state.pool, deposit.id, submitted.job_id, submitted.replayed).await
            {
                return Attempt::Retry(format!("record job: {e}"));
            }
            tracing::info!(engine_job_id = %submitted.job_id, replayed = submitted.replayed, "settlement submitted");
            Attempt::Done
        }
        Err(err @ UpstreamError::Rejected { .. }) => {
            let message = err.to_string();
            let mut tx = match state.pool.begin().await {
                Ok(tx) => tx,
                Err(e) => return Attempt::Retry(format!("begin: {e}")),
            };
            let failed = store::apply_failed(
                &mut tx,
                deposit.id,
                "engine_rejected",
                &message,
                None,
                json!({ "error": message }),
            )
            .await;
            match failed {
                Ok(_) => match tx.commit().await {
                    Ok(()) => {
                        state.outbox_wake.notify_one();
                        Attempt::Dead(message)
                    }
                    Err(e) => Attempt::Retry(format!("commit: {e}")),
                },
                Err(e) => Attempt::Retry(format!("record failure: {e}")),
            }
        }
        Err(err) => Attempt::Retry(err.to_string()),
    }
}

async fn notify_app(state: &AppState, job: &OutboxJob) -> Attempt {
    let Some(url) = job.payload.get("url").and_then(Value::as_str) else {
        return Attempt::Dead("notify_app job without url".into());
    };
    let Some(body) = job.payload.get("body") else {
        return Attempt::Dead("notify_app job without body".into());
    };
    let event_id = job.payload.get("event_id").and_then(Value::as_str).unwrap_or_default();
    let event_type = job.payload.get("event_type").and_then(Value::as_str).unwrap_or_default();

    let secret: Option<(String,)> = match sqlx::query_as(
        "SELECT u.webhook_secret FROM users u JOIN deposits d ON d.user_id = u.id WHERE d.id = $1",
    )
    .bind(job.deposit_id)
    .fetch_optional(&state.pool)
    .await
    {
        Ok(row) => row,
        Err(e) => return Attempt::Retry(format!("load secret: {e}")),
    };
    let Some((secret,)) = secret else { return Attempt::Dead("deposit owner not found".into()) };

    let raw = serde_json::to_vec(body).expect("json value serialises");
    let now = Utc::now().timestamp();
    let started = Instant::now();
    let response = state
        .webhook_http
        .post(url)
        .header("content-type", "application/json")
        .header("x-gum-event-id", event_id)
        .header("x-gum-event-type", event_type)
        .header("x-gum-deposit-id", job.deposit_id.to_string())
        .header("x-gum-delivery-attempt", job.attempts.to_string())
        .header("x-gum-signature", sign::signature_header(&secret, now, &raw))
        .body(raw)
        .send()
        .await;
    let elapsed = started.elapsed().as_secs_f64();
    match response {
        Ok(r) if r.status().is_success() => {
            metrics::counter!("gum_app_webhook_deliveries_total", "outcome" => "delivered").increment(1);
            metrics::histogram!("gum_upstream_request_duration_seconds", "service" => "app", "op" => "notify", "outcome" => "ok").record(elapsed);
            tracing::info!(event_type, status = r.status().as_u16(), "app webhook delivered");
            Attempt::Done
        }
        Ok(r) => {
            metrics::counter!("gum_app_webhook_deliveries_total", "outcome" => "rejected").increment(1);
            metrics::histogram!("gum_upstream_request_duration_seconds", "service" => "app", "op" => "notify", "outcome" => "rejected").record(elapsed);
            Attempt::Retry(format!("app webhook responded {}", r.status()))
        }
        Err(e) => {
            metrics::counter!("gum_app_webhook_deliveries_total", "outcome" => "transport").increment(1);
            metrics::histogram!("gum_upstream_request_duration_seconds", "service" => "app", "op" => "notify", "outcome" => "transport").record(elapsed);
            Attempt::Retry(format!("app webhook unreachable: {e}"))
        }
    }
}
