//! Request authentication.
//!
//! Two credentials are accepted:
//!
//! * an API key — `Authorization: Bearer gum_sk_…` or `X-Api-Key: gum_sk_…` — for apps integrating
//!   the deposit API;
//! * a Privy token — `privy-id-token: <jwt>` (identity token) or `Authorization: Bearer <jwt>`
//!   (access token) — for the web UI, which manages the user's API key.
//!
//! Both resolve to the same [`Principal`]: the Privy DID that owns the deposits.

pub mod api_key;
pub mod privy;

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use subtle::ConstantTimeEq;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    ApiKey,
    Privy,
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub method: AuthMethod,
}

/// A [`Principal`] that must have authenticated with a Privy token (web UI only routes).
#[derive(Debug, Clone)]
pub struct PrivyPrincipal(pub Principal);

/// The operator, authenticated with `admin.token`.
#[derive(Debug, Clone, Copy)]
pub struct Admin;

enum Credential {
    ApiKey(String),
    Privy(String),
}

fn credential(parts: &Parts) -> Result<Credential, ApiError> {
    if let Some(v) = parts.headers.get("privy-id-token").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(Credential::Privy(v.to_owned()));
        }
    }
    if let Some(v) = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Ok(Credential::ApiKey(v.to_owned()));
        }
    }
    if let Some(v) = parts.headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        let Some(token) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")) else {
            return Err(ApiError::unauthorized("Authorization must be a Bearer credential"));
        };
        let token = token.trim();
        if token.is_empty() {
            return Err(ApiError::unauthorized("empty bearer credential"));
        }
        return Ok(if api_key::looks_like_key(token) {
            Credential::ApiKey(token.to_owned())
        } else {
            Credential::Privy(token.to_owned())
        });
    }
    Err(ApiError::unauthorized("missing credentials: send an API key or a Privy token"))
}

async fn authenticate_api_key(state: &AppState, secret: &str) -> Result<Principal, ApiError> {
    let key_hash = api_key::hash(secret);
    let owner = match state.keys.get(&key_hash) {
        Some(owner) => owner,
        None => {
            let row: Option<(String,)> = sqlx::query_as("SELECT user_id FROM api_keys WHERE key_hash = $1")
                .bind(&key_hash)
                .fetch_optional(&state.pool)
                .await?;
            let Some((user_id,)) = row else {
                metrics::counter!("gum_auth_total", "method" => "api_key", "outcome" => "invalid").increment(1);
                return Err(ApiError::unauthorized("invalid API key"));
            };
            state.keys.insert(key_hash.clone(), api_key::KeyOwner { user_id })
        }
    };
    if !state.keys.allow(&key_hash) {
        metrics::counter!("gum_auth_total", "method" => "api_key", "outcome" => "rate_limited").increment(1);
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "API key rate limit exceeded"));
    }
    if state.first_touch(format!("key:{}", hex::encode(&key_hash))) {
        let pool = state.pool.clone();
        let hash = key_hash.clone();
        tokio::spawn(async move {
            let _ = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE key_hash = $1")
                .bind(hash)
                .execute(&pool)
                .await;
        });
    }
    metrics::counter!("gum_auth_total", "method" => "api_key", "outcome" => "ok").increment(1);
    Ok(Principal { user_id: owner.user_id.clone(), method: AuthMethod::ApiKey })
}

async fn authenticate_privy(state: &AppState, token: &str) -> Result<Principal, ApiError> {
    let claims = match state.privy.verify(token) {
        Ok(claims) => claims,
        Err(privy::PrivyError::NotConfigured) => {
            metrics::counter!("gum_auth_total", "method" => "privy", "outcome" => "unconfigured").increment(1);
            return Err(ApiError::unavailable("auth_unavailable", "Privy authentication is not configured"));
        }
        Err(privy::PrivyError::Invalid(reason)) => {
            metrics::counter!("gum_auth_total", "method" => "privy", "outcome" => "invalid").increment(1);
            tracing::debug!(reason, "privy token rejected");
            return Err(ApiError::unauthorized("invalid Privy token"));
        }
    };
    // First contact creates the user; later calls refresh last_seen_at at most once a minute.
    if state.first_touch(format!("user:{}", claims.sub)) {
        sqlx::query(
            "INSERT INTO users (id, webhook_secret) VALUES ($1, $2)
             ON CONFLICT (id) DO UPDATE SET last_seen_at = now()",
        )
        .bind(&claims.sub)
        .bind(api_key::generate_webhook_secret())
        .execute(&state.pool)
        .await?;
    }
    metrics::counter!("gum_auth_total", "method" => "privy", "outcome" => "ok").increment(1);
    Ok(Principal { user_id: claims.sub, method: AuthMethod::Privy })
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match credential(parts)? {
            Credential::ApiKey(secret) => authenticate_api_key(state, &secret).await,
            Credential::Privy(token) => authenticate_privy(state, &token).await,
        }
    }
}

impl FromRequestParts<AppState> for PrivyPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let principal = Principal::from_request_parts(parts, state).await?;
        if principal.method != AuthMethod::Privy {
            return Err(ApiError::forbidden("this route requires a Privy session, not an API key"));
        }
        Ok(Self(principal))
    }
}

impl FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let expected = state.config.admin.token.as_bytes();
        if expected.is_empty() {
            return Err(ApiError::not_found("no such route"));
        }
        let presented = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or_default();
        if presented.len() != expected.len() || presented.as_bytes().ct_eq(expected).unwrap_u8() != 1 {
            return Err(ApiError::unauthorized("invalid admin token"));
        }
        Ok(Self)
    }
}
