//! Instant change notification for long-polling readers (`GET /v1/pay/{id}`).
//!
//! `store::record_event` runs `pg_notify('gum_deposit_events', <deposit id>)` inside the
//! transaction of every transition. Postgres delivers a notification only when that transaction
//! commits, so a reader woken by one always sees the change it was woken for, and a rolled-back
//! transition wakes nobody. One `PgListener` per process relays the ids to
//! `AppState::deposit_changes`, which waiting requests subscribe to. Every replica runs its own
//! listener and Postgres delivers each notification to all of them, so a change committed anywhere
//! (a webhook on another replica, the outbox, the reconciler) wakes readers on every replica.
//!
//! Notifications are a latency optimisation, never a correctness requirement. While the
//! listener's connection is down, notifications are lost; once it is listening again it sends
//! `RESYNC` so every waiting reader re-reads. A reader that still misses a change simply returns
//! when its wait runs out, and the client polls again.

use std::time::Duration;

use sqlx::postgres::{PgListener, PgPoolOptions};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::db;
use crate::state::AppState;

/// The Postgres channel `store::record_event` notifies on; the payload is the deposit id.
pub const CHANNEL: &str = "gum_deposit_events";
/// Sent instead of a deposit id when notifications may have been missed: every reader re-reads.
pub const RESYNC: Uuid = Uuid::nil();
/// A reader that falls this far behind gets `Lagged` and re-reads, so this bounds memory, not
/// correctness.
const CAPACITY: usize = 1024;
/// Pause before reconnecting after the listener failed.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// The sender kept in `AppState`; readers call `subscribe()` on it.
pub fn sender() -> broadcast::Sender<Uuid> {
    broadcast::channel(CAPACITY).0
}

/// Relays notifications until `shutdown`. Connection failures are retried here rather than left
/// to the supervisor: a database blip is expected, not a task failure worth alerting on.
pub async fn run(state: AppState, shutdown: CancellationToken) {
    let options = match db::connect_options(&state.config.database) {
        Ok(options) => options,
        Err(err) => {
            tracing::error!(error = %err, "deposit feed cannot build its connection options");
            return;
        }
    };
    // A dedicated connection, not one borrowed from the service's pool for good: the listener
    // holds it for the life of the process and must not shrink the pool requests draw from.
    let pool = PgPoolOptions::new().max_connections(1).max_lifetime(None).idle_timeout(None).connect_lazy_with(options);
    loop {
        tokio::select! {
            result = listen(&state, &pool) => {
                if let Err(err) = result {
                    tracing::warn!(error = %err, "deposit feed lost its connection; reconnecting");
                }
            }
            _ = shutdown.cancelled() => break,
        }
        tokio::select! {
            _ = tokio::time::sleep(RETRY_DELAY) => {}
            _ = shutdown.cancelled() => break,
        }
    }
    pool.close().await;
}

async fn listen(state: &AppState, pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    tracing::info!(channel = CHANNEL, "deposit feed listening");
    // Anything committed before this point (e.g. while reconnecting) was not heard.
    let _ = state.deposit_changes.send(RESYNC);
    loop {
        // `None`: the connection dropped and has already been re-established (and re-`LISTEN`ed);
        // notifications sent in between are gone.
        match listener.try_recv().await? {
            Some(notification) => match notification.payload().parse::<Uuid>() {
                // `send` fails only when nobody is waiting, which is fine.
                Ok(id) => {
                    let _ = state.deposit_changes.send(id);
                }
                Err(_) => tracing::warn!(payload = notification.payload(), "unexpected deposit feed payload"),
            },
            None => {
                tracing::warn!("deposit feed reconnected; readers will re-read");
                let _ = state.deposit_changes.send(RESYNC);
            }
        }
    }
}
