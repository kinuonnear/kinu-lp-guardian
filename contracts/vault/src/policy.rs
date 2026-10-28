//! The owner's policy and the checks every agent action must pass. Pure functions, so the same
//! rules are unit-tested here and enforced on-chain by the vault.

use near_sdk::near;

/// What the agent may do with the owner's position. Set and changed only by the owner.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Policy {
    /// Rhea DCL pool ids the agent may use, e.g. "usdt.fakes.testnet|wrap.testnet|2000".
    pub allowed_pools: Vec<String>,
    /// Range width limits in DCL points (right_point - left_point).
    pub min_width: u32,
    pub max_width: u32,
    /// How far (in points) the new range may sit from the pool's current point.
    pub max_distance: u32,
    /// Most agent actions allowed inside one window.
    pub max_actions: u32,
    pub window_secs: u64,
    /// Slippage the contract uses when it computes minimum amounts itself (basis points).
    pub max_slippage_bps: u32,
}

/// A rebalance the agent asks for. Minimum amounts are not taken from the agent: the vault computes
/// them from the pool it reads itself.
#[near(serializers = [json])]
#[derive(Clone, Debug, PartialEq)]
pub struct RebalanceRequest {
    pub pool_id: String,
    pub left_point: i32,
    pub right_point: i32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Frozen,
    NotAgent,
    PoolNotAllowed,
    BadRange,
    TooNarrow,
    TooWide,
    TooFarFromPrice,
    RateLimited,
    BadPolicy,
}

impl Refusal {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Frozen => "Vault is frozen by its owner",
            Self::NotAgent => "Only the vault's agent can do this",
            Self::PoolNotAllowed => "Pool is not in the owner's allowed list",
            Self::BadRange => "Range is invalid: left_point must be below right_point",
            Self::TooNarrow => "Range is narrower than the owner's minimum width",
            Self::TooWide => "Range is wider than the owner's maximum width",
            Self::TooFarFromPrice => "Range is too far from the pool's current price",
            Self::RateLimited => "Too many agent actions in this window",
            Self::BadPolicy => "Policy is inconsistent",
        }
    }
}

impl Policy {
    pub fn validate(&self) -> Result<(), Refusal> {
        let ok = !self.allowed_pools.is_empty()
            && self.min_width > 0
            && self.min_width <= self.max_width
            && self.max_actions > 0
            && self.window_secs > 0
            && self.max_slippage_bps <= 1_000;
        if ok { Ok(()) } else { Err(Refusal::BadPolicy) }
    }
}

/// Rolling count of agent actions: (window start, actions so far).
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateWindow {
    pub start_secs: u64,
    pub used: u32,
}

impl RateWindow {
    /// Counts one action at `now`, or refuses when the window is full.
    pub fn take(&mut self, policy: &Policy, now_secs: u64) -> Result<(), Refusal> {
        if now_secs >= self.start_secs.saturating_add(policy.window_secs) {
            self.start_secs = now_secs;
            self.used = 0;
        }
        if self.used >= policy.max_actions {
            return Err(Refusal::RateLimited);
        }
        self.used += 1;
        Ok(())
    }
}

/// Checks a rebalance against the policy and the pool's current point (read by the vault, not the agent).
pub fn check_rebalance(policy: &Policy, req: &RebalanceRequest, current_point: i32) -> Result<(), Refusal> {
    if !policy.allowed_pools.iter().any(|p| p == &req.pool_id) {
        return Err(Refusal::PoolNotAllowed);
    }
    if req.left_point >= req.right_point {
        return Err(Refusal::BadRange);
    }
    let width = (i64::from(req.right_point) - i64::from(req.left_point)) as u64;
    if width < u64::from(policy.min_width) {
        return Err(Refusal::TooNarrow);
    }
    if width > u64::from(policy.max_width) {
        return Err(Refusal::TooWide);
    }
    // Distance from the price to the nearest edge (0 when the price is inside the range).
    let cur = i64::from(current_point);
    let distance = if cur < i64::from(req.left_point) {
        i64::from(req.left_point) - cur
    } else if cur > i64::from(req.right_point) {
        cur - i64::from(req.right_point)
    } else {
        0
    };
    if distance as u64 > u64::from(policy.max_distance) {
        return Err(Refusal::TooFarFromPrice);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            allowed_pools: vec!["usdt.fakes.testnet|wrap.testnet|2000".into()],
            min_width: 400,
            max_width: 8_000,
            max_distance: 400,
            max_actions: 3,
            window_secs: 86_400,
            max_slippage_bps: 100,
        }
    }
    fn req(pool: &str, l: i32, r: i32) -> RebalanceRequest {
        RebalanceRequest { pool_id: pool.into(), left_point: l, right_point: r }
    }
    const POOL: &str = "usdt.fakes.testnet|wrap.testnet|2000";

    #[test]
    fn accepts_a_range_around_the_price() {
        assert_eq!(check_rebalance(&policy(), &req(POOL, -1000, 1000), 0), Ok(()));
        // One-sided range right next to the price (the re-entry after an exit).
        assert_eq!(check_rebalance(&policy(), &req(POOL, 200, 1200), 0), Ok(()));
    }

    #[test]
    fn refuses_what_a_compromised_agent_would_try() {
        assert_eq!(check_rebalance(&policy(), &req("evil.near|wrap.testnet|2000", -1000, 1000), 0), Err(Refusal::PoolNotAllowed));
        assert_eq!(check_rebalance(&policy(), &req(POOL, -400_000, 400_000), 0), Err(Refusal::TooWide));
        assert_eq!(check_rebalance(&policy(), &req(POOL, 50_000, 51_000), 0), Err(Refusal::TooFarFromPrice));
        assert_eq!(check_rebalance(&policy(), &req(POOL, 10, 20), 0), Err(Refusal::TooNarrow));
        assert_eq!(check_rebalance(&policy(), &req(POOL, 1000, -1000), 0), Err(Refusal::BadRange));
    }

    #[test]
    fn rate_limit_resets_each_window() {
        let p = policy();
        let mut w = RateWindow::default();
        for _ in 0..3 {
            assert_eq!(w.take(&p, 100), Ok(()));
        }
        assert_eq!(w.take(&p, 200), Err(Refusal::RateLimited));
        assert_eq!(w.take(&p, 100 + 86_400), Ok(()));
    }

    #[test]
    fn policy_validation() {
        assert_eq!(policy().validate(), Ok(()));
        let mut p = policy();
        p.min_width = 9_000;
        assert_eq!(p.validate(), Err(Refusal::BadPolicy));
        let mut p = policy();
        p.allowed_pools.clear();
        assert_eq!(p.validate(), Err(Refusal::BadPolicy));
    }
}
