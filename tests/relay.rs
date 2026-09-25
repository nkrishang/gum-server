//! Paying a deposit with any token through Relay (`/v1/pay/{id}/sources`, `/quote`, `/routes`),
//! against a wiremock Relay. `relay_live_*` (ignored) runs against api.relay.link with
//! `RELAY_API_KEY`: `cargo test --test relay -- --ignored`.

mod common;

use std::time::Duration;

use common::{Harness, RELAY_KEY, USDC, deposit_body};
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, ResponseTemplate};

const PAYER: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
const USDC_ARB: &str = "0xaf88d065e77c8cc2239327c5edb3a432268e5831";
const DEPOSITORY: &str = "0x4cd00e387622c35bddb9b4c962c136462338bc31";
const REQUEST: &str = "0x1790248938255ff5c9db51e367a55380edb458572f010bc52415d45da935ffd5";
const NATIVE: &str = "0x0000000000000000000000000000000000000000";
const ARB: &str = "0x912ce59144191c1204e64559fe8253a0e49e6548";

async fn harness_with(tweak: impl FnOnce(&mut gum_server::config::Config)) -> Option<Harness> {
    Harness::start_with(sqlx::postgres::PgPoolOptions::new().max_connections(8), tweak).await
}

macro_rules! harness {
    ($tweak:expr) => {
        match harness_with($tweak).await {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

async fn create_deposit(h: &Harness, body: Value) -> Value {
    // gum-indexer accepts the watch, so the deposit stays open.
    Mock::given(method("POST"))
        .and(path("/v1/watches"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": Uuid::now_v7(), "chain_id": 31337, "payment_address": NATIVE, "status": "active", "confirmed_amount": "0"
        })))
        .mount(&h.indexer)
        .await;
    let key = h.api_key_for(&format!("did:privy:{}", Uuid::now_v7())).await;
    let res = h.http.post(h.url("/v1/deposit")).bearer_auth(&key).json(&body).send().await.unwrap();
    assert_eq!(res.status(), 201, "{}", res.text().await.unwrap());
    res.json().await.unwrap()
}

fn chain(id: u64, name: &str, deposit_enabled: bool) -> Value {
    json!({
        "id": id, "name": name.to_lowercase(), "displayName": name, "vmType": "evm", "disabled": false,
        "depositEnabled": deposit_enabled, "httpRpcUrl": format!("https://rpc.{id}.example"),
        "explorerUrl": format!("https://scan.{id}.example"), "iconUrl": format!("https://assets.relay.link/icons/{id}/light.png"),
        "currency": { "id": "eth", "symbol": "ETH", "name": "Ether", "address": NATIVE, "decimals": 18 },
        "featuredTokens": [], "erc20Currencies": [], "contracts": { "multicall3": "0xca11bde05977b3631167028862be2a173976ca11" },
        "protocol": { "v2": { "depository": DEPOSITORY } }
    })
}

async fn mount_chains(h: &Harness, anvil_receives: bool) {
    let arb = {
        let mut c = chain(42161, "Arbitrum", true);
        c["featuredTokens"] = json!([{ "symbol": "USDC", "name": "USD Coin", "address": USDC_ARB, "decimals": 6,
                                       "metadata": { "logoURI": "https://example.com/usdc.png" } }]);
        // ARB is a solver currency in this mock, but not one that routes in one step (not on the
        // checked list), so it is never offered or quoted.
        c["solverCurrencies"] = json!([
            { "symbol": "USDC", "name": "USD Coin", "address": USDC_ARB, "decimals": 6 },
            { "symbol": "ARB", "name": "Arbitrum", "address": ARB, "decimals": 18 },
            { "symbol": "ETH", "name": "Ether", "address": NATIVE, "decimals": 18 }
        ]);
        c
    };
    let solana = json!({ "id": 792703809, "name": "solana", "displayName": "Solana", "vmType": "svm", "httpRpcUrl": "https://sol.example" });
    Mock::given(method("GET"))
        .and(path("/chains"))
        .and(header("x-api-key", RELAY_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chains": [arb, chain(31337, "Anvil", anvil_receives), solana]
        })))
        .mount(&h.relay)
        .await;
}

fn currency(chain_id: u64, address: &str) -> Value {
    json!({ "chainId": chain_id, "address": address, "symbol": "USDC", "name": "USD Coin", "decimals": 6 })
}

/// Relay's answer for USDC on Arbitrum → the deposit, shaped like a live one.
fn relay_quote(recipient: &str, amount: &str) -> Value {
    json!({
        "requestId": REQUEST,
        "steps": [
            { "id": "approve", "kind": "transaction", "description": "Sign an approval for USDC", "items": [{ "status": "incomplete",
              "data": { "from": PAYER, "to": USDC_ARB, "value": "0", "chainId": 42161,
                        "data": format!("0x095ea7b3{:0>64}{:064x}", &DEPOSITORY[2..], 2_526_643) } }] },
            { "id": "deposit", "kind": "transaction", "description": "Depositing funds to the relayer", "items": [{ "status": "incomplete",
              "data": { "from": PAYER, "to": DEPOSITORY, "value": "0", "chainId": 42161,
                        "data": format!("0xe8017952{:0>64}{:0>64}{:064x}{}", &PAYER[2..], &USDC_ARB[2..], 2_526_643, &REQUEST[2..]) },
              "check": { "endpoint": format!("/intents/status/v3?requestId={REQUEST}"), "method": "GET" } }] }
        ],
        "details": {
            "sender": PAYER, "recipient": recipient,
            "currencyIn": { "currency": currency(42161, USDC_ARB), "amount": "2526643", "amountUsd": "2.526" },
            "currencyOut": { "currency": currency(31337, &USDC.to_lowercase()), "amount": amount, "minimumAmount": amount },
            "totalImpact": { "usd": "-0.0266", "percent": "-1.05" }, "timeEstimate": 2
        },
        "protocol": { "v2": { "orderData": {
            "inputs": [{ "payment": { "chainId": "arbitrum", "currency": USDC_ARB, "amount": "2526643" },
                         "refunds": [{ "chainId": "arbitrum", "recipient": PAYER, "currency": USDC_ARB }] }],
            "output": { "chainId": "anvil", "payments": [{ "recipient": recipient, "currency": USDC.to_lowercase(), "minimumAmount": amount }] }
        } } }
    })
}

fn quote_body(amount: &str) -> Value {
    json!({ "user": PAYER, "origin_chain_id": 42161, "origin_currency": USDC_ARB, "amount": amount })
}

#[tokio::test]
async fn a_route_is_pinned_to_the_deposit_and_checked_before_the_payer_sees_it() {
    let h = harness!(|c| c.relay.quotes_per_deposit_per_minute = 6);
    mount_chains(&h, true).await;
    let deposit = create_deposit(&h, deposit_body()).await;
    let id = deposit["id"].as_str().unwrap();
    let payment_address = deposit["payment_address"].as_str().unwrap();

    // Sources: EVM chains only, the deposit's chain first, its token listed there.
    let res = h.http.get(h.url(&format!("/v1/pay/{id}/sources"))).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["cache-control"], "private, max-age=60");
    let sources: Value = res.json().await.unwrap();
    assert_eq!(sources["available"], true, "{sources}");
    let ids: Vec<u64> = sources["chains"].as_array().unwrap().iter().map(|c| c["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [31337, 42161]);
    assert_eq!(sources["chains"][0]["tokens"][0]["address"], USDC.to_lowercase());
    assert_eq!(sources["chains"][1]["tokens"][0]["logo_uri"], "https://example.com/usdc.png");
    assert_eq!(sources["chains"][1]["native"]["symbol"], "ETH");
    assert_eq!(sources["chains"][1]["rpc_url"], "https://rpc.42161.example");

    // The quote asks Relay for exactly the deposit's terms, whatever the page might have wanted.
    Mock::given(method("POST"))
        .and(path("/quote/v2"))
        .and(header("x-api-key", RELAY_KEY))
        .and(body_partial_json(json!({
            "user": PAYER, "recipient": payment_address, "refundTo": PAYER, "refundOnOrigin": true,
            "originChainId": 42161, "originCurrency": USDC_ARB,
            "destinationChainId": 31337, "destinationCurrency": USDC.to_lowercase(),
            "amount": "2500000", "tradeType": "EXACT_OUTPUT", "referrer": format!("gum.money|{id}"),
            "enableTrueExactOutput": true,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(relay_quote(payment_address, "2500000")))
        .up_to_n_times(1)
        .mount(&h.relay)
        .await;
    let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("2500000")).send().await.unwrap();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    let route: Value = res.json().await.unwrap();
    assert_eq!(route["request_id"], REQUEST);
    assert_eq!(route["origin"]["amount"], "2526643");
    assert_eq!(route["destination"]["amount"], "2500000");
    assert_eq!(route["fees"]["route_usd"], "0.03");
    let steps: Vec<(&str, &str)> = route["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["to"].as_str().unwrap()))
        .collect();
    assert_eq!(steps, [("approve", USDC_ARB), ("deposit", DEPOSITORY)]);

    // Relay answering with another recipient never reaches the payer.
    Mock::given(method("POST"))
        .and(path("/quote/v2"))
        .and(body_partial_json(json!({ "amount": "1000000" })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(relay_quote("0x000000000000000000000000000000000000dead", "1000000")),
        )
        .up_to_n_times(1)
        .mount(&h.relay)
        .await;
    let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("1000000")).send().await.unwrap();
    assert_eq!(res.status(), 502);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "route_rejected");

    // Relay's refusals come back in the payer's terms.
    Mock::given(method("POST"))
        .and(path("/quote/v2"))
        .and(body_partial_json(json!({ "amount": "10" })))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_json(json!({ "message": "Swap output amount is too small", "errorCode": "AMOUNT_TOO_LOW" })),
        )
        .mount(&h.relay)
        .await;
    let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("10")).send().await.unwrap();
    assert_eq!(res.status(), 422);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "amount_too_low");

    // More than is owed, the requested token itself, and chains Relay doesn't route from are
    // refused without asking Relay.
    let refused = [
        (quote_body("2500001"), 400, "invalid_request"),
        (
            json!({ "user": PAYER, "origin_chain_id": 31337, "origin_currency": USDC, "amount": "1" }),
            400,
            "invalid_request",
        ),
        (
            json!({ "user": PAYER, "origin_chain_id": 792703809, "origin_currency": NATIVE, "amount": "1" }),
            400,
            "unsupported_chain",
        ),
        (
            json!({ "user": NATIVE, "origin_chain_id": 42161, "origin_currency": NATIVE, "amount": "1" }),
            400,
            "invalid_request",
        ),
        // A solver currency that doesn't route in one step (not on the checked list).
        (
            json!({ "user": PAYER, "origin_chain_id": 42161, "origin_currency": ARB, "amount": "1" }),
            400,
            "unsupported_token",
        ),
        (
            json!({ "user": PAYER, "origin_chain_id": 42161, "origin_currency": NATIVE, "amount": "1", "recipient": PAYER }),
            400,
            "invalid_request",
        ),
    ];
    for (body, status, code) in refused {
        let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&body).send().await.unwrap();
        assert_eq!(res.status(), status, "{body}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], code, "{body}");
    }

    // Three quotes so far; the fourth to sixth pass, the seventh waits (6 a minute per deposit).
    for n in 4..=7 {
        let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("10")).send().await.unwrap();
        assert_eq!(res.status(), if n <= 6 { 422 } else { 429 }, "quote {n}");
    }

    // The route's progress, from Relay.
    Mock::given(method("GET"))
        .and(path("/intents/status/v3"))
        .and(query_param("requestId", REQUEST))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "success", "inTxHashes": [format!("0x{}", "11".repeat(32))], "txHashes": [format!("0x{}", "22".repeat(32))],
            "updatedAt": 1767385462193i64, "originChainId": 42161, "destinationChainId": 31337, "failReason": "N/A"
        })))
        .mount(&h.relay)
        .await;
    let status: Value =
        h.http.get(h.url(&format!("/v1/pay/{id}/routes/{REQUEST}"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(status["status"], "success");
    assert_eq!(status["tx_hashes"][0], format!("0x{}", "22".repeat(32)));
    assert!(status.get("fail_reason").is_none());
    assert_eq!(status["updated_at"], "2026-01-02T20:24:22.193Z");
    let res = h.http.get(h.url(&format!("/v1/pay/{id}/routes/0x1234"))).send().await.unwrap();
    assert_eq!(res.status(), 404);

    // The origin transaction hint is passed on; Relay failing it changes nothing for the page.
    Mock::given(method("POST"))
        .and(path("/transactions/index"))
        .and(body_partial_json(json!({ "txHash": format!("0x{}", "11".repeat(32)), "chainId": "42161" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "message": "ok" })))
        .expect(1)
        .mount(&h.relay)
        .await;
    let res = h
        .http
        .post(h.url(&format!("/v1/pay/{id}/routes/{REQUEST}/transactions")))
        .json(&json!({ "tx_hash": format!("0x{}", "11".repeat(32)), "chain_id": 42161 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);

    // Unknown deposits are 404 on every route.
    let unknown = Uuid::now_v7();
    for url in [format!("/v1/pay/{unknown}/sources"), format!("/v1/pay/{unknown}/routes/{REQUEST}")] {
        assert_eq!(h.http.get(h.url(&url)).send().await.unwrap().status(), 404, "{url}");
    }
    let res = h.http.post(h.url(&format!("/v1/pay/{unknown}/quote"))).json(&quote_body("1")).send().await.unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn prices_and_token_search_go_through_relay() {
    let h = harness!(|_| {});
    mount_chains(&h, true).await;
    let deposit = create_deposit(&h, deposit_body()).await;
    let id = deposit["id"].as_str().unwrap();

    Mock::given(method("GET"))
        .and(path("/currencies/token/price"))
        .and(query_param("chainId", "42161"))
        .and(query_param("address", USDC_ARB))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "price": 0.9998 })))
        .expect(1) // cached afterwards
        .mount(&h.relay)
        .await;
    Mock::given(method("GET"))
        .and(path("/currencies/token/price"))
        .and(query_param("address", NATIVE))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "message": "no price" })))
        .mount(&h.relay)
        .await;
    let body = json!({ "tokens": [format!("42161:{USDC_ARB}"), format!("42161:{}", USDC_ARB.to_uppercase().replace("0X", "0x")), format!("1:{NATIVE}")] });
    for _ in 0..2 {
        let prices: Value =
            h.http.post(h.url(&format!("/v1/pay/{id}/prices"))).json(&body).send().await.unwrap().json().await.unwrap();
        assert_eq!(prices, json!({ "prices": { format!("42161:{USDC_ARB}"): 0.9998, format!("1:{NATIVE}"): null } }));
    }
    let res = h
        .http
        .post(h.url(&format!("/v1/pay/{id}/prices")))
        .json(&json!({ "tokens": ["base:usdc"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    Mock::given(method("POST"))
        .and(path("/currencies/v2"))
        .and(body_partial_json(json!({ "chainIds": [42161], "term": "usd", "verified": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "chainId": 42161, "address": USDC_ARB, "symbol": "USDC", "name": "USD Coin", "decimals": 6, "vmType": "evm",
              "metadata": { "logoURI": "https://example.com/usdc.png", "verified": true } },
            { "chainId": 792703809, "address": "EPjFWdd5", "symbol": "USDC", "name": "USD Coin", "decimals": 6, "vmType": "svm" }
        ])))
        .mount(&h.relay)
        .await;
    let found: Value = h
        .http
        .get(h.url(&format!("/v1/pay/{id}/sources/tokens?q=usd&chain_id=42161")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        found,
        json!({ "tokens": [{ "chain_id": 42161, "address": USDC_ARB, "symbol": "USDC", "name": "USD Coin", "decimals": 6, "logo_uri": "https://example.com/usdc.png" }] })
    );
    let res = h.http.get(h.url(&format!("/v1/pay/{id}/sources/tokens?q=usd&chain_id=792703809"))).send().await.unwrap();
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn routes_are_off_without_a_key_a_destination_or_an_open_deposit() {
    // No key.
    let h = harness!(|c| c.relay.api_key = String::new());
    let deposit = create_deposit(&h, deposit_body()).await;
    let id = deposit["id"].as_str().unwrap();
    let sources: Value =
        h.http.get(h.url(&format!("/v1/pay/{id}/sources"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(sources, json!({ "available": false, "reason": "disabled", "chains": [] }));
    let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("1")).send().await.unwrap();
    assert_eq!(res.status(), 503);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "relay_disabled");

    // Relay doesn't deliver to the deposit's chain.
    let h = harness!(|_| {});
    mount_chains(&h, false).await;
    let deposit = create_deposit(&h, deposit_body()).await;
    let id = deposit["id"].as_str().unwrap();
    let sources: Value =
        h.http.get(h.url(&format!("/v1/pay/{id}/sources"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(sources["reason"], "destination_unsupported");
    let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("1")).send().await.unwrap();
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "route_unavailable");

    // Closing too soon for a route to land.
    let h = harness!(|c| c.relay.min_time_left_secs = 3600);
    mount_chains(&h, true).await;
    let deposit = create_deposit(&h, deposit_body()).await;
    let id = deposit["id"].as_str().unwrap();
    sqlx::query("UPDATE deposits SET expires_at = now() + interval '10 minutes' WHERE id = $1::uuid")
        .bind(id)
        .execute(&h.pool)
        .await
        .unwrap();
    let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&quote_body("1")).send().await.unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "closing");

    // Closed.
    sqlx::query("UPDATE deposits SET status = 'expired' WHERE id = $1::uuid").bind(id).execute(&h.pool).await.unwrap();
    let sources: Value =
        h.http.get(h.url(&format!("/v1/pay/{id}/sources"))).send().await.unwrap().json().await.unwrap();
    assert_eq!(sources["reason"], "closed");
}

// ---------------------------------------------------------------------------------------------
// Live: api.relay.link with RELAY_API_KEY. Quotes only; nothing is signed or sent.
// ---------------------------------------------------------------------------------------------

fn live(config: &mut gum_server::config::Config) {
    let defaults = gum_server::config::Config::load_unchecked(std::path::Path::new("config")).unwrap();
    config.relay.base_url = defaults.relay.base_url;
    config.relay.api_key = std::env::var("RELAY_API_KEY").expect("RELAY_API_KEY");
    for name in ["base", "arbitrum", "monad", "arc"] {
        let mut chain = defaults.chains[name].clone();
        chain.enabled = true;
        config.chains.insert(name.into(), chain);
    }
}

#[tokio::test]
#[ignore = "calls api.relay.link; needs RELAY_API_KEY"]
async fn relay_live_quotes_pass_the_checks() {
    let h = harness!(live);
    for (chain, token, origins) in [
        (
            "base",
            "USDC",
            vec![
                (42161, USDC_ARB),                                 // USDC, cross-chain
                (42161, NATIVE),                                   // ETH, cross-chain
                (8453, NATIVE),                                    // ETH, same chain (solver-filled)
                (1, "0xdac17f958d2ee523a2206206994597c13d831ec7"), // USDT on Ethereum
            ],
        ),
        ("arbitrum", "USDT", vec![(8453, "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913")]),
        ("monad", "USDC", vec![(42161, USDC_ARB)]),
        ("arc", "USDC", vec![(8453, NATIVE)]),
    ] {
        let mut body = deposit_body();
        body["chain_id"] = json!(chain);
        body["token"] = json!(token);
        let deposit = create_deposit(&h, body).await;
        let id = deposit["id"].as_str().unwrap();
        let sources: Value =
            h.http.get(h.url(&format!("/v1/pay/{id}/sources"))).send().await.unwrap().json().await.unwrap();
        assert_eq!(sources["available"], true, "{chain}: {sources}");
        assert_eq!(sources["chains"][0]["id"], deposit["chain_id"], "{chain}");
        eprintln!("{chain}: {} source chains", sources["chains"].as_array().unwrap().len());
        for (origin_chain_id, origin_currency) in origins {
            let body = json!({ "user": PAYER, "origin_chain_id": origin_chain_id, "origin_currency": origin_currency, "amount": "2500000" });
            let res = h.http.post(h.url(&format!("/v1/pay/{id}/quote"))).json(&body).send().await.unwrap();
            let status = res.status();
            let route: Value = res.json().await.unwrap();
            assert_eq!(status, 200, "{chain} from {origin_chain_id}:{origin_currency}: {route}");
            assert_eq!(route["destination"]["amount"], "2500000");
            let steps: Vec<&str> =
                route["steps"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
            eprintln!(
                "{chain} {token} ← {origin_chain_id}:{origin_currency}: pay {} {} (fees ${}, ~{}s) via {steps:?}",
                route["origin"]["amount"],
                route["origin"]["currency"]["symbol"],
                route["fees"]["route_usd"],
                route["time_estimate_secs"]
            );
            let status: Value = h
                .http
                .get(h.url(&format!("/v1/pay/{id}/routes/{}", route["request_id"].as_str().unwrap())))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(["waiting", "unknown"].contains(&status["status"].as_str().unwrap()), "{status}");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

/// Keeps `relay_tokens::DIRECT_TOKENS` true: quotes every solver currency Relay lists on every EVM
/// chain it routes from into a Base USDC deposit, through every check, and compares with the list.
/// Fails, with the lines to paste, when a listed token no longer passes the checks or an unlisted
/// one now does. Relay's own refusals (no route, no liquidity) are reported, not failed: they come
/// and go with liquidity. Takes a few minutes: quotes are paced under Relay's 50 a minute.
#[tokio::test]
#[ignore = "calls api.relay.link for every solver currency; needs RELAY_API_KEY"]
async fn relay_live_direct_tokens() {
    use gum_server::deposit::relay_tokens::{DIRECT_TOKENS, is_direct};
    let h = harness!(|c| {
        live(c);
        c.relay.probe_unlisted_tokens = true;
        c.relay.quotes_per_minute = 1_000;
        c.relay.quotes_per_deposit_per_minute = 1_000;
        c.relay.reads_per_minute = 1_000;
    });
    let mut body = deposit_body();
    body["chain_id"] = json!("base");
    body["amount"] = json!("5000000");
    let deposit = create_deposit(&h, body).await;
    let id = deposit["id"].as_str().unwrap();
    let base_usdc = deposit["token_address"].as_str().unwrap().to_owned();

    let chains = h.state.relay.chains().await.expect("relay chains");
    let (mut routes, mut rejected, mut relay_refused) = (vec![], vec![], vec![]);
    for c in chains.iter().filter(|c| c.vm_type == "evm" && !c.disabled) {
        for t in &c.solver_currencies {
            let address = t.address.to_ascii_lowercase();
            if c.id == 8453 && address == base_usdc {
                routes.push((c.id, address, t.symbol.clone(), c.display_name.clone()));
                continue;
            }
            let quote =
                json!({ "user": PAYER, "origin_chain_id": c.id, "origin_currency": address, "amount": "5000000" });
            let res = h
                .http
                .post(h.url(&format!("/v1/pay/{id}/quote")))
                .json(&quote)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .unwrap();
            let status = res.status().as_u16();
            let code = res.json::<Value>().await.ok().and_then(|b| b["error"]["code"].as_str().map(str::to_owned));
            let entry = (c.id, address, t.symbol.clone(), c.display_name.clone());
            match status {
                200 => routes.push(entry),
                422 => relay_refused.push((entry, code.unwrap_or_default())),
                _ => rejected.push((entry, format!("{status} {}", code.unwrap_or_default()))),
            }
            tokio::time::sleep(Duration::from_millis(1_300)).await;
        }
    }
    let add: Vec<_> = routes.iter().filter(|(chain, address, ..)| !is_direct(*chain, address)).collect();
    let remove: Vec<_> = rejected.iter().filter(|((chain, address, ..), _)| is_direct(*chain, address)).collect();
    eprintln!(
        "{} route in one step, {} rejected (two-step), {} refused by Relay",
        routes.len(),
        rejected.len(),
        relay_refused.len()
    );
    for ((_, _, symbol, chain), why) in &relay_refused {
        eprintln!("  Relay refused {symbol} on {chain}: {why}");
    }
    for (chain_id, address, symbol, chain) in &add {
        eprintln!(
            "  ADD ({chain}): DirectToken {{ chain_id: {chain_id}, address: \"{address}\", symbol: \"{symbol}\" }},"
        );
    }
    for ((chain_id, address, symbol, chain), why) in &remove {
        eprintln!("  REMOVE {symbol} on {chain} ({chain_id}, {address}): {why}");
    }
    let listed_gone: Vec<_> = DIRECT_TOKENS
        .iter()
        .filter(|t| {
            !chains.iter().any(|c| {
                c.id == t.chain_id && c.solver_currencies.iter().any(|s| s.address.eq_ignore_ascii_case(t.address))
            })
        })
        .map(|t| format!("{} on {}", t.symbol, t.chain_id))
        .collect();
    if !listed_gone.is_empty() {
        eprintln!("  no longer solver currencies (hidden already; remove when convenient): {listed_gone:?}");
    }
    assert!(add.is_empty() && remove.is_empty(), "relay_tokens::DIRECT_TOKENS is out of date; see above");
}
