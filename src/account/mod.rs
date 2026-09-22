//! Account management for the web UI: the one canonical API key per user, its rotation, and the
//! account-level webhook settings.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::auth::api_key;
use crate::auth::{Principal, PrivyPrincipal};
use crate::error::ApiError;
use crate::state::AppState;
use crate::webhooks::target;

#[derive(Debug, Serialize, FromRow)]
pub struct ApiKeyInfo {
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub rotated_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct Account {
    pub user_id: String,
    pub api_key: Option<ApiKeyInfo>,
    pub webhook_url: Option<String>,
    /// Secret the account verifies our `X-Gum-Signature` with.
    pub webhook_secret: String,
    pub created_at: DateTime<Utc>,
}

/// The secret is returned exactly once, here.
#[derive(Debug, Serialize)]
pub struct IssuedKey {
    pub api_key: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub rotated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, FromRow)]
struct UserRow {
    webhook_secret: String,
    default_webhook_url: Option<String>,
    created_at: DateTime<Utc>,
}

async fn load_user(state: &AppState, user_id: &str) -> Result<UserRow, ApiError> {
    sqlx::query_as("SELECT webhook_secret, default_webhook_url, created_at FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("no such user"))
}

pub async fn get(State(state): State<AppState>, principal: Principal) -> Result<Json<Account>, ApiError> {
    let user = load_user(&state, &principal.user_id).await?;
    let api_key: Option<ApiKeyInfo> = sqlx::query_as(
        "SELECT key_prefix AS prefix, created_at, rotated_at, last_used_at FROM api_keys WHERE user_id = $1",
    )
    .bind(&principal.user_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(Account {
        user_id: principal.user_id,
        api_key,
        webhook_url: user.default_webhook_url,
        webhook_secret: user.webhook_secret,
        created_at: user.created_at,
    }))
}

/// Creates the user's key. `409 api_key_exists` if they already have one: rotate instead.
pub async fn create_key(
    State(state): State<AppState>,
    PrivyPrincipal(principal): PrivyPrincipal,
) -> Result<Response, ApiError> {
    let key = api_key::generate();
    let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
        "INSERT INTO api_keys (user_id, key_hash, key_prefix) VALUES ($1, $2, $3)
         ON CONFLICT (user_id) DO NOTHING RETURNING created_at",
    )
    .bind(&principal.user_id)
    .bind(&key.hash)
    .bind(&key.prefix)
    .fetch_optional(&state.pool)
    .await?;
    let Some((created_at,)) = row else {
        return Err(ApiError::conflict("api_key_exists", "this account already has an API key; rotate it instead"));
    };
    tracing::info!(user_id = %principal.user_id, prefix = %key.prefix, "api key created");
    Ok((StatusCode::CREATED, Json(IssuedKey { api_key: key.secret, prefix: key.prefix, created_at, rotated_at: None }))
        .into_response())
}

/// Replaces the user's key. The old key stops working immediately on this instance and within
/// `api_keys.cache_ttl_secs` on the others.
pub async fn rotate_key(
    State(state): State<AppState>,
    PrivyPrincipal(principal): PrivyPrincipal,
) -> Result<Json<IssuedKey>, ApiError> {
    let key = api_key::generate();
    #[derive(FromRow)]
    struct Rotated {
        old_hash: Vec<u8>,
        created_at: DateTime<Utc>,
        rotated_at: Option<DateTime<Utc>>,
    }
    let row: Option<Rotated> = sqlx::query_as(
        "UPDATE api_keys AS k SET key_hash = $2, key_prefix = $3, rotated_at = now()
         FROM (SELECT key_hash AS old_hash FROM api_keys WHERE user_id = $1) AS prev
         WHERE k.user_id = $1
         RETURNING prev.old_hash, k.created_at, k.rotated_at",
    )
    .bind(&principal.user_id)
    .bind(&key.hash)
    .bind(&key.prefix)
    .fetch_optional(&state.pool)
    .await?;
    let Some(Rotated { old_hash, created_at, rotated_at }) = row else {
        return Err(ApiError::not_found("this account has no API key yet; create one first"));
    };
    state.keys.invalidate(&old_hash);
    tracing::info!(user_id = %principal.user_id, prefix = %key.prefix, "api key rotated");
    Ok(Json(IssuedKey { api_key: key.secret, prefix: key.prefix, created_at, rotated_at }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAccount {
    /// Default webhook for deposits created without one. `null` clears it.
    pub webhook_url: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<UpdateAccount>,
) -> Result<Json<Account>, ApiError> {
    let url = match body.webhook_url.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            target::validate(raw, state.config.webhooks.allow_insecure_targets)
                .map_err(|m| ApiError::invalid(format!("webhook_url: {m}")))?
                .to_string(),
        ),
    };
    sqlx::query("UPDATE users SET default_webhook_url = $2 WHERE id = $1")
        .bind(&principal.user_id)
        .bind(url)
        .execute(&state.pool)
        .await?;
    get(State(state), principal).await
}

pub async fn rotate_webhook_secret(
    State(state): State<AppState>,
    PrivyPrincipal(principal): PrivyPrincipal,
) -> Result<Json<Account>, ApiError> {
    sqlx::query("UPDATE users SET webhook_secret = $2 WHERE id = $1")
        .bind(&principal.user_id)
        .bind(api_key::generate_webhook_secret())
        .execute(&state.pool)
        .await?;
    tracing::info!(user_id = %principal.user_id, "webhook secret rotated");
    get(State(state), principal).await
}
