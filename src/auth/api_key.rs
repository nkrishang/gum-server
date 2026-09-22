//! API keys: `gum_sk_<64 hex>` secrets, stored as SHA-256 hashes, looked up through a short-lived
//! in-memory cache, and rate limited per key.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use moka::sync::Cache;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::config::ApiKeysConfig;

pub const KEY_PREFIX: &str = "gum_sk_";
const SECRET_BYTES: usize = 32;

/// A freshly generated key. The secret is shown to the user exactly once.
pub struct GeneratedKey {
    pub secret: String,
    pub hash: Vec<u8>,
    pub prefix: String,
}

pub fn generate() -> GeneratedKey {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    let secret = format!("{KEY_PREFIX}{}", hex::encode(bytes));
    let prefix = secret[..KEY_PREFIX.len() + 8].to_owned();
    GeneratedKey { hash: hash(&secret), secret, prefix }
}

pub fn generate_webhook_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    format!("whsec_{}", hex::encode(bytes))
}

pub fn hash(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

pub fn looks_like_key(candidate: &str) -> bool {
    candidate.starts_with(KEY_PREFIX)
}

/// What a request authenticated with a key is allowed to know about its owner.
#[derive(Debug, Clone)]
pub struct KeyOwner {
    pub user_id: String,
}

type KeyedLimiter = RateLimiter<Vec<u8>, DefaultKeyedStateStore<Vec<u8>>, DefaultClock>;

/// Positive cache from key hash to owner plus a per-key token bucket.
pub struct KeyCache {
    cache: Cache<Vec<u8>, Arc<KeyOwner>>,
    limiter: KeyedLimiter,
}

impl KeyCache {
    pub fn new(cfg: &ApiKeysConfig) -> Self {
        let per_second = NonZeroU32::new(cfg.rate_limit_per_second.max(1)).unwrap();
        let burst = NonZeroU32::new(cfg.rate_limit_burst.max(1)).unwrap();
        Self {
            cache: Cache::builder().max_capacity(100_000).time_to_live(Duration::from_secs(cfg.cache_ttl_secs)).build(),
            limiter: RateLimiter::keyed(Quota::per_second(per_second).allow_burst(burst)),
        }
    }

    pub fn get(&self, key_hash: &[u8]) -> Option<Arc<KeyOwner>> {
        self.cache.get(key_hash)
    }

    pub fn insert(&self, key_hash: Vec<u8>, owner: KeyOwner) -> Arc<KeyOwner> {
        let owner = Arc::new(owner);
        self.cache.insert(key_hash, owner.clone());
        owner
    }

    pub fn invalidate(&self, key_hash: &[u8]) {
        self.cache.invalidate(key_hash);
    }

    /// `true` when the request fits the key's budget.
    pub fn allow(&self, key_hash: &[u8]) -> bool {
        self.limiter.check_key(&key_hash.to_vec()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_prefixed_and_hash_stably() {
        let a = generate();
        let b = generate();
        assert_ne!(a.secret, b.secret);
        assert!(looks_like_key(&a.secret));
        assert_eq!(a.secret.len(), KEY_PREFIX.len() + 64);
        assert_eq!(a.prefix.len(), KEY_PREFIX.len() + 8);
        assert_eq!(a.hash, hash(&a.secret));
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn cache_roundtrip_and_invalidation() {
        let cache = KeyCache::new(&ApiKeysConfig { cache_ttl_secs: 60, rate_limit_per_second: 1, rate_limit_burst: 2 });
        let h = hash("gum_sk_x");
        assert!(cache.get(&h).is_none());
        cache.insert(h.clone(), KeyOwner { user_id: "u".into() });
        assert_eq!(cache.get(&h).unwrap().user_id, "u");
        cache.invalidate(&h);
        assert!(cache.get(&h).is_none());
        assert!(cache.allow(&h));
        assert!(cache.allow(&h));
        assert!(!cache.allow(&h), "burst of 2 exhausted");
    }
}
