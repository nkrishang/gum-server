//! Prints the counterfactual payment address for a set of terms, off-chain.
//!
//! cargo run --example payment_address -- <factory> <token> <amount> <receiver> <expiry> <recovery> <salt> <chain_id>

use alloy_primitives::{Address, B256, U256};
use gum_server::chain::payment::PaymentTerms;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 8 {
        eprintln!("usage: payment_address <factory> <token> <amount> <receiver> <expiry> <recovery> <salt> <chain_id>");
        std::process::exit(2);
    }
    let factory: Address = args[0].parse().expect("factory");
    let terms = PaymentTerms {
        token: args[1].parse().expect("token"),
        amount: U256::from_str_radix(&args[2], 10).expect("amount"),
        receiver: args[3].parse().expect("receiver"),
        expiration_timestamp: args[4].parse().expect("expiry"),
        recovery: args[5].parse().expect("recovery"),
        salt: args[6].parse::<B256>().expect("salt"),
        chain_id: args[7].parse().expect("chain_id"),
    };
    println!("{:#x}", terms.payment_address(factory));
    println!("{}", terms.execute_calldata());
}
