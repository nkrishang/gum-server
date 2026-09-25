# Tokens a payer can pay with (Relay)

The pay page offers, besides the deposit's own token on its own chain, every token below: the
currencies Relay's solver takes **as they are** on each chain (`solverCurrencies` on
`GET https://api.relay.link/chains`), each checked to route into a deposit in **one step**. The
list lives in [`src/deposit/relay_tokens.rs`](../src/deposit/relay_tokens.rs); a token is offered
only while it is there *and* still among Relay's solver currencies for its chain.

Anything else (ARB, OP, POL, WETH, memecoins…) would be a two-step route: Relay first swaps it on
the payer's chain into one of these, and the solver fills from that. gum-server doesn't offer
those (its route checks tie a route to the token the payer chose, and a two-step route's signed
order takes the swapped currency instead), so the page doesn't list them, search doesn't find them,
and `POST /v1/pay/{id}/quote` refuses them with `400 unsupported_token`.

**Probed 2026-09-25**: 104 of 125 solver currencies across Relay's 52 EVM chains
route in one step (a real quote for 5 USDC into a Base deposit, and USDC on Base into a Monad deposit,
through every check).

| Chain | Token | Address |
|---|---|---|
| Abstract (2741) | ETH | native |
|  | USDC | `0x84a71ccd554cc1b02749b35d22f684cc8ec987e1` |
| ApeChain (33139) | APE | native |
| Arbitrum (42161) | ETH | native |
|  | APE | `0x7f9fbf9bdd3f4105c478b996b648fe6e828a1e98` |
|  | USDC | `0xaf88d065e77c8cc2239327c5edb3a432268e5831` |
|  | USDT | `0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9` |
| Arc (5042) | USDC | native |
|  | USDC | `0x3600000000000000000000000000000000000000` |
| Avalanche (43114) | USDC | `0xb97ef9ef8734c71904d8002f8b6bc66dd9c48a6e` |
|  | USDe | `0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34` |
| Base (8453) | ETH | native |
|  | cbBTC | `0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf` |
|  | USDC | `0x833589fcd6edb6e08f4c7c32d4f71b54bda02913` |
|  | USDT | `0xfde4c96c8593536e31f229ea8f37b2ada2699bb2` |
| Berachain (80094) | USDC | `0x549943e04f40284185054145c6e4e9568c1d3241` |
|  | WETH | `0x2f6f07cdcf3588944bf4c42ac74ff24bf56e7590` |
| Blast (81457) | WETH | `0x4300000000000000000000000000000000000004` |
| BNB (56) | BNB | native |
|  | SOMI | `0xa9616e5e23ec1582c2828b025becf3ef610e266f` |
|  | USDC | `0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d` |
|  | USDe | `0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34` |
|  | USDT | `0x55d398326f99059ff775485246999027b3197955` |
| BOB (60808) | ETH | native |
| Boba Network (288) | ETH | native |
| Celo (42220) | USDC | `0xceba9300f2b948710d2653dd7b07f33a8b32118c` |
| Cronos (25) | CRO | native |
|  | USDC | `0x3d7f2c478aafdb65542bcb44bceec05849999d2d` |
|  | USDC.e | `0xc21223249ca28397b4b6541dffaecc539bff0c59` |
| Doma (97477) | ETH | native |
|  | USDC.e | `0x31eef89d5215c305304a2fa5376a1f1b6c5dc477` |
| Ethereal (5064014) | USDe | native |
| Ethereum (1) | ETH | native |
|  | APE | `0x4d224452801aced8b2f0aebe155379bb5d594381` |
|  | AUSD | `0x00000000efe302beaa2b3e6e1b18d08d69a9012a` |
|  | DAI | `0x6b175474e89094c44da98b954eedeac495271d0f` |
|  | mUSD | `0xaca92e438df0b2401ff60da7e4337b687a2435da` |
|  | PYUSD | `0x6c3ea9036406852006290770bedfcaba0e23a0e8` |
|  | USDC | `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` |
|  | USDe | `0x4c9edd5852cd905f086c759e8383e09bff1e68b3` |
|  | USDG | `0xe343167631d89b6ffc58b88d6b7fb0228795491d` |
|  | USDm | `0xec2af1c8b110a61fd9c3fa6a554a031ca9943926` |
|  | USDT | `0xdac17f958d2ee523a2206206994597c13d831ec7` |
| Flow EVM (747) | USDC | `0xf1815bd50389c46847f0bda824ec8da914045d14` |
| Gensyn (685689) | ETH | native |
|  | USDC | `0x5b32c997211621d55a89cc5abaf1cc21f3a6ddf5` |
| Gnosis (100) | USDC | `0x2a22f9c3b484c3629090feed35f17ff8f88f76f0` |
| HyperEVM (999) | HYPE | native |
|  | USDC | `0xb88339cb7199b77e23db6e890353e22632ba630f` |
|  | USDe | `0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34` |
|  | USD₮0 | `0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb` |
| Ink (57073) | ETH | native |
|  | USDC | `0x2d270e6886d130d724215a266106e6832161eaed` |
|  | USDT0 | `0x0200c29006150606b650577bbe7b6248f58470c1` |
| Katana (747474) | ETH | native |
|  | USDC | `0x203a662b0bd271a6ed5a60edfbd04bfce608fd36` |
|  | USDT | `0x2dca96907fde857dd3d816880a0df407eeb2d2f2` |
| Linea (59144) | ETH | native |
|  | mUSD | `0xaca92e438df0b2401ff60da7e4337b687a2435da` |
|  | USDC | `0x176211869ca2b568f2a7d4ee941e073a821ee1ff` |
| Lisk (1135) | ETH | native |
| Manta Pacific (169) | ETH | native |
| Mantle (5000) | USDC | `0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9` |
| MegaETH (4326) | ETH | native |
|  | USDm | `0xfafddbb3fc7688494971a79cc65dca3ef82079e7` |
|  | USDT | `0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb` |
| Metis (1088) | WETH | `0x420000000000000000000000000000000000000a` |
| Mode (34443) | ETH | native |
| Monad (143) | MON | native |
|  | AUSD | `0x00000000efe302beaa2b3e6e1b18d08d69a9012a` |
|  | mUSD | `0xaca92e438df0b2401ff60da7e4337b687a2435da` |
|  | USDC | `0x754704bc059f8c67012fed69bc8a327a5aafb603` |
| Morph (2818) | ETH | native |
| Mythos (42018) | ETH | native |
|  | USDC.e | `0x0685b38c8228f595688dec1d7c69d036b3ee52d7` |
| Optimism (10) | ETH | native |
|  | USDC | `0x0b2c639c533813f4aa9d7837caf62653d097ff85` |
|  | USDT | `0x94b008aa00579c1307b0ef2c499ad98a8ce58e58` |
| Plasma (9745) | USD₮0 | `0xb8ce59fc3717ada4c02eadf9682a9e934f625ebb` |
| Plume (98866) | pUSD | `0xdddd73f5df1f0dc31373357beac77545dc5a6f3f` |
|  | WETH | `0xca59ca09e5602fae8b629dee83ffa819741f14be` |
| Polygon (137) | pUSD | `0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb` |
|  | USDC | `0x3c499c542cef5e3811e1192ce70d8cc03d5c3359` |
|  | USDC.e | `0x2791bca1f2de4661ed88a30c99a7a9449aa84174` |
|  | USDT | `0xc2132d05d31c914a87c6611c10748aeb04b58e8f` |
| Robinhood Chain (4663) | ETH | native |
|  | USDG | `0x5fc5360d0400a0fd4f2af552add042d716f1d168` |
| Ronin (2020) | USDC | `0x0b7007c13325c48911f73a2dad5fa5dcbf808adc` |
| Scroll (534352) | ETH | native |
| Shape (360) | ETH | native |
| Somnia (5031) | SOMI | native |
| Soneium (1868) | USDC.e | `0xba9986d2381edf1da03b0b9c1f8b00dc4aacc369` |
| Sonic (146) | USDC | `0x29219dd400f2bf60e5a23d13be72b486d4038894` |
| Stable (988) | USDT0 | `0x779ded0c9e1022225f8e0630b35a9b54be713736` |
| Superseed (5330) | ETH | native |
| Tempo (4217) | USDC | `0x20c000000000000000000000b9537d11c60e8b50` |
|  | USDT0 | `0x20c00000000000000000000014f22ca97301eb73` |
| Unichain (130) | ETH | native |
|  | USDC | `0x078d782b760474a361dda0af3839290b0ef57ad6` |
| World Chain (480) | ETH | native |
|  | USDC | `0x79a02482a880bce3f13e09da970dc34db4cd24d1` |
| X Layer (196) | USDC | `0xb6ceceab302e2e4948951ee7843fc24e92933061` |
|  | USDG | `0x4ae46a509f6b1d9056937ba4500cb143933d2dc8` |
| zkSync Era (324) | ETH | native |

## Left out

- **Two-step routes** (Relay swaps them first): WETH on Ethereum, PLUME on Ethereum, WETH on Optimism, xDAI on Gnosis, ETH on Soneium, RON on Ronin, PENGU on Abstract, PathUSD on Tempo, WETH on Base, SOL on Base, DEGEN on Base, XPL on Plasma, WETH on Arbitrum, kBTC on Ink, ETH on Blast.
  WETH is unwrapped to ETH, and ETH on Soneium and Blast is wrapped, before the solver takes it.
- **No route or liquidity at Relay** when probed: FLOW on Flow EVM, ETH on Zircuit, USDC on Plume, PLUME on Plume, USDzC on Zora, ETH on Zora. These can come back; the probe
  says so.

## Keeping it current

```sh
RELAY_API_KEY=… TEST_DATABASE_URL=… cargo test --test relay relay_live_direct_tokens -- --ignored --nocapture
```

quotes every solver currency Relay lists (a few minutes, paced under its 50 quotes a minute) and
fails with the exact lines to add to or remove from `DIRECT_TOKENS` when the list is out of date.
Update the list, the date in `relay_tokens.rs` (`PROBED_ON`) and this page together.
