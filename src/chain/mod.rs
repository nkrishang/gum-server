//! Supported chains and tokens (mirrors gum-indexer's registry) and the on-chain payment maths.

pub mod payment;
pub mod revert;

use std::collections::BTreeMap;
use std::sync::Arc;

use alloy_primitives::Address;

use crate::config::ChainConfig;

#[derive(Debug, Clone)]
pub struct TokenSpec {
    pub symbol: String,
    pub address: Address,
    pub decimals: u8,
}

#[derive(Debug, Clone)]
pub struct ChainSpec {
    pub name: String,
    pub chain_id: u64,
    pub tokens: Vec<TokenSpec>,
}

impl ChainSpec {
    /// Resolves a token by symbol (case-insensitive) or contract address.
    pub fn token(&self, raw: &str) -> Option<&TokenSpec> {
        let raw = raw.trim();
        if let Ok(address) = raw.parse::<Address>() {
            return self.tokens.iter().find(|t| t.address == address);
        }
        self.tokens.iter().find(|t| t.symbol.eq_ignore_ascii_case(raw))
    }
}

#[derive(Debug, Default)]
pub struct Registry {
    by_id: BTreeMap<u64, Arc<ChainSpec>>,
    by_name: BTreeMap<String, Arc<ChainSpec>>,
}

impl Registry {
    pub fn from_config(chains: &BTreeMap<String, ChainConfig>) -> Self {
        let mut registry = Self::default();
        for (name, chain) in chains {
            let spec = Arc::new(ChainSpec {
                name: name.clone(),
                chain_id: chain.chain_id,
                tokens: chain
                    .tokens
                    .iter()
                    .map(|t| TokenSpec {
                        symbol: t.symbol.to_ascii_uppercase(),
                        address: t.address,
                        decimals: t.decimals,
                    })
                    .collect(),
            });
            registry.by_id.insert(chain.chain_id, spec.clone());
            registry.by_name.insert(name.to_ascii_lowercase(), spec);
        }
        registry
    }

    pub fn chain_by_id(&self, chain_id: u64) -> Option<&Arc<ChainSpec>> {
        self.by_id.get(&chain_id)
    }

    /// Resolves a chain by id ("8453") or slug ("base").
    pub fn chain(&self, raw: &str) -> Option<&Arc<ChainSpec>> {
        let raw = raw.trim();
        if let Ok(id) = raw.parse::<u64>() {
            return self.by_id.get(&id);
        }
        self.by_name.get(&raw.to_ascii_lowercase())
    }

    pub fn chains(&self) -> impl Iterator<Item = &Arc<ChainSpec>> {
        self.by_id.values()
    }

    pub fn chain_ids(&self) -> Vec<u64> {
        self.by_id.keys().copied().collect()
    }
}
