//! Webhooks in both directions: verified deliveries from gum-indexer / gum-engine, and signed
//! deliveries to apps.

pub mod inbound;
pub mod sign;
pub mod target;
