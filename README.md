# gum-server

The API in front of Gum's stablecoin deposits. An app asks for a one-time payment address, hands it to
its user, and gets told when the payment landed and when it was routed to the app's chosen receiver.
Rust (axum + sqlx + Postgres), deployed on Railway.

Every blockchain interaction is delegated: [gum-indexer](https://github.com/nkrishang/gum-indexer)
watches the address, [gum-engine](https://github.com/nkrishang/gum-engine) executes the settlement, and
both call back into this service. gum-server itself never opens an RPC connection, so the request path
is a CREATE2 computation and one database transaction.

```
app ──POST /v1/deposit──▶ gum-server ──(outbox)──▶ gum-indexer  POST /v1/watches
                           │  ◀──── webhook ──────────┘   payment.pending / .confirmed / threshold.reached
                           ├──(outbox)──▶ gum-engine   POST /v1/transactions  PaymentFactory.execute(...)
                           │  ◀──── webhook ──────────┘   transaction.included / .confirmed / .failed
                           └──(outbox)──▶ app webhook  deposit.detected / .ready / .settled / .failed
```

## Supported chains

| Chain | Tokens |
|---|---|
| Monad (143) | USDC, USDT (USDT0), AUSD |
| Arbitrum One (42161) | USDC, USDT (USDT0) |
| Base (8453) | USDC |
| Arc (5042) ¹ | USDC |

All tokens have 6 decimals. `GET /v1/chains` lists the chains offered right now.

¹ Off until `GUM_CHAINS__ARC__ENABLED=true` (see [Adding a chain](#adding-a-chain)). On Arc, USDC is
the gas token: one balance with a native interface (18 decimals, `msg.value`) and an ERC-20 at
`0x3600000000000000000000000000000000000000` (6 decimals). A deposit uses the ERC-20 and 6-decimal
amounts, as on every other chain. The payer may send either way: a plain native USDC send to the
payment address counts too, because gum-indexer reads Arc's EIP-7708 system emitter, which logs both,
and reports amounts scaled to 6 decimals. The settlement's `transfer` spends that balance through the
ERC-20, whichever way it arrived. A native amount below 0.000001 USDC is below the ERC-20's precision: it is not counted,
and it stays at the payment address.

## The deposit lifecycle

| Status | Meaning |
|---|---|
| `pending` | Address issued and watched. Waiting for the payer. |
| `partial_paid` | gum-indexer saw a transfer (pending at head, or confirmed below the amount). |
| `paid` | Confirmed total ≥ amount. `PaymentFactory.execute` submitted to gum-engine. |
| `settled` | `Payment` deployed; its settlement calls paid the receiver and the receipt carries `Settled(token, amount)`. Terminal. |
| `failed` | Settlement failed (`failure.code` says why). Retryable by an operator. Terminal. |
| `expired` | `expires_at` passed before the amount was paid. Anything sent later is recoverable. Terminal. |

The payment address is `PaymentFactory.paymentAddress(token, amount, calls, expirationTimestamp,
recovery, salt, chainId)` from [gum-contracts](https://github.com/nkrishang/gum-contracts), computed
off-chain ([`src/chain/payment.rs`](src/chain/payment.rs)) and pinned against the deployed factory by
tests. `calls` is the ordered list of calls the payment makes on settlement; for a deposit it is the single
call `token.transfer(receiver, amount)`, stored with the deposit and returned as `calls`. `recovery` is
always this service's own privileged address (`payments.recovery_address`); `salt` is random per deposit.
The address commits to all seven terms, including every call target and every byte of calldata, so
nobody can redirect the funds.

```
terms    = abi.encode(token, amount, calls, expirationTimestamp, recovery, salt, chainId)
initCode = Payment.creationCode ++ abi.encode(CREATE(factory, nonce = 1), terms)
payment  = CREATE2(factory, bytes32(0), keccak256(initCode))
```

`Payment.creationCode` belongs to one contract generation and is pinned in
[`src/chain/payment_creation_code.hex`](src/chain/payment_creation_code.hex). `payments.factory_address`
must be a factory of that generation: pointing it at another generation derives addresses that factory
will never deploy.

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
  "amount": "2500000", "confirmed_amount": "0", "receiver": "0x…",
  "calls": [{ "target": "0x…token", "data": "0xa9059cbb…" }],   // token.transfer(receiver, amount)
  "recovery": "0x…", "salt": "0x…",
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

#### Why a settlement failed

`failure` is `{ "code", "message" }`, plus `revert` when the chain said why. `PaymentFactory.execute`
passes on the `Payment` constructor's revert data, which wraps a failing settlement call's own revert
data, and gum-engine reports those bytes when its simulation of `execute` reverts. gum-server decodes
them ([`src/chain/revert.rs`](src/chain/revert.rs)) into a readable `message` and a typed `revert`.
Every level of `revert` carries its raw bytes, so an app can always decode them itself, e.g. against
its own contracts' ABIs:

```ts
type Revert = {
  kind: "decoded" | "empty" | "unrecognised";
  data: string;                   // this level's raw revert data, 0x hex ("0x" when empty)
  selector?: string;              // first 4 bytes of data, when there are at least 4
  name?: string;                  // decoded: "CallFailed", "Error", "ERC20InsufficientBalance", …
  signature?: string;             // decoded: "CallFailed(uint256,bytes)"
  args?: Record<string, string>;  // decoded: uints in decimal, addresses as lowercase 0x hex, strings as-is (a NUL renders as \0)
  call?: { index: number; target: string };  // CallFailed / CallTargetHasNoCode: the deposit's call
  reason?: Revert;                // CallFailed: the call's own revert (its revertData), same shape
};
```

```json
"failure": {
  "code": "engine_simulation_reverted",
  "message": "settlement call 0 (transfer of 2500000 to 0x7099…) reverted: Blacklistable: account is blacklisted",
  "revert": {
    "kind": "decoded", "data": "0x5c0dee5d…", "selector": "0x5c0dee5d",
    "name": "CallFailed", "signature": "CallFailed(uint256,bytes)", "args": { "index": "0" },
    "call": { "index": 0, "target": "0x…token" },
    "reason": {
      "kind": "decoded", "data": "0x08c379a0…", "selector": "0x08c379a0",
      "name": "Error", "signature": "Error(string)", "args": { "message": "Blacklistable: account is blacklisted" }
    }
  }
}
```

| `revert.name` | Meaning |
|---|---|
| `CallFailed` | Settlement call `args.index` reverted; `reason` is the target's own revert. |
| `InsufficientTokenBalance` | The address holds `args.balance`, less than the `args.required` it settles. |
| `AmountNotSpent` | The calls succeeded but left `args.remaining` unspent (e.g. a token that returns `false`). |
| `CallTargetHasNoCode` | A call targets an address with no code on this chain. |
| `AlreadyDeployed` | The payment was already executed; its receipt says whether it settled or went to recovery. |
| `DeploymentFailed` | The constructor reverted without data (e.g. out of gas). |
| `InitCodeTooLarge` | The encoded terms exceed EIP-3860's 49,152-byte init-code limit (`args.length`). |
| `TransferFailed` | `Payment`'s own transfer of the excess or an expired balance to recovery failed. |
| `Error` / `Panic` | Solidity's `require`/`revert` string (`args.message`) or panic (`args.code`). |
| a token's custom error | e.g. `ERC20InsufficientBalance`, `EnforcedPause`, `AccountIsFrozen`, with named `args`. |

`kind: "unrecognised"` is an error we do not know (or a truncated one: `Payment` caps a call's revert
data at 65,535 bytes); decode `data` yourself. `failure.code` keeps its meaning: `engine_simulation_reverted`
when the simulation reverted, `engine_<code>` for the engine's other failures (`engine_failed` when it
sends no error at all, previously the malformed `engine_engine_failed`), `expired_on_chain` /
`wrong_chain` / `unexpected_outcome` from a receipt. A transaction that reverts once mined carries no
revert data (the engine does not trace it), so only simulation failures have `revert`. The
`deposit.failed` event keeps the engine's own message as `data.engine_message`, and the same
`data.revert` (the old `data.revert_data` key is gone).

### Payer view (hosted pay page)

```
GET /v1/pay/{id}?after=<sequence>&wait=<seconds>     no auth; always Cache-Control: no-store
```

For gum.money/pay/{id}. The deposit id is the capability (UUIDv7: not enumerable), so the view is
payer-safe: no owner, `receiver`, `recovery`, `salt`, `reference`, `webhook_url`, engine ids,
receipts or failure messages. Unknown or malformed ids are `404 not_found`.

```json
{ "id": "…", "status": "partial_paid", "sequence": 3, "payment_address": "0x…", "chain_id": 8453,
  "token": "USDC", "token_address": "0x…", "token_decimals": 6, "amount": "2500000", "confirmed_amount": "0",
  "expires_at": "…", "tx_hash": "0x…", "block_number": 123, "failure": { "code": "…" },   // last three only when set
  "timestamps": { …as above… }, "server_time": "2026-09-24T12:00:00.123Z",
  "events": [ { "id": "…", "sequence": 3, "type": "deposit.detected", "created_at": "…",
                "data": { "transfer": { "tx_hash", "log_index", "block_number", "block_hash", "from", "amount", "status" },
                          "confirmed_amount": "0" } } ] }
```

`events` is oldest first and limited to `deposit.created / detected / payment_confirmed /
payment_orphaned / ready / settlement_submitted / settlement_included / settled / failed / expired`;
their sequences have gaps (internal events are hidden, but still advance `sequence`). `data` keeps
only `transfer`, `confirmed_amount`, `tx_hash`, `block_number` and, on `deposit.failed`, `code`.
`server_time` lets the page correct the payer's clock.

**Long polling.** With `after` and `wait` (0–25, default 0), a request whose deposit `sequence` is
still `<= after` is held until it moves past `after` or the wait ends, then answered `200` with the
current view (never 304/204). Holds are capped 1 s under `server.request_timeout_ms` (so 9 s by
default); poll again with the returned `sequence`. Every transition runs `pg_notify` in its
transaction; each replica `LISTEN`s and wakes its waiting requests on commit, so a payment shows up
within milliseconds whichever replica processed it. A notification lost while the listener
reconnects only delays the page until its next poll.

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
| `deposit.failed` | Settlement failed; `deposit.failure` has `code`, `message` and, when known, `revert` (above). |
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
GET  /v1/chains              chains offered for new deposits, their tokens and the factory address
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
  Each handler uses exactly one database connection (its transaction), so a burst of deliveries
  larger than the pool queues for connections instead of deadlocking. An engine event for a job
  we have not recorded yet (the engine can report before our outbox records the job id) gets `503`
  and is retried; an unknown job older than 10 minutes is not ours and is acknowledged.
- **Transitions are guarded.** Every update is `WHERE status IN (…)`, so replayed, reordered or
  duplicated events are harmless, and per-(deposit, kind) outbox ordering keeps app webhooks in sequence.
- **Idempotent everywhere.** Client idempotency keys on `POST /v1/deposit`; `Idempotency-Key` on every
  engine submission (a crash between submit and record cannot broadcast twice); indexer registration
  is idempotent on the address.
- **Settlement is verified, not assumed.** `settled` requires the engine's receipt to contain
  `Settled(token, amount)` from the payment address, which `Payment` emits only after every call
  succeeded and together they spent exactly `amount`. A `Recovered`-only receipt (executed after
  expiry) is `failed / expired_on_chain`. Engine failures that never executed (`expired`,
  `stuck_cancelled`, `internal`) are resubmitted up to three times before the app is told.
- **Failures say what went wrong.** A reverted simulation's data is decoded down to the token's own
  reason (see "Why a settlement failed"), stored raw with the deposit, logged as `settlement failed`
  with `code` and `revert`, and counted in `gum_settlement_failures_total{code,revert}`. The `revert`
  label is the error's name, with a failed call's reason appended (`CallFailed:Error`,
  `CallFailed:ERC20InsufficientBalance`), or `none` / `empty` / `unrecognised`.
- **A reconciler makes webhooks an optimisation, not a dependency.** Every `reconciler.interval_secs`
  it compares quiet open deposits with the indexer's watch (`GET /v1/watches/{id}`: applies a lost
  `payment.confirmed` / `threshold.reached` / `watch.expired`, re-registers a watch the indexer no longer
  knows) and `paid` deposits with the engine's job (`GET /v1/transactions/{id}`: applies a lost
  `transaction.confirmed` / `.failed`, resubmits a job the engine no longer knows). A `paid` deposit is
  polled `reconciler.paid_stale_secs` (60 s) after its settlement was submitted — counted from the
  submission, so late webhooks do not postpone it — and again every `paid_repoll_secs` (30 s). It applies exactly
  the guarded transitions a webhook would, so it is safe on every replica. Local expiry without the
  indexer's say-so only happens a full day after `expires_at`.
- **Backpressure and limits.** Per-key token buckets, a body limit, a request timeout, a concurrency
  cap with load shedding (`503 shedding`), panic isolation, graceful drain on SIGTERM.
- **No unbounded waits.** Every pooled connection carries `statement_timeout`, `lock_timeout` and
  `idle_in_transaction_session_timeout` (`database.*_ms`), so a statement stuck behind a lock or a
  transaction left open fails instead of waiting forever. Each outbox job runs under
  `outbox.job_timeout_ms` (30 s) and is rescheduled if it overruns, which also covers a connection
  that died without closing. Migrations run on their own connection without these limits.
- **Multiple replicas are safe.** Outbox claims use `FOR UPDATE SKIP LOCKED`; every write is idempotent.

Observability: single-line JSON logs on stdout (`RUST_LOG`; `GUM_LOG_FORMAT=pretty` locally), and
Prometheus metrics: `gum_http_request_duration_seconds{route,method,status}`,
`gum_deposits_created_total`, `gum_deposit_transitions_total{event}`, `gum_settlement_failures_total{code,revert}`,
`gum_inbound_webhooks_total{source,outcome,type}`, `gum_outbox_pending`, `gum_outbox_dead`,
`gum_outbox_lag_seconds`, `gum_outbox_jobs_total{kind,outcome}`,
`gum_upstream_request_duration_seconds{service,op,outcome}`, `gum_app_webhook_deliveries_total{outcome}`,
`gum_auth_total{method,outcome}`, `gum_db_errors_total`, `gum_deposits_paid`,
`gum_deposits_paid_oldest_age_seconds`, `gum_outbox_due`, `gum_outbox_oldest_due_age_seconds`,
`gum_panics_total`, `gum_task_restarts_total{task}`. Alert on any panic or task restart: the outbox,
reconciler and deposit feed run under a supervisor that restarts them (`background task stopped
unexpectedly`), and panics are logged as structured `"message":"panic"` errors.
Alert on `gum_outbox_dead > 0`, on `gum_outbox_lag_seconds` p99, on `/readyz` flipping, on
`gum_deposits_paid_oldest_age_seconds` above a few minutes (settlement takes seconds, so an old
`paid` deposit means engine webhooks are not landing), and on
`gum_outbox_oldest_due_age_seconds` above a minute or two: the outbox is not draining (logged as
`outbox is not draining` past 120 s). Neither shows on `/readyz`. Both gauges are computed by the
reconciler, independently of the outbox itself.

## Configuration

`config/default.toml` (chains, tokens, tuning) → `config/<GUM_PROFILE>.toml` → environment
(`GUM_<SECTION>__<KEY>`). `DATABASE_URL` and `PORT` are read as-is (Railway provides both).

| Variable | |
|---|---|
| `DATABASE_URL` | Postgres |
| `GUM_SERVER__CALLBACK_BASE_URL` | Where gum-indexer / gum-engine call back: `<base>/v1/webhooks/{indexer,engine}`. Private network in production: `http://gum-server.railway.internal:8080` |
| `GUM_PRIVY__APP_ID`, `GUM_PRIVY__VERIFICATION_KEY` | From the Privy dashboard (PEM; `\n` escapes accepted) |
| `GUM_PAYMENTS__FACTORY_ADDRESS`, `GUM_PAYMENTS__RECOVERY_ADDRESS` | The `PaymentFactory` generation and our recovery wallet |
| `GUM_INDEXER__BASE_URL`, `GUM_INDEXER__WEBHOOK_SECRET` | gum-indexer's private URL (no auth), its `GUM_WEBHOOK__SECRET` |
| `GUM_ENGINE__BASE_URL`, `GUM_ENGINE__WEBHOOK_SECRET` | gum-engine's private URL (no auth), its `webhook.signing_secret` |
| `GUM_ADMIN__TOKEN` | Enables `/v1/admin` |
| `GUM_SERVER__CORS_ORIGINS` | JSON array of browser origins allowed to call the API (the web UI), e.g. `["https://app.gum.money"]` |
| `GUM_CHAINS__<NAME>__ENABLED` | `true` / `false`: offer a chain for new deposits. Arc ships `false` |
| `PORT`, `RUST_LOG` | |

The chain/token registry in `config/default.toml` must mirror gum-indexer's; adding a chain or token
is a config-only change on both.

### Adding a chain

A chain in `config/default.toml` is offered for new deposits (on `/v1/chains` and by
`POST /v1/deposit`) only while `enabled` is `true`, the default. A disabled chain still resolves, so
deposits made on it earlier are unaffected and can still be listed with `?chain_id=`. A new chain ships
with `enabled = false` and is turned on by setting `GUM_CHAINS__<NAME>__ENABLED=true` on Railway once
all of these are true:

1. **The `PaymentFactory` generation is deployed on it** at `payments.factory_address`
   (`cast code <factory> --rpc-url <chain>` is not `0x`). gum-server derives addresses without any
   RPC, so it cannot check this itself. An address with no factory behind it can be paid, but never
   settled.
2. **gum-indexer has the chain enabled** (`chain ready chain=<name>` in its logs). Otherwise watches
   cannot be registered, and payments are never seen.
3. **gum-engine has the chain running** (`chain.started` for its chain id, with a funded treasury).
   Otherwise settlements queue up in the outbox until it does.

Set the variable only after a deploy of the code that has the chain's table. On older code it is a
chain with no `chain_id`, and the service does not boot. To stop offering a chain, set the variable to
`false`.

For Arc, as of 2026-09-24: the factory is deployed at the usual address, with the same code as on the
other chains, and an `execute` simulated on Arc mainnet settles a payment funded with native USDC
(dust and excess included). gum-indexer ships Arc disabled until its endpoints are set, and gum-engine
runs it once `GUM_CHAINS__ARC__TREASURY_KEY_ID` is set: steps 2 and 3 remain.

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
operator retries, and expiry. The CREATE2 derivation, the `execute` calldata and the `Settled` event
are pinned against values observed from the real contracts on Anvil.

## Deploying

Railway, EU West, via the Railway CLI (`railway up`); Railway builds the `Dockerfile`. Service settings
(region, health check, replicas, domains) live on Railway, not in the repo.

**Topology.** gum-server, gum-indexer and gum-engine must be services of the **same Railway project and
environment**: Railway private networking (`<service>.railway.internal`, IPv6, plain http on the port
the service listens on) does not cross projects, gum-engine has no public domain or auth by design, and
gum-indexer should not need one either. Only gum-server gets a public domain (`api.gum.money`), and
only apps use it. Everything between the three services — API calls out, webhooks back — stays private.

1. Add a Postgres plugin; Railway injects `DATABASE_URL`. Migrations run at boot (`database.auto_migrate`).
2. Set the variables in the table above (`.env.production` is a fill-in template). In particular
   `GUM_SERVER__CALLBACK_BASE_URL=http://gum-server.railway.internal:8080` — the private address, not the
   public domain. Give gum-indexer's `GUM_WEBHOOK__SECRET` and gum-engine's `webhook.signing_secret` to
   this service as `GUM_INDEXER__WEBHOOK_SECRET` / `GUM_ENGINE__WEBHOOK_SECRET`.
3. On gum-indexer and gum-engine set `GUM_WEBHOOK__HOST_ALLOWLIST=["gum-server.railway.internal"]` so
   they deliver to the private callback host and nowhere else.
4. Chains that ship disabled (Arc) are enabled later, one variable each. See [Adding a chain](#adding-a-chain).
5. On the service: region EU West (same as the other two and Postgres), health check path `/readyz`,
   restart on failure, public domain `api.gum.money`. Scale replicas freely.
