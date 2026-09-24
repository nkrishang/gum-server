//! Every query the deposit lifecycle needs. State changes and the side effects they cause (watch
//! registration, settlement submission, app notifications) are committed together through the
//! `outbox` table, so nothing is lost if the process dies between the two.
//!
//! Transitions are guarded by `WHERE status IN (…)` and return `None` when the row was not in an
//! eligible state, which makes redelivered webhooks harmless.

use alloy_primitives::{Address, U256};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{AssertSqlSafe, PgConnection, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use super::request::NewDeposit;
use super::{CallView, DEPOSIT_COLUMNS, Deposit, DepositEvent, DepositStatus, events};

fn hex_lower(address: Address) -> String {
    format!("{address:#x}")
}

// The clauses are string literals from this module, never request data.
pub(crate) fn select_deposit(where_clause: &str) -> AssertSqlSafe<String> {
    AssertSqlSafe(format!("SELECT {DEPOSIT_COLUMNS} FROM deposits WHERE {where_clause}"))
}

pub(crate) fn update_deposit(set_clause: &str, where_clause: &str) -> AssertSqlSafe<String> {
    AssertSqlSafe(format!(
        "UPDATE deposits SET {set_clause}, updated_at = now() WHERE {where_clause} RETURNING {DEPOSIT_COLUMNS}"
    ))
}

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

pub enum CreateOutcome {
    Created(Deposit),
    /// Same idempotency key and same request: the original deposit.
    Replayed(Deposit),
    /// Same idempotency key, different request.
    Conflict,
}

pub struct Idempotency<'a> {
    pub key: &'a str,
    pub fingerprint: Vec<u8>,
}

pub async fn create_deposit(
    pool: &PgPool,
    user_id: &str,
    new: &NewDeposit,
    idempotency: Option<Idempotency<'_>>,
) -> Result<CreateOutcome, sqlx::Error> {
    // Fast path: a replay never takes the write lock.
    if let Some(idem) = &idempotency
        && let Some(outcome) = lookup_idempotent(pool, user_id, idem).await?
    {
        return Ok(outcome);
    }

    let mut tx = pool.begin().await?;
    let id = Uuid::now_v7();
    let deposit: Deposit = sqlx::query_as(AssertSqlSafe(format!(
        "INSERT INTO deposits (id, user_id, chain_id, token_symbol, token_address, token_decimals, amount, receiver, \
         calls, recovery, salt, expires_at, payment_address, reference, webhook_url)
         VALUES ($1, $2, $3, $4, $5, $6, CAST($7 AS numeric), $8, $9, $10, $11, $12, $13, $14, $15)
         RETURNING {DEPOSIT_COLUMNS}"
    )))
    .bind(id)
    .bind(user_id)
    .bind(new.chain_id as i64)
    .bind(&new.token_symbol)
    .bind(hex_lower(new.token_address))
    .bind(new.token_decimals as i16)
    .bind(new.amount.to_string())
    .bind(hex_lower(new.receiver))
    .bind(Json(new.calls.iter().map(CallView::from).collect::<Vec<_>>()))
    .bind(hex_lower(new.recovery))
    .bind(format!("{:#x}", new.salt))
    .bind(new.expires_at)
    .bind(hex_lower(new.payment_address))
    .bind(new.reference.map(|r| format!("{r:#x}")))
    .bind(&new.webhook_url)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(idem) = &idempotency {
        // Two concurrent requests with the same key: the loser rolls back its deposit and
        // returns the winner's.
        let claimed = sqlx::query(
            "INSERT INTO idempotency_keys (user_id, key, request_hash, deposit_id)
             VALUES ($1, $2, $3, $4) ON CONFLICT (user_id, key) DO NOTHING",
        )
        .bind(user_id)
        .bind(idem.key)
        .bind(&idem.fingerprint)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if claimed == 0 {
            tx.rollback().await?;
            return match lookup_idempotent(pool, user_id, idem).await? {
                Some(outcome) => Ok(outcome),
                None => Err(sqlx::Error::Protocol("idempotency row vanished".into())),
            };
        }
    }
    record_event(&mut tx, &deposit, events::CREATED, json!({})).await?;
    enqueue(&mut tx, "register_watch", deposit.id, json!({})).await?;
    tx.commit().await?;
    Ok(CreateOutcome::Created(deposit))
}

async fn lookup_idempotent(
    pool: &PgPool,
    user_id: &str,
    idem: &Idempotency<'_>,
) -> Result<Option<CreateOutcome>, sqlx::Error> {
    let row: Option<(Vec<u8>, Uuid)> =
        sqlx::query_as("SELECT request_hash, deposit_id FROM idempotency_keys WHERE user_id = $1 AND key = $2")
            .bind(user_id)
            .bind(idem.key)
            .fetch_optional(pool)
            .await?;
    let Some((hash, deposit_id)) = row else { return Ok(None) };
    if hash != idem.fingerprint {
        return Ok(Some(CreateOutcome::Conflict));
    }
    match get(pool, deposit_id).await? {
        Some(d) => Ok(Some(CreateOutcome::Replayed(d))),
        // The deposit insert is in the same transaction as the key; a missing row means the winner
        // is still committing. Treat as not yet visible.
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------------------------

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Option<Deposit>, sqlx::Error> {
    sqlx::query_as(select_deposit("id = $1")).bind(id).fetch_optional(pool).await
}

// The lookups below take a connection, not the pool: the inbound webhook handlers call them
// inside their open transaction. Asking the pool for a second connection while holding one
// deadlocks once pool-size handlers run at once (2026-09-23 Monad incident).

pub async fn get_by_watch(conn: &mut PgConnection, watch_id: Uuid) -> Result<Option<Deposit>, sqlx::Error> {
    sqlx::query_as(select_deposit("watch_id = $1")).bind(watch_id).fetch_optional(conn).await
}

pub async fn get_by_payment_address(
    conn: &mut PgConnection,
    chain_id: u64,
    address: Address,
) -> Result<Option<Deposit>, sqlx::Error> {
    sqlx::query_as(select_deposit("chain_id = $1 AND payment_address = $2"))
        .bind(chain_id as i64)
        .bind(hex_lower(address))
        .fetch_optional(conn)
        .await
}

pub async fn get_by_engine_job(conn: &mut PgConnection, job_id: Uuid) -> Result<Option<Deposit>, sqlx::Error> {
    sqlx::query_as(select_deposit("engine_job_id = $1")).bind(job_id).fetch_optional(conn).await
}

pub async fn events(pool: &PgPool, deposit_id: Uuid) -> Result<Vec<DepositEvent>, sqlx::Error> {
    sqlx::query_as("SELECT id, deposit_id, sequence, type, data, created_at FROM deposit_events WHERE deposit_id = $1 ORDER BY sequence")
        .bind(deposit_id)
        .fetch_all(pool)
        .await
}

#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    pub status: Option<DepositStatus>,
    pub chain_id: Option<i64>,
    pub token: Option<String>,
    pub reference: Option<String>,
    pub payment_address: Option<String>,
    pub receiver: Option<String>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub cursor: Option<Cursor>,
    pub limit: i64,
}

/// Keyset cursor over `(created_at DESC, id DESC)`.
#[derive(Debug, Clone, Copy)]
pub struct Cursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!("{}_{}", self.created_at.timestamp_micros(), self.id)
    }

    pub fn decode(raw: &str) -> Option<Self> {
        let (micros, id) = raw.split_once('_')?;
        let created_at = DateTime::from_timestamp_micros(micros.parse().ok()?)?;
        Some(Self { created_at, id: id.parse().ok()? })
    }
}

pub struct Page {
    pub items: Vec<Deposit>,
    pub next_cursor: Option<String>,
}

pub async fn list(pool: &PgPool, user_id: &str, filter: &ListFilter) -> Result<Page, sqlx::Error> {
    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {DEPOSIT_COLUMNS} FROM deposits WHERE user_id = "));
    qb.push_bind(user_id);
    if let Some(status) = filter.status {
        qb.push(" AND status = ").push_bind(status);
    }
    if let Some(chain_id) = filter.chain_id {
        qb.push(" AND chain_id = ").push_bind(chain_id);
    }
    if let Some(token) = &filter.token {
        qb.push(" AND token_symbol = ").push_bind(token.to_ascii_uppercase());
    }
    if let Some(reference) = &filter.reference {
        qb.push(" AND reference = ").push_bind(reference.to_ascii_lowercase());
    }
    if let Some(address) = &filter.payment_address {
        qb.push(" AND payment_address = ").push_bind(address.to_ascii_lowercase());
    }
    if let Some(receiver) = &filter.receiver {
        qb.push(" AND receiver = ").push_bind(receiver.to_ascii_lowercase());
    }
    if let Some(after) = filter.created_after {
        qb.push(" AND created_at >= ").push_bind(after);
    }
    if let Some(before) = filter.created_before {
        qb.push(" AND created_at < ").push_bind(before);
    }
    if let Some(cursor) = filter.cursor {
        qb.push(" AND (created_at, id) < (").push_bind(cursor.created_at).push(", ").push_bind(cursor.id).push(")");
    }
    qb.push(" ORDER BY created_at DESC, id DESC LIMIT ").push_bind(filter.limit + 1);
    let mut items: Vec<Deposit> = qb.build_query_as().fetch_all(pool).await?;
    let next_cursor = if items.len() as i64 > filter.limit {
        items.truncate(filter.limit as usize);
        items.last().map(|d| Cursor { created_at: d.created_at, id: d.id }.encode())
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

// ---------------------------------------------------------------------------------------------
// Events and outbox
// ---------------------------------------------------------------------------------------------

/// Appends to the deposit's timeline and, for app-facing events, queues the app webhook with a
/// snapshot of the deposit as it is now.
pub async fn record_event(
    conn: &mut PgConnection,
    deposit: &Deposit,
    event_type: &str,
    data: Value,
) -> Result<DepositEvent, sqlx::Error> {
    let (sequence,): (i64,) =
        sqlx::query_as("UPDATE deposits SET event_seq = event_seq + 1 WHERE id = $1 RETURNING event_seq")
            .bind(deposit.id)
            .fetch_one(&mut *conn)
            .await?;
    let event: DepositEvent = sqlx::query_as(
        "INSERT INTO deposit_events (id, deposit_id, sequence, type, data) VALUES ($1, $2, $3, $4, $5)
         RETURNING id, deposit_id, sequence, type, data, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(deposit.id)
    .bind(sequence)
    .bind(event_type)
    .bind(&data)
    .fetch_one(&mut *conn)
    .await?;
    metrics::counter!("gum_deposit_transitions_total", "event" => event_type.to_owned()).increment(1);

    if events::is_app_facing(event_type)
        && let Some(url) = webhook_url_for(conn, deposit).await?
    {
        let body = json!({
            "id": event.id,
            "type": event.event_type,
            "created_at": event.created_at,
            "sequence": event.sequence,
            "deposit": deposit.view(),
            "data": event.data,
        });
        enqueue(
            conn,
            "notify_app",
            deposit.id,
            json!({ "url": url, "event_id": event.id, "event_type": event.event_type, "body": body }),
        )
        .await?;
    }
    Ok(event)
}

async fn webhook_url_for(conn: &mut PgConnection, deposit: &Deposit) -> Result<Option<String>, sqlx::Error> {
    if deposit.webhook_url.is_some() {
        return Ok(deposit.webhook_url.clone());
    }
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT default_webhook_url FROM users WHERE id = $1")
        .bind(&deposit.user_id)
        .fetch_optional(conn)
        .await?;
    Ok(row.and_then(|(url,)| url))
}

pub async fn enqueue(conn: &mut PgConnection, kind: &str, deposit_id: Uuid, payload: Value) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO outbox (id, kind, deposit_id, payload) VALUES ($1, $2, $3, $4)")
        .bind(Uuid::now_v7())
        .bind(kind)
        .bind(deposit_id)
        .bind(payload)
        .execute(conn)
        .await?;
    Ok(())
}

/// Records an inbound webhook id. `false` means it was already processed.
pub async fn claim_inbound(conn: &mut PgConnection, source: &str, event_id: &str) -> Result<bool, sqlx::Error> {
    let inserted = sqlx::query("INSERT INTO inbound_events (source, event_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(source)
        .bind(event_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(inserted == 1)
}

// ---------------------------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------------------------

pub async fn mark_watch_registered(pool: &PgPool, id: Uuid, watch_id: Uuid) -> Result<Option<Deposit>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deposit: Option<Deposit> =
        sqlx::query_as(update_deposit("watch_id = $2, watch_registered_at = now()", "id = $1 AND watch_id IS NULL"))
            .bind(id)
            .bind(watch_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(d) = &deposit {
        record_event(&mut tx, d, events::WATCH_REGISTERED, json!({ "watch_id": watch_id })).await?;
    }
    tx.commit().await?;
    Ok(deposit)
}

/// A transfer was seen at chain head (not counted yet).
pub async fn apply_payment_pending(
    conn: &mut PgConnection,
    id: Uuid,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'partial_paid', detected_at = COALESCE(detected_at, now())",
        "id = $1 AND status IN ('pending', 'partial_paid')",
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::DETECTED, data).await?;
    }
    Ok(deposit)
}

/// A transfer was confirmed; `confirmed` is the watch's new confirmed total.
pub async fn apply_payment_confirmed(
    conn: &mut PgConnection,
    id: Uuid,
    confirmed: U256,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'partial_paid', detected_at = COALESCE(detected_at, now()), confirmed_amount = CAST($2 AS numeric)",
        "id = $1 AND status IN ('pending', 'partial_paid')",
    ))
    .bind(id)
    .bind(confirmed.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::PAYMENT_CONFIRMED, data).await?;
    }
    Ok(deposit)
}

/// A pending transfer was reorged out; nothing was counted.
pub async fn apply_payment_orphaned(
    conn: &mut PgConnection,
    id: Uuid,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> =
        sqlx::query_as(select_deposit("id = $1")).bind(id).fetch_optional(&mut *conn).await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::PAYMENT_ORPHANED, data).await?;
    }
    Ok(deposit)
}

/// The confirmed total reached the amount: submit settlement.
pub async fn apply_threshold_reached(
    conn: &mut PgConnection,
    id: Uuid,
    confirmed: U256,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'paid', detected_at = COALESCE(detected_at, now()), confirmed_amount = CAST($2 AS numeric)",
        "id = $1 AND status IN ('pending', 'partial_paid')",
    ))
    .bind(id)
    .bind(confirmed.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(d) = &deposit {
        let event = record_event(conn, d, events::READY, data).await?;
        enqueue_execute(conn, d.id, event.sequence).await?;
    }
    Ok(deposit)
}

/// Queues `PaymentFactory.execute`. The engine idempotency key includes the sequence so an
/// operator-triggered retry is a new job rather than a replay of the failed one.
pub async fn enqueue_execute(conn: &mut PgConnection, id: Uuid, sequence: i64) -> Result<(), sqlx::Error> {
    enqueue(conn, "submit_execute", id, json!({ "idempotency_key": format!("deposit:{id}:execute:{sequence}") })).await
}

pub async fn apply_expired(conn: &mut PgConnection, id: Uuid, data: Value) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'expired', expired_at = now()",
        "id = $1 AND status IN ('pending', 'partial_paid')",
    ))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::EXPIRED, data).await?;
    }
    Ok(deposit)
}

/// Marks open deposits whose expiry passed more than `grace_secs` ago as expired, in case the
/// indexer's `watch.expired` never arrived.
pub async fn expire_overdue(pool: &PgPool, grace_secs: i64, limit: i64) -> Result<usize, sqlx::Error> {
    let ids: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM deposits WHERE status IN ('pending', 'partial_paid') AND expires_at < now() - make_interval(secs => $1) LIMIT $2",
    )
    .bind(grace_secs as f64)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut n = 0;
    for (id,) in ids {
        let mut tx = pool.begin().await?;
        if apply_expired(&mut tx, id, json!({ "reason": "expiry passed without threshold" })).await?.is_some() {
            n += 1;
        }
        tx.commit().await?;
    }
    Ok(n)
}

pub async fn mark_engine_submitted(
    pool: &PgPool,
    id: Uuid,
    job_id: Uuid,
    replayed: bool,
) -> Result<Option<Deposit>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "engine_job_id = $2, engine_submitted_at = now()",
        "id = $1 AND status = 'paid' AND (engine_job_id IS NULL OR engine_job_id = $2)",
    ))
    .bind(id)
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(d) = &deposit
        && !replayed
    {
        record_event(&mut tx, d, events::SETTLEMENT_SUBMITTED, json!({ "engine_job_id": job_id })).await?;
    }
    tx.commit().await?;
    Ok(deposit)
}

pub async fn apply_settlement_included(
    conn: &mut PgConnection,
    id: Uuid,
    tx_hash: &str,
    block_number: Option<i64>,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> =
        sqlx::query_as(update_deposit("tx_hash = $2, block_number = $3", "id = $1 AND status = 'paid'"))
            .bind(id)
            .bind(tx_hash)
            .bind(block_number)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::SETTLEMENT_INCLUDED, data).await?;
    }
    Ok(deposit)
}

pub async fn apply_settled(
    conn: &mut PgConnection,
    id: Uuid,
    tx_hash: &str,
    block_number: Option<i64>,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'settled', settled_at = now(), tx_hash = $2, block_number = COALESCE($3, block_number)",
        "id = $1 AND status = 'paid'",
    ))
    .bind(id)
    .bind(tx_hash)
    .bind(block_number)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::SETTLED, data).await?;
    }
    Ok(deposit)
}

pub async fn apply_failed(
    conn: &mut PgConnection,
    id: Uuid,
    code: &str,
    message: &str,
    tx_hash: Option<&str>,
    revert_data: Option<&alloy_primitives::Bytes>,
    data: Value,
) -> Result<Option<Deposit>, sqlx::Error> {
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'failed', failed_at = now(), failure_code = $2, failure_message = $3, tx_hash = COALESCE($4, tx_hash), \
         failure_revert_data = $5",
        "id = $1 AND status = 'paid'",
    ))
    .bind(id)
    .bind(code)
    .bind(message)
    .bind(tx_hash)
    .bind(revert_data.map(ToString::to_string))
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(d) = &deposit {
        record_event(conn, d, events::FAILED, data).await?;
    }
    Ok(deposit)
}

/// Operator retry: a failed deposit goes back to `paid` and a fresh `execute` is queued.
pub async fn retry_settlement(pool: &PgPool, id: Uuid) -> Result<Option<Deposit>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "status = 'paid', engine_job_id = NULL, engine_submitted_at = NULL, failure_code = NULL, failure_message = NULL, \
         failure_revert_data = NULL, failed_at = NULL",
        "id = $1 AND status = 'failed'",
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(d) = &deposit {
        let event = record_event(&mut tx, d, events::SETTLEMENT_RETRIED, json!({})).await?;
        enqueue_execute(&mut tx, d.id, event.sequence).await?;
    }
    tx.commit().await?;
    Ok(deposit)
}

/// Deposits in `paid` that have not changed for `stale_secs`; the reconciler asks the engine.
/// How many outbox jobs are due (ready to run, or running), and how long the oldest has been due.
pub async fn outbox_due(pool: &PgPool) -> Result<(i64, f64), sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*), COALESCE(EXTRACT(EPOCH FROM now() - min(next_attempt_at))::float8, 0) \
         FROM outbox WHERE dead_at IS NULL AND next_attempt_at <= now()",
    )
    .fetch_one(pool)
    .await
}

/// How many deposits are `paid` (settlement pending), and how long ago the oldest was submitted.
pub async fn paid_backlog(pool: &PgPool) -> Result<(i64, f64), sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*), COALESCE(EXTRACT(EPOCH FROM now() - min(COALESCE(engine_submitted_at, updated_at)))::float8, 0) \
         FROM deposits WHERE status = 'paid'",
    )
    .fetch_one(pool)
    .await
}

/// `paid` deposits whose settlement was submitted more than `stale_secs` ago and that nothing has
/// touched (webhook or poll) for `repoll_secs`, least recently touched first. Staleness counts from
/// the submission, not from `updated_at`: a late webhook (e.g. `transaction.included` minutes after
/// the fact) must not push the poll back by another `stale_secs`.
pub async fn stale_paid(
    pool: &PgPool,
    stale_secs: i64,
    repoll_secs: i64,
    limit: i64,
) -> Result<Vec<Deposit>, sqlx::Error> {
    sqlx::query_as(select_deposit(
        "status = 'paid' AND engine_job_id IS NOT NULL \
         AND COALESCE(engine_submitted_at, updated_at) < now() - make_interval(secs => $1) \
         AND updated_at < now() - make_interval(secs => $2) \
         ORDER BY updated_at LIMIT $3",
    ))
    .bind(stale_secs as f64)
    .bind(repoll_secs as f64)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Open deposits with a registered watch that have not changed for `stale_secs`; the reconciler
/// compares them with the indexer's view of the watch.
pub async fn stale_open(pool: &PgPool, stale_secs: i64, limit: i64) -> Result<Vec<Deposit>, sqlx::Error> {
    sqlx::query_as(select_deposit(
        "status IN ('pending', 'partial_paid') AND watch_id IS NOT NULL AND updated_at < now() - make_interval(secs => $1) \
         ORDER BY updated_at LIMIT $2",
    ))
    .bind(stale_secs as f64)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Bumps `updated_at` so the reconciler does not re-check the row every tick.
pub async fn touch(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE deposits SET updated_at = now() WHERE id = $1").bind(id).execute(pool).await?;
    Ok(())
}

/// Forgets a watch the indexer does not know (any more) and queues a fresh registration.
pub async fn reregister_watch(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let deposit: Option<Deposit> = sqlx::query_as(update_deposit(
        "watch_id = NULL, watch_registered_at = NULL",
        "id = $1 AND status IN ('pending', 'partial_paid')",
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(d) = &deposit {
        record_event(&mut tx, d, events::WATCH_LOST, json!({})).await?;
        enqueue(&mut tx, "register_watch", d.id, json!({})).await?;
    }
    tx.commit().await?;
    Ok(deposit.is_some())
}

pub async fn prune(pool: &PgPool, idempotency_ttl_secs: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM idempotency_keys WHERE created_at < now() - make_interval(secs => $1)")
        .bind(idempotency_ttl_secs as f64)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM inbound_events WHERE received_at < now() - interval '7 days'").execute(pool).await?;
    Ok(())
}
