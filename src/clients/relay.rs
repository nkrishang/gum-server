//! Relay (relay.link): cross-chain and same-chain routing into a deposit's exact token and amount.
//!
//! Only the payer routes (`deposit::route`) use it, to let a payer pay with any token on any chain
//! Relay supports. Every call carries our `x-api-key`, which never leaves this service. The chain
//! list is cached for `relay.chains_ttl_secs`, token searches and prices for a minute: the pay page
//! asks for them on every visit, and Relay's budget per key is shared by every payer.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::Address;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use moka::sync::Cache;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use super::{UpstreamError, base_url, record};
use crate::config::RelayConfig;

const SERVICE: &str = "relay";

pub struct RelayClient {
    http: reqwest::Client,
    base_url: Option<String>,
    api_key: String,
    chains_ttl: Duration,
    /// Single-flight: one fetch refreshes the list while the other callers wait for it.
    chains: Mutex<ChainsCache>,
    searches: Cache<String, Arc<Vec<RelayCurrency>>>,
    prices: Cache<String, Option<f64>>,
    statuses: Cache<String, IntentStatus>,
    /// Token logos by `<chain id>:<address>` (None: Relay has none). Logos don't change.
    logos: Cache<String, Option<String>>,
    /// Every call but quotes (which have their own budget in `deposit::relay`) shares Relay's
    /// per-key limit for "other" endpoints; this keeps the replica under it.
    reads: RateLimiter<NotKeyed, InMemoryState, DefaultClock>,
}

#[derive(Default)]
struct ChainsCache {
    fetched: Option<(Instant, Arc<Vec<RelayChain>>)>,
    /// A failed refresh is not retried for `CHAINS_BACKOFF`: callers get the stale list (or the
    /// error) at once instead of each waiting out their own timeout behind the lock.
    failed_at: Option<Instant>,
}

const CHAINS_BACKOFF: Duration = Duration::from_secs(30);
/// Payers' pages poll a route's status; one answer serves them all for this long.
const STATUS_TTL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Wire types. Only the fields we use; Relay adds fields freely, so every one is defaulted.
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayChain {
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub vm_type: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub deposit_enabled: bool,
    #[serde(default)]
    pub http_rpc_url: String,
    #[serde(default)]
    pub explorer_url: String,
    #[serde(default)]
    pub icon_url: Option<String>,
    #[serde(default)]
    pub currency: Option<RelayChainCurrency>,
    #[serde(default)]
    pub featured_tokens: Vec<RelayChainCurrency>,
    #[serde(default)]
    pub erc20_currencies: Vec<RelayChainCurrency>,
    /// What Relay's solver takes on this chain as it is. Anything else is swapped into one of
    /// these first, a two-step route this service doesn't offer (see `deposit::relay_tokens`).
    #[serde(default)]
    pub solver_currencies: Vec<RelayChainCurrency>,
    #[serde(default)]
    pub contracts: Option<RelayContracts>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub protocol: Option<RelayProtocol>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayChainCurrency {
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub name: String,
    pub address: String,
    pub decimals: u8,
    #[serde(default)]
    pub metadata: Option<RelayMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayMetadata {
    #[serde(default, rename = "logoURI")]
    pub logo_uri: Option<String>,
    #[serde(default)]
    pub verified: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayContracts {
    #[serde(default)]
    pub multicall3: Option<String>,
    #[serde(default)]
    pub relay_receiver: Option<String>,
    #[serde(default)]
    pub erc20_router: Option<String>,
    #[serde(default)]
    pub approval_proxy: Option<String>,
    #[serde(default)]
    pub v3: Option<RelayRouterContracts>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRouterContracts {
    #[serde(default)]
    pub erc20_router: Option<String>,
    #[serde(default)]
    pub approval_proxy: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RelayProtocol {
    #[serde(default)]
    pub v2: Option<RelayProtocolV2>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RelayProtocolV2 {
    #[serde(default)]
    pub depository: Option<String>,
    /// How Relay's signed orders name this chain (`worldchain` for `world-chain`, `manta` for
    /// `manta-pacific`), which is not always its `name`.
    #[serde(default, rename = "chainId")]
    pub chain_id: Option<String>,
}

/// Relay's contracts on one chain, by role. The roles matter: a public router runs anyone's calls,
/// so an allowance or a transfer of the payer's tokens that ends at one is there for anyone to
/// sweep — only the depository may hold funds, and only the approval proxy may pull on an allowance.
#[derive(Debug, Clone, Default)]
pub struct RelayRoles {
    /// Takes deposits (`depositErc20`/`depositNative`) and holds them for solver-signed orders.
    pub depositories: Vec<Address>,
    /// Pulls the payer's allowance (`transferAndMulticall`) and runs the route's calls.
    pub approval_proxies: Vec<Address>,
    /// Forwards the transaction's value to Relay's solver (`forward`).
    pub receivers: Vec<Address>,
    /// Runs a route's nested calls (`multicall`). Never a destination for an allowance or a
    /// standalone transfer.
    pub routers: Vec<Address>,
}

impl RelayRoles {
    /// Is this one of Relay's contracts at all?
    pub fn contains(&self, a: &Address) -> bool {
        self.depositories.contains(a)
            || self.approval_proxies.contains(a)
            || self.receivers.contains(a)
            || self.routers.contains(a)
    }

    /// A spender a wallet or router may approve: whoever is trusted to draw an allowance down only
    /// as part of taking a deposit.
    pub fn may_hold_allowance(&self, a: &Address) -> bool {
        self.depositories.contains(a) || self.approval_proxies.contains(a)
    }
}

impl RelayChain {
    /// Relay's contracts on this chain, by role: where a route's origin transactions may send
    /// funds or grant allowances, and for what.
    pub fn relay_roles(&self) -> RelayRoles {
        let k = self.contracts.as_ref();
        let v3 = k.and_then(|k| k.v3.as_ref());
        let parse = |raw: Option<&str>| -> Option<Address> {
            raw.and_then(|a| a.parse::<Address>().ok().filter(|a| !a.is_zero()))
        };
        let mut roles = RelayRoles {
            depositories: [parse(
                self.protocol.as_ref().and_then(|p| p.v2.as_ref()).and_then(|v2| v2.depository.as_deref()),
            )]
            .into_iter()
            .flatten()
            .collect(),
            approval_proxies: [
                parse(k.and_then(|k| k.approval_proxy.as_deref())),
                parse(v3.and_then(|v| v.approval_proxy.as_deref())),
            ]
            .into_iter()
            .flatten()
            .collect(),
            receivers: [parse(k.and_then(|k| k.relay_receiver.as_deref()))].into_iter().flatten().collect(),
            routers: [
                parse(k.and_then(|k| k.erc20_router.as_deref())),
                parse(v3.and_then(|v| v.erc20_router.as_deref())),
            ]
            .into_iter()
            .flatten()
            .collect(),
        };
        for list in [&mut roles.depositories, &mut roles.approval_proxies, &mut roles.receivers, &mut roles.routers] {
            list.sort();
            list.dedup();
        }
        roles
    }
}

/// An entry of `POST /currencies/v2`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayCurrency {
    pub chain_id: u64,
    pub address: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub name: String,
    pub decimals: u8,
    #[serde(default)]
    pub vm_type: String,
    #[serde(default)]
    pub metadata: Option<RelayMetadata>,
}

/// `POST /quote/v2`. Addresses as `0x` hex; `amount` in base units.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    pub user: String,
    pub recipient: String,
    pub refund_to: String,
    pub refund_on_origin: bool,
    pub origin_chain_id: u64,
    pub origin_currency: String,
    pub destination_chain_id: u64,
    pub destination_currency: String,
    pub amount: String,
    pub trade_type: &'static str,
    pub referrer: String,
    /// Swap surplus goes to the solver, not the recipient, so the payment address gets exactly
    /// `amount`.
    pub enable_true_exact_output: bool,
    /// Same-chain swaps are self-executed through Relay's router by default and can overshoot;
    /// solver-filled, they deliver exactly `amount`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub force_solver_execution: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub steps: Vec<QuoteStep>,
    #[serde(default)]
    pub fees: Option<QuoteFees>,
    pub details: QuoteDetails,
    #[serde(default)]
    pub protocol: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteStep {
    pub id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub description: String,
    pub kind: String,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub items: Vec<QuoteStepItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuoteStepItem {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub check: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteFees {
    #[serde(default)]
    pub gas: Option<QuoteAmount>,
    #[serde(default)]
    pub relayer: Option<QuoteAmount>,
    #[serde(default)]
    pub app: Option<QuoteAmount>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteDetails {
    #[serde(default)]
    pub operation: String,
    #[serde(default)]
    pub sender: String,
    #[serde(default)]
    pub recipient: String,
    pub currency_in: QuoteAmount,
    pub currency_out: QuoteAmount,
    #[serde(default)]
    pub total_impact: Option<Impact>,
    #[serde(default)]
    pub time_estimate: Option<f64>,
    #[serde(default)]
    pub rate: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteAmount {
    pub currency: QuoteCurrency,
    pub amount: String,
    #[serde(default)]
    pub amount_formatted: Option<String>,
    #[serde(default)]
    pub amount_usd: Option<String>,
    #[serde(default)]
    pub minimum_amount: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteCurrency {
    pub chain_id: u64,
    pub address: String,
    #[serde(default)]
    pub symbol: String,
    #[serde(default)]
    pub name: String,
    pub decimals: u8,
    #[serde(default)]
    pub metadata: Option<RelayMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Impact {
    #[serde(default)]
    pub usd: Option<String>,
    #[serde(default)]
    pub percent: Option<String>,
}

/// `GET /intents/status/v3`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentStatus {
    pub status: String,
    #[serde(default)]
    pub in_tx_hashes: Vec<String>,
    #[serde(default)]
    pub tx_hashes: Vec<String>,
    #[serde(default)]
    pub origin_chain_id: Option<u64>,
    #[serde(default)]
    pub destination_chain_id: Option<u64>,
    #[serde(default)]
    pub fail_reason: Option<String>,
    #[serde(default)]
    pub refund_fail_reason: Option<String>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// Relay's error body: `{"message", "errorCode"?}`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    error_code: Option<String>,
}

impl RelayClient {
    pub fn new(cfg: &RelayConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .connect_timeout(Duration::from_millis(cfg.timeout_ms.min(3000)))
            .user_agent("gum-server/1")
            .build()?;
        // No key, no routes: keyless calls share an IP budget with everyone else, and `referrer`
        // is refused without one.
        let base_url = if cfg.api_key.trim().is_empty() { None } else { base_url(&cfg.base_url) };
        Ok(Self {
            http,
            base_url,
            api_key: cfg.api_key.trim().to_owned(),
            chains_ttl: Duration::from_secs(cfg.chains_ttl_secs),
            chains: Mutex::new(ChainsCache::default()),
            searches: Cache::builder().max_capacity(2_000).time_to_live(Duration::from_secs(60)).build(),
            prices: Cache::builder().max_capacity(5_000).time_to_live(Duration::from_secs(60)).build(),
            statuses: Cache::builder().max_capacity(10_000).time_to_live(STATUS_TTL).build(),
            logos: Cache::builder().max_capacity(20_000).time_to_live(Duration::from_secs(6 * 3600)).build(),
            reads: RateLimiter::direct(Quota::per_minute(
                NonZeroU32::new(cfg.reads_per_minute.max(1)).expect("at least 1"),
            )),
        })
    }

    pub fn is_configured(&self) -> bool {
        self.base_url.is_some()
    }

    fn url(&self, path: &str) -> Result<String, UpstreamError> {
        let base = self.base_url.as_ref().ok_or(UpstreamError::NotConfigured { service: SERVICE })?;
        Ok(format!("{base}{path}"))
    }

    fn url_with(&self, path: &str, query: &[(&str, &str)]) -> Result<url::Url, UpstreamError> {
        let raw = self.url(path)?;
        url::Url::parse_with_params(&raw, query)
            .map_err(|e| UpstreamError::Malformed { service: SERVICE, detail: format!("url {raw}: {e}") })
    }

    /// Relay's chains, cached. A failed refresh serves the stale list rather than nothing.
    pub async fn chains(&self) -> Result<Arc<Vec<RelayChain>>, UpstreamError> {
        let mut cache = self.chains.lock().await;
        let stale = cache.fetched.as_ref().map(|(_, chains)| chains.clone());
        if let Some((at, chains)) = cache.fetched.as_ref()
            && at.elapsed() < self.chains_ttl
        {
            return Ok(chains.clone());
        }
        if cache.failed_at.is_some_and(|at| at.elapsed() < CHAINS_BACKOFF) {
            return stale.ok_or_else(|| UpstreamError::Unavailable {
                service: SERVICE,
                status: 503,
                code: "backoff".into(),
                message: "relay chains unavailable; retrying shortly".into(),
            });
        }
        #[derive(Deserialize)]
        struct Chains {
            chains: Vec<RelayChain>,
        }
        match self.send::<Chains>("chains", self.http.get(self.url("/chains")?)).await {
            Ok(fresh) => {
                let chains = Arc::new(fresh.chains);
                *cache = ChainsCache { fetched: Some((Instant::now(), chains.clone())), failed_at: None };
                Ok(chains)
            }
            Err(err) => {
                cache.failed_at = Some(Instant::now());
                match stale {
                    Some(stale) => {
                        tracing::warn!(error = %err, "relay chains refresh failed; serving the cached list");
                        Ok(stale)
                    }
                    None => Err(err),
                }
            }
        }
    }

    /// Verified tokens matching `term` (symbol, name or address) on `chain_ids`.
    pub async fn search_currencies(
        &self,
        chain_ids: &[u64],
        term: &str,
        limit: u32,
    ) -> Result<Arc<Vec<RelayCurrency>>, UpstreamError> {
        let key = format!("{chain_ids:?}|{}|{limit}", term.to_ascii_lowercase());
        if let Some(hit) = self.searches.get(&key) {
            return Ok(hit);
        }
        let mut body = serde_json::json!({ "chainIds": chain_ids, "verified": true, "limit": limit });
        if term.is_empty() {
            body["defaultList"] = Value::Bool(true);
        } else {
            body["term"] = Value::String(term.to_owned());
        }
        let found: Vec<RelayCurrency> =
            self.send("currencies", self.http.post(self.url("/currencies/v2")?).json(&body)).await?;
        let found = Arc::new(found);
        self.searches.insert(key, found.clone());
        Ok(found)
    }

    /// Logos for `keys` (`<chain id>:<address>`, lowercase), looked up in batches through
    /// `/currencies/v2`'s exact `tokens` filter and cached. Best effort: a failed batch is left out
    /// (and asked again next time).
    pub async fn logos(&self, keys: &[String]) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        let mut missing = Vec::new();
        for key in keys {
            match self.logos.get(key) {
                Some(Some(url)) => {
                    out.insert(key.clone(), url);
                }
                Some(None) => {}
                None => missing.push(key.clone()),
            }
        }
        for batch in missing.chunks(50) {
            let body = serde_json::json!({ "tokens": batch, "limit": batch.len() });
            let Ok(url) = self.url("/currencies/v2") else { break };
            let found: Vec<RelayCurrency> = match self.send("logos", self.http.post(url).json(&body)).await {
                Ok(found) => found,
                Err(err) => {
                    tracing::info!(error = %err, "relay token logos unavailable");
                    continue;
                }
            };
            for key in batch {
                let logo = found
                    .iter()
                    .find(|c| key == &format!("{}:{}", c.chain_id, c.address.to_ascii_lowercase()))
                    .and_then(|c| c.metadata.as_ref())
                    .and_then(|m| m.logo_uri.clone());
                if let Some(url) = &logo {
                    out.insert(key.clone(), url.clone());
                }
                self.logos.insert(key.clone(), logo);
            }
        }
        out
    }

    /// The token's USD price, or `None` when Relay has none. Cached per token for a minute.
    pub async fn price(&self, chain_id: u64, address: &str) -> Result<Option<f64>, UpstreamError> {
        let key = format!("{chain_id}:{}", address.to_ascii_lowercase());
        if let Some(hit) = self.prices.get(&key) {
            return Ok(hit);
        }
        #[derive(Deserialize)]
        struct Price {
            #[serde(default)]
            price: Option<f64>,
        }
        let request = self.http.get(
            self.url_with("/currencies/token/price", &[("address", address), ("chainId", &chain_id.to_string())])?,
        );
        let price = match self.send::<Price>("price", request).await {
            Ok(p) => p.price.filter(|p| p.is_finite() && *p > 0.0),
            // Relay answers an unpriced token with an error; that's "no price", not an outage.
            Err(UpstreamError::Rejected { .. }) => None,
            Err(err) => return Err(err),
        };
        self.prices.insert(key, price);
        Ok(price)
    }

    pub async fn quote(&self, req: &QuoteRequest) -> Result<Quote, UpstreamError> {
        self.send("quote", self.http.post(self.url("/quote/v2")?).json(req)).await
    }

    pub async fn status(&self, request_id: &str) -> Result<IntentStatus, UpstreamError> {
        let key = request_id.to_ascii_lowercase();
        if let Some(hit) = self.statuses.get(&key) {
            return Ok(hit);
        }
        let request = self.http.get(self.url_with("/intents/status/v3", &[("requestId", request_id)])?);
        let status: IntentStatus = self.send("status", request).await?;
        self.statuses.insert(key, status.clone());
        Ok(status)
    }

    /// Tells Relay about an origin transaction so it indexes it now rather than on its own scan.
    /// Only an optimisation: the fill happens either way.
    pub async fn index_transaction(&self, tx_hash: &str, chain_id: u64) -> Result<(), UpstreamError> {
        let body = serde_json::json!({ "txHash": tx_hash, "chainId": chain_id.to_string() });
        self.send::<Value>("index", self.http.post(self.url("/transactions/index")?).json(&body)).await.map(|_| ())
    }

    async fn send<T: DeserializeOwned>(
        &self,
        op: &'static str,
        request: reqwest::RequestBuilder,
    ) -> Result<T, UpstreamError> {
        if op != "quote" && self.reads.check().is_err() {
            record(SERVICE, op, Instant::now(), "shed");
            return Err(UpstreamError::Unavailable {
                service: SERVICE,
                status: 429,
                code: "rate_limited".into(),
                message: "relay read budget exhausted on this replica".into(),
            });
        }
        let started = Instant::now();
        let response = request.header("x-api-key", &self.api_key).send().await.map_err(|source| {
            record(SERVICE, op, started, "transport");
            UpstreamError::Transport { service: SERVICE, source }
        })?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let text = response.text().await.unwrap_or_default();
            let (code, message) = match serde_json::from_str::<RelayErrorBody>(&text) {
                Ok(body) => (body.error_code.unwrap_or_else(|| "http_error".into()), body.message),
                Err(_) => ("http_error".to_owned(), text.chars().take(512).collect()),
            };
            let err = if status == 429 || status >= 500 {
                UpstreamError::Unavailable { service: SERVICE, status, code, message }
            } else {
                UpstreamError::Rejected { service: SERVICE, status, code, message }
            };
            record(SERVICE, op, started, if err.is_retryable() { "unavailable" } else { "rejected" });
            return Err(err);
        }
        let body = response
            .json::<T>()
            .await
            .map_err(|e| UpstreamError::Malformed { service: SERVICE, detail: e.to_string() });
        record(SERVICE, op, started, if body.is_ok() { "ok" } else { "malformed" });
        body
    }
}
