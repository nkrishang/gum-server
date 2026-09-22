//! Router, middleware stack and the operational routes.

use std::time::Duration;

use axum::error_handling::HandleErrorLayer;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderName, StatusCode};
use axum::http::{HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{BoxError, Json, Router, middleware};
use serde_json::json;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::auth::Admin;
use crate::deposit::{DepositView, routes as deposit, store};
use crate::error::ApiError;
use crate::state::AppState;
use crate::telemetry;
use crate::webhooks::inbound;
use crate::{account, chain};

pub fn router(state: AppState) -> Router {
    let cfg = &state.config.server;
    let request_id = HeaderName::from_static("x-request-id");

    let api = Router::new()
        // The user flow.
        .route("/v1/deposit", post(deposit::create).get(deposit::list_mine))
        .route("/v1/deposit/id/{id}", get(deposit::get_by_id))
        .route("/v1/deposit/user/{user_id}", get(deposit::list_for_user))
        // Account and API key management (web UI).
        .route("/v1/account", get(account::get).patch(account::update))
        .route("/v1/account/api-key", post(account::create_key))
        .route("/v1/account/api-key/rotate", post(account::rotate_key))
        .route("/v1/account/webhook-secret/rotate", post(account::rotate_webhook_secret))
        // Callbacks from the services we delegate chain work to.
        .route("/v1/webhooks/indexer", post(inbound::indexer))
        .route("/v1/webhooks/engine", post(inbound::engine))
        // Reference data.
        .route("/v1/chains", get(list_chains))
        // Operator.
        .route("/v1/admin/deposits/{id}/retry-settlement", post(admin_retry_settlement))
        .route("/v1/admin/outbox", get(admin_outbox))
        .route("/v1/admin/outbox/{id}/requeue", post(admin_requeue))
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes));

    let ops = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(render_metrics));

    let trace = TraceLayer::new_for_http()
        .make_span_with(|req: &axum::http::Request<_>| {
            let request_id = req.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("-");
            tracing::info_span!("request", method = %req.method(), path = %req.uri().path(), request_id)
        })
        .on_response(|res: &axum::http::Response<_>, latency: Duration, _span: &tracing::Span| {
            let status = res.status().as_u16();
            if status >= 500 {
                tracing::error!(status, latency_ms = latency.as_millis() as u64, "response");
            } else {
                tracing::info!(status, latency_ms = latency.as_millis() as u64, "response");
            }
        });

    // Browser clients (the web UI) live on another origin. Credentials travel in headers, never
    // cookies, so no `allow_credentials`; an empty list turns the layer into a no-op.
    let origins: Vec<HeaderValue> = cfg.cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
    let cors = if origins.is_empty() {
        CorsLayer::new()
    } else {
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                HeaderName::from_static("privy-id-token"),
                HeaderName::from_static("x-api-key"),
                HeaderName::from_static("idempotency-key"),
            ])
            .expose_headers([HeaderName::from_static("x-request-id"), HeaderName::from_static("idempotent-replayed")])
            .max_age(Duration::from_secs(600))
    };

    // Innermost first: metrics see the matched route; the timeout bounds handlers; load shedding
    // and the concurrency cap sit outside so shed requests are cheap; request ids are outermost
    // so every log line and response carries one.
    Router::new()
        .merge(api)
        .merge(ops)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn(telemetry::http_metrics))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_millis(cfg.request_timeout_ms),
        ))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_: BoxError| async {
                    ApiError::unavailable("shedding", "server is overloaded; retry")
                }))
                .layer(tower::load_shed::LoadShedLayer::new())
                .layer(tower::limit::ConcurrencyLimitLayer::new(cfg.max_in_flight)),
        )
        .layer(cors)
        .layer(trace)
        .layer(CatchPanicLayer::custom(|_| ApiError::internal().into_response()))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .with_state(state)
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", "method not allowed")
}

async fn list_chains(State(state): State<AppState>) -> Json<serde_json::Value> {
    let chains: Vec<_> = state
        .registry
        .chains()
        .map(|c: &std::sync::Arc<chain::ChainSpec>| {
            json!({
                "name": c.name,
                "chain_id": c.chain_id,
                "factory": state.factory,
                "tokens": c.tokens.iter().map(|t| json!({ "symbol": t.symbol, "address": t.address, "decimals": t.decimals })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!({ "chains": chains }))
}

// ---------------------------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------------------------

/// Liveness: the process serves HTTP.
async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Readiness: the database answers. Upstream health is reported but does not gate readiness,
/// because `POST /v1/deposit` works (and queues) while gum-indexer / gum-engine are down.
async fn readyz(State(state): State<AppState>) -> Response {
    let db = sqlx::query("SELECT 1").execute(&state.pool).await.is_ok();
    let (indexer, engine) = tokio::join!(state.indexer.healthy(), state.engine.healthy());
    let body = json!({
        "db": db,
        "indexer": indexer,
        "engine": engine,
        "privy": state.privy.is_configured(),
    });
    let status = if db { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(body)).into_response()
}

async fn render_metrics(State(state): State<AppState>) -> String {
    state.metrics.render()
}

// ---------------------------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------------------------

async fn admin_retry_settlement(
    State(state): State<AppState>,
    _admin: Admin,
    Path(id): Path<Uuid>,
) -> Result<Json<DepositView>, ApiError> {
    let deposit = store::retry_settlement(&state.pool, id).await?;
    let Some(deposit) = deposit else {
        return Err(ApiError::conflict("not_retryable", "deposit is not in the failed state"));
    };
    state.outbox_wake.notify_one();
    tracing::warn!(deposit_id = %id, "settlement retried by operator");
    Ok(Json(deposit.view()))
}

/// Outbox rows that needed more than one attempt or were given up on.
async fn admin_outbox(State(state): State<AppState>, _admin: Admin) -> Result<Json<serde_json::Value>, ApiError> {
    #[derive(serde::Serialize, sqlx::FromRow)]
    struct Row {
        id: Uuid,
        kind: String,
        deposit_id: Uuid,
        attempts: i32,
        last_error: Option<String>,
        dead_at: Option<chrono::DateTime<chrono::Utc>>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let jobs: Vec<Row> = sqlx::query_as(
        "SELECT id, kind, deposit_id, attempts, last_error, dead_at, created_at FROM outbox
         WHERE attempts > 1 OR dead_at IS NOT NULL ORDER BY created_at DESC LIMIT 200",
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({ "jobs": jobs })))
}

async fn admin_requeue(
    State(state): State<AppState>,
    _admin: Admin,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let updated = sqlx::query("UPDATE outbox SET dead_at = NULL, next_attempt_at = now(), locked_until = NULL, created_at = now() WHERE id = $1")
        .bind(id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(ApiError::not_found("no such outbox job"));
    }
    state.outbox_wake.notify_one();
    Ok(StatusCode::ACCEPTED)
}
