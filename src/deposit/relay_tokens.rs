//! The tokens a payer can pay with through Relay: what Relay's solver takes as it is on each chain
//! (`solverCurrencies` on `/chains`), each checked to route in one step.
//!
//! A route from anything else has two steps: Relay first swaps the token into one of these on the
//! payer's chain, and the solver fills from that. This service doesn't offer those. Its checks tie
//! a route to the token the payer chose (`check_quote`), and a two-step route's signed order takes
//! the swapped currency instead. So only tokens that route in one step are offered, searched and
//! quoted, and the page never lists the rest.
//!
//! A token is offered only while it is on this list *and* still among Relay's solver currencies
//! for its chain, so Relay dropping one takes effect at once. Adding one takes a probe:
//! `cargo test --test relay relay_live_direct_tokens -- --ignored` quotes every solver currency
//! Relay lists (needs `RELAY_API_KEY`) and says what to add to or remove from this list.
//!
//! Probed 2026-09-25: 104 of 125 solver currencies across Relay's EVM chains route in one
//! step. Left out as two-step routes: WETH on Ethereum, Optimism, Base and Arbitrum (Relay unwraps
//! it to ETH), xDAI on Gnosis, ETH on Soneium and Blast (wrapped first), RON, PENGU, PathUSD, SOL
//! and DEGEN on Base, XPL, kBTC and PLUME on Ethereum. Left out because Relay had no route or
//! liquidity: FLOW, USDC and PLUME on Plume, ETH on Zircuit, ETH and USDzC on Zora.

/// When the list was last probed against api.relay.link.
pub const PROBED_ON: &str = "2026-09-25";

pub struct DirectToken {
    pub chain_id: u64,
    /// Lowercase; the zero address is the chain's native currency.
    pub address: &'static str,
    pub symbol: &'static str,
}

#[rustfmt::skip]
pub const DIRECT_TOKENS: &[DirectToken] = &[
    // Abstract (2741)
    DirectToken { chain_id: 2741, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 2741, address: "0x84a71ccd554cc1b02749b35d22f684cc8ec987e1", symbol: "USDC" },
    // ApeChain (33139)
    DirectToken { chain_id: 33139, address: "0x0000000000000000000000000000000000000000", symbol: "APE" },
    // Arbitrum (42161)
    DirectToken { chain_id: 42161, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 42161, address: "0x7f9fbf9bdd3f4105c478b996b648fe6e828a1e98", symbol: "APE" },
    DirectToken { chain_id: 42161, address: "0xaf88d065e77c8cc2239327c5edb3a432268e5831", symbol: "USDC" },
    DirectToken { chain_id: 42161, address: "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9", symbol: "USDT" },
    // Arc (5042)
    DirectToken { chain_id: 5042, address: "0x0000000000000000000000000000000000000000", symbol: "USDC" },
    DirectToken { chain_id: 5042, address: "0x3600000000000000000000000000000000000000", symbol: "USDC" },
    // Avalanche (43114)
    DirectToken { chain_id: 43114, address: "0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e", symbol: "USDC" },
    DirectToken { chain_id: 43114, address: "0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34", symbol: "USDe" },
    // Base (8453)
    DirectToken { chain_id: 8453, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 8453, address: "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf", symbol: "cbBTC" },
    DirectToken { chain_id: 8453, address: "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913", symbol: "USDC" },
    DirectToken { chain_id: 8453, address: "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2", symbol: "USDT" },
    // Berachain (80094)
    DirectToken { chain_id: 80094, address: "0x549943e04f40284185054145c6e4e9568c1d3241", symbol: "USDC" },
    DirectToken { chain_id: 80094, address: "0x2f6f07cdcf3588944bf4c42ac74ff24bf56e7590", symbol: "WETH" },
    // Blast (81457)
    DirectToken { chain_id: 81457, address: "0x4300000000000000000000000000000000000004", symbol: "WETH" },
    // BNB (56)
    DirectToken { chain_id: 56, address: "0x0000000000000000000000000000000000000000", symbol: "BNB" },
    DirectToken { chain_id: 56, address: "0xa9616e5e23ec1582c2828b025becf3ef610e266f", symbol: "SOMI" },
    DirectToken { chain_id: 56, address: "0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d", symbol: "USDC" },
    DirectToken { chain_id: 56, address: "0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34", symbol: "USDe" },
    DirectToken { chain_id: 56, address: "0x55d398326f99059ff775485246999027b3197955", symbol: "USDT" },
    // BOB (60808)
    DirectToken { chain_id: 60808, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Boba Network (288)
    DirectToken { chain_id: 288, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Celo (42220)
    DirectToken { chain_id: 42220, address: "0xceba9300f2b948710d2653dd7b07f33a8b32118c", symbol: "USDC" },
    // Cronos (25)
    DirectToken { chain_id: 25, address: "0x0000000000000000000000000000000000000000", symbol: "CRO" },
    DirectToken { chain_id: 25, address: "0x3d7f2c478aafdb65542bcb44bceec05849999d2d", symbol: "USDC" },
    DirectToken { chain_id: 25, address: "0xc21223249ca28397b4b6541dffaecc539bff0c59", symbol: "USDC.e" },
    // Doma (97477)
    DirectToken { chain_id: 97477, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 97477, address: "0x31eef89d5215c305304a2fa5376a1f1b6c5dc477", symbol: "USDC.e" },
    // Ethereal (5064014)
    DirectToken { chain_id: 5064014, address: "0x0000000000000000000000000000000000000000", symbol: "USDe" },
    // Ethereum (1)
    DirectToken { chain_id: 1, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 1, address: "0x4d224452801aced8b2f0aebe155379bb5d594381", symbol: "APE" },
    DirectToken { chain_id: 1, address: "0x00000000efe302beaa2b3e6e1b18d08d69a9012a", symbol: "AUSD" },
    DirectToken { chain_id: 1, address: "0x6b175474e89094c44da98b954eedeac495271d0f", symbol: "DAI" },
    DirectToken { chain_id: 1, address: "0xaca92e438df0b2401ff60da7e4337b687a2435da", symbol: "mUSD" },
    DirectToken { chain_id: 1, address: "0x6c3ea9036406852006290770bedfcaba0e23a0e8", symbol: "PYUSD" },
    DirectToken { chain_id: 1, address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", symbol: "USDC" },
    DirectToken { chain_id: 1, address: "0x4c9edd5852cd905f086c759e8383e09bff1e68b3", symbol: "USDe" },
    DirectToken { chain_id: 1, address: "0xe343167631d89b6ffc58b88d6b7fb0228795491d", symbol: "USDG" },
    DirectToken { chain_id: 1, address: "0xec2af1c8b110a61fd9c3fa6a554a031ca9943926", symbol: "USDm" },
    DirectToken { chain_id: 1, address: "0xdac17f958d2ee523a2206206994597c13d831ec7", symbol: "USDT" },
    // Flow EVM (747)
    DirectToken { chain_id: 747, address: "0xf1815bd50389c46847f0bda824ec8da914045d14", symbol: "USDC" },
    // Gensyn (685689)
    DirectToken { chain_id: 685689, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 685689, address: "0x5b32c997211621d55a89cc5abaf1cc21f3a6ddf5", symbol: "USDC" },
    // Gnosis (100)
    DirectToken { chain_id: 100, address: "0x2a22f9c3b484c3629090feed35f17ff8f88f76f0", symbol: "USDC" },
    // HyperEVM (999)
    DirectToken { chain_id: 999, address: "0x0000000000000000000000000000000000000000", symbol: "HYPE" },
    DirectToken { chain_id: 999, address: "0xb88339cb7199b77e23db6e890353e22632ba630f", symbol: "USDC" },
    DirectToken { chain_id: 999, address: "0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34", symbol: "USDe" },
    DirectToken { chain_id: 999, address: "0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb", symbol: "USD₮0" },
    // Ink (57073)
    DirectToken { chain_id: 57073, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 57073, address: "0x2d270e6886d130d724215a266106e6832161eaed", symbol: "USDC" },
    DirectToken { chain_id: 57073, address: "0x0200c29006150606b650577bbe7b6248f58470c1", symbol: "USDT0" },
    // Katana (747474)
    DirectToken { chain_id: 747474, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 747474, address: "0x203a662b0bd271a6ed5a60edfbd04bfce608fd36", symbol: "USDC" },
    DirectToken { chain_id: 747474, address: "0x2dca96907fde857dd3d816880a0df407eeb2d2f2", symbol: "USDT" },
    // Linea (59144)
    DirectToken { chain_id: 59144, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 59144, address: "0xaca92e438df0b2401ff60da7e4337b687a2435da", symbol: "mUSD" },
    DirectToken { chain_id: 59144, address: "0x176211869ca2b568f2a7d4ee941e073a821ee1ff", symbol: "USDC" },
    // Lisk (1135)
    DirectToken { chain_id: 1135, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Manta Pacific (169)
    DirectToken { chain_id: 169, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Mantle (5000)
    DirectToken { chain_id: 5000, address: "0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9", symbol: "USDC" },
    // MegaETH (4326)
    DirectToken { chain_id: 4326, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 4326, address: "0xfafddbb3fc7688494971a79cc65dca3ef82079e7", symbol: "USDm" },
    DirectToken { chain_id: 4326, address: "0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb", symbol: "USDT" },
    // Metis (1088)
    DirectToken { chain_id: 1088, address: "0x420000000000000000000000000000000000000a", symbol: "WETH" },
    // Mode (34443)
    DirectToken { chain_id: 34443, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Monad (143)
    DirectToken { chain_id: 143, address: "0x0000000000000000000000000000000000000000", symbol: "MON" },
    DirectToken { chain_id: 143, address: "0x00000000efe302beaa2b3e6e1b18d08d69a9012a", symbol: "AUSD" },
    DirectToken { chain_id: 143, address: "0xaca92e438df0b2401ff60da7e4337b687a2435da", symbol: "mUSD" },
    DirectToken { chain_id: 143, address: "0x754704bc059f8c67012fed69bc8a327a5aafb603", symbol: "USDC" },
    // Morph (2818)
    DirectToken { chain_id: 2818, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Mythos (42018)
    DirectToken { chain_id: 42018, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 42018, address: "0x0685b38c8228f595688dec1d7c69d036b3ee52d7", symbol: "USDC.e" },
    // Optimism (10)
    DirectToken { chain_id: 10, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 10, address: "0x0b2c639c533813f4aa9d7837caf62653d097ff85", symbol: "USDC" },
    DirectToken { chain_id: 10, address: "0x94b008aa00579c1307b0ef2c499ad98a8ce58e58", symbol: "USDT" },
    // Plasma (9745)
    DirectToken { chain_id: 9745, address: "0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb", symbol: "USD₮0" },
    // Plume (98866)
    DirectToken { chain_id: 98866, address: "0xdddd73f5df1f0dc31373357beac77545dc5a6f3f", symbol: "pUSD" },
    DirectToken { chain_id: 98866, address: "0xca59ca09e5602fae8b629dee83ffa819741f14be", symbol: "WETH" },
    // Polygon (137)
    DirectToken { chain_id: 137, address: "0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb", symbol: "pUSD" },
    DirectToken { chain_id: 137, address: "0x3c499c542cef5e3811e1192ce70d8cc03d5c3359", symbol: "USDC" },
    DirectToken { chain_id: 137, address: "0x2791bca1f2de4661ed88a30c99a7a9449aa84174", symbol: "USDC.e" },
    DirectToken { chain_id: 137, address: "0xc2132d05d31c914a87c6611c10748aeb04b58e8f", symbol: "USDT" },
    // Robinhood Chain (4663)
    DirectToken { chain_id: 4663, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 4663, address: "0x5fc5360d0400a0fd4f2af552add042d716f1d168", symbol: "USDG" },
    // Ronin (2020)
    DirectToken { chain_id: 2020, address: "0x0b7007c13325c48911f73a2dad5fa5dcbf808adc", symbol: "USDC" },
    // Scroll (534352)
    DirectToken { chain_id: 534352, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Shape (360)
    DirectToken { chain_id: 360, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Somnia (5031)
    DirectToken { chain_id: 5031, address: "0x0000000000000000000000000000000000000000", symbol: "SOMI" },
    // Soneium (1868)
    DirectToken { chain_id: 1868, address: "0xba9986d2381edf1da03b0b9c1f8b00dc4aacc369", symbol: "USDC.e" },
    // Sonic (146)
    DirectToken { chain_id: 146, address: "0x29219dd400f2bf60e5a23d13be72b486d4038894", symbol: "USDC" },
    // Stable (988)
    DirectToken { chain_id: 988, address: "0x779ded0c9e1022225f8e0630b35a9b54be713736", symbol: "USDT0" },
    // Superseed (5330)
    DirectToken { chain_id: 5330, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    // Tempo (4217)
    DirectToken { chain_id: 4217, address: "0x20c000000000000000000000b9537d11c60e8b50", symbol: "USDC" },
    DirectToken { chain_id: 4217, address: "0x20c00000000000000000000014f22ca97301eb73", symbol: "USDT0" },
    // Unichain (130)
    DirectToken { chain_id: 130, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 130, address: "0x078d782b760474a361dda0af3839290b0ef57ad6", symbol: "USDC" },
    // World Chain (480)
    DirectToken { chain_id: 480, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
    DirectToken { chain_id: 480, address: "0x79a02482a880bce3f13e09da970dc34db4cd24d1", symbol: "USDC" },
    // X Layer (196)
    DirectToken { chain_id: 196, address: "0xb6ceceab302e2e4948951ee7843fc24e92933061", symbol: "USDC" },
    DirectToken { chain_id: 196, address: "0x4ae46a509f6b1d9056937ba4500cb143933d2dc8", symbol: "USDG" },
    // zkSync Era (324)
    DirectToken { chain_id: 324, address: "0x0000000000000000000000000000000000000000", symbol: "ETH" },
];

/// The token routes in one step from this chain.
pub fn is_direct(chain_id: u64, address: &str) -> bool {
    DIRECT_TOKENS.iter().any(|t| t.chain_id == chain_id && t.address.eq_ignore_ascii_case(address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_well_formed() {
        let mut seen = std::collections::BTreeSet::new();
        for t in DIRECT_TOKENS {
            assert!(t.address.parse::<alloy_primitives::Address>().is_ok(), "{} {}", t.chain_id, t.address);
            assert_eq!(t.address, t.address.to_ascii_lowercase(), "addresses are lowercase");
            assert!(seen.insert((t.chain_id, t.address)), "duplicate {} {}", t.chain_id, t.address);
        }
        assert!(is_direct(8453, "0x833589FCD6EDB6E08F4C7C32D4F71B54BDA02913"), "case-insensitive");
        assert!(!is_direct(8453, "0x4200000000000000000000000000000000000006"), "WETH on Base is a two-step route");
    }
}
