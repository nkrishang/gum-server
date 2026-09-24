//! `PaymentFactory` address derivation and calldata, reproduced off-chain so `POST /v1/deposit` never
//! touches an RPC.
//!
//! The factory (gum-contracts `PaymentFactory.sol`) is an ownerless CREATE2 deployer. A payment is
//! described by seven terms, one of which is the ordered list of calls it makes on settlement, and
//! its address commits to every one of them:
//!
//! ```text
//! terms          = abi.encode(token, amount, calls, expirationTimestamp, recovery, salt, chainId)
//! implementation = CREATE(factory, nonce = 1)
//! initCode       = Payment.creationCode ++ abi.encode(implementation, terms)
//! payment        = CREATE2(factory, bytes32(0), keccak256(initCode))
//! ```
//!
//! `terms` is exactly the calldata arguments of `execute`. `Payment.creationCode` is pinned in
//! `payment_creation_code.hex` and differs per contract generation: a new factory generation means
//! a new creation code here.

use std::sync::LazyLock;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolEvent, SolValue, sol};

sol! {
    /// gum-contracts `Payment.Call`: one call the payment makes, as itself, on settlement.
    #[derive(Debug, PartialEq, Eq)]
    struct Call {
        address target;
        bytes data;
    }

    /// gum-contracts `PaymentFactory`.
    interface IPaymentFactory {
        function execute(
            address token,
            uint256 amount,
            Call[] calls,
            uint64 expirationTimestamp,
            address recovery,
            bytes32 salt,
            uint256 chainId
        ) external;
    }

    /// gum-contracts `Payment` events, emitted by the payment address during `execute`.
    interface IPayment {
        event Settled(address indexed token, uint256 amount);
        event Recovered(address indexed recovery, address indexed token, uint256 amount);
        event WrongChain(uint256 expectedChainId, uint256 actualChainId);
    }

    interface IERC20 {
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

/// `type(Payment).creationCode` of the factory generation this server targets: the `bytecode` of
/// gum-contracts' `out/Payment.sol/Payment.json`.
static PAYMENT_CREATION_CODE: LazyLock<Vec<u8>> = LazyLock::new(|| {
    hex::decode(include_str!("payment_creation_code.hex").trim()).expect("payment_creation_code.hex is valid hex")
});

impl Call {
    /// `token.transfer(to, amount)`: the whole of a plain payment's settlement.
    pub fn transfer(token: Address, to: Address, amount: U256) -> Self {
        Self { target: token, data: IERC20::transferCall { to, amount }.abi_encode().into() }
    }
}

/// `PaymentFactory.paymentImplementation()`: `Payment`'s runtime, deployed by the factory's
/// constructor as its first creation. Every payment's stub delegatecalls it.
pub fn payment_implementation(factory: Address) -> Address {
    factory.create(1)
}

/// The seven terms a payment is described by. The counterfactual address commits to all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentTerms {
    pub token: Address,
    pub amount: U256,
    /// Run in order on settlement; together they must spend exactly `amount`.
    pub calls: Vec<Call>,
    pub expiration_timestamp: u64,
    pub recovery: Address,
    pub salt: B256,
    pub chain_id: u64,
}

impl PaymentTerms {
    /// `PaymentFactory.paymentAddress(...)` for a factory deployed at `factory`.
    pub fn payment_address(&self, factory: Address) -> Address {
        factory.create2(B256::ZERO, keccak256(self.init_code(factory)))
    }

    /// Calldata for `PaymentFactory.execute(...)`.
    pub fn execute_calldata(&self) -> Bytes {
        IPaymentFactory::executeCall {
            token: self.token,
            amount: self.amount,
            calls: self.calls.clone(),
            expirationTimestamp: self.expiration_timestamp,
            recovery: self.recovery,
            salt: self.salt,
            chainId: U256::from(self.chain_id),
        }
        .abi_encode()
        .into()
    }

    /// `Payment.creationCode ++ abi.encode(paymentImplementation(), terms)`, where `terms` is
    /// `execute`'s calldata without its selector, exactly as the factory copies it.
    fn init_code(&self, factory: Address) -> Vec<u8> {
        let terms = Bytes::copy_from_slice(&self.execute_calldata()[4..]);
        let args = (payment_implementation(factory), terms).abi_encode_params();
        [PAYMENT_CREATION_CODE.as_slice(), &args].concat()
    }
}

/// What a `Payment` constructor did, read from the receipt logs of the `execute` transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOutcome {
    /// `Settled(token, amount)` was emitted by the payment address: every call succeeded and
    /// together they spent exactly `amount`.
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

    const FACTORY: Address = address!("5FbDB2315678afecb367f032d93F642f64180aa3");
    const USDC: Address = address!("e7f1725E7734CE288F8367e1Bb143E90bb3F0512");
    const RECEIVER: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    const RECOVERY: Address = address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

    fn terms() -> PaymentTerms {
        let amount = U256::from(2_500_000u64);
        PaymentTerms {
            token: USDC,
            amount,
            calls: vec![Call::transfer(USDC, RECEIVER, amount)],
            expiration_timestamp: 1_800_000_000,
            recovery: RECOVERY,
            salt: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            chain_id: 31337,
        }
    }

    /// Pins the generation: gum-contracts `4ab1e5b` (PR #1, settlement calls), solc 0.8.33,
    /// 1,000,000 optimizer runs, EVM prague. Any change to `Payment.sol` changes this hash, and
    /// needs a new factory generation and a new `payment_creation_code.hex`.
    #[test]
    fn creation_code_is_the_pinned_generation() {
        assert_eq!(PAYMENT_CREATION_CODE.len(), 1895);
        assert_eq!(
            keccak256(PAYMENT_CREATION_CODE.as_slice()),
            b256!("4f39ceb944bf73d048a1dff309225e93ba3f3ae0935a272387916636a5e3ccc0")
        );
    }

    /// Known answers from `PaymentFactory.paymentImplementation()` and `paymentAddress(...)` on
    /// Anvil, with the factory deployed by gum-contracts' LocalBootstrap at its fixture address.
    #[test]
    fn matches_the_deployed_factory() {
        assert_eq!(payment_implementation(FACTORY), address!("a16E02E87b7454126E5E10d957A927A7F5B5d2be"));

        let salt_ab =
            PaymentTerms { salt: b256!("00000000000000000000000000000000000000000000000000000000000000ab"), ..terms() };
        assert_eq!(salt_ab.payment_address(FACTORY), address!("9278095fE07F7A3B4A4a7cB37Ff47152092E4181"));

        let amount = U256::from(999_999_999_999u64);
        let other = PaymentTerms {
            amount,
            calls: vec![Call::transfer(USDC, RECEIVER, amount)],
            salt: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            chain_id: 8453,
            ..terms()
        };
        assert_eq!(other.payment_address(FACTORY), address!("10f732E807688463d1b2eE09C1aFb34A5d7854Ee"));

        let split = PaymentTerms {
            calls: vec![
                Call::transfer(USDC, RECEIVER, U256::from(2_000_000u64)),
                Call::transfer(USDC, RECOVERY, U256::from(500_000u64)),
            ],
            ..salt_ab
        };
        assert_eq!(split.payment_address(FACTORY), address!("89B40661586f3C05F37D876E0EaaDD4e14DF39ed"));
    }

    #[test]
    fn address_commits_to_every_parameter() {
        let base = terms().payment_address(FACTORY);
        let variants = vec![
            PaymentTerms { token: RECEIVER, ..terms() },
            PaymentTerms { amount: U256::from(2_500_001u64), ..terms() },
            PaymentTerms { calls: vec![Call::transfer(USDC, RECOVERY, U256::from(2_500_000u64))], ..terms() },
            PaymentTerms { calls: vec![], ..terms() },
            PaymentTerms { expiration_timestamp: 1_800_000_001, ..terms() },
            PaymentTerms { recovery: RECEIVER, ..terms() },
            PaymentTerms { salt: B256::ZERO, ..terms() },
            PaymentTerms { chain_id: 8453, ..terms() },
        ];
        for v in variants {
            assert_ne!(v.payment_address(FACTORY), base, "{v:?}");
        }
        assert_ne!(terms().payment_address(RECOVERY), base);
        assert_eq!(terms().payment_address(FACTORY), base, "deterministic");
    }

    #[test]
    fn a_plain_payment_is_one_transfer_of_the_amount_to_the_receiver() {
        let call = Call::transfer(USDC, RECEIVER, U256::from(2_500_000u64));
        assert_eq!(call.target, USDC);
        let transfer = IERC20::transferCall::abi_decode(&call.data).unwrap();
        assert_eq!((transfer.to, transfer.amount), (RECEIVER, U256::from(2_500_000u64)));
        assert_eq!(&call.data[..4], [0xa9, 0x05, 0x9c, 0xbb], "transfer(address,uint256)");
    }

    #[test]
    fn execute_calldata_round_trips() {
        let t = terms();
        let data = t.execute_calldata();
        assert_eq!(&data[..4], IPaymentFactory::executeCall::SELECTOR);
        // Head (7 words), the array (length, one offset), the call (target, offset, length, 68 bytes padded to 96).
        assert_eq!(data.len(), 4 + 7 * 32 + 2 * 32 + 3 * 32 + 96);
        let decoded = IPaymentFactory::executeCall::abi_decode(&data).unwrap();
        assert_eq!(decoded.calls, t.calls);
        assert_eq!(decoded.expirationTimestamp, 1_800_000_000);
        assert_eq!(decoded.chainId, U256::from(31337));
    }

    #[test]
    fn event_topics_match_the_receipt_of_a_real_execute() {
        assert_eq!(
            IPayment::Settled::SIGNATURE_HASH,
            b256!("7823e479a1a4ebe2418874847436f8a1680c5ee5b17f38bb59dbff28e1b45552")
        );
        assert_eq!(
            IPayment::Recovered::SIGNATURE_HASH,
            b256!("fff3b3844276f57024e0b42afec1a37f75db36511e43819a4f2a63ab7862b648")
        );
        // The receipt of a real `execute` on Anvil: the payment was funded with 0.1 USDC too much,
        // so it forwards the excess to recovery, runs its one transfer, then settles.
        let logs: Vec<ReceiptLog> = serde_json::from_str(
            r#"[{"address":"0xe7f1725e7734ce288f8367e1bb143e90bb3f0512","topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef","0x000000000000000000000000aff4cd731512fecba987d8fd35f9c7469afa90c7","0x0000000000000000000000003c44cdddb6a900fa2b585dd299e03d12fa4293bc"],"data":"0x00000000000000000000000000000000000000000000000000000000000186a0","blockNumber":"0x5","logIndex":"0x0","removed":false},
               {"address":"0xaff4cd731512fecba987d8fd35f9c7469afa90c7","topics":["0xfff3b3844276f57024e0b42afec1a37f75db36511e43819a4f2a63ab7862b648","0x0000000000000000000000003c44cdddb6a900fa2b585dd299e03d12fa4293bc","0x000000000000000000000000e7f1725e7734ce288f8367e1bb143e90bb3f0512"],"data":"0x00000000000000000000000000000000000000000000000000000000000186a0","blockNumber":"0x5","logIndex":"0x1","removed":false},
               {"address":"0xe7f1725e7734ce288f8367e1bb143e90bb3f0512","topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef","0x000000000000000000000000aff4cd731512fecba987d8fd35f9c7469afa90c7","0x00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c8"],"data":"0x00000000000000000000000000000000000000000000000000000000002625a0","blockNumber":"0x5","logIndex":"0x2","removed":false},
               {"address":"0xaff4cd731512fecba987d8fd35f9c7469afa90c7","topics":["0x74a5b5c63602662a2c967556916428411d055eeb47c4d96e6325304cb5603a99","0x0000000000000000000000000000000000000000000000000000000000000000","0x000000000000000000000000e7f1725e7734ce288f8367e1bb143e90bb3f0512"],"data":"0x000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000044a9059cbb00000000000000000000000070997970c51812dc3a010c7d01b50e0d17dc79c800000000000000000000000000000000000000000000000000000000002625a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001","blockNumber":"0x5","logIndex":"0x3","removed":false},
               {"address":"0xaff4cd731512fecba987d8fd35f9c7469afa90c7","topics":["0x7823e479a1a4ebe2418874847436f8a1680c5ee5b17f38bb59dbff28e1b45552","0x000000000000000000000000e7f1725e7734ce288f8367e1bb143e90bb3f0512"],"data":"0x00000000000000000000000000000000000000000000000000000000002625a0","blockNumber":"0x5","logIndex":"0x4","removed":false}]"#,
        )
        .unwrap();
        let payment = address!("aff4cd731512fecba987d8fd35f9c7469afa90c7");
        let salt_cd = b256!("00000000000000000000000000000000000000000000000000000000000000cd");
        assert_eq!(PaymentTerms { salt: salt_cd, ..terms() }.payment_address(FACTORY), payment);
        assert_eq!(execution_outcome(&logs, payment), ExecutionOutcome::Settled);
        let settled =
            logs.iter().find(|l| l.address == payment && l.topics[0] == IPayment::Settled::SIGNATURE_HASH).unwrap();
        let event = IPayment::Settled::decode_raw_log(settled.topics.iter().copied(), &settled.data).unwrap();
        assert_eq!((event.token, event.amount), (USDC, U256::from(2_500_000u64)));
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
                &[log(payment, IPayment::Recovered::SIGNATURE_HASH), log(payment, IPayment::Settled::SIGNATURE_HASH)],
                payment
            ),
            ExecutionOutcome::Settled,
            "excess returned to recovery before the calls ran, then settled"
        );
    }
}
