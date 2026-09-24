//! Layered configuration: `config/default.toml` → `config/<GUM_PROFILE>.toml` → `GUM_<SECTION>__<KEY>`
//! environment overrides, plus `PORT` and `DATABASE_URL` as Railway provides them.

use std::collections::BTreeMap;
use std::path::Path;

use alloy_primitives::Address;
use anyhow::{Context, bail};
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub privy: PrivyConfig,
    pub api_keys: ApiKeysConfig,
    pub payments: PaymentsConfig,
    pub indexer: IndexerConfig,
    pub engine: EngineConfig,
    pub webhooks: WebhooksConfig,
    pub outbox: OutboxConfig,
    pub reconciler: ReconcilerConfig,
    pub admin: AdminConfig,
    #[serde(default)]
    pub chains: BTreeMap<String, ChainConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    pub callback_base_url: String,
    pub request_timeout_ms: u64,
    pub max_body_bytes: usize,
    pub max_in_flight: usize,
    pub shutdown_grace_secs: u64,
    /// Browser origins allowed to call the API (the web UI). Empty disables CORS entirely.
    #[serde(default)]
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    #[serde(default)]
    pub url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_ms: u64,
    pub auto_migrate: bool,
    /// Session limits on every pooled connection (0 disables one). See `db`.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,
    #[serde(default = "default_lock_timeout_ms")]
    pub lock_timeout_ms: u64,
    #[serde(default = "default_idle_in_transaction_timeout_ms")]
    pub idle_in_transaction_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrivyConfig {
    pub app_id: String,
    pub verification_key: String,
    pub clock_skew_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiKeysConfig {
    pub cache_ttl_secs: u64,
    pub rate_limit_per_second: u32,
    pub rate_limit_burst: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaymentsConfig {
    pub factory_address: String,
    pub recovery_address: String,
    pub min_expiry_lead_secs: i64,
    pub max_expiry_secs: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexerConfig {
    pub base_url: String,
    pub webhook_secret: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EngineConfig {
    pub base_url: String,
    pub webhook_secret: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhooksConfig {
    pub inbound_tolerance_secs: i64,
    pub allow_insecure_targets: bool,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub retry_base_ms: u64,
    pub retry_cap_ms: u64,
    pub max_age_secs: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OutboxConfig {
    pub workers: usize,
    pub poll_interval_ms: u64,
    pub batch_size: i64,
    pub lock_ttl_secs: i64,
    /// Backoff cap for jobs towards gum-indexer / gum-engine. These never give up.
    pub upstream_retry_cap_ms: u64,
    /// Longest a single job may run before it is abandoned and retried. Below `lock_ttl_secs`, so
    /// an abandoned job is never still running when another worker picks it up.
    #[serde(default = "default_job_timeout_ms")]
    pub job_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReconcilerConfig {
    pub interval_secs: u64,
    pub paid_stale_secs: i64,
    /// Minimum gap between two polls of the same `paid` deposit, and after any webhook for it.
    #[serde(default = "default_paid_repoll_secs")]
    pub paid_repoll_secs: i64,
    pub open_stale_secs: i64,
    pub idempotency_ttl_secs: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdminConfig {
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainConfig {
    pub chain_id: u64,
    /// Offered for new deposits (listed on `/v1/chains`, accepted by `POST /v1/deposit`). A new chain
    /// ships disabled and is turned on with `GUM_CHAINS__<NAME>__ENABLED=true` once the `PaymentFactory`
    /// generation is deployed on it and gum-indexer and gum-engine serve it. Deposits already created on
    /// a disabled chain carry on as usual.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenConfig {
    pub symbol: String,
    pub address: Address,
    pub decimals: u8,
}

impl Config {
    /// Loads configuration from `config_dir`, honouring `GUM_PROFILE`, `GUM_*`, `PORT` and `DATABASE_URL`.
    pub fn load(config_dir: &Path) -> anyhow::Result<Self> {
        let config = Self::load_unchecked(config_dir)?;
        config.validate()?;
        Ok(config)
    }

    /// [`Config::load`] without [`Config::validate`]; for tests that fill in the blanks themselves.
    pub fn load_unchecked(config_dir: &Path) -> anyhow::Result<Self> {
        let mut figment = Figment::from(Toml::file(config_dir.join("default.toml")));
        if let Ok(profile) = std::env::var("GUM_PROFILE") {
            let path = config_dir.join(format!("{profile}.toml"));
            if !path.exists() {
                bail!("GUM_PROFILE={profile} but {} does not exist", path.display());
            }
            figment = figment.merge(Toml::file(path));
        }
        figment = figment.merge(Env::prefixed("GUM_").split("__"));
        if let Ok(port) = std::env::var("PORT") {
            let port: u16 = port.parse().context("PORT is not a valid port")?;
            figment = figment.merge(Serialized::default("server.port", port));
        }
        if let Ok(url) = std::env::var("DATABASE_URL") {
            figment = figment.merge(Serialized::default("database.url", url));
        }
        let mut config: Config = figment.extract().context("invalid configuration")?;
        // Railway and most dashboards store multi-line secrets with literal "\n".
        config.privy.verification_key = config.privy.verification_key.replace("\\n", "\n");
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.database.url.is_empty() {
            bail!("DATABASE_URL is required");
        }
        if !self.chains.values().any(|c| c.enabled) {
            bail!("at least one chain must be enabled");
        }
        let mut chain_ids = std::collections::BTreeMap::new();
        for (name, chain) in &self.chains {
            if let Some(other) = chain_ids.insert(chain.chain_id, name) {
                bail!("chains {other} and {name} share chain_id {}", chain.chain_id);
            }
            if chain.tokens.is_empty() {
                bail!("chain {name} has no tokens");
            }
            let mut symbols = std::collections::BTreeSet::new();
            let mut addresses = std::collections::BTreeSet::new();
            for t in &chain.tokens {
                if !symbols.insert(t.symbol.to_ascii_uppercase()) || !addresses.insert(t.address) {
                    bail!("chain {name}: duplicate token {}", t.symbol);
                }
            }
        }
        self.factory_address().context("payments.factory_address")?;
        self.recovery_address().context("payments.recovery_address")?;
        if self.outbox.workers == 0 {
            bail!("outbox.workers must be at least 1");
        }
        if self.outbox.job_timeout_ms == 0
            || self.outbox.job_timeout_ms >= self.outbox.lock_ttl_secs.max(0) as u64 * 1000
        {
            bail!("outbox.job_timeout_ms must be above 0 and below outbox.lock_ttl_secs");
        }
        Ok(())
    }

    pub fn factory_address(&self) -> anyhow::Result<Address> {
        parse_nonzero_address(&self.payments.factory_address)
    }

    pub fn recovery_address(&self) -> anyhow::Result<Address> {
        parse_nonzero_address(&self.payments.recovery_address)
    }

    /// Warnings for settings that are legal in development but will not work in production.
    pub fn production_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.server.callback_base_url.is_empty() {
            out.push("server.callback_base_url is empty: gum-indexer / gum-engine cannot call back".into());
        }
        if self.privy.app_id.is_empty() || self.privy.verification_key.is_empty() {
            out.push(
                "privy.app_id / privy.verification_key are empty: Privy-authenticated routes will refuse everything"
                    .into(),
            );
        }
        if self.indexer.base_url.is_empty() {
            out.push("indexer.base_url is empty: watches cannot be registered".into());
        }
        if self.engine.base_url.is_empty() {
            out.push("engine.base_url is empty: settlements cannot be submitted".into());
        }
        if self.indexer.webhook_secret.is_empty() || self.engine.webhook_secret.is_empty() {
            out.push(
                "indexer.webhook_secret / engine.webhook_secret are empty: inbound webhooks will be rejected".into(),
            );
        }
        if self.webhooks.allow_insecure_targets {
            out.push("webhooks.allow_insecure_targets is on: app webhooks may target private hosts".into());
        }
        out
    }
}

fn parse_nonzero_address(raw: &str) -> anyhow::Result<Address> {
    let address: Address = raw.trim().parse().context("not a valid EVM address")?;
    if address.is_zero() {
        bail!("must not be the zero address");
    }
    Ok(address)
}

fn default_true() -> bool {
    true
}

fn default_paid_repoll_secs() -> i64 {
    30
}

fn default_statement_timeout_ms() -> u64 {
    30_000
}

fn default_lock_timeout_ms() -> u64 {
    5_000
}

fn default_idle_in_transaction_timeout_ms() -> u64 {
    60_000
}

fn default_job_timeout_ms() -> u64 {
    30_000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `config/default.toml` plus the secrets a deploy provides, plus `overrides` (as the
    /// environment would set them: `GUM_CHAINS__ARC__ENABLED=true` is `[chains.arc] enabled = true`).
    fn default_with(overrides: &str) -> anyhow::Result<Config> {
        let config: Config = Figment::from(Toml::string(include_str!("../config/default.toml")))
            .merge(Toml::string(
                "[database]\nurl = \"postgres://x\"\n[payments]\n\
                 factory_address = \"0x6D85B9706D8f076cB8A9fEA37d70Fcb8D22C2952\"\n\
                 recovery_address = \"0x8093fef21c5d153456a7a39b3b09e69abc01a961\"",
            ))
            .merge(Toml::string(overrides))
            .extract()?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn arc_ships_disabled_and_mirrors_the_indexer_once_enabled() {
        let config = default_with("").unwrap();
        assert!(!config.chains["arc"].enabled, "no PaymentFactory on Arc yet; enabled by variable");
        assert!(["monad", "arbitrum", "base"].iter().all(|c| config.chains[*c].enabled));

        let config = default_with("[chains.arc]\nenabled = true").unwrap();
        let arc = &config.chains["arc"];
        assert!(arc.enabled);
        assert_eq!(arc.chain_id, 5042);
        // gum-indexer's watches and webhooks use the ERC-20 and 6 decimals (its transfer_log_* keys
        // are internal to it), and the settlement call targets the same ERC-20.
        let usdc = &arc.tokens[..];
        assert_eq!(usdc.len(), 1);
        assert_eq!(
            (usdc[0].symbol.as_str(), usdc[0].address, usdc[0].decimals),
            ("USDC", "0x3600000000000000000000000000000000000000".parse().unwrap(), 6)
        );
    }

    #[test]
    fn chains_are_validated() {
        let err = |overrides: &str| default_with(overrides).unwrap_err().to_string();
        assert!(
            err("[chains.monad]\nenabled = false\n[chains.arbitrum]\nenabled = false\n[chains.base]\nenabled = false")
                .contains("at least one chain must be enabled")
        );
        assert!(err("[chains.arc]\nchain_id = 8453").contains("share chain_id 8453"));
        let mut config = default_with("").unwrap();
        let tokens = &mut config.chains.get_mut("base").unwrap().tokens;
        tokens.push(TokenConfig { symbol: "usdc".into(), address: Address::repeat_byte(1), decimals: 6 });
        assert!(config.validate().unwrap_err().to_string().contains("chain base: duplicate token usdc"));
        // Without its table, an ENABLED variable alone is a chain with no chain_id: a boot error.
        assert!(err("[chains.celo]\nenabled = true").contains("chain_id"));
    }
}
