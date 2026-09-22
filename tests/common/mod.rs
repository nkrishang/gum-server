//! Test harness: a real server on a random port, a fresh database per test, and wiremock stand-ins
//! for gum-indexer, gum-engine and the app's webhook endpoint.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use alloy_primitives::address;
use chrono::Utc;
use gum_server::config::{ChainConfig, Config, TokenConfig};
use gum_server::state::AppState;
use gum_server::webhooks::sign;
use gum_server::{MIGRATOR, app, outbox, reconciler};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

pub const PRIVY_APP_ID: &str = "test-app";
pub const INDEXER_SECRET: &str = "indexer-secret";
pub const ENGINE_SECRET: &str = "engine-secret";
pub const ADMIN_TOKEN: &str = "admin-token";
pub const FACTORY: &str = "0x5FbDB2315678afecb367f032d93F642f64180aa3";
pub const RECOVERY: &str = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
pub const USDC: &str = "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512";

const PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgU7hql39QEntx0whR
u9fUwJhXuy/iDeoMBKhKL1YrnIuhRANCAASJTbshE2SLSlSHIfUw7qrgjI7oQmm6
zUc8RblUTkKx6Gnwx4Ixw7EB9x3HtTogGCOSTXUyQ4b/qIblZbWMxl4m
-----END PRIVATE KEY-----
";
const PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEiU27IRNki0pUhyH1MO6q4IyO6EJp
us1HPEW5VE5Csehp8MeCMcOxAfcdx7U6IBgjkk11MkOG/6iG5WW1jMZeJg==
-----END PUBLIC KEY-----
";

pub struct Harness {
    pub base_url: String,
    pub http: reqwest::Client,
    pub pool: PgPool,
    pub state: AppState,
    pub indexer: MockServer,
    pub engine: MockServer,
    pub app: MockServer,
    shutdown: CancellationToken,
}

/// `TEST_DATABASE_URL` must point at a Postgres the tests may create databases on.
pub fn test_database_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL").ok().filter(|s| !s.is_empty())
}

async fn fresh_database(admin_url: &str) -> String {
    let name = format!("gum_test_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new().max_connections(1).connect(admin_url).await.expect("admin connection");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}"))).execute(&admin).await.expect("create database");
    admin.close().await;
    let mut url = url::Url::parse(admin_url).expect("url");
    url.set_path(&format!("/{name}"));
    url.to_string()
}

impl Harness {
    pub async fn start() -> Option<Self> {
        let admin_url = test_database_url()?;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let db_url = fresh_database(&admin_url).await;
        let pool = PgPoolOptions::new().max_connections(8).connect(&db_url).await.expect("db");
        MIGRATOR.run(&pool).await.expect("migrations");

        let indexer = MockServer::start().await;
        let engine = MockServer::start().await;
        let app_mock = MockServer::start().await;

        let metrics = metrics_handle();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");

        let mut config = Config::load_unchecked(Path::new("config")).expect("config/default.toml must load");
        config.server.callback_base_url = base_url.clone();
        config.database.url = db_url;
        config.privy.app_id = PRIVY_APP_ID.into();
        config.privy.verification_key = PUBLIC_KEY_PEM.into();
        config.payments.factory_address = FACTORY.into();
        config.payments.recovery_address = RECOVERY.into();
        config.payments.min_expiry_lead_secs = 60;
        config.indexer.base_url = indexer.uri();
        config.indexer.webhook_secret = INDEXER_SECRET.into();
        config.engine.base_url = engine.uri();
        config.engine.webhook_secret = ENGINE_SECRET.into();
        config.webhooks.allow_insecure_targets = true;
        config.webhooks.retry_base_ms = 50;
        config.webhooks.retry_cap_ms = 200;
        config.outbox.poll_interval_ms = 50;
        config.outbox.workers = 4;
        config.reconciler.interval_secs = 3600;
        config.admin.token = ADMIN_TOKEN.into();
        config.server.cors_origins = vec!["https://app.example".into()];
        config.chains = BTreeMap::from([(
            "anvil".to_owned(),
            ChainConfig {
                chain_id: 31337,
                tokens: vec![TokenConfig { symbol: "USDC".into(), address: USDC.parse().unwrap(), decimals: 6 }],
            },
        )]);

        config.validate().expect("test config is valid");
        let state = AppState::new(config, pool.clone(), metrics).expect("state");
        let shutdown = CancellationToken::new();
        tokio::spawn(outbox::run(state.clone(), shutdown.clone()));
        tokio::spawn(reconciler::run(state.clone(), shutdown.clone()));
        let router = app::router(state.clone());
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, router).with_graceful_shutdown(server_shutdown.cancelled_owned()).await.unwrap();
        });

        Some(Self {
            base_url,
            http: reqwest::Client::builder().timeout(Duration::from_secs(5)).build().unwrap(),
            pool,
            state,
            indexer,
            engine,
            app: app_mock,
            shutdown,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub fn privy_token(&self, sub: &str) -> String {
        #[derive(Serialize)]
        struct Claims<'a> {
            sub: &'a str,
            iss: &'a str,
            aud: &'a str,
            iat: i64,
            exp: i64,
            sid: &'a str,
        }
        let now = Utc::now().timestamp();
        let key = EncodingKey::from_ec_pem(PRIVATE_KEY_PEM.as_bytes()).unwrap();
        encode(
            &Header::new(Algorithm::ES256),
            &Claims { sub, iss: "privy.io", aud: PRIVY_APP_ID, iat: now, exp: now + 3600, sid: "sess" },
            &key,
        )
        .unwrap()
    }

    /// Creates the user's API key through the web-UI route and returns the secret.
    pub async fn api_key_for(&self, sub: &str) -> String {
        let token = self.privy_token(sub);
        let res = self.http.post(self.url("/v1/account/api-key")).bearer_auth(&token).send().await.unwrap();
        assert_eq!(res.status(), 201, "{}", res.text().await.unwrap());
        res.json::<serde_json::Value>().await.unwrap()["api_key"].as_str().unwrap().to_owned()
    }

    /// Delivers a signed webhook the way gum-indexer / gum-engine would.
    pub async fn deliver(&self, path: &str, secret: &str, body: &serde_json::Value) -> reqwest::Response {
        let raw = serde_json::to_vec(body).unwrap();
        let signature = sign::signature_header(secret, Utc::now().timestamp(), &raw);
        self.http
            .post(self.url(path))
            .header("content-type", "application/json")
            .header("x-gum-signature", signature)
            .body(raw)
            .send()
            .await
            .unwrap()
    }

    /// Polls until `f` returns `Some`, or panics after `timeout`.
    pub async fn wait_for<T, F, Fut>(&self, what: &str, timeout: Duration, mut f: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(v) = f().await {
                return v;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn deposit_status(&self, id: uuid::Uuid) -> String {
        let (status,): (String,) = sqlx::query_as("SELECT status::text FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        status
    }

    pub fn recovery() -> alloy_primitives::Address {
        address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    use std::sync::OnceLock;
    static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let _ = metrics::set_global_recorder(recorder);
            handle
        })
        .clone()
}

/// A `POST /v1/deposit` body that passes validation.
pub fn deposit_body() -> serde_json::Value {
    serde_json::json!({
        "chain_id": "anvil",
        "token": "USDC",
        "amount": "2500000",
        "receiver": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
        "expires_at": (Utc::now() + chrono::Duration::hours(2)).to_rfc3339(),
        "reference": format!("0x{}", "ab".repeat(32)),
    })
}
