//! `GET /v1/pay/{id}`: the payer's view of a deposit, behind the hosted payment page
//! (gum.money/pay/{id}).
//!
//! No authentication. Like a checkout link, knowing the deposit id is the capability: ids are
//! UUIDv7, and after the timestamp come a randomly seeded counter and 32 fresh random bits, so they
//! cannot be enumerated. The view is therefore payer-safe: what is needed to pay and to watch the
//! payment land, never the owner, receiver, recovery, salt, reference, webhook url, engine ids,
//! receipts or failure messages (which can be internal).
//!
//! Long polling: with `after=<sequence the client has>` and `wait=<seconds>`, the request is held
//! until the deposit's sequence moves past `after` or the wait runs out, then answered with the
//! current view. Changes arrive through `feed` the moment their transaction commits, so the page
//! sees a detected payment as fast as the indexer reports it. A held request holds no database
//! connection, only a broadcast receiver.

use std::time::Duration;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use super::{Deposit, DepositEvent, DepositStatus, Timestamps, events, feed, store};
use crate::error::ApiError;
use crate::state::AppState;

/// The longest hold a client may ask for.
const MAX_WAIT_SECS: i64 = 25;
/// Left between the longest hold and `server.request_timeout_ms` for the final reads and the
/// response.
const TIMEOUT_HEADROOM: Duration = Duration::from_secs(1);

/// The timeline entries a payer sees. The rest (watch bookkeeping, reconciliation, retries) is
/// operational.
const PAYER_EVENTS: &[&str] = &[
    events::CREATED,
    events::DETECTED,
    events::PAYMENT_CONFIRMED,
    events::PAYMENT_ORPHANED,
    events::READY,
    events::SETTLEMENT_SUBMITTED,
    events::SETTLEMENT_INCLUDED,
    events::SETTLED,
    events::FAILED,
    events::EXPIRED,
];
/// The fields of a gum-indexer transfer passed through to the payer.
const TRANSFER_FIELDS: &[&str] = &["tx_hash", "log_index", "block_number", "block_hash", "from", "amount", "status"];
/// Top-level event data fields passed through to the payer (`transfer` is filtered separately).
const DATA_FIELDS: &[&str] = &["confirmed_amount", "tx_hash", "block_number"];

#[derive(Debug, Default, Deserialize)]
pub struct PayQuery {
    /// The `sequence` the client already has.
    pub after: Option<i64>,
    /// Seconds to hold the request while nothing newer than `after` exists.
    pub wait: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PayerView {
    pub id: Uuid,
    pub status: DepositStatus,
    /// The deposit's event sequence; pass it back as `after`.
    pub sequence: i64,
    pub payment_address: String,
    pub chain_id: u64,
    pub token: String,
    pub token_address: String,
    pub token_decimals: u8,
    /// Base units, decimal string.
    pub amount: String,
    pub confirmed_amount: String,
    pub expires_at: DateTime<Utc>,
    /// The settlement transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<PayerFailure>,
    pub timestamps: Timestamps,
    /// When this response was built (RFC 3339, milliseconds), so the page can correct for the
    /// payer's clock in its countdown and latency readouts.
    pub server_time: String,
    pub events: Vec<PayerEvent>,
}

#[derive(Debug, Serialize)]
pub struct PayerFailure {
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct PayerEvent {
    pub id: Uuid,
    pub sequence: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub created_at: DateTime<Utc>,
    pub data: Value,
}

impl PayerView {
    fn new(d: &Deposit, events: Vec<DepositEvent>, now: DateTime<Utc>) -> Self {
        Self {
            id: d.id,
            status: d.status,
            sequence: d.event_seq,
            payment_address: d.payment_address.clone(),
            chain_id: d.chain_id as u64,
            token: d.token_symbol.clone(),
            token_address: d.token_address.clone(),
            token_decimals: d.token_decimals as u8,
            amount: d.amount.clone(),
            confirmed_amount: d.confirmed_amount.clone(),
            expires_at: d.expires_at,
            tx_hash: d.tx_hash.clone(),
            block_number: d.block_number.map(|n| n as u64),
            failure: d.failure_code.as_ref().map(|code| PayerFailure { code: code.clone() }),
            timestamps: Timestamps {
                created_at: d.created_at,
                updated_at: d.updated_at,
                detected_at: d.detected_at,
                settled_at: d.settled_at,
                failed_at: d.failed_at,
                expired_at: d.expired_at,
            },
            server_time: now.to_rfc3339_opts(SecondsFormat::Millis, true),
            events: events
                .into_iter()
                .map(|e| PayerEvent {
                    data: payer_data(&e.event_type, &e.data),
                    id: e.id,
                    sequence: e.sequence,
                    event_type: e.event_type,
                    created_at: e.created_at,
                })
                .collect(),
        }
    }
}

/// Keeps only the payer-safe fields of an event's data (an allowlist, so fields added to events
/// later stay private until chosen): the indexer's transfer, the confirmed total, the settlement
/// transaction and, on `deposit.failed`, the failure code. Nulls are dropped.
fn payer_data(event_type: &str, data: &Value) -> Value {
    fn pick(from: &Map<String, Value>, fields: &[&str], into: &mut Map<String, Value>) {
        for &field in fields {
            if let Some(value) = from.get(field).filter(|v| !v.is_null()) {
                into.insert(field.to_owned(), value.clone());
            }
        }
    }
    let mut out = Map::new();
    let Some(data) = data.as_object() else { return Value::Object(out) };
    if let Some(Value::Object(transfer)) = data.get("transfer") {
        let mut kept = Map::new();
        pick(transfer, TRANSFER_FIELDS, &mut kept);
        out.insert("transfer".into(), Value::Object(kept));
    }
    pick(data, DATA_FIELDS, &mut out);
    if event_type == events::FAILED {
        pick(data, &["code"], &mut out);
    }
    Value::Object(out)
}

/// How long to hold a request that asked to wait `requested` seconds.
///
/// Capped just under `server.request_timeout_ms` (so ~9 s by default) instead of exempting this
/// route from the timeout layer: every request stays bounded by that one setting, the middleware
/// stack needs no special case, and graceful shutdown drains held requests as fast as any other.
/// The client asks for 25 s and simply polls again, so a shorter hold costs one extra request.
fn hold_for(requested: Option<i64>, request_timeout: Duration) -> Duration {
    let requested = Duration::from_secs(requested.unwrap_or(0).clamp(0, MAX_WAIT_SECS) as u64);
    requested.min(request_timeout.saturating_sub(TIMEOUT_HEADROOM))
}

/// Always `Cache-Control: no-store`: the view changes by the second and a cached one would show a
/// payer a stale status.
pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<PayQuery>, QueryRejection>,
) -> Response {
    let mut response = view(&state, &id, query).await.into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn view(
    state: &AppState,
    id: &str,
    query: Result<Query<PayQuery>, QueryRejection>,
) -> Result<Json<PayerView>, ApiError> {
    let Query(query) = query.map_err(|rej| ApiError::invalid(rej.body_text()))?;
    // A malformed id is just another id that does not exist.
    let id: Uuid = id.parse().map_err(|_| no_such_deposit())?;
    let hold = hold_for(query.wait, Duration::from_millis(state.config.server.request_timeout_ms));

    // Subscribe before the first read: a change committed between the read and a later
    // subscription would go unheard until the hold ran out.
    let mut changes = state.deposit_changes.subscribe();
    let mut deposit = store::get(&state.pool, id).await?.ok_or_else(no_such_deposit)?;
    if let Some(after) = query.after
        && !hold.is_zero()
        && deposit.event_seq <= after
    {
        let deadline = tokio::time::Instant::now() + hold;
        loop {
            match tokio::time::timeout_at(deadline, changes.recv()).await {
                Err(_elapsed) => break,
                Ok(Ok(changed)) if changed != id && changed != feed::RESYNC => continue,
                // Ours, a resync after the listener reconnected, or we fell behind and may have
                // missed ours: re-read.
                Ok(Ok(_) | Err(RecvError::Lagged(_))) => {}
                // The sender lives in `AppState`, so this does not happen; do not spin if it does.
                Ok(Err(RecvError::Closed)) => break,
            }
            deposit = store::get(&state.pool, id).await?.ok_or_else(no_such_deposit)?;
            if deposit.event_seq > after {
                break;
            }
        }
    }
    let events = store::events_through(&state.pool, id, deposit.event_seq, PAYER_EVENTS).await?;
    Ok(Json(PayerView::new(&deposit, events, Utc::now())))
}

fn no_such_deposit() -> ApiError {
    ApiError::not_found("no such deposit")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn holds_are_clamped_and_stay_under_the_request_timeout() {
        let timeout = Duration::from_secs(10);
        assert_eq!(hold_for(None, timeout), Duration::ZERO);
        assert_eq!(hold_for(Some(-3), timeout), Duration::ZERO);
        assert_eq!(hold_for(Some(4), timeout), Duration::from_secs(4));
        assert_eq!(hold_for(Some(25), timeout), Duration::from_secs(9));
        assert_eq!(hold_for(Some(25), Duration::from_secs(60)), Duration::from_secs(25));
        assert_eq!(hold_for(Some(1_000), Duration::from_secs(60)), Duration::from_secs(25));
        assert_eq!(hold_for(Some(5), Duration::from_millis(500)), Duration::ZERO);
    }

    #[test]
    fn event_data_is_projected_to_an_allowlist() {
        let detected = json!({
            "indexer_event_id": "ev-1",
            "confirmed_amount": "0",
            "transfer": { "tx_hash": "0x11", "log_index": 1, "block_number": 10, "block_hash": "0x22",
                          "from": "0x09", "amount": "2500000", "status": "pending", "extra": "x" },
        });
        assert_eq!(
            payer_data(events::DETECTED, &detected),
            json!({
                "confirmed_amount": "0",
                "transfer": { "tx_hash": "0x11", "log_index": 1, "block_number": 10, "block_hash": "0x22",
                              "from": "0x09", "amount": "2500000", "status": "pending" },
            })
        );
        let settled = json!({
            "engine_event_id": "e", "engine_job_id": "j", "tx_hash": "0xaa", "block_number": 5,
            "outcome": "success", "reincluded": false, "receipt": { "logs": [] },
        });
        assert_eq!(payer_data(events::SETTLED, &settled), json!({ "tx_hash": "0xaa", "block_number": 5 }));
        let failed = json!({ "code": "engine_reverted", "message": "internal detail", "tx_hash": null, "error": "x" });
        assert_eq!(payer_data(events::FAILED, &failed), json!({ "code": "engine_reverted" }));
        assert_eq!(payer_data(events::EXPIRED, &json!({ "code": "x", "transfer": null })), json!({}));
        assert_eq!(payer_data(events::CREATED, &json!({})), json!({}));
    }
}
