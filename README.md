# gum-server

The API in front of Gum's stablecoin deposits. An app asks for a one-time payment address, hands it to
its user, and gets told when the payment landed and when it was routed to the app's chosen receiver.
Rust (axum + sqlx + Postgres), deployed on Railway.

Every blockchain interaction is delegated: [gum-indexer](https://github.com/nkrishang/gum-indexer)
watches the address, [gum-engine](https://github.com/nkrishang/gum-engine) executes the settlement, and
both call back into this service. gum-server itself never opens an RPC connection, so the request path
is a CREATE3 computation and one database transaction.

```
app ──POST /v1/deposit──▶ gum-server ──(outbox)──▶ gum-indexer  POST /v1/watches
                           │  ◀──── webhook ──────────┘   payment.pending / .confirmed / threshold.reached
                           ├──(outbox)──▶ gum-engine   POST /v1/transactions  PaymentFactory.execute(...)
                           │  ◀──── webhook ──────────┘   transaction.included / .confirmed / .failed
                           └──(outbox)──▶ app webhook  deposit.detected / .ready / .settled / .failed
```

## The deposit lifecycle

| Status | Meaning |
|---|---|
| `pending` | Address issued and watched. Waiting for the payer. |
| `partial_paid` | gum-indexer saw a transfer (pending at head, or confirmed below the amount). |
| `paid` | Confirmed total ≥ amount. `PaymentFactory.execute` submitted to gum-engine. |
| `settled` | `Payment` deployed; the receipt carries `Settled(receiver, amount)`. Terminal. |
| `failed` | Settlement failed (`failure.code` says why). Retryable by an operator. Terminal. |
| `expired` | `expires_at` passed before the amount was paid. Anything sent later is recoverable. Terminal. |

The payment address is `PaymentFactory.paymentAddress(token, amount, receiver, expirationTimestamp,
recovery, salt, chainId)` from [gum-contracts](https://github.com/nkrishang/gum-contracts), computed
off-chain ([`src/chain/payment.rs`](src/chain/payment.rs)) and pinned against the deployed factory by
tests. `recovery` is always this service's own privileged address (`payments.recovery_address`); `salt`
is random per deposit. The address commits to all seven terms, so nobody can redirect the funds.

## API

Conventions: JSON; addresses and hashes are `0x`-prefixed lowercase hex; token amounts are **base-unit
integer strings** (USDC has 6 decimals, so `"2500000"` = 2.5 USDC); timestamps are RFC 3339. Errors are
`{"error":{"code":"…","message":"…"}}`. Every response carries `x-request-id`.

### Authentication

| Credential | Header | Who |
|---|---|---|
| API key `gum_sk_…` | `Authorization: Bearer <key>` or `X-Api-Key: <key>` | The app's backend |
| Privy access token | `Authorization: Bearer <jwt>` | The web UI |
| Privy identity token | `privy-id-token: <jwt>` | The web UI |

Privy tokens are verified locally (ES256, issuer `privy.io`, audience = `privy.app_id`, key from
`privy.verification_key`); the `sub` DID is the user id. API keys resolve to the user that created them.
Key management routes require a Privy session so a leaked key cannot rotate itself.

### Deposits

```
POST /v1/deposit
Idempotency-Key: <≤128 chars, optional but recommended>

{ "chain_id": "base",                       // id (8453) or slug
  "token": "USDC",                          // symbol or contract address; must be in the chain's registry
  "amount": "2500000",                      // base units, whole-number string; numbers and decimals are rejected
  "receiver": "0x…",                        // where the funds go; any non-zero address
  "expires_at": "2026-10-01T00:00:00Z",     // between payments.min_expiry_lead_secs and max_expiry_secs from now
  "reference": "0x…",                       // optional bytes32 for the app's own bookkeeping
  "webhook_url": "https://…" }              // optional; defaults to the account's webhook_url
```

| Status | Meaning |
|---|---|
| `201` | Created. Body: the deposit (below) with `payment_address`. |
| `200` + `Idempotent-Replayed: true` | Same key and same request seen before; the original deposit. |
| `409 idempotency_conflict` | Same key, different request. |
| `400 invalid_request` / `unsupported_chain` / `unsupported_token` | Validation failed. Unknown fields are rejected. |
| `401` / `429 rate_limited` | Bad key / per-key limit exceeded. |
| `503 store_unavailable` / `shedding` | Back off and retry. |

```json
{ "id": "…", "status": "pending", "payment_address": "0x…", "chain_id": 8453,
  "token": "USDC", "token_address": "0x…", "token_decimals": 6,
  "amount": "2500000", "confirmed_amount": "0", "receiver": "0x…", "recovery": "0x…", "salt": "0x…",
  "reference": "0x…", "webhook_url": "…", "expires_at": "…",
  "tx_hash": null, "block_number": null, "failure": null,
  "timestamps": { "created_at": "…", "updated_at": "…", "detected_at": null, "settled_at": null, "failed_at": null, "expired_at": null } }
```

```
GET /v1/deposit/id/{id}            the deposit plus its full `events` timeline (owner only; 404 otherwise)
GET /v1/deposit                    the caller's deposits
GET /v1/deposit/user/{user_id}     same, for the web UI (403 unless user_id is the caller)
    ?status=&chain_id=&token=&reference=&payment_address=&receiver=
    &created_after=&created_before=&limit=50&cursor=
```

Lists are newest first with keyset pagination: pass `next_cursor` back as `cursor` until it is absent.

### App webhooks

`POST <webhook_url>` with `content-type: application/json`, delivered at-least-once, in order per
deposit (`sequence`), retried with exponential backoff for 24 h. Respond `2xx` within 10 s. Dedupe on `id`.

```json
{ "id": "<event uuid>", "type": "deposit.settled", "created_at": "…", "sequence": 8,
  "deposit": { …the deposit as above, as it was when the event happened… },
  "data": { "engine_job_id": "…", "tx_hash": "0x…", "block_number": 100, "receipt": { …node receipt with logs… }, … } }
```

| Type | When |
|---|---|
| `deposit.detected` | A transfer to the address was seen at chain head. Show a "payment detected" UI. |
| `deposit.payment_confirmed` | A transfer was confirmed; `deposit.confirmed_amount` is the new total. |
| `deposit.payment_orphaned` | A previously detected transfer was reorged out. It was never counted. |
| `deposit.ready` | Confirmed total reached the amount. Settlement was submitted. |
| `deposit.settled` | The receiver was paid. `data.receipt` is the full transaction receipt. Credit the user. |
| `deposit.failed` | Settlement failed; `deposit.failure` has `code` and `message`. |
| `deposit.expired` | Expired before the amount was paid. |

Headers: `X-Gum-Event-Id`, `X-Gum-Event-Type`, `X-Gum-Deposit-Id`, `X-Gum-Delivery-Attempt`,
`X-Gum-Signature: t=<unix>,v1=<hex>` where `v1 = HMAC-SHA256(webhook_secret, "<t>.<raw body>")`.
The `webhook_secret` is on `GET /v1/account`. Reject timestamps older than a few minutes. In
production the URL must be public `https`; private, loopback and `.internal` hosts are refused.

### Account (web UI)

```
GET   /v1/account                         user_id, api_key {prefix, created_at, rotated_at, last_used_at}, webhook_url, webhook_secret
PATCH /v1/account                         { "webhook_url": "https://…" | null }   default for deposits created without one
POST  /v1/account/api-key                 201 { "api_key": "gum_sk_…" }   shown once; 409 api_key_exists if one exists
POST  /v1/account/api-key/rotate          200 { "api_key": "gum_sk_…" }   the old key stops working immediately
POST  /v1/account/webhook-secret/rotate   new webhook_secret
```

A user has exactly one API key. Keys are stored as SHA-256 hashes and cached in memory for
`api_keys.cache_ttl_secs`; a rotation invalidates the local cache at once and the other replicas' within
the TTL.

### Callbacks and operations

```
POST /v1/webhooks/indexer    gum-indexer events (signed with indexer.webhook_secret)
POST /v1/webhooks/engine     gum-engine events  (signed with engine.webhook_secret)
GET  /v1/chains              supported chains, tokens and the factory address
GET  /healthz                liveness
GET  /readyz                 200 when Postgres answers; also reports indexer / engine / privy status
GET  /metrics                Prometheus
POST /v1/admin/deposits/{id}/retry-settlement   failed → paid with a fresh execute   (Authorization: Bearer admin.token)
GET  /v1/admin/outbox                           jobs that needed retries or were given up on
POST /v1/admin/outbox/{id}/requeue
```

## How it stays fast and reliable

- **The request path does no I/O it can avoid.** `POST /v1/deposit` validates, derives the address in
  memory, and commits one transaction (deposit + idempotency key + outbox row). Registering the watch,
  submitting the settlement and notifying the app are outbox jobs executed by background workers with
  exponential backoff, so an upstream outage never fails or slows the API.
- **Retry policy per job.** Jobs towards gum-indexer / gum-engine are on the settlement's critical path
  (an `execute` after `expires_at` pays recovery, not the receiver): they back off to
  `outbox.upstream_retry_cap_ms` (30 s) and never give up — they end when the deposit is terminal or
  expired. App webhooks back off to an hour and are marked dead after `webhooks.max_age_secs` (24 h);
  dead ones are listed on `/v1/admin/outbox` and can be requeued.
- **Webhooks are acknowledged as soon as their state change commits.** Each inbound event is verified,
  deduplicated on its id *inside the same transaction* as the transition it causes, and answered `200`.
  Nothing slow happens on the sender's clock. Database trouble returns `503` so the sender retries.
- **Transitions are guarded.** Every update is `WHERE status IN (…)`, so replayed, reordered or
  duplicated events are harmless, and per-(deposit, kind) outbox ordering keeps app webhooks in sequence.
- **Idempotent everywhere.** Client idempotency keys on `POST /v1/deposit`; `Idempotency-Key` on every
  engine submission (a crash between submit and record cannot broadcast twice); indexer registration
  is idempotent on the address.
- **Settlement is verified, not assumed.** `settled` requires the engine's receipt to contain
  `Settled(receiver, amount)` from the payment address. A `Recovered`-only receipt (executed after
  expiry) is `failed / expired_on_chain`. Engine failures that never executed (`expired`,
  `stuck_cancelled`, `internal`) are resubmitted up to three times before the app is told.
- **A reconciler makes webhooks an optimisation, not a dependency.** Every `reconciler.interval_secs`
  it compares quiet open deposits with the indexer's watch (`GET /v1/watches/{id}`: applies a lost
  `payment.confirmed` / `threshold.reached` / `watch.expired`, re-registers a watch the indexer no longer
  knows) and quiet `paid` deposits with the engine's job (`GET /v1/transactions/{id}`: applies a lost
  `transaction.confirmed` / `.failed`, resubmits a job the engine no longer knows). It applies exactly
  the guarded transitions a webhook would, so it is safe on every replica. Local expiry without the
  indexer's say-so only happens a full day after `expires_at`.
- **Backpressure and limits.** Per-key token buckets, a body limit, a request timeout, a concurrency
  cap with load shedding (`503 shedding`), panic isolation, graceful drain on SIGTERM.
- **Multiple replicas are safe.** Outbox claims use `FOR UPDATE SKIP LOCKED`; every write is idempotent.

Observability: single-line JSON logs on stdout (`RUST_LOG`; `GUM_LOG_FORMAT=pretty` locally), and
Prometheus metrics: `gum_http_request_duration_seconds{route,method,status}`,
`gum_deposits_created_total`, `gum_deposit_transitions_total{event}`,
`gum_inbound_webhooks_total{source,outcome,type}`, `gum_outbox_pending`, `gum_outbox_dead`,
`gum_outbox_lag_seconds`, `gum_outbox_jobs_total{kind,outcome}`,
`gum_upstream_request_duration_seconds{service,op,outcome}`, `gum_app_webhook_deliveries_total{outcome}`,
`gum_auth_total{method,outcome}`, `gum_db_errors_total`. Alert on `gum_outbox_dead > 0`, on
`gum_outbox_lag_seconds` p99, and on `/readyz` flipping.

## Configuration

`config/default.toml` (chains, tokens, tuning) → `config/<GUM_PROFILE>.toml` → environment
(`GUM_<SECTION>__<KEY>`). `DATABASE_URL` and `PORT` are read as-is (Railway provides both).

| Variable | |
|---|---|
| `DATABASE_URL` | Postgres |
| `GUM_SERVER__PUBLIC_BASE_URL` | This service's public origin; callbacks are `<origin>/v1/webhooks/{indexer,engine}` |
| `GUM_PRIVY__APP_ID`, `GUM_PRIVY__VERIFICATION_KEY` | From the Privy dashboard (PEM; `\n` escapes accepted) |
| `GUM_PAYMENTS__FACTORY_ADDRESS`, `GUM_PAYMENTS__RECOVERY_ADDRESS` | The `PaymentFactory` generation and our recovery wallet |
| `GUM_INDEXER__BASE_URL`, `GUM_INDEXER__API_KEY`, `GUM_INDEXER__WEBHOOK_SECRET` | gum-indexer's private URL, one of its `GUM_API__KEYS`, its `GUM_WEBHOOK__SECRET` |
| `GUM_ENGINE__BASE_URL`, `GUM_ENGINE__WEBHOOK_SECRET` | gum-engine's private URL (no auth), its `webhook.signing_secret` |
| `GUM_ADMIN__TOKEN` | Enables `/v1/admin` |
| `PORT`, `RUST_LOG` | |

The chain/token registry in `config/default.toml` must mirror gum-indexer's; adding a chain or token
is a config-only change on both.

## Local development

```sh
docker compose up -d postgres
cp .env.example .env && set -a && . ./.env && set +a
cargo run                     # GUM_PROFILE=local: Anvil chain, insecure webhook targets allowed
```

For a full local loop run Anvil with gum-contracts' `LocalBootstrap.s.sol`, gum-indexer and gum-engine
against it, and point the `GUM_INDEXER__*` / `GUM_ENGINE__*` variables at them.

## Tests

```sh
cargo test                                                    # unit tests only
TEST_DATABASE_URL=postgres://gum:gum@127.0.0.1:54340/gum_server cargo test   # + end-to-end
```

The end-to-end suite starts the real server on a random port with a fresh database per test and
stubs gum-indexer, gum-engine and the app webhook with wiremock. It walks the whole lifecycle
(create → watch registered → partial_paid → paid → execute submitted → settled → app notified),
idempotency, key rotation, pagination, ownership, signature verification, failure, automatic and
operator retries, and expiry. The CREATE3 derivation and the `Settled` topic are pinned against
values observed from the real contracts on Anvil.

## Deploying

Railway, EU West, Dockerfile deploy (`railway.json`). The engine and indexer are on the same project's
private network; reference them as `http://gum-engine.railway.internal:8080` and
`http://gum-indexer.railway.internal:8080`.

1. Add a Postgres plugin; Railway injects `DATABASE_URL`. Migrations run at boot (`database.auto_migrate`).
2. Generate a public domain and set `GUM_SERVER__PUBLIC_BASE_URL` to it. Both upstreams need to reach
   `/v1/webhooks/*`; gum-indexer additionally requires the webhook endpoint to be public `https`.
3. Set the variables in the table above. Give gum-indexer's `GUM_WEBHOOK__SECRET` and gum-engine's
   `webhook.signing_secret` to this service as `GUM_INDEXER__WEBHOOK_SECRET` / `GUM_ENGINE__WEBHOOK_SECRET`.
4. Set the region to EU West on the service settings so it sits next to Postgres and the other two services.
5. `/readyz` is the health check. Scale replicas freely.
