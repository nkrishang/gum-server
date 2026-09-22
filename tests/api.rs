//! End-to-end tests over HTTP against a real Postgres, with gum-indexer, gum-engine and the app's
//! webhook stubbed by wiremock. Run with `TEST_DATABASE_URL=postgres://…` (see README).

mod common;

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use chrono::Utc;
use common::{ADMIN_TOKEN, ENGINE_SECRET, FACTORY, Harness, INDEXER_KEY, INDEXER_SECRET, USDC, deposit_body};
use gum_server::chain::payment::PaymentTerms;
use gum_server::webhooks::sign;
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

macro_rules! harness {
    () => {
        match Harness::start().await {
            Some(h) => h,
            None => {
                eprintln!("TEST_DATABASE_URL not set; skipping");
                return;
            }
        }
    };
}

const WATCH_ID: &str = "0193a1b2-0000-7000-8000-000000000001";
const JOB_ID: &str = "0193a1b2-0000-7000-8000-000000000002";

async fn mount_upstreams(h: &Harness) {
    Mock::given(method("POST"))
        .and(path("/v1/watches"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": WATCH_ID, "chain": "anvil", "chain_id": 31337, "token": "USDC", "token_address": USDC,
            "payment_address": "0x0000000000000000000000000000000000000001", "balance_threshold": "2500000",
            "confirmed_amount": "0", "status": "active", "webhook_endpoint": "x", "start_block": 1,
            "created_at": Utc::now(), "expires_at": null, "completed_at": null
        })))
        .mount(&h.indexer)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/transactions"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "job_id": JOB_ID })))
        .mount(&h.engine)
        .await;
    Mock::given(method("POST")).and(path("/hooks")).respond_with(ResponseTemplate::new(200)).mount(&h.app).await;
}

fn indexer_event(id: &str, event_type: &str, payment_address: &str, confirmed: &str, transfer: Option<Value>) -> Value {
    json!({
        "id": id, "type": event_type, "created_at": Utc::now(), "sequence": 1,
        "watch": { "id": WATCH_ID, "chain": "anvil", "chain_id": 31337, "token": "USDC", "token_address": USDC,
                   "payment_address": payment_address, "balance_threshold": "2500000", "confirmed_amount": confirmed, "status": "active" },
        "transfer": transfer,
    })
}

fn settled_receipt(payment_address: &str) -> Value {
    json!({ "transactionHash": "0xe15d10f3812c0d9a6c0d30cf5e84868309e0cc28b0fd7272f03df90ca4de222c", "status": "0x1", "logs": [
        { "address": payment_address, "topics": ["0x7823e479a1a4ebe2418874847436f8a1680c5ee5b17f38bb59dbff28e1b45552", "0x00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c8"], "data": "0x00000000000000000000000000000000000000000000000000000000002625a0" }
    ]})
}

fn engine_event(
    event_id: &str,
    event: &str,
    outcome: Option<&str>,
    receipt: Option<Value>,
    error: Option<Value>,
) -> Value {
    json!({
        "event_id": event_id, "event": event, "sequence": 1, "job_id": JOB_ID, "chain_id": 31337,
        "status": event.trim_start_matches("transaction."), "outcome": outcome,
        "tx_hash": "0xe15d10f3812c0d9a6c0d30cf5e84868309e0cc28b0fd7272f03df90ca4de222c", "block_number": 5,
        "receipt": receipt, "reincluded": false, "error": error, "timestamp": Utc::now(),
    })
}

/// App webhook deliveries received so far, verified against the account's secret.
async fn app_deliveries(h: &Harness, secret: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for req in h.app.received_requests().await.unwrap_or_default() {
        let header = req.headers.get("x-gum-signature").unwrap().to_str().unwrap();
        assert!(sign::verify(secret, header, &req.body, Utc::now().timestamp(), 300), "app webhook signature");
        assert!(req.headers.get("x-gum-event-id").is_some());
        out.push(serde_json::from_slice(&req.body).unwrap());
    }
    out
}

#[tokio::test]
async fn full_deposit_lifecycle() {
    let h = harness!();
    mount_upstreams(&h).await;
    let key = h.api_key_for("did:privy:alice").await;
    let account: Value = h.http.get(h.url("/v1/account")).bearer_auth(&key).send().await.unwrap().json().await.unwrap();
    let webhook_secret = account["webhook_secret"].as_str().unwrap().to_owned();
    assert_eq!(account["api_key"]["prefix"].as_str().unwrap(), &key[..15]);

    // 1. Create: fast path returns the address; nothing upstream has been called yet.
    let mut body = deposit_body();
    body["webhook_url"] = json!(format!("{}/hooks", h.app.uri()));
    let res = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .header("Idempotency-Key", "k1")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201, "{}", res.text().await.unwrap());
    let created: Value = res.json().await.unwrap();
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let payment_address = created["payment_address"].as_str().unwrap().to_owned();
    assert_eq!(created["status"], "pending");
    assert_eq!(created["amount"], "2500000");
    assert_eq!(created["chain_id"], 31337);
    assert_eq!(created["token_address"], USDC.to_ascii_lowercase());
    assert_eq!(created["recovery"], "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc");

    // The address is exactly what PaymentFactory would derive from the stored terms.
    let terms = PaymentTerms {
        token: USDC.parse().unwrap(),
        amount: U256::from(2_500_000u64),
        receiver: created["receiver"].as_str().unwrap().parse().unwrap(),
        expiration_timestamp: chrono::DateTime::parse_from_rfc3339(created["expires_at"].as_str().unwrap())
            .unwrap()
            .timestamp() as u64,
        recovery: Harness::recovery(),
        salt: created["salt"].as_str().unwrap().parse::<B256>().unwrap(),
        chain_id: 31337,
    };
    let factory: Address = FACTORY.parse().unwrap();
    assert_eq!(format!("{:#x}", terms.payment_address(factory)), payment_address);

    // 2. The outbox registers the watch with gum-indexer.
    let watch_id = h
        .wait_for("watch registration", Duration::from_secs(5), || async {
            sqlx::query_as::<_, (Option<Uuid>,)>("SELECT watch_id FROM deposits WHERE id = $1")
                .bind(id)
                .fetch_one(&h.pool)
                .await
                .unwrap()
                .0
        })
        .await;
    assert_eq!(watch_id.to_string(), WATCH_ID);
    let watch_req = &h.indexer.received_requests().await.unwrap()[0];
    assert_eq!(watch_req.headers.get("authorization").unwrap().to_str().unwrap(), format!("Bearer {INDEXER_KEY}"));
    let watch_body: Value = serde_json::from_slice(&watch_req.body).unwrap();
    assert_eq!(watch_body["payment_address"], payment_address);
    assert_eq!(watch_body["balance_threshold"], "2500000");
    assert_eq!(watch_body["chain"], "31337");
    assert_eq!(watch_body["token"], USDC.to_ascii_lowercase());
    assert_eq!(watch_body["webhook_endpoint"], format!("{}/v1/webhooks/indexer", h.base_url));
    assert_eq!(watch_body["expires_at"].as_str().unwrap(), created["expires_at"].as_str().unwrap());

    // 3. gum-indexer sees a transfer at chain head.
    let transfer = json!({ "tx_hash": "0x11", "log_index": 1, "block_number": 10, "block_hash": "0x22", "from": "0x0000000000000000000000000000000000000009", "amount": "2500000", "status": "pending" });
    let res = h
        .deliver(
            "/v1/webhooks/indexer",
            INDEXER_SECRET,
            &indexer_event("ev-1", "payment.pending", &payment_address, "0", Some(transfer.clone())),
        )
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.deposit_status(id).await, "partial_paid");
    h.wait_for("deposit.detected webhook", Duration::from_secs(5), || async {
        app_deliveries(&h, &webhook_secret).await.into_iter().find(|d| d["type"] == "deposit.detected")
    })
    .await;

    // 4. Confirmed and threshold reached: settlement is submitted to gum-engine.
    let res = h
        .deliver(
            "/v1/webhooks/indexer",
            INDEXER_SECRET,
            &indexer_event("ev-2", "payment.confirmed", &payment_address, "2500000", Some(transfer)),
        )
        .await;
    assert_eq!(res.status(), 200);
    let res = h
        .deliver(
            "/v1/webhooks/indexer",
            INDEXER_SECRET,
            &indexer_event("ev-3", "threshold.reached", &payment_address, "2500000", None),
        )
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.deposit_status(id).await, "paid");
    let job_id = h
        .wait_for("engine submission", Duration::from_secs(5), || async {
            sqlx::query_as::<_, (Option<Uuid>,)>("SELECT engine_job_id FROM deposits WHERE id = $1")
                .bind(id)
                .fetch_one(&h.pool)
                .await
                .unwrap()
                .0
        })
        .await;
    assert_eq!(job_id.to_string(), JOB_ID);
    let tx_req = &h.engine.received_requests().await.unwrap()[0];
    assert!(
        tx_req.headers.get("idempotency-key").unwrap().to_str().unwrap().starts_with(&format!("deposit:{id}:execute"))
    );
    let tx_body: Value = serde_json::from_slice(&tx_req.body).unwrap();
    assert_eq!(tx_body["chain_id"], 31337);
    assert_eq!(tx_body["to"], FACTORY.to_ascii_lowercase());
    assert_eq!(tx_body["data"], terms.execute_calldata().to_string());
    assert_eq!(tx_body["webhook"]["url"], format!("{}/v1/webhooks/engine", h.base_url));
    assert!(tx_body.get("gas_limit").is_none(), "the engine estimates and simulates");

    // 5. gum-engine mines and confirms; the receipt proves the receiver was paid.
    let res = h
        .deliver(
            "/v1/webhooks/engine",
            ENGINE_SECRET,
            &engine_event(
                "eng-1",
                "transaction.included",
                Some("success"),
                Some(settled_receipt(&payment_address)),
                None,
            ),
        )
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.deposit_status(id).await, "paid");
    let res = h
        .deliver(
            "/v1/webhooks/engine",
            ENGINE_SECRET,
            &engine_event(
                "eng-2",
                "transaction.confirmed",
                Some("success"),
                Some(settled_receipt(&payment_address)),
                None,
            ),
        )
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.deposit_status(id).await, "settled");

    // Redelivery of the same event is a no-op.
    let res = h
        .deliver(
            "/v1/webhooks/engine",
            ENGINE_SECRET,
            &engine_event(
                "eng-2",
                "transaction.confirmed",
                Some("success"),
                Some(settled_receipt(&payment_address)),
                None,
            ),
        )
        .await;
    assert_eq!(res.status(), 200);

    // 6. The app learned about every step, in order, with a valid signature.
    let deliveries = h
        .wait_for("deposit.settled webhook", Duration::from_secs(5), || async {
            let d = app_deliveries(&h, &webhook_secret).await;
            d.iter().any(|d| d["type"] == "deposit.settled").then_some(d)
        })
        .await;
    let types: Vec<&str> = deliveries.iter().map(|d| d["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["deposit.detected", "deposit.payment_confirmed", "deposit.ready", "deposit.settled"]);
    let sequences: Vec<i64> = deliveries.iter().map(|d| d["sequence"].as_i64().unwrap()).collect();
    assert!(sequences.windows(2).all(|w| w[0] < w[1]), "{sequences:?}");
    assert_eq!(deliveries[3]["deposit"]["status"], "settled");
    assert_eq!(
        deliveries[3]["deposit"]["tx_hash"],
        "0xe15d10f3812c0d9a6c0d30cf5e84868309e0cc28b0fd7272f03df90ca4de222c"
    );
    assert_eq!(deliveries[3]["deposit"]["reference"], format!("0x{}", "ab".repeat(32)));
    assert_eq!(deliveries[3]["data"]["receipt"], settled_receipt(&payment_address), "full receipt relayed");

    // 7. The status route shows the whole timeline.
    let view: Value = h
        .http
        .get(h.url(&format!("/v1/deposit/id/{id}")))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["status"], "settled");
    assert_eq!(view["confirmed_amount"], "2500000");
    let timeline: Vec<&str> = view["events"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        timeline,
        [
            "deposit.created",
            "deposit.watch_registered",
            "deposit.detected",
            "deposit.payment_confirmed",
            "deposit.ready",
            "deposit.settlement_submitted",
            "deposit.settlement_included",
            "deposit.settled",
        ]
    );
    // Nothing left to do and nothing dead.
    let (pending,): (i64,) = sqlx::query_as("SELECT count(*) FROM outbox").fetch_one(&h.pool).await.unwrap();
    assert_eq!(pending, 0);
}

#[tokio::test]
async fn idempotency_validation_and_keys() {
    let h = harness!();
    mount_upstreams(&h).await;
    let key = h.api_key_for("did:privy:bob").await;
    let body = deposit_body();

    let first: Value = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .header("Idempotency-Key", "same")
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .header("Idempotency-Key", "same")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(replay.status(), 200);
    assert_eq!(replay.headers().get("idempotent-replayed").unwrap(), "true");
    let replayed: Value = replay.json().await.unwrap();
    assert_eq!(replayed["id"], first["id"]);
    assert_eq!(replayed["payment_address"], first["payment_address"]);

    let mut other = body.clone();
    other["amount"] = json!("2500001");
    let conflict = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .header("Idempotency-Key", "same")
        .json(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);
    assert_eq!(conflict.json::<Value>().await.unwrap()["error"]["code"], "idempotency_conflict");

    // Without a key every call is a new deposit with a new address.
    let a: Value =
        h.http.post(h.url("/v1/deposit")).bearer_auth(&key).json(&body).send().await.unwrap().json().await.unwrap();
    let b: Value =
        h.http.post(h.url("/v1/deposit")).bearer_auth(&key).json(&body).send().await.unwrap().json().await.unwrap();
    assert_ne!(a["payment_address"], b["payment_address"]);

    // Validation fails fast with a precise code.
    let mut bad = body.clone();
    bad["amount"] = json!(2500000);
    let res = h.http.post(h.url("/v1/deposit")).bearer_auth(&key).json(&bad).send().await.unwrap();
    assert_eq!(res.status(), 400);
    let mut bad = body.clone();
    bad["chain_id"] = json!(1);
    let res = h.http.post(h.url("/v1/deposit")).bearer_auth(&key).json(&bad).send().await.unwrap();
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "unsupported_chain");
    let res = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .body("{not json")
        .header("content-type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let mut bad = body.clone();
    bad["webhook_url"] = json!("ftp://x.example.com");
    let res = h.http.post(h.url("/v1/deposit")).bearer_auth(&key).json(&bad).send().await.unwrap();
    assert_eq!(res.status(), 400);

    // Auth.
    let res = h.http.post(h.url("/v1/deposit")).json(&body).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = h.http.post(h.url("/v1/deposit")).bearer_auth("gum_sk_deadbeef").json(&body).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = h.http.post(h.url("/v1/deposit")).header("x-api-key", &key).json(&body).send().await.unwrap();
    assert_eq!(res.status(), 201, "X-Api-Key works too");

    // One canonical key: a second create is refused, rotation replaces it.
    let token = h.privy_token("did:privy:bob");
    let res = h.http.post(h.url("/v1/account/api-key")).bearer_auth(&token).send().await.unwrap();
    assert_eq!(res.status(), 409);
    let res = h.http.post(h.url("/v1/account/api-key/rotate")).bearer_auth(&key).send().await.unwrap();
    assert_eq!(res.status(), 403, "key management needs a Privy session");
    let rotated: Value = h
        .http
        .post(h.url("/v1/account/api-key/rotate"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new_key = rotated["api_key"].as_str().unwrap();
    assert_ne!(new_key, key);
    let res = h.http.get(h.url("/v1/deposit")).bearer_auth(&key).send().await.unwrap();
    assert_eq!(res.status(), 401, "old key is dead");
    let res = h.http.get(h.url("/v1/deposit")).bearer_auth(new_key).send().await.unwrap();
    assert_eq!(res.status(), 200);
    // The Privy identity-token header is accepted as well.
    let res = h.http.get(h.url("/v1/account")).header("privy-id-token", &token).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let res = h.http.get(h.url("/v1/account")).header("privy-id-token", "garbage").send().await.unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn listing_pagination_and_ownership() {
    let h = harness!();
    mount_upstreams(&h).await;
    let alice = h.api_key_for("did:privy:alice2").await;
    let carol = h.api_key_for("did:privy:carol").await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let mut body = deposit_body();
        body["reference"] = json!(format!("0x{:064x}", i + 1));
        let v: Value = h
            .http
            .post(h.url("/v1/deposit"))
            .bearer_auth(&alice)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ids.push(v["id"].as_str().unwrap().to_owned());
    }

    let page1: Value = h
        .http
        .get(h.url("/v1/deposit/user/did:privy:alice2?limit=2"))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(page1["items"][0]["id"], ids[2], "newest first");
    let cursor = page1["next_cursor"].as_str().unwrap();
    let page2: Value = h
        .http
        .get(h.url(&format!("/v1/deposit/user/did:privy:alice2?limit=2&cursor={cursor}")))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["items"][0]["id"], ids[0]);
    assert!(page2.get("next_cursor").is_none());

    let by_ref: Value = h
        .http
        .get(h.url(&format!("/v1/deposit?reference=0x{:064X}", 2)))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(by_ref["items"].as_array().unwrap().len(), 1);
    assert_eq!(by_ref["items"][0]["id"], ids[1]);
    let by_status: Value =
        h.http.get(h.url("/v1/deposit?status=settled")).bearer_auth(&alice).send().await.unwrap().json().await.unwrap();
    assert_eq!(by_status["items"].as_array().unwrap().len(), 0);
    let res = h.http.get(h.url("/v1/deposit?status=bogus")).bearer_auth(&alice).send().await.unwrap();
    assert_eq!(res.status(), 400);

    // Carol cannot see Alice's deposits.
    let res = h.http.get(h.url("/v1/deposit/user/did:privy:alice2")).bearer_auth(&carol).send().await.unwrap();
    assert_eq!(res.status(), 403);
    let res = h.http.get(h.url(&format!("/v1/deposit/id/{}", ids[0]))).bearer_auth(&carol).send().await.unwrap();
    assert_eq!(res.status(), 404);
    let mine: Value = h.http.get(h.url("/v1/deposit")).bearer_auth(&carol).send().await.unwrap().json().await.unwrap();
    assert_eq!(mine["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn inbound_webhooks_are_verified_and_unknown_ones_acknowledged() {
    let h = harness!();
    let res = h
        .deliver(
            "/v1/webhooks/indexer",
            "wrong-secret",
            &indexer_event("x", "payment.pending", "0x0000000000000000000000000000000000000001", "0", None),
        )
        .await;
    assert_eq!(res.status(), 401);
    let res = h.http.post(h.url("/v1/webhooks/engine")).json(&json!({})).send().await.unwrap();
    assert_eq!(res.status(), 401);
    // Signed but for a deposit we never issued: acknowledged so the sender stops retrying.
    let res = h
        .deliver(
            "/v1/webhooks/indexer",
            INDEXER_SECRET,
            &indexer_event("x", "payment.pending", "0x0000000000000000000000000000000000000001", "0", None),
        )
        .await;
    assert_eq!(res.status(), 200);
    let res = h
        .deliver("/v1/webhooks/engine", ENGINE_SECRET, &engine_event("y", "transaction.failed", None, None, None))
        .await;
    assert_eq!(res.status(), 200);
    // Signed garbage is rejected as such.
    let res = h.deliver("/v1/webhooks/indexer", INDEXER_SECRET, &json!({ "hello": 1 })).await;
    assert_eq!(res.status(), 400);
}

#[tokio::test]
async fn settlement_failure_expiry_and_operator_retry() {
    let h = harness!();
    mount_upstreams(&h).await;
    let key = h.api_key_for("did:privy:dave").await;
    let account: Value = h.http.get(h.url("/v1/account")).bearer_auth(&key).send().await.unwrap().json().await.unwrap();
    let webhook_secret = account["webhook_secret"].as_str().unwrap().to_owned();
    // Account-level default webhook is used when the deposit has none.
    let res = h
        .http
        .patch(h.url("/v1/account"))
        .bearer_auth(&key)
        .json(&json!({ "webhook_url": format!("{}/hooks", h.app.uri()) }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let created: Value = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .json(&deposit_body())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let payment_address = created["payment_address"].as_str().unwrap().to_owned();
    h.wait_for("watch", Duration::from_secs(5), || async {
        sqlx::query_as::<_, (Option<Uuid>,)>("SELECT watch_id FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap()
            .0
    })
    .await;

    h.deliver(
        "/v1/webhooks/indexer",
        INDEXER_SECRET,
        &indexer_event("t-1", "threshold.reached", &payment_address, "2500000", None),
    )
    .await;
    h.wait_for("engine submission", Duration::from_secs(5), || async {
        sqlx::query_as::<_, (Option<Uuid>,)>("SELECT engine_job_id FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap()
            .0
    })
    .await;

    // The engine's simulation reverted: a permanent failure the app hears about.
    let error = json!({ "code": "simulation_reverted", "message": "CREATE3.DeploymentFailed", "revert_data": "0x" });
    let res = h
        .deliver(
            "/v1/webhooks/engine",
            ENGINE_SECRET,
            &engine_event("f-1", "transaction.failed", None, None, Some(error)),
        )
        .await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.deposit_status(id).await, "failed");
    let failed = h
        .wait_for("deposit.failed webhook", Duration::from_secs(5), || async {
            app_deliveries(&h, &webhook_secret).await.into_iter().find(|d| d["type"] == "deposit.failed")
        })
        .await;
    assert_eq!(failed["deposit"]["failure"]["code"], "engine_simulation_reverted");

    // Operator retry queues a fresh execute under a new idempotency key.
    let res = h
        .http
        .post(h.url(&format!("/v1/admin/deposits/{id}/retry-settlement")))
        .bearer_auth("nope")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = h
        .http
        .post(h.url(&format!("/v1/admin/deposits/{id}/retry-settlement")))
        .bearer_auth(ADMIN_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    assert_eq!(h.deposit_status(id).await, "paid");
    h.wait_for("second engine submission", Duration::from_secs(5), || async {
        let reqs = h.engine.received_requests().await.unwrap();
        (reqs.len() == 2).then_some(())
    })
    .await;
    let reqs = h.engine.received_requests().await.unwrap();
    let k1 = reqs[0].headers.get("idempotency-key").unwrap().to_str().unwrap();
    let k2 = reqs[1].headers.get("idempotency-key").unwrap().to_str().unwrap();
    assert_ne!(k1, k2);

    // A transient engine failure (never executed) is resubmitted automatically, silently.
    let error = json!({ "code": "stuck_cancelled", "message": "replaced by cancel", "revert_data": null });
    let mut ev = engine_event("f-2", "transaction.failed", None, None, Some(error));
    ev["tx_hash"] = Value::Null;
    h.deliver("/v1/webhooks/engine", ENGINE_SECRET, &ev).await;
    assert_eq!(h.deposit_status(id).await, "paid");
    h.wait_for("third engine submission", Duration::from_secs(5), || async {
        (h.engine.received_requests().await.unwrap().len() == 3).then_some(())
    })
    .await;
    let failures = app_deliveries(&h, &webhook_secret).await.iter().filter(|d| d["type"] == "deposit.failed").count();
    assert_eq!(failures, 1, "the automatic retry did not alarm the app");

    // A second deposit expires unpaid.
    let created: Value = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .json(&deposit_body())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id2: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let addr2 = created["payment_address"].as_str().unwrap().to_owned();
    let mut ev = indexer_event("x-1", "watch.expired", &addr2, "0", None);
    ev["watch"]["id"] = json!(Uuid::now_v7());
    ev["watch"]["status"] = json!("expired");
    let res = h.deliver("/v1/webhooks/indexer", INDEXER_SECRET, &ev).await;
    assert_eq!(res.status(), 200);
    assert_eq!(h.deposit_status(id2).await, "expired");
    // A late transfer does not resurrect it.
    let mut ev = indexer_event("x-2", "payment.pending", &addr2, "0", None);
    ev["watch"]["id"] = json!(Uuid::now_v7());
    h.deliver("/v1/webhooks/indexer", INDEXER_SECRET, &ev).await;
    assert_eq!(h.deposit_status(id2).await, "expired");
}

#[tokio::test]
async fn health_and_reference_routes() {
    let h = harness!();
    let res = h.http.get(h.url("/healthz")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers().get("x-request-id").is_some());
    let res = h.http.get(h.url("/readyz")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["db"], true);
    let res = h.http.get(h.url("/metrics")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let chains: Value = h.http.get(h.url("/v1/chains")).send().await.unwrap().json().await.unwrap();
    assert_eq!(chains["chains"][0]["chain_id"], 31337);
    assert_eq!(chains["chains"][0]["tokens"][0]["symbol"], "USDC");
    let res = h.http.get(h.url("/nope")).send().await.unwrap();
    assert_eq!(res.status(), 404);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "not_found");
}

async fn age(pool: &sqlx::PgPool, id: Uuid, secs: i64) {
    sqlx::query("UPDATE deposits SET updated_at = now() - make_interval(secs => $2) WHERE id = $1")
        .bind(id)
        .bind(secs as f64)
        .execute(pool)
        .await
        .unwrap();
}

/// Webhooks are the fast path; the reconciler must reach the same outcome without them.
#[tokio::test]
async fn reconciler_recovers_lost_webhooks() {
    let h = harness!();
    mount_upstreams(&h).await;
    let key = h.api_key_for("did:privy:erin").await;
    let created: Value = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .json(&deposit_body())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let payment_address = created["payment_address"].as_str().unwrap().to_owned();
    h.wait_for("watch", Duration::from_secs(5), || async {
        sqlx::query_as::<_, (Option<Uuid>,)>("SELECT watch_id FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap()
            .0
    })
    .await;

    // 1. A fresh deposit is left alone; a quiet one is compared with the indexer's watch.
    let watch_json = |status: &str, confirmed: &str| {
        json!({ "id": WATCH_ID, "chain": "anvil", "chain_id": 31337, "token": "USDC", "token_address": USDC,
                "payment_address": payment_address, "balance_threshold": "2500000", "confirmed_amount": confirmed,
                "status": status, "webhook_endpoint": "x", "start_block": 1, "created_at": Utc::now(),
                "expires_at": null, "completed_at": null, "transfers": [] })
    };
    Mock::given(method("GET"))
        .and(path(format!("/v1/watches/{WATCH_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(watch_json("active", "1000000")))
        .up_to_n_times(1)
        .mount(&h.indexer)
        .await;
    gum_server::reconciler::tick(&h.state).await.unwrap();
    assert_eq!(h.deposit_status(id).await, "pending", "not stale yet");
    age(&h.pool, id, 3600).await;
    gum_server::reconciler::tick(&h.state).await.unwrap();
    assert_eq!(h.deposit_status(id).await, "partial_paid");
    let view: Value = h
        .http
        .get(h.url(&format!("/v1/deposit/id/{id}")))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["confirmed_amount"], "1000000");

    // 2. The indexer completed the watch but threshold.reached never arrived.
    Mock::given(method("GET"))
        .and(path(format!("/v1/watches/{WATCH_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(watch_json("completed", "2500000")))
        .mount(&h.indexer)
        .await;
    age(&h.pool, id, 3600).await;
    gum_server::reconciler::tick(&h.state).await.unwrap();
    assert_eq!(h.deposit_status(id).await, "paid");
    h.wait_for("engine submission", Duration::from_secs(5), || async {
        sqlx::query_as::<_, (Option<Uuid>,)>("SELECT engine_job_id FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap()
            .0
    })
    .await;

    // 3. The engine lost our job (404): it is resubmitted under a new idempotency key.
    Mock::given(method("GET"))
        .and(path(format!("/v1/transactions/{JOB_ID}")))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({ "error": { "code": "not_found", "message": "" } })),
        )
        .up_to_n_times(1)
        .mount(&h.engine)
        .await;
    age(&h.pool, id, 3600).await;
    gum_server::reconciler::tick(&h.state).await.unwrap();
    h.wait_for("resubmission", Duration::from_secs(5), || async {
        (h.engine.received_requests().await.unwrap().iter().filter(|r| r.method == "POST").count() == 2).then_some(())
    })
    .await;
    let posts: Vec<String> = h
        .engine
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| r.headers.get("idempotency-key").unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_ne!(posts[0], posts[1]);
    assert_eq!(h.deposit_status(id).await, "paid");

    // 4. The engine confirmed the job but transaction.confirmed never arrived.
    Mock::given(method("GET"))
        .and(path(format!("/v1/transactions/{JOB_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": JOB_ID, "chain_id": 31337, "status": "confirmed", "outcome": "success",
            "tx_hash": "0xe15d10f3812c0d9a6c0d30cf5e84868309e0cc28b0fd7272f03df90ca4de222c", "block_number": 5,
            "receipt": settled_receipt(&payment_address), "error": null
        })))
        .mount(&h.engine)
        .await;
    h.wait_for("job recorded", Duration::from_secs(5), || async {
        sqlx::query_as::<_, (Option<Uuid>,)>("SELECT engine_job_id FROM deposits WHERE id = $1")
            .bind(id)
            .fetch_one(&h.pool)
            .await
            .unwrap()
            .0
    })
    .await;
    age(&h.pool, id, 3600).await;
    gum_server::reconciler::tick(&h.state).await.unwrap();
    assert_eq!(h.deposit_status(id).await, "settled");
    let view: Value = h
        .http
        .get(h.url(&format!("/v1/deposit/id/{id}")))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let timeline: Vec<&str> = view["events"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert!(timeline.contains(&"deposit.settlement_retried"));
    assert_eq!(timeline.last().copied(), Some("deposit.settled"));
    assert_eq!(view["events"].as_array().unwrap().last().unwrap()["data"]["source"], "reconciler");

    // 5. A watch the indexer no longer knows is re-registered.
    let created: Value = h
        .http
        .post(h.url("/v1/deposit"))
        .bearer_auth(&key)
        .json(&deposit_body())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id2: Uuid = created["id"].as_str().unwrap().parse().unwrap();
    h.wait_for("watch 2", Duration::from_secs(5), || async {
        sqlx::query_as::<_, (Option<Uuid>,)>("SELECT watch_id FROM deposits WHERE id = $1")
            .bind(id2)
            .fetch_one(&h.pool)
            .await
            .unwrap()
            .0
    })
    .await;
    // Point this deposit at a watch id the mock does not serve.
    let lost = Uuid::now_v7();
    sqlx::query("UPDATE deposits SET watch_id = $2, updated_at = now() - interval '1 hour' WHERE id = $1")
        .bind(id2)
        .bind(lost)
        .execute(&h.pool)
        .await
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/v1/watches/{lost}")))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({ "error": { "code": "not_found", "message": "" } })),
        )
        .mount(&h.indexer)
        .await;
    let watches_before = h.indexer.received_requests().await.unwrap().iter().filter(|r| r.method == "POST").count();
    gum_server::reconciler::tick(&h.state).await.unwrap();
    let watch_id = h
        .wait_for("re-registration", Duration::from_secs(5), || async {
            sqlx::query_as::<_, (Option<Uuid>,)>("SELECT watch_id FROM deposits WHERE id = $1")
                .bind(id2)
                .fetch_one(&h.pool)
                .await
                .unwrap()
                .0
                .filter(|w| *w != lost)
        })
        .await;
    assert_eq!(watch_id.to_string(), WATCH_ID);
    let posts = h.indexer.received_requests().await.unwrap().iter().filter(|r| r.method == "POST").count();
    assert_eq!(posts, watches_before + 1);
}
