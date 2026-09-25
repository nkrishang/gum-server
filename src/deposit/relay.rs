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
use crate::clients::relay::{
    Quote, QuoteAmount, QuoteRequest, RelayChain, RelayChainCurrency, RelayMetadata, RelayRoles,
};
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
    /// Relay's contracts on the origin chain (from `/chains`), by role: the only addresses a
    /// route's transactions may call, approve or transfer to, besides the origin token itself —
    /// and the roles decide which call may go where.
    pub relay_roles: RelayRoles,
    /// The origin token's decimals, from Relay's own token lists — not from the quote, whose
    /// decimals decide what the route is worth on screen while the calldata spends base units.
    pub origin_decimals: u8,
    /// The deposit token's decimals, from our own record of the deposit.
    pub token_decimals: u8,
    /// Relay's names for the origin chain (from `/chains`), lowercased: signed orders spell chains
    /// by name.
    pub origin_chain_names: Vec<String>,
    /// Relay's names for the deposit's chain, lowercased.
    pub destination_chain_names: Vec<String>,
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
    expected.relay_roles = origin.relay_roles();
    expected.origin_chain_names = chain_names(origin);
    expected.destination_chain_names =
        chains.iter().find(|c| c.id == expected.destination_chain_id).map(chain_names).unwrap_or_default();
    expected.origin_decimals = match chain_token_decimals(origin, expected.origin_currency) {
        Some(decimals) => decimals,
        // Not among the chain's listed tokens: Relay's verified-token search knows it, or the
        // quote's own decimals would go unverified — and the quote could spend far more than it
        // shows.
        None => {
            state.relay_limits.read(deposit.id)?;
            let term = format!("{:#x}", expected.origin_currency);
            let found =
                state.relay.search_currencies(&[expected.origin_chain_id], &term, 5).await.map_err(relay_error)?;
            // The search is scoped to the origin chain, and so is this match: the same address on
            // another chain is a different token, with different decimals.
            let decimals = found
                .iter()
                .find(|t| t.chain_id == expected.origin_chain_id && t.address.eq_ignore_ascii_case(&term))
                .map(|t| t.decimals);
            decimals.ok_or_else(|| {
                ApiError::invalid("that token is not known to Relay on this chain; it can't be quoted safely")
            })?
        }
    };
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
        relay_roles: RelayRoles::default(),
        origin_decimals: 0,
        token_decimals: deposit.token_decimals as u8,
        origin_chain_names: Vec::new(),
        destination_chain_names: Vec::new(),
    })
}

/// A chain's names as Relay's signed orders spell it: the slug and the display name, lowercased.
fn chain_names(c: &RelayChain) -> Vec<String> {
    let mut names = vec![c.name.to_ascii_lowercase()];
    if !c.display_name.is_empty() {
        names.push(c.display_name.to_ascii_lowercase());
    }
    names.sort();
    names.dedup();
    names
}

/// The decimals of one of Relay's own tokens on a chain: the native currency for the zero address,
/// otherwise its listing among the chain's tokens. `None` for a token Relay doesn't list there —
/// including a chain whose own native currency Relay never named, whose decimals would otherwise
/// be guessed.
fn chain_token_decimals(c: &RelayChain, currency: Address) -> Option<u8> {
    if currency.is_zero() {
        return c.currency.as_ref().map(|n| n.decimals);
    }
    let raw = format!("{currency:#x}");
    c.featured_tokens
        .iter()
        .chain(&c.erc20_currencies)
        .find(|t| t.address.eq_ignore_ascii_case(&raw))
        .map(|t| t.decimals)
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
    if !same_address(&d.sender, want.user) {
        return Err(format!("sender {} is not the payer", d.sender));
    }
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
    if out.currency.decimals != want.token_decimals {
        return Err(format!(
            "pays in {} decimals, not the deposit token's {}",
            out.currency.decimals, want.token_decimals
        ));
    }
    // The guarantee must be a real one: a missing or malformed `minimumAmount` is a refusal, not a
    // fall back to the indicative amount.
    let guaranteed = base_units(out.minimum_amount.as_deref())
        .ok_or_else(|| format!("guarantees {:?}, which is no number of base units", out.minimum_amount))?;
    if guaranteed < want.amount {
        return Err(format!("guarantees {:?}, less than {}", out.minimum_amount, want.amount));
    }
    let input = &d.currency_in;
    if input.currency.chain_id != want.origin_chain_id || !same_address(&input.currency.address, want.origin_currency) {
        return Err(format!(
            "takes {} on chain {}, not what the payer chose",
            input.currency.address, input.currency.chain_id
        ));
    }
    if input.currency.decimals != want.origin_decimals {
        return Err(format!(
            "takes {} decimals on the origin chain, not the {} Relay lists for it",
            input.currency.decimals, want.origin_decimals
        ));
    }
    let Some(input_amount) = base_units(Some(&input.amount)) else {
        return Err(format!("input amount {:?} is not base units", input.amount));
    };
    // Every route we ask for is solver-filled, so it comes with the order the solver signs.
    let order = quote
        .protocol
        .as_ref()
        .and_then(|p| p.pointer("/v2/orderData"))
        .filter(|o| o.is_object())
        .ok_or("no signed order (protocol.v2.orderData) to check")?;
    check_order(order, want, input_amount)?;

    let mut spend = Spend::default();
    let mut steps = Vec::new();
    for step in &quote.steps {
        if step.kind != "transaction" {
            return Err(format!("step {} is a {} step; only transactions are supported", step.id, step.kind));
        }
        for item in &step.items {
            steps.push(check_step(&step.id, &step.description, &item.data, want, input_amount, &mut spend)?);
        }
    }
    if steps.is_empty() {
        return Err("no steps to execute".into());
    }
    // The value the wallet sends is the price on screen: nothing for a token, at most the quoted
    // input for the native currency.
    let mut value = U256::ZERO;
    for step in &steps {
        let step_value = base_units(Some(&step.value)).unwrap_or(U256::MAX);
        value = value.checked_add(step_value).ok_or("the route's native value overflows")?;
    }
    let limit = if want.origin_currency.is_zero() { input_amount } else { U256::ZERO };
    if value > limit {
        return Err(format!("the steps send {value} wei of value; the quote takes {limit}"));
    }
    // And the token it may take — transfers now, plus what approvals let Relay draw down — is the
    // quoted input, across the whole route, not just each step on its own.
    let taken = spend.exposure()?;
    if taken > input_amount {
        return Err(format!("the steps take {taken} from the wallet; the quote is for {input_amount}"));
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

/// Is this signed-order chain field the given chain? Relay's orders spell chains by name; a number
/// is also accepted. Anything that names neither the origin nor the destination is a refusal.
fn is_chain(field: Option<&Value>, id: u64, names: &[String]) -> bool {
    let Some(field) = field else { return false };
    if field.as_u64() == Some(id) {
        return true;
    }
    let Some(spelled) = field.as_str().map(str::trim) else { return false };
    if let Ok(n) = spelled.parse::<u64>() {
        return n == id;
    }
    names.iter().any(|name| name.as_str() == spelled.to_ascii_lowercase())
}

/// The signed order behind the route (`protocol.v2.orderData`). Its terms are the route's: it pays
/// the payment address the deposit's token on the deposit's chain, at least `amount` guaranteed;
/// it takes the quoted input on the origin chain, and no more than that; and refunds go to the
/// payer, or (Relay's fallback) to the payment address in the deposit's own token. A missing or
/// malformed term is a refusal — never taken on faith.
fn check_order(order: &Value, want: &Expected, input_amount: U256) -> Result<(), String> {
    let output = order.get("output").ok_or("the signed order has no output")?;
    let payments =
        output.get("payments").and_then(Value::as_array).ok_or("the signed order has no payments to check")?;
    if payments.is_empty() {
        return Err("the signed order pays nothing".into());
    }
    if !is_chain(output.get("chainId"), want.destination_chain_id, &want.destination_chain_names) {
        return Err(format!("the signed order settles on {:?}", output.get("chainId")));
    }
    let mut total = U256::ZERO;
    for p in payments {
        let recipient = p.get("recipient").and_then(Value::as_str).unwrap_or_default();
        let currency = p.get("currency").and_then(Value::as_str).unwrap_or_default();
        if !same_address(recipient, want.recipient) || !same_address(currency, want.token) {
            return Err(format!("the order pays {currency} to {recipient}"));
        }
        let minimum = p
            .get("minimumAmount")
            .and_then(Value::as_str)
            .and_then(|s| base_units(Some(s)))
            .ok_or("a payment of the signed order has no minimumAmount")?;
        total = total.checked_add(minimum).ok_or("the signed order's payments overflow")?;
    }
    if total < want.amount {
        return Err(format!("the order guarantees {total}, less than {}", want.amount));
    }

    let inputs = order.get("inputs").and_then(Value::as_array).ok_or("the signed order has no inputs")?;
    if inputs.is_empty() {
        return Err("the signed order takes nothing".into());
    }
    let mut taken = U256::ZERO;
    for input in inputs {
        let payment =
            input.get("payment").filter(|p| p.is_object()).ok_or("an input of the signed order has no payment")?;
        let currency = payment.get("currency").and_then(Value::as_str).unwrap_or_default();
        if !same_address(currency, want.origin_currency) {
            return Err(format!("the order takes {currency}, not what the payer chose"));
        }
        if !is_chain(payment.get("chainId"), want.origin_chain_id, &want.origin_chain_names) {
            return Err(format!("the order takes {currency} on {:?}", payment.get("chainId")));
        }
        let amount = base_units(payment.get("amount").and_then(Value::as_str))
            .ok_or("an input of the signed order has no amount")?;
        taken = taken.checked_add(amount).ok_or("the signed order's inputs overflow")?;
        // Refunds go to the payer, or (Relay's fallback) to the payment address in the deposit's
        // own token, which is where it counts; never anywhere else, and never on a chain that is
        // neither the origin nor the destination.
        let refunds =
            input.get("refunds").and_then(Value::as_array).ok_or("an input of the signed order has no refunds")?;
        if refunds.is_empty() {
            return Err("an input of the signed order has nowhere its refunds go".into());
        }
        for refund in refunds {
            let to = refund.get("recipient").and_then(Value::as_str).unwrap_or_default();
            let currency = refund.get("currency").and_then(Value::as_str).unwrap_or_default();
            let to_payer = same_address(to, want.user) && same_address(currency, want.origin_currency);
            let to_deposit = same_address(to, want.recipient) && same_address(currency, want.token);
            let on_origin = is_chain(refund.get("chainId"), want.origin_chain_id, &want.origin_chain_names);
            let on_destination =
                is_chain(refund.get("chainId"), want.destination_chain_id, &want.destination_chain_names);
            if !(on_origin && to_payer) && !(on_destination && to_deposit) {
                return Err(format!(
                    "the order refunds {currency} to {to} on {:?}, neither the payer's nor this payment's",
                    refund.get("chainId")
                ));
            }
        }
    }
    // What the order says it takes is what the quote asks the wallet for: an order that takes more
    // would spend past the quote, and one that takes less does not match it.
    if taken != input_amount {
        return Err(format!("the order takes {taken}, not the quoted {input_amount}"));
    }
    Ok(())
}

/// `approve(address,uint256)` and `transfer(address,uint256)`: the only calls a route may make on
/// the payer's token, and only towards Relay's contracts.
const APPROVE: [u8; 4] = [0x09, 0x5e, 0xa7, 0xb3];
const TRANSFER: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

// ---------------------------------------------------------------------------------------------
// Relay's contracts: what their calldata may do
// ---------------------------------------------------------------------------------------------
//
// Relay's own contracts are not safe to call with arbitrary calldata: their entrypoints forward
// nested calls, pull tokens by allowance and choose refund recipients. Every entrypoint a route may
// use is decoded here and held against the route's terms; anything the checks don't decode is
// refused, so a new entrypoint fails closed until it is understood.
//
// Signatures from Relay's periphery (relay-protocol/relay-periphery, relay-protocol/
// relay-depository), verified against their keccak selectors in the tests.
const DEPOSIT_ERC20: [u8; 4] = [0xe8, 0x01, 0x79, 0x52]; // depositErc20(address,address,uint256,bytes32)
const DEPOSIT_ERC20_ALL: [u8; 4] = [0x5a, 0x1e, 0xe3, 0xac]; // depositErc20(address,address,bytes32)
const DEPOSIT_NATIVE: [u8; 4] = [0x49, 0x29, 0x0c, 0x1c]; // depositNative(address,bytes32)
const MULTICALL: [u8; 4] = [0xcd, 0x6e, 0x13, 0xf7]; // multicall((address,bool,uint256,bytes)[],address,address,bytes)
const MULTICALL_V2: [u8; 4] = [0x30, 0xbe, 0x55, 0x67]; // multicall((address,bool,uint256,bytes)[],address,address)
const TRANSFER_AND_MULTICALL: [u8; 4] = [0xf9, 0xe4, 0xba, 0xb4]; // transferAndMulticall(address[],uint256[],(address,bool,uint256,bytes)[],address,address,bytes)
const TRANSFER_AND_MULTICALL_V2: [u8; 4] = [0x30, 0x87, 0x50, 0x56]; // transferAndMulticall(address[],uint256[],(address,bool,uint256,bytes)[],address,address)
const FORWARD: [u8; 4] = [0xd9, 0x48, 0xd4, 0x68]; // forward(bytes)

/// What a route may yet take from the payer's wallet in the token it quoted: token moved out at
/// once, plus approvals Relay can still draw down. Held against the quoted input across the whole
/// route — a route that approves and then spends spends once, and a route that spends twice from
/// one input is refused.
#[derive(Default)]
struct Spend {
    pulled: U256,
    /// Allowance granted by this route, still unspent, per spender.
    allowances: BTreeMap<Address, U256>,
}

impl Spend {
    /// An approval. A later approval of the same spender replaces it, so the largest wins.
    fn grant(&mut self, spender: Address, amount: U256) {
        let slot = self.allowances.entry(spender).or_default();
        if amount > *slot {
            *slot = amount;
        }
    }

    /// A draw-down of the payer's allowance by a Relay contract (a deposit by `depositErc20`, or a
    /// pull through the approval proxy). The whole amount leaves the wallet — what an approval of
    /// this route covered, and what a pre-existing one did — so it all counts, and the allowance
    /// that was drawn down stops counting.
    fn pull(&mut self, spender: Address, amount: U256) -> Result<(), String> {
        if let Some(slot) = self.allowances.get_mut(&spender) {
            let covered = (*slot).min(amount);
            *slot -= covered;
        }
        self.pulled = self.pulled.checked_add(amount).ok_or("the route's token spending overflows")?;
        Ok(())
    }

    /// A plain `transfer` from the wallet.
    fn transfer(&mut self, amount: U256) -> Result<(), String> {
        self.pulled = self.pulled.checked_add(amount).ok_or("the route's token spending overflows")?;
        Ok(())
    }

    /// Everything the route could still take: what moved, plus every allowance it granted.
    fn exposure(&self) -> Result<U256, String> {
        self.allowances.values().try_fold(self.pulled, |sum, a| {
            sum.checked_add(*a).ok_or_else(|| "the route's token spending overflows".to_owned())
        })
    }
}

/// Reads ABI-encoded words, bounds-checked throughout; anything odd reads as nothing.
struct Abi<'a> {
    bytes: &'a [u8],
    /// Where this reader's dynamic offsets are relative to.
    base: usize,
    pos: usize,
}

/// Bounds on decoded calldata: a route's funding calls are small.
const MAX_ABI_OFFSET: usize = 1 << 20;
const MAX_ABI_BYTES: usize = 8 << 10;
const MAX_ABI_ITEMS: usize = 8;

impl<'a> Abi<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, base: 0, pos: 0 }
    }

    fn at(bytes: &'a [u8], base: usize, pos: usize) -> Self {
        Self { bytes, base, pos }
    }

    fn word(&mut self) -> Option<&'a [u8; 32]> {
        let word = self.bytes.get(self.pos..self.pos.checked_add(32)?)?.try_into().ok()?;
        self.pos = self.pos.checked_add(32)?;
        Some(word)
    }

    fn address(&mut self) -> Option<Address> {
        let word = self.word()?;
        if word[..12].iter().any(|b| *b != 0) {
            return None;
        }
        Some(Address::from_slice(&word[12..]))
    }

    fn uint(&mut self) -> Option<U256> {
        Some(U256::from_be_slice(self.word()?))
    }

    fn offset(&mut self) -> Option<usize> {
        usize::try_from(self.uint()?).ok().filter(|off| *off <= MAX_ABI_OFFSET)
    }

    /// A dynamic `bytes` argument: an offset to a length and its data.
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let at = self.base.checked_add(self.offset()?)?;
        let mut r = Abi::at(self.bytes, at, at);
        let len = r.uint()?.try_into().ok()?;
        if len > MAX_ABI_BYTES {
            return None;
        }
        self.bytes.get(at.checked_add(32)?..at.checked_add(32)?.checked_add(len)?)
    }

    /// A dynamic array of statically-sized items (`address[]`, `uint256[]`).
    fn static_items<T>(&mut self, item: impl Fn(&mut Self) -> Option<T>) -> Option<Vec<T>> {
        let at = self.base.checked_add(self.offset()?)?;
        let mut r = Abi::at(self.bytes, at, at);
        let len: usize = r.uint()?.try_into().ok()?;
        if len > MAX_ABI_ITEMS {
            return None;
        }
        // Elements' offsets are relative to the position after the length.
        r.base = r.pos;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(item(&mut r)?);
        }
        Some(out)
    }

    /// Relay's nested calls, `(address target, bool allowFailure, uint256 value, bytes callData)[]`.
    fn nested_calls(&mut self) -> Option<Vec<NestedCall<'a>>> {
        self.static_items(|r| {
            let at = r.base.checked_add(r.offset()?)?;
            let mut el = Abi::at(r.bytes, at, at);
            let target = el.address()?;
            // `bool` is a byte in a word; anything else is not a bool.
            let allow_failure = match el.uint()? {
                raw if raw.is_zero() => false,
                raw if raw == U256::ONE => true,
                _ => return None,
            };
            let value = el.uint()?;
            let data = el.bytes()?;
            Some(NestedCall { target, allow_failure, value, data })
        })
    }
}

/// A call a route makes: to one of Relay's contracts, or to the payer's token.
struct WalletCall<'a> {
    to: Address,
    calldata: &'a [u8],
    /// Wei the wallet attaches.
    value: U256,
}

/// What one `transferAndMulticall` pull must account for. The proxy moves the pulled tokens to the
/// router, whose nested calls must then get them out of it — into the depository, as the deposit —
/// because ERC-20s left in the router are there for anyone to sweep (the router's cleanup is
/// permissionless, and only native is refunded on its own).
#[derive(Default)]
struct Flow {
    /// Allowances the nested calls granted the depository, still undrawn.
    granted: BTreeMap<Address, U256>,
    /// Tokens the nested calls moved into the depository.
    consumed: U256,
}

/// One of Relay's nested calls, `(address target, bool allowFailure, uint256 value, bytes callData)`.
struct NestedCall<'a> {
    target: Address,
    allow_failure: bool,
    value: U256,
    data: &'a [u8],
}

/// What every call of a route is held against: the step it belongs to, the route's terms, how much
/// the quote takes, and how deep the nesting has gone.
struct Route<'a> {
    id: &'a str,
    want: &'a Expected,
    input_amount: U256,
    depth: u8,
}

/// A call to one of Relay's contracts. The payer-facing entrypoints are known and decoded; each is
/// checked for what it debits from the wallet, who it can pay back, and what it nests. `spend` is
/// the wallet's tab, kept only for calls the wallet itself makes (`Some`); a Relay contract's own
/// nested calls spend what the transaction brought it — already on the tab at the pull — so they
/// are bounded per call but debited nowhere.
fn check_relay_call(call: &WalletCall, route: &Route, mut spend: Option<&mut Spend>) -> Result<(), String> {
    let Route { id, want, input_amount, depth } = *route;
    let to = call.to;
    let tx_value = call.value;
    let calldata = call.calldata;
    let entrypoint =
        |selector: &[u8; 4]| format!("makes a call the checks don't decode ({:#x}{}…)", to, hex::encode(selector));
    let (head, args) =
        calldata.split_at_checked(4).ok_or_else(|| format!("step {id} calls {to:#x} with no selector"))?;
    let selector: &[u8; 4] = head.try_into().map_err(|_| format!("step {id} has malformed calldata"))?;
    let refuse = |what: String| format!("step {id} {what}");
    let roles = &want.relay_roles;

    // Each entrypoint belongs to one role, and a role is only trusted at its own address: the
    // depository takes deposits, the approval proxy pulls on an allowance, the receiver forwards
    // value, the router runs nested calls. A known selector sent to a contract that plays no such
    // part is a refusal, not a decode.
    if matches!(*selector, DEPOSIT_ERC20 | DEPOSIT_ERC20_ALL | DEPOSIT_NATIVE) && !roles.depositories.contains(&to) {
        return Err(refuse(format!("deposits through {to:#x}, which is not Relay's depository")));
    }
    if matches!(*selector, MULTICALL | MULTICALL_V2) && !roles.routers.contains(&to) {
        return Err(refuse(format!("runs nested calls through {to:#x}, which is not Relay's router")));
    }
    if matches!(*selector, TRANSFER_AND_MULTICALL | TRANSFER_AND_MULTICALL_V2) && !roles.approval_proxies.contains(&to)
    {
        return Err(refuse(format!("pulls on an allowance through {to:#x}, which is not Relay's approval proxy")));
    }
    if *selector == FORWARD && !roles.receivers.contains(&to) {
        return Err(refuse(format!("forwards value through {to:#x}, which is not Relay's receiver")));
    }

    match *selector {
        // A deposit: Relay's depository draws the amount down from the caller's allowance to it
        // (the payer's, at the top level), for the order the route was quoted for.
        DEPOSIT_ERC20 => {
            let mut r = Abi::new(args);
            let depositor = r.address().ok_or_else(|| refuse("has malformed depositErc20 arguments".into()))?;
            let token = r.address().ok_or_else(|| refuse("has malformed depositErc20 arguments".into()))?;
            let amount = r.uint().ok_or_else(|| refuse("has malformed depositErc20 arguments".into()))?;
            r.word().ok_or_else(|| refuse("has malformed depositErc20 arguments".into()))?; // the order id
            if !depositor.is_zero() && depositor != want.user {
                return Err(refuse(format!("deposits as {depositor:#x}, not the payer")));
            }
            if token != want.origin_currency {
                return Err(refuse(format!("deposits {token:#x}, not the token being paid with")));
            }
            if amount > input_amount {
                return Err(refuse(format!("deposits {amount}, more than the quoted {input_amount}")));
            }
            if let Some(spend) = spend.as_deref_mut() {
                spend.pull(to, amount).map_err(refuse)?;
            }
        }
        // This one draws down the wallet's whole remaining allowance: never bounded by a quote.
        DEPOSIT_ERC20_ALL => {
            return Err(refuse("deposits by the wallet's whole allowance, which no quote bounds".into()));
        }
        // A native deposit: its value is the transaction's, bounded with the rest of the route's.
        DEPOSIT_NATIVE => {
            let mut r = Abi::new(args);
            let depositor = r.address().ok_or_else(|| refuse("has malformed depositNative arguments".into()))?;
            r.word().ok_or_else(|| refuse("has malformed depositNative arguments".into()))?; // the order id
            if !depositor.is_zero() && depositor != want.user {
                return Err(refuse(format!("deposits as {depositor:#x}, not the payer")));
            }
        }
        // The router and the approval proxy run the route's swap as nested calls; the proxy first
        // moves the quoted input from the wallet to the router, through the allowance to it.
        MULTICALL | MULTICALL_V2 | TRANSFER_AND_MULTICALL | TRANSFER_AND_MULTICALL_V2 => {
            let pulls = matches!(*selector, TRANSFER_AND_MULTICALL | TRANSFER_AND_MULTICALL_V2);
            let with_metadata = matches!(*selector, MULTICALL | TRANSFER_AND_MULTICALL);
            let mut r = Abi::new(args);
            let (tokens, amounts) = if pulls {
                let tokens = r
                    .static_items(|r| r.address())
                    .ok_or_else(|| refuse("has malformed transferAndMulticall arguments".into()))?;
                let amounts = r
                    .static_items(|r| r.uint())
                    .ok_or_else(|| refuse("has malformed transferAndMulticall arguments".into()))?;
                (Some(tokens), Some(amounts))
            } else {
                (None, None)
            };
            let calls = r.nested_calls().ok_or_else(|| refuse("has malformed multicall arguments".into()))?;
            let refund_to = r.address().ok_or_else(|| refuse("has malformed multicall arguments".into()))?;
            let nft_recipient = r.address().ok_or_else(|| refuse("has malformed multicall arguments".into()))?;
            if with_metadata {
                r.bytes().ok_or_else(|| refuse("has malformed multicall arguments".into()))?;
            }
            if !refund_to.is_zero() && refund_to != want.user {
                return Err(refuse(format!("sends its surplus to {refund_to:#x}, not the payer")));
            }
            if !nft_recipient.is_zero() && nft_recipient != want.user {
                return Err(refuse(format!("sends its claims to {nft_recipient:#x}, not the payer")));
            }
            // Whatever native is left over, the router refunds to `refundTo` — the payer, above.
            if pulls && calls.is_empty() {
                return Err(refuse("pulls the quoted input but runs no calls to deposit it".into()));
            }
            let mut nested_value = U256::ZERO;
            let mut pulled_total = U256::ZERO;
            if let (Some(tokens), Some(amounts)) = (tokens, amounts) {
                if tokens.len() != amounts.len() {
                    return Err(refuse("pulls tokens and amounts of different lengths".into()));
                }
                for (token, amount) in tokens.into_iter().zip(amounts) {
                    if token != want.origin_currency {
                        return Err(refuse(format!("pulls {token:#x}, not the token being paid with")));
                    }
                    if want.origin_currency.is_zero() {
                        return Err(refuse("pulls the chain's native currency, which no allowance can".into()));
                    }
                    if amount > input_amount {
                        return Err(refuse(format!("pulls {amount}, more than the quoted {input_amount}")));
                    }
                    if let Some(spend) = spend.as_deref_mut() {
                        spend.pull(to, amount).map_err(refuse)?;
                    }
                    pulled_total = pulled_total
                        .checked_add(amount)
                        .ok_or_else(|| refuse("pulls more than a quote can bound".into()))?;
                }
            }
            // A pull with nowhere to go strands the tokens in the router, where anyone can sweep
            // them: every pulled token must come out the far side, into the depository.
            let mut flow = if pulls { Some(Flow::default()) } else { None };
            for NestedCall { target, allow_failure, value, data } in calls {
                if allow_failure {
                    return Err(refuse("nests a call that may fail without saying so".into()));
                }
                nested_value = nested_value.checked_add(value).ok_or_else(|| refuse("nested calls overflow".into()))?;
                if nested_value > tx_value {
                    return Err(refuse("nests calls worth more than the transaction's value".into()));
                }
                let nested = Route { id, want, input_amount, depth: depth + 1 };
                check_nested_call(target, value, data, &nested, flow.as_mut()).map_err(refuse)?;
            }
            if let Some(flow) = flow.filter(|f| f.consumed != pulled_total) {
                return Err(refuse(format!(
                    "leaves {} of the pulled {} in Relay's router, where anyone could sweep it",
                    pulled_total - flow.consumed,
                    pulled_total
                )));
            }
        }
        // The receiver only forwards the transaction's value to Relay's solver; the value is
        // bounded with the rest of the route's.
        FORWARD => {}
        other => return Err(refuse(entrypoint(&other))),
    }
    Ok(())
}

/// A call nested in a Relay router's multicall, run by the router. It can only move what the
/// transaction brought the router — counted on the wallet's tab at the pull — so each call is
/// bounded by the quoted input, and nothing here debits the wallet twice. `flow`, for a proxy's
/// pull, tracks the pulled tokens out towards the depository.
fn check_nested_call(
    target: Address,
    value: U256,
    data: &[u8],
    route: &Route,
    mut flow: Option<&mut Flow>,
) -> Result<(), String> {
    let Route { id, want, input_amount, depth } = *route;
    let refuse = |what: String| format!("step {id} {what}");
    if depth > 1 {
        return Err(refuse("nests calls within nested calls".into()));
    }
    if !want.origin_currency.is_zero() && target == want.origin_currency {
        if !value.is_zero() {
            return Err(refuse("sends value with a call on the token, which takes none".into()));
        }
        let (selector, args) =
            data.split_at_checked(4).ok_or_else(|| refuse("nests a call on the token with no selector".into()))?;
        if *selector != APPROVE && *selector != TRANSFER {
            return Err(refuse(format!("nests an unexpected call on the payer's token ({})", hex::encode(selector))));
        }
        if args.len() < 64 {
            return Err(refuse("nests a call on the token with truncated arguments".into()));
        }
        let spender = Address::from_slice(&args[12..32]);
        let amount = U256::from_be_slice(&args[32..64]);
        if amount > input_amount {
            return Err(refuse(format!("nests a call for {amount}, more than the quoted {input_amount}")));
        }
        if *selector == APPROVE {
            // The router's allowance may only go to whoever turns an allowance into a deposit.
            if !want.relay_roles.may_hold_allowance(&spender) {
                return Err(refuse(format!("nests an allowance to {spender:#x}, which may not hold one")));
            }
            if let Some(flow) = flow.as_deref_mut() {
                let slot = flow.granted.entry(spender).or_default();
                *slot = slot.checked_add(amount).ok_or_else(|| refuse("nested allowances overflow".into()))?;
            }
        } else {
            // And the router's tokens may only go where deposits are held.
            if !want.relay_roles.depositories.contains(&spender) {
                return Err(refuse(format!("nests a transfer to {spender:#x}, which is not Relay's depository")));
            }
            if let Some(flow) = flow {
                flow.consumed =
                    flow.consumed.checked_add(amount).ok_or_else(|| refuse("nested transfers overflow".into()))?;
            }
        }
        return Ok(());
    }
    if want.relay_roles.contains(&target) {
        let nested = WalletCall { to: target, calldata: data, value };
        // A nested deposit draws the router's fresh allowance down; charge it to the flow.
        let pulls_deposit =
            flow.is_some() && want.relay_roles.depositories.contains(&target) && data.starts_with(&DEPOSIT_ERC20);
        let deeper = Route { id, want, input_amount, depth: depth + 1 };
        check_relay_call(&nested, &deeper, None)?;
        if pulls_deposit {
            // depositErc20(depositor, token, amount, id): the amount is the third word of the
            // arguments, and the recursion above has already checked the calldata's shape.
            let amount = U256::from_be_slice(data.get(68..100).unwrap_or(&[]));
            let flow = flow.expect("pulls_deposit means flow is some");
            let slot = flow
                .granted
                .get_mut(&target)
                .ok_or_else(|| refuse(format!("draws down {amount} no nested call approved")))?;
            if *slot < amount {
                return Err(refuse(format!("draws down {amount}, more than the nested calls approved")));
            }
            *slot -= amount;
            flow.consumed =
                flow.consumed.checked_add(amount).ok_or_else(|| refuse("nested deposits overflow".into()))?;
        }
        return Ok(());
    }
    Err(refuse(format!("nests a call to {target:#x}, which is neither Relay's nor the payer's token")))
}

fn check_step(
    id: &str,
    description: &str,
    data: &Value,
    want: &Expected,
    input_amount: U256,
    spend: &mut Spend,
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
    let value = match data.get("value") {
        None | Some(Value::Null) => "0".to_owned(),
        Some(Value::String(s)) if base_units(Some(s)).is_some() => s.trim().to_owned(),
        Some(other) => return Err(format!("step {id} has value {other}")),
    };
    let value_wei = base_units(Some(&value)).unwrap_or(U256::MAX);
    if !want.origin_currency.is_zero() && to == want.origin_currency {
        // A call on the payer's token: an allowance or a transfer to Relay, for no more than the
        // quoted input. A public router may be neither's destination — its calls are anyone's, so
        // an allowance or tokens left with it are there for anyone to sweep.
        let (selector, args) =
            bytes.split_at_checked(4).ok_or_else(|| format!("step {id} calls the token with no selector"))?;
        if (selector != APPROVE && selector != TRANSFER) || args.len() < 64 {
            return Err(format!("step {id} makes an unexpected call on the payer's token"));
        }
        let target = Address::from_slice(&args[12..32]);
        let amount = U256::from_be_slice(&args[32..64]);
        if amount > input_amount {
            return Err(format!("step {id} approves or transfers {amount}, more than the quoted {input_amount}"));
        }
        if selector == APPROVE {
            if !want.relay_roles.may_hold_allowance(&target) {
                return Err(format!("step {id} approves {target:#x}, which may not hold an allowance"));
            }
            spend.grant(target, amount);
        } else {
            if !want.relay_roles.depositories.contains(&target) {
                return Err(format!("step {id} transfers to {target:#x}, which is not Relay's depository"));
            }
            spend.transfer(amount).map_err(|e| format!("step {id} {e}"))?;
        }
    } else if want.relay_roles.contains(&to) {
        // A call on one of Relay's contracts: decoded and held against the route's terms.
        let call = WalletCall { to, calldata: &bytes, value: value_wei };
        let route = Route { id, want, input_amount, depth: 0 };
        check_relay_call(&call, &route, Some(spend))?;
    } else {
        return Err(format!("step {id} calls {to:#x}, which is not one of Relay's contracts on the origin chain"));
    }
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
    const ROUTER: &str = "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f";
    /// `approve(depository, 2526643)`, as Relay sends it.
    const APPROVE_DEPOSITORY: &str = "0x095ea7b30000000000000000000000004cd00e387622c35bddb9b4c962c136462338bc310000000000000000000000000000000000000000000000000000000000268db3";

    fn approve(spender: &str, amount: u64) -> String {
        format!("0x095ea7b3{:0>64}{amount:064x}", spender.trim_start_matches("0x"))
    }

    fn transfer(to: &str, amount: u64) -> String {
        format!("0xa9059cbb{:0>64}{amount:064x}", to.trim_start_matches("0x"))
    }

    /// `depositErc20(depositor, token, amount, id)`, as Relay's depository takes it.
    fn deposit_erc20(depositor: &str, token: &str, amount: u64, id: &str) -> String {
        format!(
            "0xe8017952{:0>64}{:0>64}{amount:064x}{}",
            depositor.trim_start_matches("0x"),
            token.trim_start_matches("0x"),
            id.trim_start_matches("0x")
        )
    }

    /// `depositNative(depositor, id)`.
    fn deposit_native(depositor: &str, id: &str) -> String {
        format!("0x49290c1c{:0>64}{}", depositor.trim_start_matches("0x"), id.trim_start_matches("0x"))
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
            relay_roles: RelayRoles {
                depositories: vec![DEPOSITORY.parse().unwrap()],
                approval_proxies: vec![APPROVAL_PROXY.parse().unwrap()],
                receivers: vec![],
                routers: vec![ROUTER.parse().unwrap()],
            },
            origin_decimals: 6,
            token_decimals: 6,
            origin_chain_names: vec!["arbitrum".into()],
            destination_chain_names: vec!["base".into()],
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
                                "data": deposit_erc20(PAYER, USDC_ARB, 2_526_643, REQUEST), "value": "0", "chainId": 42161 },
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
        refuse(&|q| q["details"]["sender"] = json!(other), "is not the payer");
        refuse(&|q| q["details"]["recipient"] = json!(other), "not the payment address");
        refuse(&|q| q["details"]["currencyOut"]["currency"]["address"] = json!(other), "not the deposit's token");
        refuse(&|q| q["details"]["currencyOut"]["currency"]["chainId"] = json!(1), "not the deposit's token");
        refuse(&|q| q["details"]["currencyOut"]["currency"]["decimals"] = json!(18), "not the deposit token's");
        refuse(&|q| q["details"]["currencyOut"]["minimumAmount"] = Value::Null, "no number of base units");
        refuse(&|q| q["details"]["currencyIn"]["currency"]["decimals"] = json!(18), "Relay lists for it");
        refuse(&|q| q["details"]["currencyIn"]["currency"]["chainId"] = json!(10), "not what the payer chose");
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["output"]["payments"][0]["recipient"] = json!(other),
            "the order pays",
        );
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["output"]["payments"][0]["minimumAmount"] = json!("1"),
            "the order guarantees 1",
        );
        refuse(&|q| q["protocol"]["v2"]["orderData"]["output"]["payments"] = json!([]), "pays nothing");
        refuse(&|q| q["protocol"]["v2"]["orderData"]["output"]["payments"] = Value::Null, "no payments to check");
        refuse(&|q| q["protocol"]["v2"]["orderData"]["output"]["chainId"] = json!("polygon"), "settles on");
        refuse(&|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["payment"] = Value::Null, "has no payment");
        refuse(&|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"] = Value::Null, "has no refunds");
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["payment"]["amount"] = json!("1"),
            "the order takes 1",
        );
        refuse(&|q| q["protocol"]["v2"]["orderData"]["inputs"] = json!([]), "takes nothing");
        refuse(&|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"] = json!([]), "nowhere its refunds go");
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["currency"] = json!(other),
            "neither the payer's nor this payment's",
        );
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["recipient"] = json!(other),
            "the order refunds",
        );
        refuse(
            &|q| q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["chainId"] = json!("polygon"),
            "neither the payer's nor this payment's",
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
            "which may not hold an allowance",
        );
        // A public router runs anyone's calls: an allowance or a transfer that ends at one is
        // there for anyone to sweep.
        refuse(
            &|q| q["steps"][0]["items"][0]["data"]["data"] = json!(approve(ROUTER, 2_526_643)),
            "may not hold an allowance",
        );
        refuse(
            &|q| {
                q["steps"][0]["items"][0]["data"]["data"] = json!(transfer(ROUTER, 2_526_643));
                q["steps"][1]["items"][0]["data"]["data"] = json!(deposit_erc20(PAYER, USDC_ARB, 0, REQUEST));
            },
            "not Relay's depository",
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
        refuse(&|q| q["protocol"] = json!({ "v2": {} }), "no signed order");
        // The deposit's own calldata: truncated arguments, someone else's depositor, another token,
        // an unbounded whole-allowance deposit, an entrypoint the checks don't decode, and a route
        // that pulls twice.
        refuse(&|q| q["steps"][1]["items"][0]["data"]["data"] = json!("0xe8017952aa"), "malformed depositErc20");
        refuse(
            &|q| {
                q["steps"][1]["items"][0]["data"]["data"] =
                    json!(format!("0xe8017952{:0>192}", PAYER.trim_start_matches("0x")))
            },
            "malformed depositErc20",
        );
        refuse(
            &|q| q["steps"][1]["items"][0]["data"]["data"] = json!(deposit_erc20(other, USDC_ARB, 2_526_643, REQUEST)),
            "deposits as",
        );
        refuse(
            &|q| q["steps"][1]["items"][0]["data"]["data"] = json!(deposit_erc20(PAYER, other, 2_526_643, REQUEST)),
            "not the token being paid with",
        );
        refuse(
            &|q| {
                q["steps"][1]["items"][0]["data"]["data"] =
                    json!(format!("0x5a1ee3ac{:0>64}{}", &PAYER[2..], &REQUEST[2..]))
            },
            "whole allowance",
        );
        refuse(
            &|q| q["steps"][1]["items"][0]["data"]["data"] = json!(format!("0xdd4ed837{:0>128}", "0")),
            "the checks don't decode",
        );
        refuse(
            &|q| {
                q["steps"][0]["items"][0]["data"]["data"] = json!(transfer(DEPOSITORY, 2_526_643));
                q["steps"][0]["items"][0]["data"]["to"] = json!(USDC_ARB);
                q["steps"][1]["items"][0]["data"]["data"] = json!(transfer(DEPOSITORY, 2_526_643));
                q["steps"][1]["items"][0]["data"]["to"] = json!(USDC_ARB);
            },
            "take 5053286 from the wallet",
        );
    }

    #[test]
    fn a_native_route_sends_no_more_than_the_quoted_input() {
        let mut want = want();
        want.origin_currency = Address::ZERO;
        let mut q = relay_quote();
        q["details"]["currencyIn"]["currency"]["address"] = json!(NATIVE);
        q["details"]["currencyIn"]["amount"] = json!("955024944952040");
        q["protocol"]["v2"]["orderData"]["inputs"][0]["payment"]["currency"] = json!(NATIVE);
        q["protocol"]["v2"]["orderData"]["inputs"][0]["payment"]["amount"] = json!("955024944952040");
        q["protocol"]["v2"]["orderData"]["inputs"][0]["refunds"][0]["currency"] = json!(NATIVE);
        q["steps"] = json!([q["steps"][1].clone()]);
        q["steps"][0]["items"][0]["data"]["data"] = json!(deposit_native(PAYER, REQUEST));
        q["steps"][0]["items"][0]["data"]["value"] = json!("955024944952040");
        let quote: Quote = serde_json::from_value(q.clone()).unwrap();
        assert_eq!(check_quote(&quote, &want).unwrap().steps[0].value, "955024944952040");
        q["steps"][0]["items"][0]["data"]["value"] = json!("955024944952041");
        let quote: Quote = serde_json::from_value(q).unwrap();
        assert!(check_quote(&quote, &want).unwrap_err().contains("wei of value"));
    }

    /// A router route (`multicall` nesting the deposit), canonically encoded by viem from Relay's
    /// periphery signatures: refund and claim recipients are the payer's, the nested deposit takes
    /// the quoted input of the quoted token.
    const MULTICALL_ROUTE: &str = "0xcd6e13f70000000000000000000000000000000000000000000000000000000000000080000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa960450000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000004cd00e387622c35bddb9b4c962c136462338bc310000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000084e8017952000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e58310000000000000000000000000000000000000000000000000000000000268db31790248938255ff5c9db51e367a55380edb458572f010bc52415d45da935ffd5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

    fn router_quote(data: &str, to: &str) -> Value {
        let mut q = relay_quote();
        q["steps"] = json!([{ "id": "swap", "kind": "transaction", "description": "Sign to route",
            "items": [{ "status": "incomplete",
                        "data": { "from": "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045", "to": to,
                                  "data": data, "value": "0", "chainId": 42161 } }] }]);
        q
    }

    #[test]
    fn a_router_multicall_route_is_decoded_and_held_to_the_terms() {
        // The happy route: the wallet calls Relay's router, which nests the depository's deposit.
        let route = check(router_quote(MULTICALL_ROUTE, ROUTER)).unwrap();
        assert_eq!(route.steps.len(), 1);
        assert_eq!(route.steps[0].to, ROUTER.to_ascii_lowercase());

        let refuse = |data: &str, to: &str, why: &str| {
            let err = check(router_quote(data, to)).unwrap_err();
            assert!(err.contains(why), "{err} (expected {why})");
        };
        // An entrypoint only works at a contract of its own role: the router's multicall sent to
        // the depository, and a deposit sent to the router, are both refused.
        refuse(MULTICALL_ROUTE, DEPOSITORY, "not Relay's router");
        refuse(&deposit_erc20(PAYER, USDC_ARB, 2_526_643, REQUEST), ROUTER, "not Relay's depository");
        // Canonical encoding, but the surplus and the claims go to someone else.
        const BAD_REFUND: &str = "0xcd6e13f700000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000bad000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa960450000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000004cd00e387622c35bddb9b4c962c136462338bc310000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000084e8017952000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e58310000000000000000000000000000000000000000000000000000000000268db31790248938255ff5c9db51e367a55380edb458572f010bc52415d45da935ffd5000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        refuse(BAD_REFUND, ROUTER, "not the payer");
        // A nested transfer of the router's tokens to an address that is not the depository's.
        const BAD_NESTED: &str = "0xcd6e13f70000000000000000000000000000000000000000000000000000000000000080000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa96045000000000000000000000000d8da6bf26964af9d7eed9e03e53415d37aa9604500000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000020000000000000000000000000af88d065e77c8cc2239327c5edb3a432268e58310000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000044a9059cbb0000000000000000000000000000000000000000000000000000000000000bad0000000000000000000000000000000000000000000000000000000000268db3000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
        refuse(BAD_NESTED, ROUTER, "not Relay's depository");
        // An entrypoint the checks don't decode.
        refuse("0x2d9fb478", ROUTER, "the checks don't decode");
    }

    /// One nested call as its ABI tuple, `(address, bool, uint256, bytes)` — `data` is the calldata
    /// hex, padded right to whole words.
    fn nested_call(target: &str, allow_failure: bool, value: u64, data: &str) -> String {
        let data = data.trim_start_matches("0x");
        let words = data.len().div_ceil(64);
        format!(
            "{:0>64}{:064x}{value:064x}{:064x}{:064x}{:0<width$}",
            target.trim_start_matches("0x"),
            u8::from(allow_failure),
            4 * 32,         // the bytes argument's offset, from the tuple's start
            data.len() / 2, // its byte length
            data,
            width = words * 64
        )
    }

    /// `transferAndMulticall(address[],uint256[],(address,bool,uint256,bytes)[],address,address,bytes)`
    /// as Relay's approval proxy takes it: one pull (`None` for none), the nested calls, and where
    /// the surplus goes.
    fn transfer_and_multicall(
        pull: Option<(&str, u64)>,
        calls: &[String],
        refund_to: &str,
        nft_recipient: &str,
    ) -> String {
        let pulled = usize::from(pull.is_some());
        // Head offsets, relative to the arguments: the two static arrays, then the calls array,
        // then the (empty) metadata bytes.
        let tokens_at = 6 * 32;
        let amounts_at = tokens_at + 32 + pulled * 32;
        let calls_at = amounts_at + 32 + pulled * 32;
        let tuples: String = calls.concat();
        let metadata_at = calls_at + 32 + calls.len() * 32 + tuples.len() / 2;
        let mut out = String::from("0xf9e4bab4");
        for at in [tokens_at, amounts_at, calls_at] {
            out += &format!("{at:064x}");
        }
        out += &format!("{:0>64}", refund_to.trim_start_matches("0x"));
        out += &format!("{:0>64}", nft_recipient.trim_start_matches("0x"));
        out += &format!("{metadata_at:064x}");
        out += &format!("{:064x}", pulled);
        if let Some((token, _)) = pull {
            out += &format!("{:0>64}", token.trim_start_matches("0x"));
        }
        out += &format!("{:064x}", pulled);
        if let Some((_, amount)) = pull {
            out += &format!("{amount:064x}");
        }
        out += &format!("{:064x}", calls.len());
        // The calls' element offsets are relative to after the length word.
        let mut at = 32 * calls.len();
        for call in calls {
            out += &format!("{at:064x}");
            at += call.len() / 2;
        }
        out += &tuples;
        out += &"0".repeat(64); // metadata: zero length, no data
        out
    }

    /// The proxy route: approve the proxy, then one pull of the quoted input whose nested calls
    /// take it on to the depository.
    fn proxy_quote(approve_amount: u64, pull: Option<(&str, u64)>, calls: &[String]) -> Value {
        let mut q = relay_quote();
        q["steps"] = json!([
            { "id": "approve", "kind": "transaction", "description": "Sign an approval",
              "items": [{ "status": "incomplete", "data": { "from": "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
                                                            "to": USDC_ARB, "data": approve(APPROVAL_PROXY, approve_amount),
                                                            "value": "0", "chainId": 42161 } }] },
            { "id": "proxy", "kind": "transaction", "description": "Sign to route",
              "items": [{ "status": "incomplete", "data": { "from": "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
                                                            "to": APPROVAL_PROXY,
                                                            "data": transfer_and_multicall(pull, calls, PAYER, PAYER),
                                                            "value": "0", "chainId": 42161 } }] }
        ]);
        q
    }

    #[test]
    fn a_proxy_pull_must_end_in_the_depository() {
        let amount = 2_526_643u64;
        // The honest shape: pull the quoted input, approve the depository for it, deposit it —
        // counted once, despite passing through two contracts.
        let honest = vec![
            nested_call(USDC_ARB, false, 0, &approve(DEPOSITORY, amount)),
            nested_call(DEPOSITORY, false, 0, &deposit_erc20(PAYER, USDC_ARB, amount, REQUEST)),
        ];
        let route = check(proxy_quote(amount, Some((USDC_ARB, amount)), &honest)).unwrap();
        assert_eq!(route.steps.len(), 2);
        assert_eq!(route.steps[1].to, APPROVAL_PROXY.to_ascii_lowercase());

        let refuse = |calls: &[String], pull: Option<(&str, u64)>, why: &str| {
            let err = check(proxy_quote(amount, pull, calls)).unwrap_err();
            assert!(err.contains(why), "{err} (expected {why})");
        };
        // A pull that runs no calls strands the tokens in the router, where anyone can sweep them.
        refuse(&[], Some((USDC_ARB, amount)), "runs no calls");
        // So does one that approves without depositing, or deposits less than it pulled.
        refuse(&[nested_call(USDC_ARB, false, 0, &approve(DEPOSITORY, amount))], Some((USDC_ARB, amount)), "leaves");
        refuse(
            &[
                nested_call(USDC_ARB, false, 0, &approve(DEPOSITORY, amount)),
                nested_call(DEPOSITORY, false, 0, &deposit_erc20(PAYER, USDC_ARB, amount - 1, REQUEST)),
            ],
            Some((USDC_ARB, amount)),
            "leaves",
        );
        // A deposit that draws more than the nested calls approved, and one with no approval at all.
        refuse(
            &[
                nested_call(USDC_ARB, false, 0, &approve(DEPOSITORY, amount - 1)),
                nested_call(DEPOSITORY, false, 0, &deposit_erc20(PAYER, USDC_ARB, amount, REQUEST)),
            ],
            Some((USDC_ARB, amount)),
            "more than the nested calls approved",
        );
        refuse(
            &[nested_call(DEPOSITORY, false, 0, &deposit_erc20(PAYER, USDC_ARB, amount, REQUEST))],
            Some((USDC_ARB, amount)),
            "no nested call approved",
        );
        // A call allowed to fail silently could strand the tokens just the same.
        refuse(
            &[
                nested_call(USDC_ARB, true, 0, &approve(DEPOSITORY, amount)),
                nested_call(DEPOSITORY, false, 0, &deposit_erc20(PAYER, USDC_ARB, amount, REQUEST)),
            ],
            Some((USDC_ARB, amount)),
            "fail without saying so",
        );
        // And the pulled tokens may only land in the depository, not back with the router.
        refuse(
            &[nested_call(USDC_ARB, false, 0, &transfer(ROUTER, amount))],
            Some((USDC_ARB, amount)),
            "not Relay's depository",
        );
    }

    #[test]
    fn relay_roles_come_from_the_chain() {
        let chain: RelayChain = serde_json::from_value(json!({
            "id": 42161, "vmType": "evm",
            "contracts": { "multicall3": "0xca11bde05977b3631167028862be2a173976ca11", "relayReceiver": "",
                           "erc20Router": "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f", "approvalProxy": APPROVAL_PROXY,
                           "v3": { "erc20Router": "0xb92fe925dc43a0ecde6c8b1a2709c170ec4fff4f" } },
            "protocol": { "v2": { "chainId": "arbitrum", "depository": "0x4cD00E387622C35bDDB9b4c962C136462338BC31" } }
        }))
        .unwrap();
        let roles = chain.relay_roles();
        assert!(roles.depositories.contains(&DEPOSITORY.parse().unwrap()));
        assert!(roles.approval_proxies.contains(&APPROVAL_PROXY.parse().unwrap()));
        assert!(roles.routers.contains(&ROUTER.parse().unwrap()));
        assert_eq!(roles.routers.len(), 1, "the same router listed twice is one router");
        assert!(
            !roles.contains(&"0xca11bde05977b3631167028862be2a173976ca11".parse().unwrap()),
            "multicall3 is not Relay's"
        );
    }

    #[test]
    fn a_missing_minimum_is_refused_not_fallen_back() {
        let mut q = relay_quote();
        q["details"]["currencyOut"].as_object_mut().unwrap().remove("minimumAmount");
        assert!(check(q.clone()).unwrap_err().contains("no number of base units"));
        // A malformed one is refused just the same, whatever the indicative amount says.
        q["details"]["currencyOut"]["minimumAmount"] = json!("soon");
        assert!(check(q).unwrap_err().contains("no number of base units"));
    }

    #[test]
    fn the_selectors_are_the_signatures_they_claim() {
        let sig = |s: &str| {
            let digest = alloy_primitives::keccak256(s.as_bytes());
            [digest[0], digest[1], digest[2], digest[3]]
        };
        for (selector, signature) in [
            (DEPOSIT_ERC20, "depositErc20(address,address,uint256,bytes32)"),
            (DEPOSIT_ERC20_ALL, "depositErc20(address,address,bytes32)"),
            (DEPOSIT_NATIVE, "depositNative(address,bytes32)"),
            (MULTICALL, "multicall((address,bool,uint256,bytes)[],address,address,bytes)"),
            (MULTICALL_V2, "multicall((address,bool,uint256,bytes)[],address,address)"),
            (
                TRANSFER_AND_MULTICALL,
                "transferAndMulticall(address[],uint256[],(address,bool,uint256,bytes)[],address,address,bytes)",
            ),
            (
                TRANSFER_AND_MULTICALL_V2,
                "transferAndMulticall(address[],uint256[],(address,bool,uint256,bytes)[],address,address)",
            ),
            (FORWARD, "forward(bytes)"),
            (APPROVE, "approve(address,uint256)"),
            (TRANSFER, "transfer(address,uint256)"),
        ] {
            assert_eq!(selector, sig(signature), "{signature}");
        }
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
