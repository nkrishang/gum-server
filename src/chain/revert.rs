//! Why a settlement reverted, decoded from `PaymentFactory.execute`'s revert data.
//!
//! `execute` deploys the `Payment` with CREATE2 and passes on the constructor's revert data
//! unchanged, and the constructor wraps a failing settlement call's own revert data in
//! `CallFailed(index, revertData)`. So the bytes gum-engine reports when its simulation of
//! `execute` reverts say exactly what went wrong, down to the token's own reason:
//!
//! ```text
//! CallFailed(0, Error("Blacklistable: account is blacklisted"))
//! ```
//!
//! Decoding is best effort. `Payment`'s and `PaymentFactory`'s errors are known exactly; a call
//! target's reason is decoded when it is a Solidity `Error(string)` or `Panic(uint256)`, or one of
//! the custom errors below that the registry's tokens are known to use. Anything else is kept as
//! an unrecognised error with its selector, and the raw bytes are always kept alongside.

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolError, SolInterface, sol};
use serde_json::{Map, Value, json};

use super::payment::{Call, IERC20};

sol! {
    /// gum-contracts `Payment` constructor errors, passed on by `PaymentFactory.execute`.
    interface IPaymentErrors {
        error InsufficientTokenBalance(uint256 balance, uint256 required);
        error CallFailed(uint256 index, bytes revertData);
        error CallTargetHasNoCode(uint256 index, address target);
        error AmountNotSpent(uint256 remaining);
    }

    /// gum-contracts `PaymentFactory` errors.
    interface IPaymentFactoryErrors {
        error AlreadyDeployed();
        error DeploymentFailed();
    }

    /// Custom errors settlement call targets are known to revert with. Tokens that revert with a
    /// string (Circle's FiatToken, Tether's USDT0) need no entry: `Error(string)` covers them.
    interface ICallTargetErrors {
        // OpenZeppelin v5 `IERC20Errors`, e.g. AgoraDollar (AUSD).
        error ERC20InsufficientBalance(address sender, uint256 balance, uint256 needed);
        error ERC20InvalidSender(address sender);
        error ERC20InvalidReceiver(address receiver);
        error ERC20InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        error ERC20InvalidApprover(address approver);
        error ERC20InvalidSpender(address spender);
        // OpenZeppelin v5 `Pausable`.
        error EnforcedPause();
        // AgoraDollar (AUSD).
        error AccountIsFrozen(address frozenAccount);
        error TransferPaused();
        // Solady `ERC20` (gum-contracts' local MockStablecoin).
        error InsufficientBalance();
        error InsufficientAllowance();
        // Solady `SafeTransferLib`. `Payment` itself reverts with `TransferFailed` when forwarding
        // the excess or an expired balance to recovery fails.
        error TransferFailed();
        error TransferFromFailed();
        error ApproveFailed();
    }
}

/// A decoded revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// `Payment`: the address holds less than the amount it settles.
    InsufficientTokenBalance { balance: U256, required: U256 },
    /// `Payment`: settlement call `index` reverted with `reason`, the target's own revert.
    CallFailed { index: U256, reason: Box<Reason> },
    /// `Payment`: settlement call `index` targets an address with no code.
    CallTargetHasNoCode { index: U256, target: Address },
    /// `Payment`: the calls succeeded but left `remaining` of the amount unspent.
    AmountNotSpent { remaining: U256 },
    /// `PaymentFactory`: the payment was already executed.
    AlreadyDeployed,
    /// `PaymentFactory`: the constructor reverted without data (e.g. out of gas).
    DeploymentFailed,
    /// `Error(string)`: `require(…, "reason")` or `revert("reason")`.
    Message(String),
    /// `Panic(uint256)`: a failed assert, arithmetic overflow, out-of-bounds index, …
    Panic(U256),
    /// One of [`ICallTargetErrors`], with its arguments.
    Custom { name: &'static str, args: Vec<(&'static str, Value)> },
    /// Reverted without data.
    Empty,
    /// Data we cannot decode: an error we do not know, or a truncated one.
    Unrecognised(Bytes),
}

/// Decodes the revert data of `PaymentFactory.execute`.
pub fn decode_execute(data: &[u8]) -> Reason {
    if let Ok(error) = IPaymentErrors::IPaymentErrorsErrors::abi_decode(data) {
        use IPaymentErrors::IPaymentErrorsErrors as E;
        return match error {
            E::InsufficientTokenBalance(e) => {
                Reason::InsufficientTokenBalance { balance: e.balance, required: e.required }
            }
            E::CallFailed(e) => Reason::CallFailed { index: e.index, reason: Box::new(decode_call(&e.revertData)) },
            E::CallTargetHasNoCode(e) => Reason::CallTargetHasNoCode { index: e.index, target: e.target },
            E::AmountNotSpent(e) => Reason::AmountNotSpent { remaining: e.remaining },
        };
    }
    if let Ok(error) = IPaymentFactoryErrors::IPaymentFactoryErrorsErrors::abi_decode(data) {
        use IPaymentFactoryErrors::IPaymentFactoryErrorsErrors as E;
        return match error {
            E::AlreadyDeployed(_) => Reason::AlreadyDeployed,
            E::DeploymentFailed(_) => Reason::DeploymentFailed,
        };
    }
    decode_call(data)
}

/// Decodes the revert data of a settlement call's target (or of anything else `Payment` calls).
pub fn decode_call(data: &[u8]) -> Reason {
    if data.is_empty() {
        return Reason::Empty;
    }
    if let Ok(e) = alloy_sol_types::Revert::abi_decode(data) {
        return Reason::Message(e.reason);
    }
    if let Ok(e) = alloy_sol_types::Panic::abi_decode(data) {
        return Reason::Panic(e.code);
    }
    let Ok(error) = ICallTargetErrors::ICallTargetErrorsErrors::abi_decode(data) else {
        return Reason::Unrecognised(Bytes::copy_from_slice(data));
    };
    use ICallTargetErrors::ICallTargetErrorsErrors as E;
    let address = |a: Address| json!(format!("{a:#x}"));
    let amount = |v: U256| json!(v.to_string());
    let (name, args) = match error {
        E::ERC20InsufficientBalance(e) => (
            "ERC20InsufficientBalance",
            vec![("sender", address(e.sender)), ("balance", amount(e.balance)), ("needed", amount(e.needed))],
        ),
        E::ERC20InvalidSender(e) => ("ERC20InvalidSender", vec![("sender", address(e.sender))]),
        E::ERC20InvalidReceiver(e) => ("ERC20InvalidReceiver", vec![("receiver", address(e.receiver))]),
        E::ERC20InsufficientAllowance(e) => (
            "ERC20InsufficientAllowance",
            vec![("spender", address(e.spender)), ("allowance", amount(e.allowance)), ("needed", amount(e.needed))],
        ),
        E::ERC20InvalidApprover(e) => ("ERC20InvalidApprover", vec![("approver", address(e.approver))]),
        E::ERC20InvalidSpender(e) => ("ERC20InvalidSpender", vec![("spender", address(e.spender))]),
        E::EnforcedPause(_) => ("EnforcedPause", vec![]),
        E::AccountIsFrozen(e) => ("AccountIsFrozen", vec![("frozenAccount", address(e.frozenAccount))]),
        E::TransferPaused(_) => ("TransferPaused", vec![]),
        E::InsufficientBalance(_) => ("InsufficientBalance", vec![]),
        E::InsufficientAllowance(_) => ("InsufficientAllowance", vec![]),
        E::TransferFailed(_) => ("TransferFailed", vec![]),
        E::TransferFromFailed(_) => ("TransferFromFailed", vec![]),
        E::ApproveFailed(_) => ("ApproveFailed", vec![]),
    };
    Reason::Custom { name, args }
}

impl Reason {
    /// The Solidity error's name; `None` when there is no error to name (empty or unrecognised).
    pub fn name(&self) -> Option<&'static str> {
        Some(match self {
            Self::InsufficientTokenBalance { .. } => "InsufficientTokenBalance",
            Self::CallFailed { .. } => "CallFailed",
            Self::CallTargetHasNoCode { .. } => "CallTargetHasNoCode",
            Self::AmountNotSpent { .. } => "AmountNotSpent",
            Self::AlreadyDeployed => "AlreadyDeployed",
            Self::DeploymentFailed => "DeploymentFailed",
            Self::Message(_) => "Error",
            Self::Panic(_) => "Panic",
            Self::Custom { name, .. } => name,
            Self::Empty | Self::Unrecognised(_) => return None,
        })
    }

    /// A bounded label for metrics: the error's name, `empty` or `unrecognised`, and for
    /// `CallFailed` the call's reason too (`CallFailed:Error`).
    pub fn label(&self) -> String {
        let name = |r: &Reason| match r {
            Self::Empty => "empty",
            Self::Unrecognised(_) => "unrecognised",
            other => other.name().expect("named"),
        };
        match self {
            Self::CallFailed { reason, .. } => format!("CallFailed:{}", name(reason)),
            other => name(other).to_owned(),
        }
    }

    /// One sentence saying what went wrong, for `failure.message`. `calls` are the deposit's
    /// settlement calls, which the call index refers to.
    pub fn describe(&self, calls: &[Call]) -> String {
        match self {
            Self::InsufficientTokenBalance { balance, required } => {
                format!("the payment address holds {balance}, less than the {required} it settles")
            }
            Self::CallFailed { index, reason } => {
                format!("settlement {} reverted: {}", describe_call(*index, calls), reason.describe_call_reason())
            }
            Self::CallTargetHasNoCode { index, target } => {
                format!("settlement call {index} targets {target:#x}, which has no code on this chain")
            }
            Self::AmountNotSpent { remaining } => format!(
                "the settlement calls succeeded but left {remaining} of the amount unspent \
                 (a token that returns false instead of reverting does this)"
            ),
            Self::AlreadyDeployed => "the payment was already executed: an earlier execute went through, and its \
                 receipt says whether it settled or went to recovery"
                .to_owned(),
            Self::DeploymentFailed => "the Payment constructor reverted without data (e.g. out of gas)".to_owned(),
            Self::Custom { name: "TransferFailed", .. } => {
                "the payment's own transfer of the token to the recovery address failed".to_owned()
            }
            other => format!("reverted: {}", other.describe_call_reason()),
        }
    }

    /// A call target's reason, as it reads after "reverted: ".
    fn describe_call_reason(&self) -> String {
        match self {
            Self::Message(message) => message.clone(),
            Self::Panic(code) => match u32::try_from(*code).ok().and_then(alloy_sol_types::PanicKind::from_number) {
                Some(kind) => format!("panic {code:#x} ({kind})"),
                None => format!("panic {code:#x}"),
            },
            Self::Custom { name, args } => {
                let args: Vec<String> = args.iter().map(|(k, v)| format!("{k}: {}", plain(v))).collect();
                format!("{name}({})", args.join(", "))
            }
            Self::Empty => "no reason given".to_owned(),
            Self::Unrecognised(data) if data.len() >= 4 => {
                format!("an unrecognised error (selector 0x{})", hex::encode(&data[..4]))
            }
            Self::Unrecognised(data) => format!("unrecognised data {data}"),
            // Payment's own errors never come from a call target; describe them in full.
            other => other.describe(&[]),
        }
    }

    /// The structured form served as `failure.revert`:
    /// `{"name": "CallFailed", "args": {"index": 0, "reason": {…}}, "call": {"index": 0, "target": "0x…"}}`.
    /// `name` is null for an empty or unrecognised revert; an unrecognised one carries `selector`.
    pub fn to_json(&self, calls: &[Call]) -> Value {
        let mut out = Map::new();
        out.insert("name".into(), json!(self.name()));
        let args: Vec<(&str, Value)> = match self {
            Self::InsufficientTokenBalance { balance, required } => {
                vec![("balance", json!(balance.to_string())), ("required", json!(required.to_string()))]
            }
            Self::CallFailed { index, reason } => vec![("index", index_json(*index)), ("reason", reason.to_json(&[]))],
            Self::CallTargetHasNoCode { index, target } => {
                vec![("index", index_json(*index)), ("target", json!(format!("{target:#x}")))]
            }
            Self::AmountNotSpent { remaining } => vec![("remaining", json!(remaining.to_string()))],
            Self::Message(message) => vec![("message", json!(message))],
            Self::Panic(code) => vec![("code", json!(format!("{code:#x}")))],
            Self::Custom { args, .. } => args.clone(),
            Self::Unrecognised(data) if data.len() >= 4 => {
                out.insert("selector".into(), json!(format!("0x{}", hex::encode(&data[..4]))));
                vec![]
            }
            _ => vec![],
        };
        if !args.is_empty() {
            out.insert("args".into(), Value::Object(args.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()));
        }
        if let Self::CallFailed { index, .. } | Self::CallTargetHasNoCode { index, .. } = self
            && let Some(call) = call_at(*index, calls)
        {
            out.insert("call".into(), json!({ "index": index_json(*index), "target": format!("{:#x}", call.target) }));
        }
        Value::Object(out)
    }
}

/// `failure.revert`: the decoded reason of `execute`'s revert `data`, plus the raw bytes.
pub fn view(data: &Bytes, calls: &[Call]) -> Value {
    let mut view = decode_execute(data).to_json(calls);
    view["data"] = json!(data.to_string());
    view
}

fn call_at(index: U256, calls: &[Call]) -> Option<&Call> {
    usize::try_from(index).ok().and_then(|i| calls.get(i))
}

/// Call indices are small; one that is not is shown as a string rather than lost.
fn index_json(index: U256) -> Value {
    u64::try_from(index).map(Value::from).unwrap_or_else(|_| json!(index.to_string()))
}

/// "call 0 (transfer of 2500000 to 0x…)" for a token transfer; "call 0 (to 0x…)" otherwise.
fn describe_call(index: U256, calls: &[Call]) -> String {
    let Some(call) = call_at(index, calls) else { return format!("call {index}") };
    match IERC20::transferCall::abi_decode(&call.data) {
        Ok(t) => format!("call {index} (transfer of {} to {:#x})", t.amount, t.to),
        Err(_) => format!("call {index} (to {:#x})", call.target),
    }
}

/// A JSON string without its quotes.
fn plain(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, hex};

    const USDC: Address = address!("e7f1725E7734CE288F8367e1Bb143E90bb3F0512");
    const RECEIVER: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");

    fn calls() -> Vec<Call> {
        vec![Call::transfer(USDC, RECEIVER, U256::from(2_500_000u64))]
    }

    fn describe(data: &[u8]) -> String {
        decode_execute(data).describe(&calls())
    }

    // Revert data returned by `eth_estimateGas` for `PaymentFactory.execute` on Anvil, with the
    // gum-contracts PR #1 factory and MockStablecoin deployed by LocalBootstrap: exactly what
    // gum-engine relays when its simulation reverts.
    const UNDERFUNDED: &[u8] = &hex!(
        "a17124f800000000000000000000000000000000000000000000000000000000000f424000000000000000000000000000000000000000000000000000000000002625a0"
    );
    const BLACKLISTED: &[u8] = &hex!(
        "5c0dee5d00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008408c379a000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000025426c61636b6c69737461626c653a206163636f756e7420697320626c61636b6c697374656400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    );
    const PAUSED: &[u8] = &hex!(
        "5c0dee5d00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000006408c379a0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000105061757361626c653a207061757365640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
    );
    const OVERSPENT: &[u8] = &hex!(
        "5c0dee5d000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000004f4d678b800000000000000000000000000000000000000000000000000000000"
    );
    const UNDERSPENT: &[u8] = &hex!("e58991f10000000000000000000000000000000000000000000000000000000000000001");
    const NO_CODE: &[u8] = &hex!(
        "5dcee19d000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090f79bf6eb2c4f870365e785982e1f101e93b906"
    );
    const ALREADY_EXECUTED: &[u8] = &hex!("a6ef0ba1");
    const EXCESS_TO_RECOVERY_FAILED: &[u8] = &hex!("90b8ec18");

    #[test]
    fn payment_errors_from_a_real_factory() {
        assert_eq!(
            decode_execute(UNDERFUNDED),
            Reason::InsufficientTokenBalance { balance: U256::from(1_000_000u64), required: U256::from(2_500_000u64) }
        );
        assert_eq!(describe(UNDERFUNDED), "the payment address holds 1000000, less than the 2500000 it settles");

        assert_eq!(decode_execute(UNDERSPENT), Reason::AmountNotSpent { remaining: U256::from(1u64) });
        assert!(describe(UNDERSPENT).starts_with("the settlement calls succeeded but left 1 of the amount unspent"));

        let eoa = address!("90F79bf6EB2c4f870365E785982E1f101E93b906");
        assert_eq!(decode_execute(NO_CODE), Reason::CallTargetHasNoCode { index: U256::ZERO, target: eoa });
        assert_eq!(
            describe(NO_CODE),
            "settlement call 0 targets 0x90f79bf6eb2c4f870365e785982e1f101e93b906, which has no code on this chain"
        );

        assert_eq!(decode_execute(ALREADY_EXECUTED), Reason::AlreadyDeployed);
        assert!(describe(ALREADY_EXECUTED).starts_with("the payment was already executed"));

        assert_eq!(decode_execute(EXCESS_TO_RECOVERY_FAILED), Reason::Custom { name: "TransferFailed", args: vec![] });
        assert_eq!(
            describe(EXCESS_TO_RECOVERY_FAILED),
            "the payment's own transfer of the token to the recovery address failed"
        );
    }

    #[test]
    fn a_failed_call_carries_the_tokens_own_reason() {
        assert_eq!(
            decode_execute(BLACKLISTED),
            Reason::CallFailed {
                index: U256::ZERO,
                reason: Box::new(Reason::Message("Blacklistable: account is blacklisted".into()))
            }
        );
        assert_eq!(
            describe(BLACKLISTED),
            "settlement call 0 (transfer of 2500000 to 0x70997970c51812dc3a010c7d01b50e0d17dc79c8) reverted: \
             Blacklistable: account is blacklisted"
        );
        assert!(describe(PAUSED).ends_with("reverted: Pausable: paused"));
        assert!(describe(OVERSPENT).ends_with("reverted: InsufficientBalance()"), "Solady ERC20");
    }

    fn call_failed(index: u64, revert_data: Vec<u8>) -> Vec<u8> {
        IPaymentErrors::CallFailed { index: U256::from(index), revertData: revert_data.into() }.abi_encode()
    }

    #[test]
    fn custom_token_errors_panics_and_the_unknown() {
        // Monad AUSD (AgoraDollar, OpenZeppelin v5), observed via eth_call of transfer(…, 1000000)
        // from an address holding none.
        let ausd = hex!(
            "e450d38c"
            "0000000000000000000000001111111111111111111111111111111111111112"
            "0000000000000000000000000000000000000000000000000000000000000000"
            "00000000000000000000000000000000000000000000000000000000000f4240"
        );
        assert_eq!(
            describe(&call_failed(0, ausd.to_vec())),
            "settlement call 0 (transfer of 2500000 to 0x70997970c51812dc3a010c7d01b50e0d17dc79c8) reverted: \
             ERC20InsufficientBalance(sender: 0x1111111111111111111111111111111111111112, balance: 0, needed: 1000000)"
        );
        let frozen = ICallTargetErrors::AccountIsFrozen { frozenAccount: RECEIVER }.abi_encode();
        assert!(
            describe(&call_failed(0, frozen))
                .ends_with("AccountIsFrozen(frozenAccount: 0x70997970c51812dc3a010c7d01b50e0d17dc79c8)")
        );

        let overflow = alloy_sol_types::Panic { code: U256::from(0x11) }.abi_encode();
        assert!(
            describe(&call_failed(0, overflow)).ends_with("reverted: panic 0x11 (arithmetic underflow or overflow)")
        );
        assert!(describe(&call_failed(0, vec![])).ends_with("reverted: no reason given"));
        assert!(
            describe(&call_failed(0, hex!("deadbeef00").to_vec()))
                .ends_with("an unrecognised error (selector 0xdeadbeef)")
        );
        // A call index past the list (never from our own calls) still reads.
        assert!(describe(&call_failed(7, vec![])).starts_with("settlement call 7 reverted"));

        // Outside CallFailed: an unknown top-level error, a bare string, no data at all.
        assert_eq!(describe(&hex!("12345678")), "reverted: an unrecognised error (selector 0x12345678)");
        assert_eq!(describe(&alloy_sol_types::Revert::from("nope").abi_encode()), "reverted: nope");
        assert_eq!(decode_execute(&[]), Reason::Empty);
        assert_eq!(describe(&hex!("00")), "reverted: unrecognised data 0x00");
    }

    #[test]
    fn a_truncated_reason_is_kept_as_unrecognised() {
        // Payment caps a call's revert data at 0xffff bytes, so a long reason may not decode.
        let mut long = alloy_sol_types::Revert::from("x".repeat(100)).abi_encode();
        long.truncate(80);
        let reason = decode_execute(&call_failed(0, long.clone()));
        assert_eq!(
            reason,
            Reason::CallFailed { index: U256::ZERO, reason: Box::new(Reason::Unrecognised(long.into())) }
        );
        assert_eq!(reason.label(), "CallFailed:unrecognised");
    }

    #[test]
    fn structured_view() {
        let view = view(&Bytes::from_static(BLACKLISTED), &calls());
        assert_eq!(
            view,
            json!({
                "name": "CallFailed",
                "args": { "index": 0, "reason": { "name": "Error", "args": { "message": "Blacklistable: account is blacklisted" } } },
                "call": { "index": 0, "target": "0xe7f1725e7734ce288f8367e1bb143e90bb3f0512" },
                "data": format!("0x{}", hex::encode(BLACKLISTED)),
            })
        );
        assert_eq!(
            decode_execute(UNDERFUNDED).to_json(&calls()),
            json!({ "name": "InsufficientTokenBalance", "args": { "balance": "1000000", "required": "2500000" } })
        );
        assert_eq!(decode_execute(&hex!("12345678")).to_json(&[]), json!({ "name": null, "selector": "0x12345678" }));
        assert_eq!(decode_execute(&[]).to_json(&[]), json!({ "name": null }));
    }

    #[test]
    fn labels_are_bounded() {
        assert_eq!(decode_execute(BLACKLISTED).label(), "CallFailed:Error");
        assert_eq!(decode_execute(OVERSPENT).label(), "CallFailed:InsufficientBalance");
        assert_eq!(decode_execute(UNDERFUNDED).label(), "InsufficientTokenBalance");
        assert_eq!(decode_execute(ALREADY_EXECUTED).label(), "AlreadyDeployed");
        assert_eq!(decode_execute(&hex!("12345678")).label(), "unrecognised");
        assert_eq!(decode_execute(&[]).label(), "empty");
    }
}
