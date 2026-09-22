//! `POST /v1/deposit` input validation. Every check fails fast with a specific message; nothing is
//! coerced silently.

use alloy_primitives::{Address, B256, U256};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::chain::Registry;
use crate::chain::payment::PaymentTerms;
use crate::config::{PaymentsConfig, WebhooksConfig};
use crate::error::ApiError;
use crate::webhooks::target;

/// Unknown fields are rejected so a typo (`ammount`) cannot silently produce a wrong address.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDepositRequest {
    /// Chain id (`8453`, as number or string) or slug (`"base"`).
    pub chain_id: serde_json::Value,
    /// Token symbol (`"USDC"`) or contract address; must be supported on the chain.
    pub token: String,
    /// Base-unit integer as a decimal string (`"2500000"` = 2.5 USDC). Never a number, never a decimal.
    pub amount: serde_json::Value,
    /// Where the funds go: the app's treasury, the user's in-app account, anything.
    pub receiver: String,
    /// RFC 3339. After this the payment no longer settles and any funds go to recovery.
    pub expires_at: DateTime<Utc>,
    /// Optional `bytes32` the app associates with this deposit.
    #[serde(default)]
    pub reference: Option<String>,
    /// Optional per-deposit webhook. Falls back to the account's default webhook.
    #[serde(default)]
    pub webhook_url: Option<String>,
}

/// A validated request, ready to be stored.
#[derive(Debug, Clone)]
pub struct NewDeposit {
    pub chain_id: u64,
    pub token_symbol: String,
    pub token_address: Address,
    pub token_decimals: u8,
    pub amount: U256,
    pub receiver: Address,
    pub recovery: Address,
    pub salt: B256,
    pub expires_at: DateTime<Utc>,
    pub payment_address: Address,
    pub reference: Option<B256>,
    pub webhook_url: Option<String>,
}

pub struct Validator<'a> {
    pub registry: &'a Registry,
    pub payments: &'a PaymentsConfig,
    pub webhooks: &'a WebhooksConfig,
    pub factory: Address,
    pub recovery: Address,
}

impl Validator<'_> {
    pub fn validate(&self, req: &CreateDepositRequest, now: DateTime<Utc>) -> Result<NewDeposit, ApiError> {
        let chain_raw = match &req.chain_id {
            serde_json::Value::String(s) => s.trim().to_owned(),
            serde_json::Value::Number(n) if n.is_u64() => n.to_string(),
            _ => return Err(ApiError::invalid("chain_id must be a chain id or slug")),
        };
        let chain = self.registry.chain(&chain_raw).ok_or_else(|| {
            let known: Vec<String> = self.registry.chains().map(|c| format!("{} ({})", c.name, c.chain_id)).collect();
            ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "unsupported_chain",
                format!("unsupported chain {chain_raw:?}; supported: {}", known.join(", ")),
            )
        })?;
        let token = chain.token(&req.token).ok_or_else(|| {
            let known: Vec<&str> = chain.tokens.iter().map(|t| t.symbol.as_str()).collect();
            ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "unsupported_token",
                format!("token {:?} is not supported on {}; supported: {}", req.token, chain.name, known.join(", ")),
            )
        })?;

        let amount = parse_amount(&req.amount)?;
        let receiver = parse_address("receiver", &req.receiver)?;
        if receiver == self.recovery {
            return Err(ApiError::invalid("receiver must not be the recovery address"));
        }
        if receiver == token.address || receiver == self.factory {
            return Err(ApiError::invalid("receiver must not be a contract of the payment system"));
        }

        let lead = req.expires_at.signed_duration_since(now);
        if lead.num_seconds() < self.payments.min_expiry_lead_secs {
            return Err(ApiError::invalid(format!(
                "expires_at must be at least {} seconds in the future",
                self.payments.min_expiry_lead_secs
            )));
        }
        if lead.num_seconds() > self.payments.max_expiry_secs {
            return Err(ApiError::invalid(format!(
                "expires_at must be at most {} seconds in the future",
                self.payments.max_expiry_secs
            )));
        }
        // The contract takes whole seconds; keep the stored value identical to what is hashed.
        let expires_at = DateTime::<Utc>::from_timestamp(req.expires_at.timestamp(), 0).expect("in range");

        let reference = match req.reference.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => {
                Some(raw.parse::<B256>().map_err(|_| ApiError::invalid("reference must be a 0x-prefixed bytes32"))?)
            }
        };

        let webhook_url = match req.webhook_url.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => Some(
                target::validate(raw, self.webhooks.allow_insecure_targets)
                    .map_err(|m| ApiError::invalid(format!("webhook_url: {m}")))?
                    .to_string(),
            ),
        };

        let salt = random_salt();
        let terms = PaymentTerms {
            token: token.address,
            amount,
            receiver,
            expiration_timestamp: expires_at.timestamp() as u64,
            recovery: self.recovery,
            salt,
            chain_id: chain.chain_id,
        };
        Ok(NewDeposit {
            chain_id: chain.chain_id,
            token_symbol: token.symbol.clone(),
            token_address: token.address,
            token_decimals: token.decimals,
            amount,
            receiver,
            recovery: self.recovery,
            salt,
            expires_at,
            payment_address: terms.payment_address(self.factory),
            reference,
            webhook_url,
        })
    }
}

/// Amounts must be whole-number decimal strings so no client can lose precision through floats.
pub fn parse_amount(v: &serde_json::Value) -> Result<U256, ApiError> {
    let text = match v {
        serde_json::Value::String(s) => s.trim(),
        serde_json::Value::Number(_) => {
            return Err(ApiError::invalid("amount must be a string of base units, not a JSON number"));
        }
        _ => return Err(ApiError::invalid("amount must be a string of base units")),
    };
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::invalid(
            "amount must contain only decimal digits (base units, no sign, no decimal point)",
        ));
    }
    if text.len() > 78 {
        return Err(ApiError::invalid("amount does not fit uint256"));
    }
    let amount = U256::from_str_radix(text, 10).map_err(|_| ApiError::invalid("amount does not fit uint256"))?;
    if amount.is_zero() {
        return Err(ApiError::invalid("amount must be greater than zero"));
    }
    Ok(amount)
}

pub fn parse_address(field: &str, raw: &str) -> Result<Address, ApiError> {
    let raw = raw.trim();
    let address: Address = raw.parse().map_err(|_| ApiError::invalid(format!("{field} is not a valid EVM address")))?;
    if address.is_zero() {
        return Err(ApiError::invalid(format!("{field} must not be the zero address")));
    }
    // A mixed-case address must carry a valid EIP-55 checksum; all-lower/upper is accepted as-is.
    let hex = raw.trim_start_matches("0x");
    let mixed = hex.bytes().any(|b| b.is_ascii_lowercase()) && hex.bytes().any(|b| b.is_ascii_uppercase());
    if mixed && address.to_checksum(None) != raw {
        return Err(ApiError::invalid(format!("{field} has an invalid EIP-55 checksum")));
    }
    Ok(address)
}

fn random_salt() -> B256 {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    B256::from(bytes)
}

/// Canonical hash of a request body for idempotency comparison. Field order and whitespace do
/// not matter; values do.
pub fn request_fingerprint(req: &CreateDepositRequest) -> Vec<u8> {
    let canonical = serde_json::json!({
        "chain_id": req.chain_id.to_string(),
        "token": req.token.trim().to_ascii_lowercase(),
        "amount": req.amount.to_string(),
        "receiver": req.receiver.trim().to_ascii_lowercase(),
        "expires_at": req.expires_at.timestamp(),
        "reference": req.reference.as_deref().map(str::trim).map(str::to_ascii_lowercase),
        "webhook_url": req.webhook_url.as_deref().map(str::trim),
    });
    Sha256::digest(canonical.to_string().as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChainConfig, TokenConfig};
    use alloy_primitives::address;
    use chrono::Duration;
    use std::collections::BTreeMap;

    fn registry() -> Registry {
        let mut chains = BTreeMap::new();
        chains.insert(
            "base".to_owned(),
            ChainConfig {
                chain_id: 8453,
                tokens: vec![TokenConfig {
                    symbol: "USDC".into(),
                    address: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
                    decimals: 6,
                }],
            },
        );
        Registry::from_config(&chains)
    }

    fn payments() -> PaymentsConfig {
        PaymentsConfig {
            factory_address: String::new(),
            recovery_address: String::new(),
            min_expiry_lead_secs: 300,
            max_expiry_secs: 30 * 86400,
        }
    }

    fn webhooks() -> WebhooksConfig {
        WebhooksConfig {
            inbound_tolerance_secs: 300,
            allow_insecure_targets: false,
            connect_timeout_ms: 1,
            request_timeout_ms: 1,
            retry_base_ms: 1,
            retry_cap_ms: 1,
            max_age_secs: 1,
        }
    }

    fn req(json: serde_json::Value) -> CreateDepositRequest {
        serde_json::from_value(json).unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn base_req() -> serde_json::Value {
        serde_json::json!({
            "chain_id": "base",
            "token": "USDC",
            "amount": "2500000",
            "receiver": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
            "expires_at": (now() + Duration::hours(1)).to_rfc3339(),
        })
    }

    fn validate(json: serde_json::Value) -> Result<NewDeposit, ApiError> {
        let registry = registry();
        let payments = payments();
        let webhooks = webhooks();
        let v = Validator {
            registry: &registry,
            payments: &payments,
            webhooks: &webhooks,
            factory: address!("5FbDB2315678afecb367f032d93F642f64180aa3"),
            recovery: address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
        };
        v.validate(&req(json), now())
    }

    #[test]
    fn happy_path_derives_address_and_normalises() {
        let d = validate(base_req()).unwrap();
        assert_eq!(d.chain_id, 8453);
        assert_eq!(d.token_symbol, "USDC");
        assert_eq!(d.amount, U256::from(2_500_000u64));
        assert!(!d.payment_address.is_zero());
        assert_ne!(validate(base_req()).unwrap().salt, d.salt, "each deposit gets its own salt");
        let by_id = validate({
            let mut r = base_req();
            r["chain_id"] = serde_json::json!(8453);
            r["token"] = serde_json::json!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
            r
        })
        .unwrap();
        assert_eq!(by_id.token_address, d.token_address);
    }

    #[test]
    fn amounts_must_be_whole_number_strings() {
        for bad in [
            serde_json::json!(2500000),
            serde_json::json!("2.5"),
            serde_json::json!("0"),
            serde_json::json!(""),
            serde_json::json!("-1"),
            serde_json::json!("+1"),
            serde_json::json!("1e6"),
            serde_json::json!("0x10"),
            serde_json::json!("115792089237316195423570985008687907853269984665640564039457584007913129639936"),
        ] {
            let mut r = base_req();
            r["amount"] = bad.clone();
            assert!(validate(r).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn addresses_chains_tokens_and_expiry_are_checked() {
        let with = |k: &str, v: serde_json::Value| {
            let mut r = base_req();
            r[k] = v;
            r
        };
        assert_eq!(
            validate(with("receiver", "0x0000000000000000000000000000000000000000".into())).unwrap_err().code,
            "invalid_request"
        );
        assert!(
            validate(with("receiver", "0x70997970c51812dc3a010c7d01b50e0d17dc79C8".into())).is_err(),
            "bad checksum"
        );
        assert!(
            validate(with("receiver", "0x70997970c51812dc3a010c7d01b50e0d17dc79c8".into())).is_ok(),
            "all lowercase ok"
        );
        assert!(validate(with("receiver", "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC".into())).is_err(), "recovery");
        assert_eq!(validate(with("chain_id", "ethereum".into())).unwrap_err().code, "unsupported_chain");
        assert_eq!(validate(with("chain_id", 1.into())).unwrap_err().code, "unsupported_chain");
        assert_eq!(validate(with("token", "DAI".into())).unwrap_err().code, "unsupported_token");
        assert!(validate(with("expires_at", (now() + Duration::seconds(299)).to_rfc3339().into())).is_err());
        assert!(validate(with("expires_at", (now() - Duration::hours(1)).to_rfc3339().into())).is_err());
        assert!(validate(with("expires_at", (now() + Duration::days(31)).to_rfc3339().into())).is_err());
        assert!(validate(with("reference", "0x1234".into())).is_err());
        assert!(validate(with("reference", format!("0x{}", "ab".repeat(32)).into())).is_ok());
        assert!(validate(with("webhook_url", "http://localhost/x".into())).is_err());
        assert!(validate(with("webhook_url", "https://app.example.com/hooks".into())).is_ok());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut r = base_req();
        r["ammount"] = "1".into();
        assert!(serde_json::from_value::<CreateDepositRequest>(r).is_err());
    }

    #[test]
    fn fingerprint_ignores_case_and_formatting_but_not_values() {
        let a = request_fingerprint(&req(base_req()));
        let mut lower = base_req();
        lower["receiver"] = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8".into();
        assert_eq!(a, request_fingerprint(&req(lower)));
        let mut other = base_req();
        other["amount"] = "2500001".into();
        assert_ne!(a, request_fingerprint(&req(other)));
    }
}
