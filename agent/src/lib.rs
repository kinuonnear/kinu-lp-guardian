//! KINU LP Guardian agent: the decision logic and the NEAR transaction encoding, host-free so they
//! are unit-tested natively. `main.rs` wires them to OutLayer's host functions inside the TEE.
//!
//! It serves the shared Guardian contract (one contract, a ledger and a policy per user).
//!
//! Trust model: a run's input is never trusted (any contract the caller transacts with can start a
//! run with input of its own choosing). The input only names users; the agent decides from chain
//! state it reads itself, on the one Guardian account pinned in this build and only while that
//! account runs the published code (code hash). It acts for a user only if the user names this
//! agent, and every action is checked again by the contract against the user's policy. It never
//! opens a position (the user does) and never moves funds out (only the user can).

use borsh::BorshSerialize;
use serde::Deserialize;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------- chain views

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Policy {
    pub allowed_pools: Vec<String>,
    pub min_width: u32,
    pub max_width: u32,
    pub max_distance: u32,
    pub exit_drop_points: u32,
    #[serde(default)]
    pub exit_on_rise: bool,
    #[serde(default)]
    pub rebalance_buffer_points: u32,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct PositionRef {
    pub lpt_id: String,
    pub pool_id: String,
    pub left_point: i32,
    pub right_point: i32,
}

/// One user's record in the shared contract (`get_account`); ledger and owed are not needed here.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct AccountView {
    pub agent: Option<String>,
    pub policy: Policy,
    pub frozen: bool,
    pub position: Option<PositionRef>,
    pub high_point: Option<i32>,
    pub low_point: Option<i32>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct PoolView {
    pub pool_id: String,
    pub token_x: String,
    pub token_y: String,
    pub current_point: i32,
    pub point_delta: i32,
    /// Fee tier in millionths (2000 = 0.2%) and the active liquidity at the current point.
    #[serde(default)]
    pub fee: u32,
    #[serde(default)]
    pub liquidity: String,
}

/// What the agent reads before deciding (all from the chain).
pub struct World<'a> {
    pub me: &'a str,
    pub account: &'a AccountView,
    pub pool: &'a PoolView,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Noop(&'static str),
    Poke,
    Rebalance { pool_id: String, left_point: i32, right_point: i32 },
    Exit,
}

/// The price the Guardian contract read itself at the last poke (`get_seen`, contract v1.3).
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct SeenView {
    pub point: i32,
    pub at_secs: u64,
}

/// Contract v1.3 accepts an agent re-centre or exit only when the rule was already met at a price
/// it read 3 to 60 minutes before (so one block's pushed price is not enough).
pub const SEEN_MIN_AGE_SECS: u64 = 180;
pub const SEEN_MAX_AGE_SECS: u64 = 3_600;

fn moved_against(v: &AccountView, point: i32) -> i64 {
    if v.policy.exit_on_rise {
        i64::from(point) - i64::from(v.low_point.unwrap_or(point).min(point))
    } else {
        i64::from(v.high_point.unwrap_or(point).max(point)) - i64::from(point)
    }
}

/// Whether `action` was already due at the earlier price `then`, as the contract checks it.
fn confirmed_at(v: &AccountView, action: &Action, then: i32) -> bool {
    match action {
        Action::Exit => v.policy.exit_drop_points > 0 && moved_against(v, then) >= i64::from(v.policy.exit_drop_points),
        Action::Rebalance { left_point, right_point, .. } => {
            let Some(pos) = &v.position else { return false };
            let t = i64::from(then);
            let outside = if t < i64::from(pos.left_point) { i64::from(pos.left_point) - t } else if t >= i64::from(pos.right_point) { t - i64::from(pos.right_point) } else { 0 };
            let distance = if t < i64::from(*left_point) { i64::from(*left_point) - t } else if t > i64::from(*right_point) { t - i64::from(*right_point) } else { 0 };
            outside > 0 && outside > i64::from(v.policy.rebalance_buffer_points) && distance <= i64::from(v.policy.max_distance)
        }
        _ => true,
    }
}

/// `decide`, then contract v1.3's confirmation: an action goes out only if it was due at the price the
/// contract read 3–60 minutes ago; otherwise the agent pokes (the contract reads the price now) and
/// acts on a later tick if the rule still holds.
pub fn decide_confirmed(w: &World, seen: Option<&SeenView>, now_secs: u64) -> Action {
    let action = decide(w);
    if !matches!(action, Action::Exit | Action::Rebalance { .. }) {
        return action;
    }
    let age = seen.map(|s| now_secs.saturating_sub(s.at_secs));
    match (seen, age) {
        (Some(s), Some(a)) if (SEEN_MIN_AGE_SECS..=SEEN_MAX_AGE_SECS).contains(&a) && confirmed_at(w.account, &action, s.point) => action,
        (Some(_), Some(a)) if a < SEEN_MIN_AGE_SECS => Action::Noop("rule met; waiting for the confirmation window"),
        _ => Action::Poke,
    }
}

/// A one-sided range starts at least this many points away from the price, so a small move between
/// reading the pool and the transaction landing cannot put the price inside it (Rhea then wants both tokens).
pub const ONE_SIDED_MARGIN: i32 = 20;

fn floor_to(v: i32, d: i32) -> i32 {
    v.div_euclid(d) * d
}

/// Range width: the owner's minimum, rounded up to the pool's point spacing, within the maximum.
fn width(policy: &Policy, delta: i32) -> i32 {
    let d = delta.max(1);
    let w = (policy.min_width as i32 + d - 1) / d * d;
    w.min(floor_to(policy.max_width as i32, d)).max(d)
}

pub fn decide(w: &World) -> Action {
    let v = w.account;
    if v.agent.as_deref() != Some(w.me) {
        return Action::Noop("this user does not name this agent");
    }
    if v.frozen {
        return Action::Noop("the user froze the agent");
    }
    // Idle tokens are the user's to place: the agent only looks after an open position.
    let Some(pos) = &v.position else { return Action::Noop("no open position") };
    let p = w.pool;
    let d = p.point_delta.max(1);
    let cur = p.current_point;
    let wd = width(&v.policy, d);
    let exit_on = v.policy.exit_drop_points > 0;
    // How far the price moved against the user from the best point seen (the contract checks the
    // same thing again before it lets the agent exit).
    let moved = if v.policy.exit_on_rise {
        i64::from(cur) - i64::from(v.low_point.unwrap_or(cur).min(cur))
    } else {
        i64::from(v.high_point.unwrap_or(cur).max(cur)) - i64::from(cur)
    };
    if exit_on && moved >= i64::from(v.policy.exit_drop_points) {
        return Action::Exit;
    }
    // Re-centre only once the price is outside by more than the user's buffer, and never within
    // the one-sided margin: a re-entry sits up to MARGIN + spacing away from the price on purpose
    // and must not count as "out of range" on the next tick.
    let threshold = i64::from(v.policy.rebalance_buffer_points).max(i64::from(ONE_SIDED_MARGIN + d));
    if cur < pos.left_point && i64::from(pos.left_point) - i64::from(cur) > threshold {
        // Price fell below the range: the position is all token X; re-enter just above the price.
        let left = floor_to(cur + ONE_SIDED_MARGIN, d) + d;
        return Action::Rebalance { pool_id: pos.pool_id.clone(), left_point: left, right_point: left + wd };
    }
    if cur >= pos.right_point && i64::from(cur) - i64::from(pos.right_point) > threshold {
        // Price rose above the range: all token Y; re-enter just below the price.
        let right = floor_to(cur - ONE_SIDED_MARGIN, d);
        return Action::Rebalance { pool_id: pos.pool_id.clone(), left_point: right - wd, right_point: right };
    }
    // Record a new best point for the exit rule; with the rule off a poke would only cost gas.
    let new_mark = if v.policy.exit_on_rise { v.low_point.is_none_or(|l| cur < l) } else { v.high_point.is_none_or(|h| cur > h) };
    if exit_on && new_mark {
        return Action::Poke;
    }
    Action::Noop(if cur < pos.left_point || cur >= pos.right_point { "outside the range, within the buffer" } else { "in range" })
}

// ---------------------------------------------------------------- NEAR transactions (borsh, V0)

#[derive(BorshSerialize)]
pub enum PublicKey {
    Ed25519([u8; 32]),
}

#[derive(BorshSerialize)]
pub struct FunctionCall {
    pub method_name: String,
    pub args: Vec<u8>,
    pub gas: u64,
    pub deposit: u128,
}

/// Only the variants the agent sends; borsh tags follow NEAR's Action enum order.
pub enum NearAction {
    FunctionCall(FunctionCall),
    Transfer(u128),
    /// AddKey with a function-call permission: `receiver_id` only, these methods only, no deposit
    /// possible, gas from the account's balance (no allowance cap, so the key never runs dry).
    AddFunctionCallKey { public_key: [u8; 32], receiver_id: String, method_names: Vec<String> },
    DeleteKey([u8; 32]),
}

impl BorshSerialize for NearAction {
    fn serialize<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        match self {
            NearAction::FunctionCall(f) => {
                2u8.serialize(w)?; // CreateAccount 0, DeployContract 1, FunctionCall 2
                f.serialize(w)
            }
            NearAction::Transfer(amount) => {
                3u8.serialize(w)?; // Transfer 3
                amount.serialize(w)
            }
            NearAction::AddFunctionCallKey { public_key, receiver_id, method_names } => {
                5u8.serialize(w)?; // Stake 4, AddKey 5
                PublicKey::Ed25519(*public_key).serialize(w)?;
                0u64.serialize(w)?; // AccessKey.nonce (the chain sets its own)
                0u8.serialize(w)?; // AccessKeyPermission::FunctionCall (FullAccess is 1)
                Option::<u128>::None.serialize(w)?; // allowance
                receiver_id.serialize(w)?;
                method_names.serialize(w)
            }
            NearAction::DeleteKey(public_key) => {
                6u8.serialize(w)?; // DeleteKey 6
                PublicKey::Ed25519(*public_key).serialize(w)
            }
        }
    }
}

/// The only methods the agent's key may call after `restrict_key`, on the Guardian contract only.
pub const AGENT_METHODS: [&str; 3] = ["poke", "request_rebalance", "exit"];

/// One transaction that turns the agent's own full-access key into a function-call key: delete it,
/// add the same public key back limited to the Guardian contract and AGENT_METHODS. After it the key
/// cannot send NEAR, deploy, add keys or call anything else, even if it leaked from the TEE.
pub fn restrict_key_actions(public_key: [u8; 32], guardian: &str) -> Vec<NearAction> {
    vec![
        NearAction::DeleteKey(public_key),
        NearAction::AddFunctionCallKey { public_key, receiver_id: guardian.to_string(), method_names: AGENT_METHODS.iter().map(|m| m.to_string()).collect() },
    ]
}

#[derive(BorshSerialize)]
pub struct Transaction {
    pub signer_id: String,
    pub public_key: PublicKey,
    pub nonce: u64,
    pub receiver_id: String,
    pub block_hash: [u8; 32],
    pub actions: Vec<NearAction>,
}

#[derive(BorshSerialize)]
pub enum Signature {
    Ed25519([u8; 64]),
}

#[derive(BorshSerialize)]
pub struct SignedTransaction {
    pub transaction: Transaction,
    pub signature: Signature,
}

impl Transaction {
    /// What the key signs: sha256 of the borsh-encoded transaction.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(borsh::to_vec(self).expect("borsh")).into()
    }
}

pub fn implicit_account(public_key: &[u8; 32]) -> String {
    hex::encode(public_key)
}

pub fn near_public_key(public_key: &[u8; 32]) -> String {
    format!("ed25519:{}", bs58::encode(public_key).into_string())
}

// ---------------------------------------------------------------- cost guard

// Rhea DCL (iZiSwap) liquidity sums over points [l, r), the same as the vault contract and KINU's replay.
const SQRT_RATE: f64 = 1.000_049_998_750_062_5; // sqrt(1.0001)
fn sqrt_price(point: f64) -> f64 {
    SQRT_RATE.powf(point)
}
fn unit_y(l: f64, r: f64) -> f64 {
    if r <= l { 0.0 } else { (sqrt_price(r) - sqrt_price(l)) / (SQRT_RATE - 1.0) }
}
fn unit_x(l: f64, r: f64) -> f64 {
    if r <= l { 0.0 } else { (1.0 / sqrt_price(l) - 1.0 / sqrt_price(r)) * SQRT_RATE / (SQRT_RATE - 1.0) }
}

/// Raw token amounts a position of liquidity `l` over [left, right) holds at point `cur`.
pub fn position_amounts(l: f64, left: i32, right: i32, cur: i32) -> (f64, f64) {
    let (a, b) = (f64::from(left), f64::from(right));
    let s = f64::from(cur).clamp(a, b);
    (l * unit_x(s, b), l * unit_y(a, s))
}

/// Liquidity a re-entry gets from what the old range holds: the one-sided range uses one token only.
pub fn reentry_liquidity(old: &PositionRef, old_liquidity: f64, cur: i32, left: i32, right: i32) -> f64 {
    let (x, y) = position_amounts(old_liquidity, old.left_point, old.right_point, cur);
    let (a, b) = (f64::from(left), f64::from(right));
    if f64::from(cur) < a {
        let u = unit_x(a, b);
        if u > 0.0 { x / u } else { 0.0 }
    } else {
        let u = unit_y(a, b);
        if u > 0.0 { y / u } else { 0.0 }
    }
}

/// A day of fees for liquidity `l` at the current point: 24h volume × fee tier × its share of the
/// active liquidity. Fee tier in millionths (2000 = 0.2%).
pub fn daily_fee_usd(volume_24h_usd: f64, fee_millionths: u32, l: f64, pool_liquidity: f64) -> f64 {
    if l <= 0.0 {
        return 0.0;
    }
    volume_24h_usd * f64::from(fee_millionths) / 1e6 * l / (pool_liquidity + l)
}

/// NEAR one rebalance costs: 48.8 TGas burnt on Rhea DCL 2.4.0 (tests/shared.rs) at 0.1 NEAR per
/// 1000 TGas is 0.0049 NEAR; rounded up for gas-price swings. (300 TGas are attached, the rest refunded.)
pub const REBALANCE_GAS_NEAR: f64 = 0.006;

/// The cost guard: move the position only when a day of fees in the new range pays for the move.
/// The contract does not need this (its limits stand either way); it keeps the agent from spending
/// more on gas than a quiet pool would ever earn back.
pub fn rebalance_pays(daily_fee_usd: f64, near_usd: f64) -> bool {
    daily_fee_usd >= REBALANCE_GAS_NEAR * near_usd
}

/// The one call on the shared contract the agent makes for a user, as (method, args json).
pub fn call_for(action: &Action, user: &str) -> Option<(&'static str, String)> {
    use serde_json::json;
    match action {
        Action::Noop(_) => None,
        Action::Poke => Some(("poke", json!({ "user": user }).to_string())),
        Action::Exit => Some(("exit", json!({ "user": user }).to_string())),
        Action::Rebalance { pool_id, left_point, right_point } => Some((
            "request_rebalance",
            json!({ "user": user, "request": { "pool_id": pool_id, "left_point": left_point, "right_point": right_point } }).to_string(),
        )),
    }
}

/// At most this many users per run (one transaction each, and the output stays under 900 bytes).
pub const MAX_USERS_PER_RUN: usize = 5;

/// Gas the agent attaches per call: what the contract's chain for it reserves (v1.1 floors: move
/// 235, exit 120, poke ~30 TGas) plus margin. A wallet must hold about a tenth of a NEAR per 100 TGas
/// attached at signing (refunded), so the small calls no longer need the 0.3 NEAR a move does.
pub fn gas_for(method: &str) -> u64 {
    const T: u64 = 1_000_000_000_000;
    match method {
        "poke" => 50 * T,
        "exit" => 150 * T,
        _ => 300 * T,
    }
}

/// The users a run looks at: well-formed NEAR account ids from the (untrusted) input, deduplicated.
pub fn users_from_input(input: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in input["users"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
        let ok = (2..=64).contains(&u.len()) && u.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
        if ok && !out.iter().any(|o| o == u) {
            out.push(u.to_string());
        }
        if out.len() == MAX_USERS_PER_RUN {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrict_key_encodes_as_near_actions() {
        let pk = [7u8; 32];
        let actions = restrict_key_actions(pk, "lp.kinuonnear.near");
        let del = borsh::to_vec(&actions[0]).unwrap();
        assert_eq!(del[0], 6, "DeleteKey");
        assert_eq!(&del[1..], &[[0u8].as_slice(), &pk].concat()[..], "ed25519 key");
        let add = borsh::to_vec(&actions[1]).unwrap();
        assert_eq!(add[0], 5, "AddKey");
        assert_eq!(add[1], 0, "ed25519");
        assert_eq!(&add[2..34], &pk);
        assert_eq!(&add[34..42], &[0u8; 8], "nonce");
        assert_eq!(add[42], 0, "FunctionCall permission");
        assert_eq!(add[43], 0, "no allowance cap");
        // receiver (u32 length + bytes), then 3 method names.
        assert_eq!(&add[44..48], &(18u32).to_le_bytes());
        assert_eq!(&add[48..66], b"lp.kinuonnear.near");
        assert_eq!(&add[66..70], &(3u32).to_le_bytes());
        assert!(add.windows(17).any(|w| w == b"request_rebalance"));
        assert!(!add.windows(8).any(|w| w == b"withdraw"));
    }

    const ME: &str = "aa11";
    fn policy() -> Policy {
        Policy { allowed_pools: vec!["x|y|100".into()], min_width: 150, max_width: 4000, max_distance: 400, exit_drop_points: 2877, exit_on_rise: false, rebalance_buffer_points: 0 }
    }
    fn vault(position: Option<(i32, i32)>, high: Option<i32>) -> AccountView {
        AccountView {
            agent: Some(ME.into()),
            policy: policy(),
            frozen: false,
            position: position.map(|(l, r)| PositionRef { lpt_id: "x|y|100#1".into(), pool_id: "x|y|100".into(), left_point: l, right_point: r }),
            high_point: high,
            low_point: None,
        }
    }
    fn pool(point: i32) -> PoolView {
        PoolView { pool_id: "x|y|100".into(), token_x: "x".into(), token_y: "y".into(), current_point: point, point_delta: 40, fee: 100, liquidity: "0".into() }
    }
    fn run(v: &AccountView, point: i32, _wallet: &[(String, u128)], _funded: bool) -> Action {
        decide(&World { me: ME, account: v, pool: &pool(point) })
    }

    fn confirmed(v: &AccountView, point: i32, seen: Option<(i32, u64)>, now: u64) -> Action {
        let s = seen.map(|(point, at_secs)| SeenView { point, at_secs });
        decide_confirmed(&World { me: ME, account: v, pool: &pool(point) }, s.as_ref(), now)
    }

    #[test]
    fn acts_only_on_a_rule_already_met_minutes_earlier() {
        // Below the range by 100: a re-centre is due now.
        let v = vault(Some((-100, 100)), None);
        assert!(matches!(run(&v, -200, &[], true), Action::Rebalance { .. }));
        // Never read: poke first.
        assert_eq!(confirmed(&v, -200, None, 10_000), Action::Poke);
        // Read a minute ago: wait for the window.
        assert_eq!(confirmed(&v, -200, Some((-200, 9_940)), 10_000), Action::Noop("rule met; waiting for the confirmation window"));
        // Read 5 minutes ago while still in range (today's price may be a one-block push): read again.
        assert_eq!(confirmed(&v, -200, Some((0, 9_700)), 10_000), Action::Poke);
        // Read 5 minutes ago already outside: act.
        assert!(matches!(confirmed(&v, -200, Some((-190, 9_700)), 10_000), Action::Rebalance { .. }));
        // Read over an hour ago: read again.
        assert_eq!(confirmed(&v, -200, Some((-200, 6_000)), 10_000), Action::Poke);
        // Nothing due: pokes and no-ops pass through untouched.
        assert_eq!(confirmed(&vault(Some((-100, 100)), Some(10)), 0, None, 10_000), Action::Noop("in range"));
        assert_eq!(confirmed(&vault(Some((-100, 100)), Some(10)), 40, None, 10_000), Action::Poke, "a new high is still recorded");
    }

    #[test]
    fn never_opens_a_position() {
        // Idle tokens (a deposit without a range, or after an exit) are the user's to place.
        assert_eq!(run(&vault(None, None), 1000, &[], true), Action::Noop("no open position"));
    }

    #[test]
    fn re_enters_one_sided_next_to_the_price() {
        // Below the range by 100 (> margin 20 + spacing 40): just above the price, width 160.
        assert_eq!(run(&vault(Some((1000, 1160)), Some(1100)), 900, &[], false), Action::Rebalance { pool_id: "x|y|100".into(), left_point: 960, right_point: 1120 });
        // Above the range by 80: just below the price.
        assert_eq!(run(&vault(Some((1000, 1160)), Some(1100)), 1240, &[], false), Action::Rebalance { pool_id: "x|y|100".into(), left_point: 1040, right_point: 1200 });
    }

    /// A vault with the exit rule off, so only the rebalance logic speaks.
    fn rebalance_only(position: (i32, i32)) -> AccountView {
        let mut v = vault(Some(position), None);
        v.policy.exit_drop_points = 0;
        v
    }

    #[test]
    fn a_re_entry_is_not_rebalanced_again_on_the_next_tick() {
        // The v1 agent re-opened a one-sided range every tick while the price sat just outside it.
        for cur in [900, 1240] {
            let mut v = rebalance_only((1000, 1160));
            let Action::Rebalance { left_point, right_point, .. } = run(&v, cur, &[], false) else { panic!("{cur}") };
            v.position = Some(PositionRef { lpt_id: "x|y|100#2".into(), pool_id: "x|y|100".into(), left_point, right_point });
            assert_eq!(run(&v, cur, &[], false), Action::Noop("outside the range, within the buffer"), "{cur}");
        }
    }

    #[test]
    fn waits_for_the_owners_buffer() {
        let mut v = rebalance_only((1000, 1160));
        v.policy.rebalance_buffer_points = 200;
        assert_eq!(run(&v, 1300, &[], false), Action::Noop("outside the range, within the buffer"));
        assert_eq!(run(&v, 1360, &[], false), Action::Noop("outside the range, within the buffer"));
        assert!(matches!(run(&v, 1361, &[], false), Action::Rebalance { .. }));
        assert!(matches!(run(&v, 799, &[], false), Action::Rebalance { .. }));
    }

    #[test]
    fn exits_only_on_the_owners_rule() {
        assert_eq!(run(&vault(Some((-3000, -2800)), Some(0)), -2900, &[], false), Action::Exit);
        assert_eq!(run(&vault(Some((-100, 100)), Some(0)), -50, &[], false), Action::Noop("in range"));
    }

    #[test]
    fn exit_on_rise_watches_the_low() {
        let mut v = vault(Some((2800, 3000)), Some(5000));
        v.policy.exit_on_rise = true;
        v.low_point = Some(0);
        assert_eq!(run(&v, 2900, &[], false), Action::Exit);
        // A fall from the high is the owner's token rising: no exit.
        let mut v = vault(Some((-100, 100)), Some(3000));
        v.policy.exit_on_rise = true;
        v.low_point = Some(-10);
        assert_eq!(run(&v, 0, &[], false), Action::Noop("in range"));
        // A new low is recorded.
        assert_eq!(run(&v, -40, &[], false), Action::Poke);
    }

    #[test]
    fn cost_guard_weighs_a_day_of_fees_against_gas() {
        // A centred range of L = 1e6 at point 0 holds both tokens; leaving it below makes it all X.
        let old = PositionRef { lpt_id: "1".into(), pool_id: "x|y|100".into(), left_point: -400, right_point: 400 };
        let (x, y) = position_amounts(1e6, -400, 400, -1000);
        assert!(x > 0.0 && y == 0.0);
        // Re-entering just above the price with the same width keeps the same liquidity (same X, same width).
        let l = reentry_liquidity(&old, 1e6, -1000, -960, -160);
        let (x2, _) = position_amounts(l, -960, -160, -1000);
        assert!((x2 / x - 1.0).abs() < 1e-9, "{x2} vs {x}");
        // $100k a day at 0.2% is $200 of fees; a 1% share is $2, enough for 0.02 NEAR at $5 ($0.10).
        let fee = daily_fee_usd(100_000.0, 2000, 1.0, 99.0);
        assert!((fee - 2.0).abs() < 1e-9);
        assert!(rebalance_pays(fee, 5.0));
        // A quiet pool: $50 a day, a 0.1% share, earns $0.0001 a day — not worth $0.10 of gas.
        assert!(!rebalance_pays(daily_fee_usd(50.0, 2000, 1.0, 999.0), 5.0));
    }

    #[test]
    fn no_pokes_when_the_exit_rule_is_off() {
        let mut v = vault(Some((-100, 100)), Some(0));
        v.policy.exit_drop_points = 0;
        assert_eq!(run(&v, 40, &[], false), Action::Noop("in range"));
    }

    #[test]
    fn pokes_on_a_new_high() {
        assert_eq!(run(&vault(Some((-100, 100)), Some(0)), 40, &[], false), Action::Poke);
    }

    #[test]
    fn ignores_users_that_do_not_name_it() {
        let mut v = vault(Some((-100, 100)), None);
        v.agent = Some("someone-else".into());
        assert_eq!(run(&v, 0, &[], true), Action::Noop("this user does not name this agent"));
        v.agent = Some(ME.into());
        v.frozen = true;
        assert_eq!(run(&v, 0, &[], true), Action::Noop("the user froze the agent"));
    }

    #[test]
    fn calls_name_the_user() {
        let r = Action::Rebalance { pool_id: "x|y|100".into(), left_point: 1, right_point: 2 };
        let (m, a) = call_for(&r, "alice.near").unwrap();
        assert_eq!(m, "request_rebalance");
        let a: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(a["user"], "alice.near");
        assert_eq!(a["request"]["left_point"], 1);
        assert_eq!(call_for(&Action::Exit, "bob.near").unwrap(), ("exit", r#"{"user":"bob.near"}"#.to_string()));
        assert_eq!(call_for(&Action::Noop("x"), "bob.near"), None);
    }

    #[test]
    fn reads_the_shared_contracts_get_account() {
        // The JSON shape `get_account` returns (contracts/guardian), unused fields included.
        let raw = r#"{"policy":{"allowed_pools":["tkq.near|wrap.near|10000"],"min_width":1400,"max_width":5600,"max_distance":3000,"max_actions":12,"window_secs":86400,"max_slippage_bps":100,"exit_drop_points":2000,"exit_on_rise":false,"rebalance_buffer_points":100},"agent":"aa11","frozen":false,"rate":{"start_secs":0,"used":1},"position":{"lpt_id":"tkq.near|wrap.near|10000#3","pool_id":"tkq.near|wrap.near|10000","left_point":303200,"right_point":304600},"high_point":304800,"low_point":304800,"ledger":[["wrap.near","4228714614"]],"owed":[]}"#;
        let a: AccountView = serde_json::from_str(raw).unwrap();
        assert_eq!(a.position.as_ref().unwrap().right_point, 304600);
        assert_eq!(a.policy.exit_drop_points, 2000);
        // At 291077 the price fell 13,723 points from the high: the agent exits (the contract agreed in tests/shared.rs).
        let pool = PoolView { pool_id: "tkq.near|wrap.near|10000".into(), token_x: "tkq.near".into(), token_y: "wrap.near".into(), current_point: 291_077, point_delta: 200, fee: 10000, liquidity: "0".into() };
        assert_eq!(decide(&World { me: ME, account: &a, pool: &pool }), Action::Exit);
    }

    #[test]
    fn each_call_gets_its_chains_gas() {
        assert_eq!(gas_for("request_rebalance"), 300_000_000_000_000);
        assert_eq!(gas_for("exit"), 150_000_000_000_000);
        assert_eq!(gas_for("poke"), 50_000_000_000_000);
    }

    #[test]
    fn users_from_input_are_checked_and_capped() {
        let input = serde_json::json!({ "users": ["alice.near", "alice.near", "Bad.Near", "x", 7, "b.near", "c.near", "d.near", "e.near", "f.near"] });
        assert_eq!(users_from_input(&input), vec!["alice.near", "b.near", "c.near", "d.near", "e.near"]);
        assert!(users_from_input(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn transaction_encoding_matches_near_layout() {
        let tx = Transaction {
            signer_id: "a".into(),
            public_key: PublicKey::Ed25519([1; 32]),
            nonce: 7,
            receiver_id: "b".into(),
            block_hash: [2; 32],
            actions: vec![NearAction::FunctionCall(FunctionCall { method_name: "m".into(), args: b"{}".to_vec(), gas: 3, deposit: 0 })],
        };
        let b = borsh::to_vec(&tx).unwrap();
        // signer "a": u32 len 1 + 'a'; key tag 0 + 32 bytes; nonce u64; receiver "b"; block hash; 1 action tag 2.
        assert_eq!(&b[..5], &[1, 0, 0, 0, b'a']);
        assert_eq!(b[5], 0);
        assert_eq!(&b[38..46], &7u64.to_le_bytes());
        assert_eq!(&b[46..51], &[1, 0, 0, 0, b'b']);
        assert_eq!(&b[83..87], &1u32.to_le_bytes());
        assert_eq!(b[87], 2);
        assert_eq!(implicit_account(&[0xab; 32]).len(), 64);
    }
}
