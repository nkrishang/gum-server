-- gum-server schema. Addresses and hashes are stored as 0x-prefixed lowercase hex text.
-- Token amounts are base-unit integers (NUMERIC(78,0) fits uint256).

CREATE TABLE users (
    id                  TEXT PRIMARY KEY,               -- Privy DID (did:privy:...)
    webhook_secret      TEXT NOT NULL,                  -- HMAC secret for webhooks we send to this user
    default_webhook_url TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One canonical key per user; rotation rewrites the row in place.
CREATE TABLE api_keys (
    user_id        TEXT PRIMARY KEY REFERENCES users (id),
    key_hash       BYTEA NOT NULL UNIQUE,               -- sha256(secret)
    key_prefix     TEXT NOT NULL,                       -- display only, e.g. gum_sk_1a2b3c4d
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    rotated_at     TIMESTAMPTZ,
    last_used_at   TIMESTAMPTZ
);

CREATE TYPE deposit_status AS ENUM (
    'pending',      -- address issued, waiting for payment
    'partial_paid', -- indexer saw a transfer (pending or confirmed), amount not yet reached
    'paid',         -- amount reached; PaymentFactory.execute submitted to gum-engine
    'settled',      -- Payment deployed and receiver paid
    'failed',       -- settlement failed; see failure_code / failure_message
    'expired'       -- expired before the amount was reached
);

CREATE TABLE deposits (
    id                 UUID PRIMARY KEY,
    user_id            TEXT NOT NULL REFERENCES users (id),
    chain_id           BIGINT NOT NULL,
    token_symbol       TEXT NOT NULL,
    token_address      TEXT NOT NULL,
    token_decimals     SMALLINT NOT NULL,
    amount             NUMERIC(78, 0) NOT NULL,
    receiver           TEXT NOT NULL,
    recovery           TEXT NOT NULL,
    salt               TEXT NOT NULL,
    expires_at         TIMESTAMPTZ NOT NULL,
    payment_address    TEXT NOT NULL,
    reference          TEXT,
    webhook_url        TEXT,
    status             deposit_status NOT NULL DEFAULT 'pending',
    confirmed_amount   NUMERIC(78, 0) NOT NULL DEFAULT 0,
    watch_id           UUID,
    watch_registered_at TIMESTAMPTZ,
    engine_job_id      UUID,
    engine_submitted_at TIMESTAMPTZ,
    tx_hash            TEXT,
    block_number       BIGINT,
    failure_code       TEXT,
    failure_message    TEXT,
    event_seq          BIGINT NOT NULL DEFAULT 0,       -- per-deposit sequence for app webhooks
    detected_at        TIMESTAMPTZ,
    settled_at         TIMESTAMPTZ,
    failed_at          TIMESTAMPTZ,
    expired_at         TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (chain_id, payment_address)
);

CREATE INDEX deposits_user_created_idx ON deposits (user_id, created_at DESC, id DESC);
CREATE INDEX deposits_user_status_created_idx ON deposits (user_id, status, created_at DESC, id DESC);
CREATE INDEX deposits_user_reference_idx ON deposits (user_id, reference) WHERE reference IS NOT NULL;
CREATE INDEX deposits_watch_idx ON deposits (watch_id) WHERE watch_id IS NOT NULL;
CREATE INDEX deposits_engine_job_idx ON deposits (engine_job_id) WHERE engine_job_id IS NOT NULL;
CREATE INDEX deposits_open_expiry_idx ON deposits (expires_at) WHERE status IN ('pending', 'partial_paid');
CREATE INDEX deposits_paid_idx ON deposits (updated_at) WHERE status = 'paid';

-- Timeline of everything that happened to a deposit; served on GET and mirrored to app webhooks.
CREATE TABLE deposit_events (
    id         UUID PRIMARY KEY,
    deposit_id UUID NOT NULL REFERENCES deposits (id),
    sequence   BIGINT NOT NULL,
    type       TEXT NOT NULL,
    data       JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (deposit_id, sequence)
);

-- POST /v1/deposit idempotency, scoped per user.
CREATE TABLE idempotency_keys (
    user_id      TEXT NOT NULL,
    key          TEXT NOT NULL,
    request_hash BYTEA NOT NULL,
    deposit_id   UUID NOT NULL REFERENCES deposits (id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, key)
);
CREATE INDEX idempotency_keys_created_idx ON idempotency_keys (created_at);

-- Inbound webhook dedupe (gum-indexer and gum-engine both deliver at-least-once).
CREATE TABLE inbound_events (
    source      TEXT NOT NULL,
    event_id    TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (source, event_id)
);
CREATE INDEX inbound_events_received_idx ON inbound_events (received_at);

-- Transactional outbox: side effects are committed with the state change that caused them
-- and executed by background workers with retries. Rows are deleted on success.
CREATE TABLE outbox (
    id              UUID PRIMARY KEY,                   -- uuid v7: creation order
    kind            TEXT NOT NULL,                      -- register_watch | submit_execute | notify_app
    deposit_id      UUID NOT NULL REFERENCES deposits (id),
    payload         JSONB NOT NULL DEFAULT '{}'::jsonb,
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until    TIMESTAMPTZ,
    last_error      TEXT,
    dead_at         TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX outbox_due_idx ON outbox (next_attempt_at) WHERE dead_at IS NULL;
CREATE INDEX outbox_deposit_idx ON outbox (deposit_id, kind, id);
