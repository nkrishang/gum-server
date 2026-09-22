//! Deposit requests: the server's own representation of a one-time payment address.

pub mod request;
pub mod routes;
pub mod store;

use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::chain::payment::PaymentTerms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "deposit_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum DepositStatus {
    Pending,
    PartialPaid,
    Paid,
    Settled,
    Failed,
    Expired,
}

impl DepositStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::PartialPaid => "partial_paid",
            Self::Paid => "paid",
            Self::Settled => "settled",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Settled | Self::Failed | Self::Expired)
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => Self::Pending,
            "partial_paid" => Self::PartialPaid,
            "paid" => Self::Paid,
            "settled" => Self::Settled,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => return None,
        })
    }
}

/// A `deposits` row. Addresses are lowercase hex text; amounts are decimal strings (`::text` casts).
#[derive(Debug, Clone, FromRow)]
pub struct Deposit {
    pub id: Uuid,
    pub user_id: String,
    pub chain_id: i64,
    pub token_symbol: String,
    pub token_address: String,
    pub token_decimals: i16,
    pub amount: String,
    pub receiver: String,
    pub recovery: String,
    pub salt: String,
    pub expires_at: DateTime<Utc>,
    pub payment_address: String,
    pub reference: Option<String>,
    pub webhook_url: Option<String>,
    pub status: DepositStatus,
    pub confirmed_amount: String,
    pub watch_id: Option<Uuid>,
    pub watch_registered_at: Option<DateTime<Utc>>,
    pub engine_job_id: Option<Uuid>,
    pub engine_submitted_at: Option<DateTime<Utc>>,
    pub tx_hash: Option<String>,
    pub block_number: Option<i64>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    pub event_seq: i64,
    pub detected_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub expired_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Columns selected for every `Deposit` read.
pub const DEPOSIT_COLUMNS: &str = "id, user_id, chain_id, token_symbol, token_address, token_decimals, amount::text AS amount, \
     receiver, recovery, salt, expires_at, payment_address, reference, webhook_url, status, \
     confirmed_amount::text AS confirmed_amount, watch_id, watch_registered_at, engine_job_id, engine_submitted_at, \
     tx_hash, block_number, failure_code, failure_message, event_seq, detected_at, settled_at, failed_at, expired_at, \
     created_at, updated_at";

impl Deposit {
    pub fn payment_address(&self) -> Address {
        self.payment_address.parse().expect("stored addresses are valid")
    }

    pub fn terms(&self) -> PaymentTerms {
        PaymentTerms {
            token: self.token_address.parse().expect("stored addresses are valid"),
            amount: U256::from_str_radix(&self.amount, 10).expect("stored amounts are valid"),
            receiver: self.receiver.parse().expect("stored addresses are valid"),
            expiration_timestamp: self.expires_at.timestamp() as u64,
            recovery: self.recovery.parse().expect("stored addresses are valid"),
            salt: self.salt.parse::<B256>().expect("stored salts are valid"),
            chain_id: self.chain_id as u64,
        }
    }

    pub fn view(&self) -> DepositView {
        DepositView {
            id: self.id,
            status: self.status,
            payment_address: self.payment_address.clone(),
            chain_id: self.chain_id as u64,
            token: self.token_symbol.clone(),
            token_address: self.token_address.clone(),
            token_decimals: self.token_decimals as u8,
            amount: self.amount.clone(),
            confirmed_amount: self.confirmed_amount.clone(),
            receiver: self.receiver.clone(),
            recovery: self.recovery.clone(),
            salt: self.salt.clone(),
            reference: self.reference.clone(),
            webhook_url: self.webhook_url.clone(),
            expires_at: self.expires_at,
            tx_hash: self.tx_hash.clone(),
            block_number: self.block_number.map(|n| n as u64),
            failure: self
                .failure_code
                .as_ref()
                .map(|code| Failure { code: code.clone(), message: self.failure_message.clone().unwrap_or_default() }),
            timestamps: Timestamps {
                created_at: self.created_at,
                updated_at: self.updated_at,
                detected_at: self.detected_at,
                settled_at: self.settled_at,
                failed_at: self.failed_at,
                expired_at: self.expired_at,
            },
            events: None,
        }
    }
}

/// The wire representation of a deposit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositView {
    pub id: Uuid,
    pub status: DepositStatus,
    pub payment_address: String,
    pub chain_id: u64,
    pub token: String,
    pub token_address: String,
    pub token_decimals: u8,
    /// Base units, decimal string.
    pub amount: String,
    pub confirmed_amount: String,
    pub receiver: String,
    pub recovery: String,
    pub salt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    pub expires_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    pub timestamps: Timestamps,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<DepositEventView>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timestamps {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub detected_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub expired_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, FromRow)]
pub struct DepositEvent {
    pub id: Uuid,
    pub deposit_id: Uuid,
    pub sequence: i64,
    #[sqlx(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositEventView {
    pub id: Uuid,
    pub sequence: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl From<DepositEvent> for DepositEventView {
    fn from(e: DepositEvent) -> Self {
        Self { id: e.id, sequence: e.sequence, event_type: e.event_type, data: e.data, created_at: e.created_at }
    }
}

/// Event types recorded on the deposit timeline. Those marked "app" are also delivered to the
/// app's webhook.
pub mod events {
    pub const CREATED: &str = "deposit.created";
    pub const WATCH_REGISTERED: &str = "deposit.watch_registered";
    /// The indexer no longer knew our watch; a new one was registered.
    pub const WATCH_LOST: &str = "deposit.watch_lost";
    /// A state change recovered by polling instead of a webhook.
    pub const RECONCILED: &str = "deposit.reconciled";
    /// app: a transfer to the payment address was seen at chain head.
    pub const DETECTED: &str = "deposit.detected";
    /// app: a transfer was confirmed; `confirmed_amount` is the new total.
    pub const PAYMENT_CONFIRMED: &str = "deposit.payment_confirmed";
    /// app: a previously detected transfer was reorged out and never counted.
    pub const PAYMENT_ORPHANED: &str = "deposit.payment_orphaned";
    /// app: the confirmed total reached the amount; settlement was submitted.
    pub const READY: &str = "deposit.ready";
    pub const SETTLEMENT_SUBMITTED: &str = "deposit.settlement_submitted";
    pub const SETTLEMENT_INCLUDED: &str = "deposit.settlement_included";
    pub const SETTLEMENT_RETRIED: &str = "deposit.settlement_retried";
    /// app: the receiver was paid.
    pub const SETTLED: &str = "deposit.settled";
    /// app: settlement failed; see `failure`.
    pub const FAILED: &str = "deposit.failed";
    /// app: the deposit expired before the amount was paid.
    pub const EXPIRED: &str = "deposit.expired";

    pub fn is_app_facing(event_type: &str) -> bool {
        matches!(event_type, DETECTED | PAYMENT_CONFIRMED | PAYMENT_ORPHANED | READY | SETTLED | FAILED | EXPIRED)
    }
}
