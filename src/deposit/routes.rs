//! `POST /v1/deposit`, `GET /v1/deposit/id/{id}`, `GET /v1/deposit/user/{user_id}`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::request::{CreateDepositRequest, Validator, request_fingerprint};
use super::store::{self, CreateOutcome, Cursor, Idempotency, ListFilter};
use super::{DepositEventView, DepositStatus, DepositView};
use crate::auth::Principal;
use crate::error::ApiError;
use crate::state::AppState;

const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;
const DEFAULT_PAGE: i64 = 50;
const MAX_PAGE: i64 = 200;

fn idempotency_key(headers: &HeaderMap) -> Result<Option<&str>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else { return Ok(None) };
    let key = value.to_str().map_err(|_| ApiError::invalid("Idempotency-Key must be ASCII"))?.trim();
    if key.is_empty() {
        return Ok(None);
    }
    if key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(ApiError::invalid(format!("Idempotency-Key must be at most {MAX_IDEMPOTENCY_KEY_LEN} characters")));
    }
    Ok(Some(key))
}

/// `201` with the payment address; `200` + `Idempotent-Replayed: true` for a replay.
pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    body: Result<Json<CreateDepositRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = body?;
    let key = idempotency_key(&headers)?;
    let validator = Validator {
        registry: &state.registry,
        payments: &state.config.payments,
        webhooks: &state.config.webhooks,
        factory: state.factory,
        recovery: state.recovery,
    };
    let new = validator.validate(&req, Utc::now())?;
    let idempotency = key.map(|key| Idempotency { key, fingerprint: request_fingerprint(&req) });

    match store::create_deposit(&state.pool, &principal.user_id, &new, idempotency).await? {
        CreateOutcome::Created(deposit) => {
            metrics::counter!("gum_deposits_created_total", "chain" => deposit.chain_id.to_string(), "token" => deposit.token_symbol.clone())
                .increment(1);
            state.outbox_wake.notify_one();
            tracing::info!(deposit_id = %deposit.id, user_id = %principal.user_id, chain_id = deposit.chain_id, payment_address = %deposit.payment_address, "deposit created");
            Ok((StatusCode::CREATED, Json(deposit.view())).into_response())
        }
        CreateOutcome::Replayed(deposit) => {
            let mut response = (StatusCode::OK, Json(deposit.view())).into_response();
            response.headers_mut().insert("idempotent-replayed", HeaderValue::from_static("true"));
            Ok(response)
        }
        CreateOutcome::Conflict => Err(ApiError::conflict(
            "idempotency_conflict",
            "this Idempotency-Key was already used with a different request",
        )),
    }
}

pub async fn get_by_id(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<Uuid>,
) -> Result<Json<DepositView>, ApiError> {
    let deposit = store::get(&state.pool, id).await?.filter(|d| d.user_id == principal.user_id);
    let Some(deposit) = deposit else { return Err(ApiError::not_found("no such deposit")) };
    let events = store::events(&state.pool, deposit.id).await?;
    let mut view = deposit.view();
    view.events = Some(events.into_iter().map(DepositEventView::from).collect());
    Ok(Json(view))
}

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    pub status: Option<String>,
    pub chain_id: Option<String>,
    pub token: Option<String>,
    pub reference: Option<String>,
    pub payment_address: Option<String>,
    pub receiver: Option<String>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<DepositView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl ListQuery {
    fn into_filter(self, state: &AppState) -> Result<ListFilter, ApiError> {
        let status = match self.status.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(DepositStatus::parse(raw).ok_or_else(|| {
                ApiError::invalid("status must be one of pending, partial_paid, paid, settled, failed, expired")
            })?),
        };
        let chain_id = match self.chain_id.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(
                state
                    .registry
                    .chain(raw)
                    .map(|c| c.chain_id as i64)
                    .ok_or_else(|| ApiError::invalid("unsupported chain_id"))?,
            ),
        };
        let limit = self.limit.unwrap_or(DEFAULT_PAGE);
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(ApiError::invalid(format!("limit must be between 1 and {MAX_PAGE}")));
        }
        let cursor = match self.cursor.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(Cursor::decode(raw).ok_or_else(|| ApiError::invalid("invalid cursor"))?),
        };
        let non_empty = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        Ok(ListFilter {
            status,
            chain_id,
            token: non_empty(self.token),
            reference: non_empty(self.reference),
            payment_address: non_empty(self.payment_address),
            receiver: non_empty(self.receiver),
            created_after: self.created_after,
            created_before: self.created_before,
            cursor,
            limit,
        })
    }
}

/// A user's deposits, newest first. Callers may only list their own.
pub async fn list_for_user(
    State(state): State<AppState>,
    principal: Principal,
    Path(user_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    if user_id != principal.user_id {
        return Err(ApiError::forbidden("you can only list your own deposits"));
    }
    list(state, principal, query).await
}

/// The caller's deposits.
pub async fn list_mine(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse>, ApiError> {
    list(state, principal, query).await
}

async fn list(state: AppState, principal: Principal, query: ListQuery) -> Result<Json<ListResponse>, ApiError> {
    let filter = query.into_filter(&state)?;
    let page = store::list(&state.pool, &principal.user_id, &filter).await?;
    Ok(Json(ListResponse { items: page.items.iter().map(|d| d.view()).collect(), next_cursor: page.next_cursor }))
}
