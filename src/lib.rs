//! gum-server: one-time stablecoin deposit addresses as an API.
//!
//! ```text
//! app ──POST /v1/deposit──▶ gum-server ──(outbox)──▶ gum-indexer  POST /v1/watches
//!                            │  ◀──── webhook ─────────┘   payment.* / threshold.reached
//!                            ├──(outbox)──▶ gum-engine   POST /v1/transactions  PaymentFactory.execute
//!                            │  ◀──── webhook ─────────┘   transaction.confirmed
//!                            └──(outbox)──▶ app webhook   deposit.detected / confirmed / settled / failed
//! ```
//!
//! The request path computes a CREATE2 address and writes one transaction; everything that
//! touches a chain is delegated and driven by webhooks plus a transactional outbox.

pub mod account;
pub mod app;
pub mod auth;
pub mod chain;
pub mod clients;
pub mod config;
pub mod db;
pub mod deposit;
pub mod error;
pub mod outbox;
pub mod reconciler;
pub mod state;
pub mod supervise;
pub mod telemetry;
pub mod webhooks;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
