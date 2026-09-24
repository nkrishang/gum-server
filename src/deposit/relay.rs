//! Paying a deposit with any token on any chain, through Relay (relay.link).
//!
//! The payer's side of the hosted pay page, next to `GET /v1/pay/{id}` and just as public: the
//! deposit id is the capability. The page picks what to pay with; this service asks Relay for a
//! route whose destination it pins itself, from the deposit: its chain, its token, the payment
//! address as the recipient, and `EXACT_OUTPUT` for no more than what is still owed. Relay's
//! answer is checked against those terms before the page sees it, so a route that would pay
//! anyone else, in anything else, or less, never reaches a wallet. Refunds go to the payer.
//!
//! ```text
//! GET  /v1/pay/{id}/sources                      chains and tokens to pay from
//! GET  /v1/pay/{id}/sources/tokens?q=&chain_id=  token search
//! POST /v1/pay/{id}/prices                       USD prices, for the payer's balances
//! POST /v1/pay/{id}/quote                        a checked route and the transactions to sign
//! GET  /v1/pay/{id}/routes/{request_id}          where the route is
//! POST /v1/pay/{id}/routes/{request_id}/transactions   "I sent the origin transaction"
//! ```
//!
//! The deposit itself is unaware of Relay: the fill is an ordinary transfer of the deposit's token
//! to the payment address, which gum-indexer sees and gum-engine settles like any other.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::{Address, U256};
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{Deposit, DepositStatus, store};
use crate::clients::UpstreamError;
use crate::clients::relay::{Quote, QuoteAmount, QuoteRequest, RelayChain, RelayChainCurrency, RelayMetadata};
use crate::config::RelayConfig;
use crate::error::ApiError;
use crate::state::AppState;

const NATIVE: &str = "0x0000000000000000000000000000000000000000";
/// Chains whose native currency is the same balance as one of their ERC-20s (Arc's USDC: 18
/// decimals native, 6 through the ERC-20). Listing both would show the payer the same money twice;
/// the ERC-20 is listed, and the native balance still pays for gas.
const NATIVE_ALIASES: &[(u64, &str)] = &[(5042, "0x3600000000000000000000000000000000000000")];
const MAX_PRICED_TOKENS: usize = 40;
const MAX_SEARCH_LEN: usize = 64;

// ---------------------------------------------------------------------------------------------
// Quote budget
// ---------------------------------------------------------------------------------------------

type Global = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;
type PerDeposit = RateLimiter<Uuid, DefaultKeyedStateStore<Uuid>, DefaultClock>;

/// Relay allows a key a fixed number of quotes a minute, shared by every payer on every replica's
/// share of it. A page re-quotes as its payer changes their mind, so one deposit gets a slice and
/// the whole replica a ceiling below Relay's, and a burst answers `429` here, not at Relay. Token
/// searches, prices and route statuses get their own per-deposit slice (the replica-wide ceiling
/// for those lives in the client, which only counts calls that miss its caches).
pub struct RouteLimits {
    quotes: Global,
    quotes_per_deposit: PerDeposit,
    reads_per_deposit: PerDeposit,
    checks: AtomicU64,
}

impl RouteLimits {
    pub fn new(cfg: &RelayConfig) -> Self {
        let per_min = |n: u32| Quota::per_minute(NonZeroU32::new(n.max(1)).expect("at least 1"));
        Self {
            quotes: RateLimiter::direct(per_min(cfg.quotes_per_minute)),
            quotes_per_deposit: RateLimiter::keyed(per_min(cfg.quotes_per_deposit_per_minute)),
            reads_per_deposit: RateLimiter::keyed(per_min(cfg.reads_per_deposit_per_minute)),
            checks: AtomicU64::new(0),
        }
    }

    fn quote(&self, deposit: Uuid) -> Result<(), ApiError> {
        self.forget_idle();
        if self.quotes_per_deposit.check_key(&deposit).is_err() {
            return Err(rate_limited("too many quotes for this payment; wait a few seconds"));
        }
        if self.quotes.check().is_err() {
            metrics::counter!("gum_relay_quotes_total", "outcome" => "shed").increment(1);
            return Err(rate_limited("quotes are busy; try again in a few seconds"));
        }
        Ok(())
    }

    fn read(&self, deposit: Uuid) -> Result<(), ApiError> {
        self.forget_idle();
        self.reads_per_deposit
            .check_key(&deposit)
            .map_err(|_| rate_limited("too many requests for this payment; wait a few seconds"))
    }

    /// The keyed limiters remember every deposit they've seen; now and then, drop the ones whose
    /// buckets have refilled.
    fn forget_idle(&self) {
        if self.checks.fetch_add(1, Ordering::Relaxed) % 1024 == 1023 {
            self.quotes_per_deposit.retain_recent();
            self.reads_per_deposit.retain_recent();
        }
    }
}

fn rate_limited(message: &str) -> ApiError {
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", message)
}

// ---------------------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Sources {
    /// Whether the page can offer other tokens and chains for this deposit at all.
    pub available: bool,
    /// Why not, when not: `disabled` (no Relay key), `destination_unsupported`, `closed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub chains: Vec<SourceChain>,
}

#[derive(Debug, Serialize)]
pub struct SourceChain {
    pub id: u64,
    pub name: String,
    pub icon_url: Option<String>,
    pub explorer_url: String,
    /// Relay's public RPC, for the page's balance reads.
    pub rpc_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multicall3: Option<String>,
    pub native: SourceToken,
    /// The ERC-20 that is the same balance as the native currency, when there is one (Arc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_alias: Option<String>,
    pub tokens: Vec<SourceToken>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceToken {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
}

impl SourceToken {
    fn of(c: &RelayChainCurrency) -> Self {
        Self {
            address: c.address.to_ascii_lowercase(),
            symbol: c.symbol.clone(),
            name: c.name.clone(),
            decimals: c.decimals,
            logo_uri: logo(&c.metadata),
        }
    }
}

fn logo(metadata: &Option<RelayMetadata>) -> Option<String> {
    metadata.as_ref().and_then(|m| m.logo_uri.clone()).filter(|u| u.starts_with("https://"))
}

/// An EVM chain Relay routes from (and, with `deposit_enabled`, to).
fn usable(c: &RelayChain) -> bool {
    c.vm_type == "evm" && !c.disabled && c.http_rpc_url.starts_with("https://")
}

fn can_receive(chains: &[RelayChain], chain_id: u64) -> bool {
    chains.iter().any(|c| c.id == chain_id && c.vm_type == "evm" && !c.disabled && c.deposit_enabled)
}

fn source_chain(c: &RelayChain, deposit: &Deposit) -> SourceChain {
    let native_alias = NATIVE_ALIASES.iter().find(|(id, _)| *id == c.id).map(|(_, token)| (*token).to_owned());
    let native = c.currency.as_ref().map(SourceToken::of).unwrap_or_else(|| SourceToken {
        address: NATIVE.into(),
        symbol: "ETH".into(),
        name: "Ether".into(),
        decimals: 18,
        logo_uri: None,
    });
    let mut seen = BTreeSet::from([NATIVE.to_owned()]);
    let mut tokens: Vec<SourceToken> = c
        .featured_tokens
        .iter()
        .chain(&c.erc20_currencies)
        .filter(|t| seen.insert(t.address.to_ascii_lowercase()))
        .map(SourceToken::of)
        .collect();
    // The deposit's own token is always listed on its chain, whatever Relay features.
    if c.id as i64 == deposit.chain_id && seen.insert(deposit.token_address.clone()) {
        tokens.insert(
            0,
            SourceToken {
                address: deposit.token_address.clone(),
                symbol: deposit.token_symbol.clone(),
                name: deposit.token_symbol.clone(),
                decimals: deposit.token_decimals as u8,
                logo_uri: None,
            },
        );
    }
    SourceChain {
        id: c.id,
        name: if c.display_name.is_empty() { c.name.clone() } else { c.display_name.clone() },
        icon_url: c.icon_url.clone().filter(|u| u.starts_with("https://")),
        explorer_url: c.explorer_url.trim_end_matches('/').to_owned(),
        rpc_url: c.http_rpc_url.clone(),
        multicall3: c
            .contracts
            .as_ref()
            .and_then(|k| k.multicall3.clone())
            .filter(|a| a.parse::<Address>().is_ok_and(|a| !a.is_zero())),
        native,
        native_alias,
        tokens,
    }
}

async fn open_deposit(state: &AppState, id: &str) -> Result<Deposit, ApiError> {
    let id: Uuid = id.parse().map_err(|_| super::pay::no_such_deposit())?;
    store::get(&state.pool, id).await?.ok_or_else(super::pay::no_such_deposit)
}

fn accepts_payment(d: &Deposit) -> bool {
    matches!(d.status, DepositStatus::Pending | DepositStatus::PartialPaid)
}

/// Cached by the browser for a minute: Relay's chain list moves slowly.
pub async fn sources(State(state): State<AppState>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let deposit = open_deposit(&state, &id).await?;
    let unavailable = |reason| Sources { available: false, reason: Some(reason), chains: vec![] };
    let body = if !state.relay.is_configured() {
        unavailable("disabled")
    } else if !accepts_payment(&deposit) {
        unavailable("closed")
    } else {
        let chains = state.relay.chains().await.map_err(relay_error)?;
        if !can_receive(&chains, deposit.chain_id as u64) {
            unavailable("destination_unsupported")
        } else {
            let mut list: Vec<SourceChain> =
                chains.iter().filter(|c| usable(c)).map(|c| source_chain(c, &deposit)).collect();
            // The deposit's chain first; the rest as Relay orders them.
            list.sort_by_key(|c| c.id as i64 != deposit.chain_id);
            Sources { available: true, reason: None, chains: list }
        }
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=60"));
    Ok(response)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenSearch {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub chain_id: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct FoundToken {
    pub chain_id: u64,
    #[serde(flatten)]
    pub token: SourceToken,
}

/// Relay's verified tokens matching `q` (a symbol, name or address), on one chain or all.
pub async fn search_tokens(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<TokenSearch>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|rej| ApiError::invalid(rej.body_text()))?;
    let deposit = open_deposit(&state, &id).await?;
    state.relay_limits.read(deposit.id)?;
    let term = query.q.trim();
    if term.len() > MAX_SEARCH_LEN {
        return Err(ApiError::invalid(format!("q must be at most {MAX_SEARCH_LEN} characters")));
    }
    let chains = state.relay.chains().await.map_err(relay_error)?;
    let usable_ids: Vec<u64> = chains.iter().filter(|c| usable(c)).map(|c| c.id).collect();
    let chain_ids = match query.chain_id {
        Some(id) if usable_ids.contains(&id) => vec![id],
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_chain",
                "Relay does not route from this chain",
            ));
        }
        None => usable_ids.clone(),
    };
    let found = state.relay.search_currencies(&chain_ids, term, 30).await.map_err(relay_error)?;
    let tokens: Vec<FoundToken> = found
        .iter()
        .filter(|c| c.vm_type.is_empty() || c.vm_type == "evm")
        .filter(|c| usable_ids.contains(&c.chain_id) && c.address.parse::<Address>().is_ok())
        .map(|c| FoundToken {
            chain_id: c.chain_id,
            token: SourceToken {
                address: c.address.to_ascii_lowercase(),
                symbol: c.symbol.clone(),
                name: c.name.clone(),
                decimals: c.decimals,
                logo_uri: logo(&c.metadata),
            },
        })
        .collect();
    Ok(Json(serde_json::json!({ "tokens": tokens })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricesRequest {
    /// `"<chain_id>:<address>"`, as Relay spells a token.
    pub tokens: Vec<String>,
}

/// USD prices for the payer's balances, so the page can say what each is worth and whether it
/// covers the payment before asking for a quote. `null` where Relay has no price.
pub async fn prices(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<PricesRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(req) = body?;
    let deposit = open_deposit(&state, &id).await?;
    state.relay_limits.read(deposit.id)?;
    if req.tokens.len() > MAX_PRICED_TOKENS {
        return Err(ApiError::invalid(format!("at most {MAX_PRICED_TOKENS} tokens")));
    }
    let mut wanted = BTreeMap::new();
    for raw in &req.tokens {
        let parsed = raw
            .split_once(':')
            .and_then(|(chain, address)| Some((chain.parse::<u64>().ok()?, address.parse::<Address>().ok()?)));
        let Some((chain_id, address)) = parsed else {
            return Err(ApiError::invalid(format!("{raw} is not <chain_id>:<address>")));
        };
        wanted.insert(format!("{chain_id}:{address:#x}"), (chain_id, format!("{address:#x}")));
    }
    let mut tasks = tokio::task::JoinSet::new();
    for (key, (chain_id, address)) in wanted {
        let state = state.clone();
        tasks.spawn(async move { (key, state.relay.price(chain_id, &address).await) });
    }
    let mut prices = serde_json::Map::new();
    while let Some(joined) = tasks.join_next().await {
        let Ok((key, price)) = joined else { continue };
        // One token Relay can't price is `null`; an outage is too, for the tokens it hit.
        let price = price.ok().flatten();
        prices.insert(key, price.map_or(Value::Null, |p| serde_json::json!(p)));
    }
    Ok(Json(serde_json::json!({ "prices": prices })))
}

// ---------------------------------------------------------------------------------------------
// Quote
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteBody {
    /// The payer's wallet: sends on the origin chain and gets any refund there.
    pub user: String,
    pub origin_chain_id: u64,
    /// ERC-20 address, or the zero address for the chain's native currency.
    pub origin_currency: String,
    /// Base units of the deposit's token to deliver. At most what is still owed.
    pub amount: String,
}

#[derive(Debug, Serialize)]
pub struct RouteQuote {
    pub request_id: String,
    pub quoted_at: String,
    /// What the payer sends.
    pub origin: RouteAmount,
    /// What the payment address receives: exactly `amount` of the deposit's token.
    pub destination: RouteAmount,
    pub fees: RouteFees,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_estimate_secs: Option<f64>,
    /// The origin-chain transactions to send, in order (an approval, then the deposit).
    pub steps: Vec<RouteStep>,
}

#[derive(Debug, Serialize)]
pub struct RouteAmount {
    pub chain_id: u64,
    pub currency: SourceToken,
    pub amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount_usd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteFees {
    /// What the route costs on top of the amount (Relay's fees and any swap impact), USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_usd: Option<String>,
    /// Origin gas the payer's wallet pays, in the origin chain's native currency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas: Option<RouteAmount>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteStep {
    /// Relay's step id: `approve`, `deposit`, `swap`, `send`.
    pub id: String,
    pub description: String,
    pub chain_id: u64,
    pub to: String,
    pub data: String,
    /// Wei, decimal string.
    pub value: String,
}

/// What a route must do, from the deposit and the payer's choice. Anything Relay answers is held
/// against it.
#[derive(Debug, Clone)]
pub struct Expected {
    pub user: Address,
    pub origin_chain_id: u64,
    pub origin_currency: Address,
    pub destination_chain_id: u64,
    pub token: Address,
    pub recipient: Address,
    pub amount: U256,
    /// Relay's contracts on the origin chain (from `/chains`): the only addresses a route's
    /// transactions may call, approve or transfer to, besides the origin token itself.
    pub relay_contracts: Vec<Address>,
}

pub async fn quote(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<QuoteBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = body?;
    if !state.relay.is_configured() {
        return Err(ApiError::unavailable("relay_disabled", "paying with other tokens is not available"));
    }
    let deposit = open_deposit(&state, &id).await?;
    if !accepts_payment(&deposit) {
        return Err(ApiError::conflict("closed", "this payment is no longer accepting funds"));
    }
    let left = (deposit.expires_at - Utc::now()).num_seconds();
    if left < state.config.relay.min_time_left_secs {
        return Err(ApiError::conflict(
            "closing",
            "this payment closes too soon for a route to land; pay in the requested token instead",
        ));
    }
    let mut expected = expected(&deposit, &body)?;
    if expected.origin_chain_id == expected.destination_chain_id && expected.origin_currency == expected.token {
        return Err(ApiError::invalid("that is the requested token itself: send it with a plain transfer"));
    }
    let chains = state.relay.chains().await.map_err(relay_error)?;
    let Some(origin) = chains.iter().find(|c| c.id == expected.origin_chain_id && usable(c)) else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_chain",
            "Relay does not route from this chain",
        ));
    };
    expected.relay_contracts = origin.relay_contracts();
    if !can_receive(&chains, expected.destination_chain_id) {
        return Err(ApiError::conflict("route_unavailable", "Relay does not route to this payment's chain right now"));
    }
    state.relay_limits.quote(deposit.id)?;

    let request = QuoteRequest {
        user: format!("{:#x}", expected.user),
        recipient: format!("{:#x}", expected.recipient),
        refund_to: format!("{:#x}", expected.user),
        refund_on_origin: true,
        origin_chain_id: expected.origin_chain_id,
        origin_currency: format!("{:#x}", expected.origin_currency),
        destination_chain_id: expected.destination_chain_id,
        destination_currency: format!("{:#x}", expected.token),
        amount: expected.amount.to_string(),
        trade_type: "EXACT_OUTPUT",
        referrer: format!("{}|{}", state.config.relay.referrer, deposit.id),
        enable_true_exact_output: true,
        force_solver_execution: expected.origin_chain_id == expected.destination_chain_id,
    };
    let quote = match state.relay.quote(&request).await {
        Ok(q) => q,
        Err(err) => {
            let outcome = if matches!(err, UpstreamError::Rejected { .. }) { "no_route" } else { "unavailable" };
            metrics::counter!("gum_relay_quotes_total", "outcome" => outcome).increment(1);
            return Err(quote_error(err));
        }
    };
    let route = check_quote(&quote, &expected).map_err(|problem| {
        metrics::counter!("gum_relay_quotes_total", "outcome" => "rejected_by_check").increment(1);
        tracing::error!(deposit_id = %deposit.id, problem, "relay quote failed its checks; not shown to the payer");
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "route_rejected",
            "the route did not match this payment; try another token",
        )
    })?;
    metrics::counter!("gum_relay_quotes_total", "outcome" => "ok").increment(1);
    tracing::info!(
        deposit_id = %deposit.id, request_id = %route.request_id, origin_chain_id = expected.origin_chain_id,
        origin_currency = %request.origin_currency, amount = %request.amount, "relay route quoted"
    );
    let mut response = Json(route).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn expected(deposit: &Deposit, body: &QuoteBody) -> Result<Expected, ApiError> {
    let user: Address = body.user.trim().parse().map_err(|_| ApiError::invalid("user is not an address"))?;
    if user.is_zero() {
        return Err(ApiError::invalid("user must not be the zero address"));
    }
    let origin_currency: Address =
        body.origin_currency.trim().parse().map_err(|_| ApiError::invalid("origin_currency is not an address"))?;
    let amount = body.amount.trim();
    if amount.is_empty() || !amount.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::invalid("amount must be a whole number of base units"));
    }
    let amount = U256::from_str_radix(amount, 10).map_err(|_| ApiError::invalid("amount does not fit uint256"))?;
    let total = U256::from_str_radix(&deposit.amount, 10).unwrap_or_default();
    let confirmed = U256::from_str_radix(&deposit.confirmed_amount, 10).unwrap_or_default();
    let owed = total.saturating_sub(confirmed);
    if amount.is_zero() || amount > owed {
        return Err(ApiError::invalid(format!("amount must be between 1 and {owed}, what is still owed")));
    }
    Ok(Expected {
        user,
        origin_chain_id: body.origin_chain_id,
        origin_currency,
        destination_chain_id: deposit.chain_id as u64,
        token: deposit.token_address.parse().expect("stored addresses are valid"),
        recipient: deposit.payment_address(),
        amount,
        relay_contracts: Vec::new(),
    })
}

fn same_address(raw: &str, want: Address) -> bool {
    raw.parse::<Address>().is_ok_and(|a| a == want)
}

fn base_units(raw: Option<&str>) -> Option<U256> {
    raw.and_then(|s| U256::from_str_radix(s.trim(), 10).ok())
}

fn route_amount(a: &QuoteAmount) -> RouteAmount {
    RouteAmount {
        chain_id: a.currency.chain_id,
        currency: SourceToken {
            address: a.currency.address.to_ascii_lowercase(),
            symbol: a.currency.symbol.clone(),
            name: a.currency.name.clone(),
            decimals: a.currency.decimals,
            logo_uri: logo(&a.currency.metadata),
        },
        amount: a.amount.clone(),
        amount_usd: a.amount_usd.clone(),
    }
}

/// Holds Relay's quote to the terms. Returns what the page may execute, or why not.
///
/// The destination is the whole point: the payment address, the deposit's token on the deposit's
/// chain, at least `amount` guaranteed (`minimumAmount`, and every payment of the signed order,
/// which must be there). Refunds may only go to the payer, or to the payment address in the
/// deposit's token. And what the wallet signs matches: every step is a plain transaction on the
/// origin chain from the payer's wallet, to one of Relay's contracts there or to the payer's token
/// (then only an approval or a transfer to Relay's contracts, for no more than the quoted input),
/// with no value for a token and no more than the quoted input for the native currency.
pub fn check_quote(quote: &Quote, want: &Expected) -> Result<RouteQuote, String> {
    let request_id = quote.request_id.clone().unwrap_or_default();
    if !is_hash(&request_id) {
        return Err(format!("request id {request_id:?} is not a 32-byte hex id"));
    }
    let d = &quote.details;
    if !same_address(&d.recipient, want.recipient) {
        return Err(format!("recipient {} is not the payment address", d.recipient));
    }
    let out = &d.currency_out;
    if out.currency.chain_id != want.destination_chain_id || !same_address(&out.currency.address, want.token) {
        return Err(format!(
            "pays {} on chain {}, not the deposit's token",
            out.currency.address, out.currency.chain_id
        ));
    }
    let guaranteed = base_units(out.minimum_amount.as_deref()).or_else(|| base_units(Some(&out.amount)));
    if guaranteed.is_none_or(|g| g < want.amount) {
        return Err(format!("guarantees {:?}, less than {}", out.minimum_amount, want.amount));
    }
    let input = &d.currency_in;
    if input.currency.chain_id != want.origin_chain_id || !same_address(&input.currency.address, want.origin_currency) {
        return Err(format!(
            "takes {} on chain {}, not what the payer chose",
            input.currency.address, input.currency.chain_id
        ));
    }
    let Some(input_amount) = base_units(Some(&input.amount)) else {
        return Err(format!("input amount {:?} is not base units", input.amount));
    };
    // Every route we ask for is solver-filled, so it comes with the order the solver signs.
    let Some(order) = quote.protocol.as_ref().and_then(|p| p.pointer("/v2/orderData")) else {
        return Err("no signed order (protocol.v2.orderData) to check".into());
    };
    check_order(order, want)?;

    let mut steps = Vec::new();
    for step in &quote.steps {
        if step.kind != "transaction" {
            return Err(format!("step {} is a {} step; only transactions are supported", step.id, step.kind));
        }
        for item in &step.items {
            steps.push(check_step(&step.id, &step.description, &item.data, want, input_amount)?);
        }
    }
    if steps.is_empty() {
        return Err("no steps to execute".into());
    }
    // The value the wallet sends is the price on screen: nothing for a token, at most the quoted
    // input for the native currency.
    let mut value = U256::ZERO;
    for step in &steps {
        value = value.saturating_add(base_units(Some(&step.value)).unwrap_or(U256::MAX));
    }
    let limit = if want.origin_currency.is_zero() { input_amount } else { U256::ZERO };
    if value > limit {
        return Err(format!("the steps send {value} wei of value; the quote takes {limit}"));
    }

    let fees = quote.fees.as_ref();
    let route_usd = d
        .total_impact
        .as_ref()
        .and_then(|i| i.usd.as_deref())
        .and_then(|usd| usd.parse::<f64>().ok())
        .map(|usd| format!("{:.2}", (-usd).max(0.0)));
    Ok(RouteQuote {
        request_id: request_id.to_ascii_lowercase(),
        quoted_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        origin: route_amount(input),
        destination: RouteAmount { amount: want.amount.to_string(), ..route_amount(out) },
        fees: RouteFees {
            route_usd,
            gas: fees.and_then(|f| f.gas.as_ref()).filter(|g| g.amount != "0").map(route_amount),
        },
        time_estimate_secs: d.time_estimate.filter(|t| t.is_finite() && *t >= 0.0),
        steps,
    })
}

/// The signed order behind the route (`protocol.v2.orderData`), when Relay includes it.
fn check_order(order: &Value, want: &Expected) -> Result<(), String> {
    if let Some(payments) = order.pointer("/output/payments").and_then(Value::as_array) {
        let mut total = U256::ZERO;
        for p in payments {
            let recipient = p.get("recipient").and_then(Value::as_str).unwrap_or_default();
            let currency = p.get("currency").and_then(Value::as_str).unwrap_or_default();
            if !same_address(recipient, want.recipient) || !same_address(currency, want.token) {
                return Err(format!("the order pays {currency} to {recipient}"));
            }
            total += base_units(p.get("minimumAmount").and_then(Value::as_str)).unwrap_or_default();
        }
        if total < want.amount {
            return Err(format!("the order guarantees {total}, less than {}", want.amount));
        }
    }
    // Refunds go to the payer, or (Relay's fallback) to the payment address in the deposit's own
    // token, which is where it counts; never anywhere else.
    for input in order.get("inputs").and_then(Value::as_array).into_iter().flatten() {
        for refund in input.get("refunds").and_then(Value::as_array).into_iter().flatten() {
            let to = refund.get("recipient").and_then(Value::as_str).unwrap_or_default();
            let currency = refund.get("currency").and_then(Value::as_str).unwrap_or_default();
            let to_payer = same_address(to, want.user);
            let to_deposit = same_address(to, want.recipient) && same_address(currency, want.token);
            if !to_payer && !to_deposit {
                return Err(format!("the order refunds {currency} to {to}, neither the payer nor this payment"));
            }
        }
    }
    Ok(())
}

/// `approve(address,uint256)` and `transfer(address,uint256)`: the only calls a route may make on
/// the payer's token, and only towards Relay's contracts.
const APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
const TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

fn check_step(
    id: &str,
    description: &str,
    data: &Value,
    want: &Expected,
    input_amount: U256,
) -> Result<RouteStep, String> {
    let field = |name: &str| data.get(name).and_then(Value::as_str).unwrap_or_default();
    let chain_id = data.get("chainId").and_then(Value::as_u64).unwrap_or_default();
    if chain_id != want.origin_chain_id {
        return Err(format!("step {id} runs on chain {chain_id}, not the origin"));
    }
    if !same_address(field("from"), want.user) {
        return Err(format!("step {id} is sent from {}, not the payer", field("from")));
    }
    let to: Address = field("to").parse().map_err(|_| format!("step {id} has no valid target"))?;
    let calldata = field("data");
    if !(calldata.starts_with("0x") && calldata.len() % 2 == 0 && calldata[2..].bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(format!("step {id} has malformed calldata"));
    }
    let bytes = hex::decode(&calldata[2..]).map_err(|_| format!("step {id} has malformed calldata"))?;
    if !want.origin_currency.is_zero() && to == want.origin_currency {
        // A call on the payer's token: an allowance or a transfer to Relay, for no more than the
        // quoted input. (A deposit by transfer appends Relay's order id after the arguments.)
        let (selector, args) =
            bytes.split_at_checked(4).ok_or_else(|| format!("step {id} calls the token with no selector"))?;
        if (selector != APPROVE && selector != TRANSFER) || args.len() < 64 {
            return Err(format!("step {id} makes an unexpected call on the payer's token"));
        }
        let target = Address::from_slice(&args[12..32]);
        let amount = U256::from_be_slice(&args[32..64]);
        if !want.relay_contracts.contains(&target) {
            return Err(format!("step {id} approves or transfers to {target:#x}, which is not Relay's"));
        }
        if amount > input_amount {
            return Err(format!("step {id} approves or transfers {amount}, more than the quoted {input_amount}"));
        }
    } else if !want.relay_contracts.contains(&to) {
        return Err(format!("step {id} calls {to:#x}, which is not one of Relay's contracts on the origin chain"));
    }
    let value = match data.get("value") {
        None | Some(Value::Null) => "0".to_owned(),
        Some(Value::String(s)) if base_units(Some(s)).is_some() => s.trim().to_owned(),
        Some(other) => return Err(format!("step {id} has value {other}")),
    };
    Ok(RouteStep {
        id: id.to_owned(),
        description: description.to_owned(),
        chain_id,
        to: format!("{to:#x}"),
        data: calldata.to_ascii_lowercase(),
        value,
    })
}

fn is_hash(raw: &str) -> bool {
    raw.len() == 66 && raw.starts_with("0x") && raw[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn relay_error(err: UpstreamError) -> ApiError {
    match err {
        UpstreamError::NotConfigured { .. } => {
            ApiError::unavailable("relay_disabled", "paying with other tokens is not available")
        }
        other => {
            tracing::warn!(error = %other, "relay request failed");
            ApiError::unavailable("relay_unavailable", "routing is unavailable right now; try again shortly")
        }
    }
}

/// Relay's refusals, in words a payer can act on. Its own message is passed on for the ones that
/// are specific (amount limits), never for internal ones.
fn quote_error(err: UpstreamError) -> ApiError {
    let UpstreamError::Rejected { code, message, .. } = &err else { return relay_error(err) };
    let unprocessable =
        |code: &'static str, message: String| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, code, message);
    match code.as_str() {
        "AMOUNT_TOO_LOW" => unprocessable("amount_too_low", "this amount is too small to route from that token".into()),
        "AMOUNT_TOO_HIGH" | "INSUFFICIENT_LIQUIDITY" => unprocessable("insufficient_liquidity", message.clone()),
        "SWAP_IMPACT_TOO_HIGH" => {
            unprocessable("price_impact_too_high", "that token's price impact is too high for this amount".into())
        }
        "SANCTIONED_WALLET_ADDRESS" | "SANCTIONED_CURRENCY" => {
            unprocessable("blocked", "Relay can't route from this wallet or token".into())
        }
        "CHAIN_DISABLED" | "ROUTE_TEMPORARILY_RESTRICTED" => {
            unprocessable("route_unavailable", "this route is paused right now; try another token or chain".into())
        }
        _ => {
            tracing::info!(relay_code = %code, relay_message = %message, "relay found no route");
            unprocessable("no_route", "no route from that token right now; try another".into())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Route status
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct RouteStatus {
    pub request_id: String,
    /// Relay's status: `waiting`, `depositing`, `pending`, `submitted`, `success`, `failure`,
    /// `refund`, or `unknown`.
    pub status: String,
    /// Origin transactions.
    pub in_tx_hashes: Vec<String>,
    /// Destination transactions: the fill into the payment address.
    pub tx_hashes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

pub async fn route_status(
    State(state): State<AppState>,
    Path((id, request_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let deposit = open_deposit(&state, &id).await?;
    if !is_hash(&request_id) {
        return Err(ApiError::not_found("no such route"));
    }
    state.relay_limits.read(deposit.id)?;
    let s = state.relay.status(&request_id).await.map_err(relay_error)?;
    let body = RouteStatus {
        request_id: request_id.to_ascii_lowercase(),
        status: s.status,
        in_tx_hashes: s.in_tx_hashes,
        tx_hashes: s.tx_hashes,
        fail_reason: s.fail_reason.filter(|r| r != "N/A"),
        updated_at: s.updated_at.and_then(DateTime::from_timestamp_millis),
    };
    let mut response = Json(body).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentTransaction {
    pub tx_hash: String,
    pub chain_id: u64,
}

/// The page sent a route's origin transaction: Relay is told at once, so it fills without waiting
/// for its own scan. Best effort; `202` either way.
pub async fn route_transaction(
    State(state): State<AppState>,
    Path((id, request_id)): Path<(String, String)>,
    body: Result<Json<SentTransaction>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body?;
    let deposit = open_deposit(&state, &id).await?;
    if !is_hash(&request_id) {
        return Err(ApiError::not_found("no such route"));
    }
    if !is_hash(&body.tx_hash) {
        return Err(ApiError::invalid("tx_hash is not a transaction hash"));
    }
    state.relay_limits.read(deposit.id)?;
    if let Err(err) = state.relay.index_transaction(&body.tx_hash, body.chain_id).await {
        tracing::info!(error = %err, %request_id, "relay did not take the transaction hint");
    }
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PAYER: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
    const PAYMENT: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
    const USDC_BASE: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const USDC_ARB: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
    const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
    const REQUEST: &str = "0x1790248938255ff5c9db51e367a55380edb458572f010bc52415d45da935ffd5";
    const APPROVAL_PROXY: &str = "0xccc88a9d1b4ed6b0eaba998850414b24f1c315be";
    /// `approve(depository, 2526643)`, as Relay sends it.
    const APPROVE_DEPOSITORY: &str = "0x095ea7b30000000000000000000000004cd00e387622c35bddb9b4c962c136462338bc310000000000000000000000000000000000000000000000000000000000268db3";

    fn approve(spender: &str, amount: u64) -> String {
        format!("0x095ea7b3{:0>64}{amount:064x}", spender.trim_start_matches("0x"))
    }

    fn want() -> Expected {
        Expected {
            user: PAYER.parse().unwrap(),
            origin_chain_id: 42161,
            origin_currency: USDC_ARB.parse().unwrap(),
            destination_chain_id: 8453,
            token: USDC_BASE.parse().unwrap(),
            recipient: PAYMENT.parse().unwrap(),
            amount: U256::from(2_500_000u64),
            relay_contracts: vec![DEPOSITORY.parse().unwrap(), APPROVAL_PROXY.parse().unwrap()],
        }
    }

    fn currency(chain_id: u64, address: &str) -> Value {
        json!({ "chainId": chain_id, "address": address, "symbol": "USDC", "name": "USD Coin", "decimals": 6,
                "metadata": { "logoURI": "https://assets.relay.link/icons/currencies/usdc.png", "verified": true } })
    }

    /// Relay's USDC Arbitrum → USDC Base EXACT_OUTPUT quote (docs/relay-api.md §5.3), trimmed.
    fn relay_quote() -> Value {
        json!({
            "requestId": REQUEST,
            "steps": [
                { "id": "approve", "action": "Confirm transaction in your wallet", "description": "Sign an approval for USDC",
                  "kind": "transaction", "requestId": REQUEST,
                  "items": [{ "status": "incomplete", "data": {
                      "from": "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", "to": USDC_ARB,
                      "data": APPROVE_DEPOSITORY,
                      "value": "0", "chainId": 42161, "gas": "75439" } }] },
                { "id": "deposit", "action": "Confirm transaction in your wallet",
                  "description": "Depositing funds to the relayer to execute the swap for USDC", "kind": "transaction",
                  "items": [{ "status": "incomplete",
                      "data": { "from": "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", "to": DEPOSITORY,
                                "data": "0xe8017952aa", "value": "0", "chainId": 42161 },
                      "check": { "endpoint": format!("/intents/status/v3?requestId={REQUEST}"), "method": "GET" } }] }
            ],
            "fees": {
                "gas": { "currency": { "chainId": 42161, "address": NATIVE, "symbol": "ETH", "name": "Ether", "decimals": 18 },
                         "amount": "1748055157200", "amountUsd": "0.004628" },
                "relayer": { "currency": currency(42161, USDC_ARB), "amount": "26643", "amountUsd": "0.026640" }
            },
            "details": {
                "operation": "swap", "sender": PAYER, "recipient": "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf",
                "currencyIn": { "currency": currency(42161, USDC_ARB), "amount": "2526643", "amountUsd": "2.526325", "minimumAmount": "2526643" },
                "currencyOut": { "currency": currency(8453, USDC_BASE), "amount": "2500000", "amountUsd": "2.499685", "minimumAmount": "2500000" },
                "totalImpact": { "usd": "-0.026640", "percent": "-1.05" },
                "timeEstimate": 2, "rate": "0.98945"
            },
            "protocol": { "v2": { "orderData": {
                "inputs": [{ "payment": { "chainId": "arbitrum", "currency": USDC_ARB, "amount": "2526643" },
                             "refunds": [{ "chainId": "arbitrum", "recipient": PAYER, "currency": USDC_ARB },
                                         { "chainId": "base", "recipient": PAYMENT, "currency": USDC_BASE }] }],
                "output": { "chainId": "base", "payments": [
                    { "recipient": PAYMENT, "currency": USDC_BASE, "minimumAmount": "2500000", "expectedAmount": "2500000" }] }
            } } }
        })
    }

    fn check(quote: Value) -> Result<RouteQuote, String> {
        check_quote(&serde_json::from_value(quote).unwrap(), &want())
    }

    #[test]
    fn a_matching_route_becomes_the_steps_to_sign() {
        let route = check(relay_quote()).unwrap();
        assert_eq!(route.request_id, REQUEST);
        assert_eq!(route.origin.amount, "2526643");
        assert_eq!(route.origin.chain_id, 42161);
        assert_eq!(route.destination.amount, "2500000");
        assert_eq!(route.destination.currency.address, USDC_BASE);
        assert_eq!(route.fees.route_usd.as_deref(), Some("0.03"));
        assert_eq!(route.fees.gas.as_ref().map(|g| g.currency.symbol.as_str()), Some("ETH"));
        assert_eq!(route.time_estimate_secs, Some(2.0));
        let ids: Vec<_> = route.steps.iter().map(|s| (s.id.as_str(), s.to.as_str())).collect();
        assert_eq!(ids, [("approve", USDC_ARB), ("deposit", DEPOSITORY)]);
        assert!(route.steps.iter().all(|s| s.chain_id == 42161 && s.value == "0"));
    }

    #[test]
    fn routes_that_would_pay_anyone_else_anything_else_or_less_are_refused() {
        let refuse = |edit: &dyn Fn(&mut Value), why: &str| {
            let mut q = relay_quote();
            edit(&mut q);
            let err = check(q).unwrap_err();
            assert!(err.contains(why), "{err} (expected {why})");
        };
        let other = "0x0000000000000000000000000000000000000bad";
        refuse(&|q| q["details"]["recipient"] = json!(other), "not the payment address");
        refuse(&|q| q["details"]["currencyOut"]["currency"]["address"] = json!(other), "not the deposit's token");
        refuse(&|q| q["details"]["currencyOut"]["currency"]["chainId"] = json!(1), "not the deposit's token");
        refuse(&|q| q["details"]["currencyOut"]["minimumAmount"] = json!("2499999"), "less than 2500000");
        refuse(&|q| q["details"]["currencyIn"]["currency"]["chainId"] = json!(10), "not what the payer chose");
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["output"]["payments"][0]["recipient"] = json!(other),
            "the order pays",
        );
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["output"]["payments"][0]["minimumAmount"] = json!("1"),
            "the order guarantees 1",
        );
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["recipient"] = json!(other),
            "the order refunds",
        );
        refuse(&|q| q["steps"][1]["items"][0]["data"]["from"] = json!(other), "not the payer");
        refuse(&|q| q["steps"][1]["items"][0]["data"]["chainId"] = json!(8453), "not the origin");
        refuse(&|q| q["steps"][1]["items"][0]["data"]["data"] = json!("0xzz"), "malformed calldata");
        refuse(&|q| q["steps"][1]["items"][0]["data"]["value"] = json!("-1"), "has value");
        refuse(&|q| q["steps"][0]["kind"] = json!("signature"), "only transactions");
        refuse(&|q| q["steps"] = json!([]), "no steps");
        refuse(&|q| q["requestId"] = json!("0x12"), "request id");
        // What the wallet signs, not only what Relay says.
        refuse(&|q| q["steps"][1]["items"][0]["data"]["to"] = json!(other), "not one of Relay's contracts");
        refuse(
            &|q| q["steps"][0]["items"][0]["data"]["data"] = json!(approve(other, 2_526_643)),
            "which is not Relay's",
        );
        refuse(
            &|q| q["steps"][0]["items"][0]["data"]["data"] = json!(approve(DEPOSITORY, 2_526_644)),
            "more than the quoted",
        );
        refuse(
            &|q| q["steps"][0]["items"][0]["data"]["data"] = json!(format!("0x23b872dd{}", &APPROVE_DEPOSITORY[10..])),
            "unexpected call on the payer's token",
        );
        refuse(&|q| q["steps"][1]["items"][0]["data"]["value"] = json!("1"), "wei of value");
        refuse(&|q| q["protocol"] = json!(null), "no signed order");
        // A refund to the payment address only counts in the deposit's own token.
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][1]["currency"] = json!(USDC_ARB),
            "neither the payer nor this payment",
        );
    }

    #[test]
    fn a_native_route_sends_no_more_than_the_quoted_input() {
        let mut want = want();
        want.origin_currency = Address::ZERO;
        let mut q = relay_quote();
        q["details"]["currencyIn"]["currency"]["address"] = json!(NATIVE);
        q["details"]["currencyIn"]["amount"] = json!("955024944952040");
        q["steps"] = json!([q["steps"][1].clone()]);
        q["steps"][0]["items"][0]["data"]["value"] = json!("955024944952040");
        let quote: Quote = serde_json::from_value(q.clone()).unwrap();
        assert_eq!(check_quote(&quote, &want).unwrap().steps[0].value, "955024944952040");
        q["steps"][0]["items"][0]["data"]["value"] = json!("955024944952041");
        let quote: Quote = serde_json::from_value(q).unwrap();
        assert!(check_quote(&quote, &want).unwrap_err().contains("wei of value"));
    }

    #[test]
    fn relay_contracts_come_from_the_chain() {
        let chain: RelayChain = serde_json::from_value(json!({
            "id": 42161, "vmType": "evm",
            "contracts": { "multicall3": "0xca11bde05977b3631167028862be2a173976ca11", "relayReceiver": "",
                           "erc20Router": "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f", "approvalProxy": APPROVAL_PROXY,
                           "v3": { "erc20Router": "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f" } },
            "protocol": { "v2": { "chainId": "arbitrum", "depository": "0x4cD00E387622C35bDDB9b4c962C136462338BC31" } }
        }))
        .unwrap();
        let contracts = chain.relay_contracts();
        assert!(contracts.contains(&DEPOSITORY.parse().unwrap()));
        assert!(contracts.contains(&APPROVAL_PROXY.parse().unwrap()));
        assert!(
            !contracts.contains(&"0xca11bde05977b3631167028862be2a173976ca11".parse().unwrap()),
            "multicall3 is not Relay's"
        );
        assert_eq!(contracts.len(), 4);
    }

    #[test]
    fn a_missing_minimum_falls_back_to_the_amount() {
        let mut q = relay_quote();
        q["details"]["currencyOut"].as_object_mut().unwrap().remove("minimumAmount");
        assert!(check(q.clone()).is_ok());
        q["details"]["currencyOut"]["amount"] = json!("100");
        assert!(check(q).is_err());
    }

    #[test]
    fn arc_lists_its_usdc_once() {
        let chain: RelayChain = serde_json::from_value(json!({
            "id": 5042, "name": "arc", "displayName": "Arc", "vmType": "evm", "disabled": false, "depositEnabled": true,
            "httpRpcUrl": "https://rpc.mainnet.arc.io", "explorerUrl": "https://explorer.arc.io/",
            "currency": { "symbol": "USDC", "name": "USD Coin (Gas Currency)", "address": NATIVE, "decimals": 18 },
            "featuredTokens": [{ "symbol": "USDC", "name": "USD Coin", "address": "0x3600000000000000000000000000000000000000", "decimals": 6 }],
            "erc20Currencies": [{ "symbol": "USDC", "name": "USD Coin", "address": "0x3600000000000000000000000000000000000000", "decimals": 6 }],
            "contracts": { "multicall3": "" }
        }))
        .unwrap();
        let deposit = test_deposit(5042, "0x3600000000000000000000000000000000000000");
        let source = source_chain(&chain, &deposit);
        assert_eq!(source.native_alias.as_deref(), Some("0x3600000000000000000000000000000000000000"));
        assert_eq!(source.tokens.len(), 1);
        assert_eq!(source.multicall3, None);
        assert_eq!(source.explorer_url, "https://explorer.arc.io");
    }

    #[test]
    fn the_deposit_token_is_always_listed_on_its_chain() {
        let chain: RelayChain = serde_json::from_value(json!({
            "id": 143, "name": "monad", "displayName": "Monad", "vmType": "evm", "httpRpcUrl": "https://rpc3.monad.xyz",
            "currency": { "symbol": "MON", "name": "Monad", "address": NATIVE, "decimals": 18 },
            "featuredTokens": [{ "symbol": "USDC", "address": "0x754704bc059f8c67012fed69bc8a327a5aafb603", "decimals": 6 }]
        }))
        .unwrap();
        let source = source_chain(&chain, &test_deposit(143, "0x00000000efe302beaa2b3e6e1b18d08d69a9012a"));
        let symbols: Vec<_> = source.tokens.iter().map(|t| t.address.as_str()).collect();
        assert_eq!(
            symbols,
            ["0x00000000efe302beaa2b3e6e1b18d08d69a9012a", "0x754704bc059f8c67012fed69bc8a327a5aafb603"]
        );
    }

    fn test_deposit(chain_id: i64, token: &str) -> Deposit {
        let now = Utc::now();
        Deposit {
            id: Uuid::now_v7(),
            user_id: "u".into(),
            chain_id,
            token_symbol: "AUSD".into(),
            token_address: token.into(),
            token_decimals: 6,
            amount: "2500000".into(),
            receiver: PAYER.into(),
            calls: sqlx::types::Json(vec![]),
            recovery: PAYER.into(),
            salt: format!("0x{}", "00".repeat(32)),
            expires_at: now,
            payment_address: PAYMENT.into(),
            reference: None,
            webhook_url: None,
            status: DepositStatus::Pending,
            confirmed_amount: "0".into(),
            watch_id: None,
            watch_registered_at: None,
            engine_job_id: None,
            engine_submitted_at: None,
            tx_hash: None,
            block_number: None,
            failure_code: None,
            failure_message: None,
            failure_revert_data: None,
            event_seq: 1,
            detected_at: None,
            settled_at: None,
            failed_at: None,
            expired_at: None,
            created_at: now,
            updated_at: now,
        }
    }
}
