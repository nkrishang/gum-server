//! `PaymentFactory` address derivation and calldata, reproduced off-chain so `POST /v1/deposit` never
//! touches an RPC.
//!
//! The factory (gum-contracts `PaymentFactory.sol`) is an ownerless Solady CREATE3 deployer:
//!
//! ```text
//! deploymentSalt = keccak256(abi.encode(token, amount, receiver, expirationTimestamp, recovery, salt, chainId))
//! proxy          = CREATE2(factory, deploymentSalt, keccak256(PROXY_INITCODE))
//! payment        = CREATE(proxy, nonce = 1)
//! ```

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, SolValue, sol};

sol! {
    /// gum-contracts `PaymentFactory`.
    interface IPaymentFactory {
        function execute(
            address token,
            uint256 amount,
            address receiver,
            uint64 expirationTimestamp,
            address recovery,
            bytes32 salt,
            uint256 chainId
        ) external;
    }

    /// gum-contracts `Payment` events, emitted by the payment address during `execute`.
    interface IPayment {
        event Settled(address indexed receiver, uint256 amount);
        event Recovered(address indexed recovery, address indexed token, uint256 amount);
        event WrongChain(uint256 expectedChainId, uint256 actualChainId);
    }
}

/// Solady's CREATE3 proxy: `67363d3d37363d34f03d5260086018f3`.
const PROXY_INITCODE: [u8; 16] =
    [0x67, 0x36, 0x3d, 0x3d, 0x37, 0x36, 0x3d, 0x34, 0xf0, 0x3d, 0x52, 0x60, 0x08, 0x60, 0x18, 0xf3];

/// The seven parameters a payment is described by. The counterfactual address commits to all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentTerms {
    pub token: Address,
    pub amount: U256,
    pub receiver: Address,
    pub expiration_timestamp: u64,
    pub recovery: Address,
    pub salt: B256,
    pub chain_id: u64,
}

impl PaymentTerms {
    /// `PaymentFactory.deploymentSalt(...)`.
    pub fn deployment_salt(&self) -> B256 {
        let encoded = (
            self.token,
            self.amount,
            self.receiver,
            self.expiration_timestamp,
            self.recovery,
            self.salt,
            U256::from(self.chain_id),
        )
            .abi_encode();
        keccak256(encoded)
    }

    /// `PaymentFactory.paymentAddress(...)` for a factory deployed at `factory`.
    pub fn payment_address(&self, factory: Address) -> Address {
        let proxy = factory.create2(self.deployment_salt(), keccak256(PROXY_INITCODE));
        proxy.create(1)
    }

    /// Calldata for `PaymentFactory.execute(...)`.
    pub fn execute_calldata(&self) -> Bytes {
        IPaymentFactory::executeCall {
            token: self.token,
            amount: self.amount,
            receiver: self.receiver,
            expirationTimestamp: self.expiration_timestamp,
            recovery: self.recovery,
            salt: self.salt,
            chainId: U256::from(self.chain_id),
        }
        .abi_encode()
        .into()
    }
}

/// What a `Payment` constructor did, read from the receipt logs of the `execute` transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcome {
    /// `Settled(receiver, amount)` was emitted by the payment address.
    Settled,
    /// Only `Recovered` was emitted: the payment had expired, so the balance went to `recovery`.
    RecoveredOnly,
    /// `WrongChain` was emitted (cannot happen when the engine honours `chain_id`; kept for completeness).
    WrongChain,
    /// Neither event was found for the payment address.
    NoEvents,
}

/// A receipt log in the node's JSON encoding (`address`, `topics`, `data`).
#[derive(Debug, serde::Deserialize)]
pub struct ReceiptLog {
    pub address: Address,
    pub topics: Vec<B256>,
    #[serde(default)]
    pub data: Bytes,
}

pub fn execution_outcome(logs: &[ReceiptLog], payment_address: Address) -> ExecutionOutcome {
    let mut recovered = false;
    for log in logs.iter().filter(|l| l.address == payment_address) {
        let Some(topic0) = log.topics.first() else { continue };
        if *topic0 == IPayment::Settled::SIGNATURE_HASH {
            return ExecutionOutcome::Settled;
        }
        if *topic0 == IPayment::WrongChain::SIGNATURE_HASH {
            return ExecutionOutcome::WrongChain;
        }
        if *topic0 == IPayment::Recovered::SIGNATURE_HASH {
            recovered = true;
        }
    }
    if recovered { ExecutionOutcome::RecoveredOnly } else { ExecutionOutcome::NoEvents }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    fn terms() -> PaymentTerms {
        PaymentTerms {
            token: address!("e7f1725E7734CE288F8367e1Bb143E90bb3F0512"),
            amount: U256::from(2_500_000u64),
            receiver: address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
            expiration_timestamp: 1_800_000_000,
            recovery: address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
            salt: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            chain_id: 31337,
        }
    }

    #[test]
    fn proxy_initcode_hash_matches_solady() {
        // Solady CREATE3._PROXY_INITCODE_HASH.
        assert_eq!(
            keccak256(PROXY_INITCODE),
            b256!("21c35dbe1b344a2488cf3321d6ce542f8e9f305544ff09e4993a62319a497c1f")
        );
    }

    /// Known answers from `PaymentFactory.paymentAddress(...)` on Anvil, with the factory deployed
    /// from the gum-contracts repo at the LocalBootstrap fixture address.
    #[test]
    fn matches_the_deployed_factory() {
        let factory = address!("5FbDB2315678afecb367f032d93F642f64180aa3");
        let salt_ab =
            PaymentTerms { salt: b256!("00000000000000000000000000000000000000000000000000000000000000ab"), ..terms() };
        assert_eq!(salt_ab.payment_address(factory), address!("79104AE2cbd7c67478e27c214EF03cB1DCb7dB3e"));
        let other = PaymentTerms {
            amount: U256::from(999_999_999_999u64),
            salt: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            chain_id: 8453,
            ..terms()
        };
        assert_eq!(other.payment_address(factory), address!("2BEe890EA7F4fcd78742Fa288410C59F0E30A157"));
    }

    #[test]
    fn deployment_salt_is_abi_encode_of_all_seven_words() {
        let t = terms();
        let mut expected = Vec::new();
        expected.extend_from_slice(&[0u8; 12]);
        expected.extend_from_slice(t.token.as_slice());
        expected.extend_from_slice(&t.amount.to_be_bytes::<32>());
        expected.extend_from_slice(&[0u8; 12]);
        expected.extend_from_slice(t.receiver.as_slice());
        expected.extend_from_slice(&U256::from(t.expiration_timestamp).to_be_bytes::<32>());
        expected.extend_from_slice(&[0u8; 12]);
        expected.extend_from_slice(t.recovery.as_slice());
        expected.extend_from_slice(t.salt.as_slice());
        expected.extend_from_slice(&U256::from(t.chain_id).to_be_bytes::<32>());
        assert_eq!(expected.len(), 7 * 32);
        assert_eq!(t.deployment_salt(), keccak256(expected));
    }

    #[test]
    fn address_commits_to_every_parameter() {
        let factory = address!("5FbDB2315678afecb367f032d93F642f64180aa3");
        let base = terms().payment_address(factory);
        let mut variants = vec![
            PaymentTerms { amount: U256::from(2_500_001u64), ..terms() },
            PaymentTerms { receiver: Address::ZERO, ..terms() },
            PaymentTerms { expiration_timestamp: 1_800_000_001, ..terms() },
            PaymentTerms { chain_id: 8453, ..terms() },
            PaymentTerms { salt: B256::ZERO, ..terms() },
        ];
        variants.push(PaymentTerms { token: terms().receiver, ..terms() });
        for v in variants {
            assert_ne!(v.payment_address(factory), base);
        }
        assert_ne!(terms().payment_address(Address::ZERO), base);
        assert_eq!(terms().payment_address(factory), base, "deterministic");
    }

    #[test]
    fn execute_calldata_has_selector_and_seven_words() {
        let data = terms().execute_calldata();
        assert_eq!(data.len(), 4 + 7 * 32);
        assert_eq!(&data[..4], IPaymentFactory::executeCall::SELECTOR);
        let decoded = IPaymentFactory::executeCall::abi_decode(&data).unwrap();
        assert_eq!(decoded.expirationTimestamp, 1_800_000_000);
        assert_eq!(decoded.chainId, U256::from(31337));
    }

    #[test]
    fn event_topics_match_the_receipt_of_a_real_execute() {
        // Topics observed on Anvil when executing a funded payment via gum-contracts.
        assert_eq!(
            IPayment::Settled::SIGNATURE_HASH,
            b256!("7823e479a1a4ebe2418874847436f8a1680c5ee5b17f38bb59dbff28e1b45552")
        );
        assert_eq!(
            IPayment::Recovered::SIGNATURE_HASH,
            b256!("fff3b3844276f57024e0b42afec1a37f75db36511e43819a4f2a63ab7862b648")
        );
        // The node's receipt encoding parses straight into ReceiptLog.
        let logs: Vec<ReceiptLog> = serde_json::from_str(
            r#"[{"address":"0xe7f1725e7734ce288f8367e1bb143e90bb3f0512","topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef","0x00000000000000000000000079104ae2cbd7c67478e27c214ef03cb1dcb7db3e","0x00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c8"],"data":"0x00000000000000000000000000000000000000000000000000000000002625a0","blockNumber":"0x5","logIndex":"0x0","removed":false},
               {"address":"0x79104ae2cbd7c67478e27c214ef03cb1dcb7db3e","topics":["0x7823e479a1a4ebe2418874847436f8a1680c5ee5b17f38bb59dbff28e1b45552","0x00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c8"],"data":"0x00000000000000000000000000000000000000000000000000000000002625a0","blockNumber":"0x5","logIndex":"0x1","removed":false},
               {"address":"0x79104ae2cbd7c67478e27c214ef03cb1dcb7db3e","topics":["0xfff3b3844276f57024e0b42afec1a37f75db36511e43819a4f2a63ab7862b648","0x0000000000000000000000003c44cdddb6a900fa2b585dd299e03d12fa4293bc","0x000000000000000000000000e7f1725e7734ce288f8367e1bb143e90bb3f0512"],"data":"0x00000000000000000000000000000000000000000000000000000000000186a0","blockNumber":"0x5","logIndex":"0x3","removed":false}]"#,
        )
        .unwrap();
        let payment = address!("79104ae2cbd7c67478e27c214ef03cb1dcb7db3e");
        assert_eq!(execution_outcome(&logs, payment), ExecutionOutcome::Settled);
    }

    #[test]
    fn outcome_from_logs() {
        let payment = address!("1111111111111111111111111111111111111111");
        let other = address!("2222222222222222222222222222222222222222");
        let log = |address: Address, topic0: B256| ReceiptLog { address, topics: vec![topic0], data: Bytes::new() };
        assert_eq!(execution_outcome(&[], payment), ExecutionOutcome::NoEvents);
        assert_eq!(
            execution_outcome(&[log(other, IPayment::Settled::SIGNATURE_HASH)], payment),
            ExecutionOutcome::NoEvents,
            "events from other addresses are ignored"
        );
        assert_eq!(
            execution_outcome(&[log(payment, IPayment::Recovered::SIGNATURE_HASH)], payment),
            ExecutionOutcome::RecoveredOnly
        );
        assert_eq!(
            execution_outcome(
                &[log(payment, IPayment::Settled::SIGNATURE_HASH), log(payment, IPayment::Recovered::SIGNATURE_HASH)],
                payment
            ),
            ExecutionOutcome::Settled,
            "settled with excess returned to recovery"
        );
    }
}
