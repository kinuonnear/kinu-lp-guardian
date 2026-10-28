//! KINU LP Guardian agent: the decision logic and the NEAR transaction encoding, host-free so they
//! are unit-tested natively. `main.rs` wires them to OutLayer's host functions inside the TEE.
//!
//! Trust model: a run's input is never trusted (any contract the caller transacts with can start a
//! run with input of its own choosing). The agent decides only from chain state it reads itself:
//! it acts on a vault only if that vault runs the published vault build (code hash) and names this
//! agent; every action is still checked again by the vault against the owner's policy.

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

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct VaultView {
    pub owner: String,
    pub agent: Option<String>,
    pub policy: Policy,
    pub frozen: bool,
    pub exit_zec: Option<String>,
    pub position: Option<PositionRef>,
    pub high_point: Option<i32>,
    #[serde(default)]
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
    pub vault: &'a VaultView,
    pub pool: &'a PoolView,
    /// The vault's own wallet balances of the pool tokens (funds after an exit).
    pub wallet: &'a [(String, u128)],
    /// The vault's Rhea DCL balances of token X and token Y (funded, no position yet).
    pub dcl: (u128, u128),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Noop(&'static str),
    Poke,
    Rebalance { pool_id: String, left_point: i32, right_point: i32 },
    Exit,
    BunkerExit { token: String, amount: u128 },
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
    let v = w.vault;
    if v.agent.as_deref() != Some(w.me) {
        return Action::Noop("this vault does not name this agent");
    }
    if v.frozen {
        return Action::Noop("vault is frozen");
    }
    let p = w.pool;
    let d = p.point_delta.max(1);
    let cur = p.current_point;
    let wd = width(&v.policy, d);
    match &v.position {
        Some(pos) => {
            let exit_on = v.policy.exit_drop_points > 0;
            // How far the price moved against the owner from the best point the vault saw (the
            // vault checks the same thing again before it lets the agent exit).
            let moved = if v.policy.exit_on_rise {
                i64::from(cur) - i64::from(v.low_point.unwrap_or(cur).min(cur))
            } else {
                i64::from(v.high_point.unwrap_or(cur).max(cur)) - i64::from(cur)
            };
            if exit_on && moved >= i64::from(v.policy.exit_drop_points) {
                return Action::Exit;
            }
            // Re-centre only once the price is outside by more than the owner's buffer, and never
            // within the one-sided margin: a re-entry sits up to MARGIN + spacing away from the
            // price on purpose and must not count as "out of range" on the next tick.
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
        None => {
            if v.exit_zec.is_some() {
                if let Some((token, amount)) = w.wallet.iter().find(|(_, a)| *a > 0) {
                    return Action::BunkerExit { token: token.clone(), amount: *amount };
                }
            }
            let (x, y) = w.dcl;
            if x > 0 && y > 0 {
                let left = floor_to(cur - wd / 2, d);
                return Action::Rebalance { pool_id: p.pool_id.clone(), left_point: left, right_point: left + wd };
            }
            if x > 0 {
                // Only token X: a range just above the price uses X alone.
                let left = floor_to(cur + ONE_SIDED_MARGIN, d) + d;
                return Action::Rebalance { pool_id: p.pool_id.clone(), left_point: left, right_point: left + wd };
            }
            if y > 0 {
                // Only token Y: just below the price.
                let right = floor_to(cur - ONE_SIDED_MARGIN, d);
                return Action::Rebalance { pool_id: p.pool_id.clone(), left_point: right - wd, right_point: right };
            }
            Action::Noop("nothing to manage")
        }
    }
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
        }
    }
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

/// NEAR one rebalance costs: about 0.02 NEAR of gas at today's price (the agent attaches 300 TGas).
pub const REBALANCE_GAS_NEAR: f64 = 0.02;

/// The cost guard: move the position only when a day of fees in the new range pays for the move.
/// The vault does not need this (its limits stand either way); it keeps the agent from spending
/// more on gas than a quiet pool would ever earn back.
pub fn rebalance_pays(daily_fee_usd: f64, near_usd: f64) -> bool {
    daily_fee_usd >= REBALANCE_GAS_NEAR * near_usd
}

/// The one call the agent makes per action, as (method, args json).
pub fn call_for(action: &Action, quote: Option<(&str, &str)>) -> Option<(&'static str, String)> {
    match action {
        Action::Noop(_) => None,
        Action::Poke => Some(("poke", "{}".into())),
        Action::Exit => Some(("exit", "{}".into())),
        Action::Rebalance { pool_id, left_point, right_point } => Some((
            "request_rebalance",
            serde_json::json!({ "request": { "pool_id": pool_id, "left_point": left_point, "right_point": right_point } }).to_string(),
        )),
        Action::BunkerExit { token, .. } => {
            let (signed, signature) = quote?;
            Some(("bunker_exit", serde_json::json!({ "token": token, "signed": signed, "signature": signature }).to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "aa11";
    fn policy() -> Policy {
        Policy { allowed_pools: vec!["x|y|100".into()], min_width: 150, max_width: 4000, max_distance: 400, exit_drop_points: 2877, exit_on_rise: false, rebalance_buffer_points: 0 }
    }
    fn vault(position: Option<(i32, i32)>, high: Option<i32>) -> VaultView {
        VaultView {
            owner: "owner".into(),
            agent: Some(ME.into()),
            policy: policy(),
            frozen: false,
            exit_zec: Some("u1...".into()),
            position: position.map(|(l, r)| PositionRef { lpt_id: "x|y|100#1".into(), pool_id: "x|y|100".into(), left_point: l, right_point: r }),
            high_point: high,
            low_point: None,
        }
    }
    fn pool(point: i32) -> PoolView {
        PoolView { pool_id: "x|y|100".into(), token_x: "x".into(), token_y: "y".into(), current_point: point, point_delta: 40, fee: 100, liquidity: "0".into() }
    }
    fn run(v: &VaultView, point: i32, wallet: &[(String, u128)], funded: bool) -> Action {
        let funded = if funded { (1, 1) } else { (0, 0) };
        decide(&World { me: ME, vault: v, pool: &pool(point), wallet, dcl: funded })
    }

    #[test]
    fn opens_a_centred_range_when_funded() {
        assert_eq!(run(&vault(None, None), 1000, &[], true), Action::Rebalance { pool_id: "x|y|100".into(), left_point: 920, right_point: 1080 });
    }

    #[test]
    fn opens_one_sided_when_funded_with_one_token() {
        let v = vault(None, None);
        let only_x = decide(&World { me: ME, vault: &v, pool: &pool(1000), wallet: &[], dcl: (5, 0) });
        assert_eq!(only_x, Action::Rebalance { pool_id: "x|y|100".into(), left_point: 1040, right_point: 1200 });
        let only_y = decide(&World { me: ME, vault: &v, pool: &pool(1000), wallet: &[], dcl: (0, 5) });
        assert_eq!(only_y, Action::Rebalance { pool_id: "x|y|100".into(), left_point: 800, right_point: 960 });
    }

    #[test]
    fn re_enters_one_sided_next_to_the_price() {
        // Below the range by 100 (> margin 20 + spacing 40): just above the price, width 160.
        assert_eq!(run(&vault(Some((1000, 1160)), Some(1100)), 900, &[], false), Action::Rebalance { pool_id: "x|y|100".into(), left_point: 960, right_point: 1120 });
        // Above the range by 80: just below the price.
        assert_eq!(run(&vault(Some((1000, 1160)), Some(1100)), 1240, &[], false), Action::Rebalance { pool_id: "x|y|100".into(), left_point: 1040, right_point: 1200 });
    }

    /// A vault with the exit rule off, so only the rebalance logic speaks.
    fn rebalance_only(position: (i32, i32)) -> VaultView {
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
    fn pokes_on_a_new_high_and_sends_exited_funds_to_the_bunker() {
        assert_eq!(run(&vault(Some((-100, 100)), Some(0)), 40, &[], false), Action::Poke);
        let wallet = vec![("x".to_string(), 0u128), ("y".to_string(), 5u128)];
        assert_eq!(run(&vault(None, None), 0, &wallet, false), Action::BunkerExit { token: "y".into(), amount: 5 });
    }

    #[test]
    fn ignores_vaults_that_do_not_name_it() {
        let mut v = vault(None, None);
        v.agent = Some("someone-else".into());
        assert_eq!(run(&v, 0, &[], true), Action::Noop("this vault does not name this agent"));
        v.agent = Some(ME.into());
        v.frozen = true;
        assert_eq!(run(&v, 0, &[], true), Action::Noop("vault is frozen"));
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
