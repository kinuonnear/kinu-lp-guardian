//! KINU LP Guardian vault.
//!
//! One vault per owner. The owner funds it, sets a policy, an agent account and a shielded exit
//! address. The vault itself holds the Rhea DCL position. The agent can only *ask*: every request is
//! checked against the owner's policy and against the pool the vault reads itself, and refusals fail
//! on-chain with a clear reason. Exit by the agent is allowed only when the owner's exit rule is met;
//! exited funds stay in the vault until the owner moves them.

pub mod policy;
pub mod zcash;

use near_sdk::json_types::U128;
use near_sdk::serde_json::{Value, json};
use near_sdk::{AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError, PromiseOrValue, env, near};
use policy::{Policy, RateWindow, RebalanceRequest, Refusal, check_rebalance};
use std::collections::HashMap;

const GAS_VIEW: Gas = Gas::from_tgas(10);
const GAS_STORAGE: Gas = Gas::from_tgas(10);
const GAS_FT_CALL: Gas = Gas::from_tgas(60);
const GAS_ADD: Gas = Gas::from_tgas(60);
const GAS_REMOVE: Gas = Gas::from_tgas(40);
const GAS_WITHDRAW: Gas = Gas::from_tgas(30);
const GAS_TRANSFER: Gas = Gas::from_tgas(10);
const GAS_CB: Gas = Gas::from_tgas(20);
const GAS_CB_CHAIN: Gas = Gas::from_tgas(100);
const ZERO: NearToken = NearToken::from_yoctonear(0);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);

/// The fields of Rhea DCL `get_pool` the vault needs.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct PoolView {
    pub token_x: AccountId,
    pub token_y: AccountId,
    pub current_point: i32,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct PositionRef {
    pub lpt_id: String,
    pub pool_id: String,
    pub left_point: i32,
    pub right_point: i32,
}

#[near(serializers = [json])]
pub struct VaultView {
    pub owner: AccountId,
    pub agent: Option<AccountId>,
    pub dcl: AccountId,
    pub policy: Policy,
    pub frozen: bool,
    pub rate: RateWindow,
    pub exit_zec: Option<String>,
    pub position: Option<PositionRef>,
    pub high_point: Option<i32>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Vault {
    owner: AccountId,
    agent: Option<AccountId>,
    dcl: AccountId,
    policy: Policy,
    frozen: bool,
    rate: RateWindow,
    exit_zec: Option<String>,
    position: Option<PositionRef>,
    /// Highest pool point the vault has read itself while holding the position (exit rule).
    high_point: Option<i32>,
}

fn refuse(r: Refusal) -> ! {
    env::panic_str(r.message())
}

/// NEP-297 event log.
fn emit(event: &str, data: Value) {
    let line = json!({ "standard": "kinu_guardian", "version": "0.2.0", "event": event, "data": [data] });
    env::log_str(&format!("EVENT_JSON:{line}"));
}

fn call(to: &AccountId, method: &str, args: Value, deposit: NearToken, gas: Gas) -> Promise {
    Promise::new(to.clone()).function_call(method.to_string(), args.to_string().into_bytes(), deposit, gas)
}

/// Minimum amounts the vault demands from add_liquidity, computed by the vault (never by the agent).
/// One-sided ranges must use the deposited side fully, minus the owner's slippage.
/// In-range: no minimum yet (the price check above bounds it); exact CLMM math is the next slice.
pub fn min_amounts(left: i32, right: i32, current: i32, x: u128, y: u128, slippage_bps: u32) -> (u128, u128) {
    let keep = |v: u128| v - v * u128::from(slippage_bps) / 10_000;
    if left > current {
        (keep(x), 0)
    } else if right <= current {
        (0, keep(y))
    } else {
        (0, 0)
    }
}

#[near]
impl Vault {
    #[init]
    pub fn new(owner: AccountId, agent: Option<AccountId>, dcl: AccountId, policy: Policy) -> Self {
        policy.validate().unwrap_or_else(|r| refuse(r));
        Self { owner, agent, dcl, policy, frozen: false, rate: RateWindow::default(), exit_zec: None, position: None, high_point: None }
    }

    // ---------- owner: setup ----------

    /// Registers the vault on Rhea DCL (attach the storage NEAR, e.g. 0.5).
    #[payable]
    pub fn register_dcl(&mut self) -> Promise {
        self.assert_owner();
        call(&self.dcl, "storage_deposit", json!({ "account_id": env::current_account_id(), "registration_only": false }), env::attached_deposit(), GAS_STORAGE)
    }

    /// Registers the vault on a token (attach e.g. 0.00125 NEAR).
    #[payable]
    pub fn register_token(&mut self, token: AccountId) -> Promise {
        self.assert_owner();
        call(&token, "storage_deposit", json!({ "account_id": env::current_account_id(), "registration_only": true }), env::attached_deposit(), GAS_STORAGE)
    }

    pub fn set_policy(&mut self, policy: Policy) {
        self.assert_owner();
        policy.validate().unwrap_or_else(|r| refuse(r));
        self.policy = policy;
        emit("policy_set", json!({}));
    }

    pub fn set_agent(&mut self, agent: AccountId) {
        self.assert_owner();
        emit("agent_set", json!({ "agent": agent }));
        self.agent = Some(agent);
    }

    /// Removes the agent at once; nothing it signs afterwards reaches the position.
    pub fn revoke_agent(&mut self) {
        self.assert_owner();
        self.agent = None;
        emit("agent_revoked", json!({}));
    }

    pub fn freeze(&mut self) {
        self.assert_owner();
        self.frozen = true;
        emit("frozen", json!({}));
    }

    pub fn unfreeze(&mut self) {
        self.assert_owner();
        self.frozen = false;
        emit("unfrozen", json!({}));
    }

    /// The only place a Bunker exit can send funds: a Zcash unified address with an Orchard receiver.
    pub fn set_exit_zec(&mut self, address: String) {
        self.assert_owner();
        let receivers = zcash::validate_orchard_ua(&address).unwrap_or_else(|e| env::panic_str(e.message()));
        emit("exit_address_set", json!({ "chain": "zcash", "receivers": receivers }));
        self.exit_zec = Some(address);
    }

    /// Owner funding: tokens sent by the owner with ft_transfer_call go straight into the vault's DCL balance.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let _ = msg;
        if sender_id != self.owner {
            env::panic_str("Only the vault's owner can fund it");
        }
        let token = env::predecessor_account_id();
        emit("funded", json!({ "token": token, "amount": amount }));
        call(&token, "ft_transfer_call", json!({ "receiver_id": self.dcl, "amount": amount, "msg": "\"Deposit\"" }), ONE_YOCTO, GAS_FT_CALL).detach();
        PromiseOrValue::Value(U128(0))
    }

    /// Moves a token the vault holds in its own wallet (e.g. after an exit) to the owner.
    pub fn sweep_to_owner(&mut self, token: AccountId, amount: U128) -> Promise {
        self.assert_owner();
        emit("swept", json!({ "token": token, "amount": amount }));
        call(&token, "ft_transfer", json!({ "receiver_id": self.owner, "amount": amount }), ONE_YOCTO, GAS_TRANSFER)
    }

    // ---------- agent: rebalance ----------

    /// The agent asks for a new range. The vault reads the pool itself, checks the request, then
    /// closes the old position and opens the new one with everything it holds in that pool.
    pub fn request_rebalance(&mut self, request: RebalanceRequest) -> Promise {
        self.assert_agent_and_live();
        let now = env::block_timestamp() / 1_000_000_000;
        let policy = self.policy.clone();
        self.rate.take(&policy, now).unwrap_or_else(|r| refuse(r));
        if !self.policy.allowed_pools.iter().any(|p| p == &request.pool_id) {
            refuse(Refusal::PoolNotAllowed);
        }
        call(&self.dcl, "get_pool", json!({ "pool_id": request.pool_id }), ZERO, GAS_VIEW)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB_CHAIN.saturating_mul(2)).on_pool_for_rebalance(request))
    }

    #[private]
    pub fn on_pool_for_rebalance(&mut self, request: RebalanceRequest, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> Promise {
        let Ok(Some(pool)) = pool else { env::panic_str("Could not read the pool from Rhea DCL") };
        if self.frozen {
            refuse(Refusal::Frozen);
        }
        check_rebalance(&self.policy, &request, pool.current_point).unwrap_or_else(|r| refuse(r));
        self.note_point(pool.current_point);
        emit("rebalance_approved", json!({ "pool_id": request.pool_id, "left_point": request.left_point, "right_point": request.right_point, "current_point": pool.current_point }));
        let next = Self::ext(env::current_account_id()).with_static_gas(GAS_CB_CHAIN).on_ready_to_add(request.clone(), pool.clone());
        match self.position.clone() {
            Some(old) => call(&self.dcl, "remove_liquidity", json!({ "lpt_id": old.lpt_id, "amount": "0", "min_amount_x": "0", "min_amount_y": "0" }), ZERO, GAS_REMOVE)
                .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB).on_removed())
                .then(call(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }), ZERO, GAS_VIEW))
                .then(next),
            None => call(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }), ZERO, GAS_VIEW).then(next),
        }
    }

    /// The old position is gone only if remove_liquidity succeeded; otherwise stop here.
    #[private]
    pub fn on_removed(&mut self, #[callback_result] removed: Result<Value, PromiseError>) {
        if removed.is_err() {
            env::panic_str("Rhea DCL refused to close the old position; nothing else was done");
        }
        if let Some(old) = self.position.take() {
            emit("position_closed", json!({ "lpt_id": old.lpt_id }));
        }
    }

    #[private]
    pub fn on_ready_to_add(&mut self, request: RebalanceRequest, pool: PoolView, #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> Promise {
        let Ok(assets) = assets else { env::panic_str("Could not read the vault's balances on Rhea DCL") };
        if self.position.is_some() {
            env::panic_str("Old position is still open; not opening another");
        }
        let x = assets.get(&pool.token_x).map(|a| a.0).unwrap_or(0);
        let y = assets.get(&pool.token_y).map(|a| a.0).unwrap_or(0);
        let (min_x, min_y) = min_amounts(request.left_point, request.right_point, pool.current_point, x, y, self.policy.max_slippage_bps);
        call(
            &self.dcl,
            "add_liquidity",
            json!({ "pool_id": request.pool_id, "left_point": request.left_point, "right_point": request.right_point, "amount_x": U128(x), "amount_y": U128(y), "min_amount_x": U128(min_x), "min_amount_y": U128(min_y) }),
            ZERO,
            GAS_ADD,
        )
        .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB).on_added(request))
    }

    #[private]
    pub fn on_added(&mut self, request: RebalanceRequest, #[callback_result] lpt_id: Result<String, PromiseError>) {
        let Ok(lpt_id) = lpt_id else { env::panic_str("Rhea DCL refused the new position; funds stay in the vault's DCL balance") };
        emit("position_opened", json!({ "lpt_id": lpt_id, "pool_id": request.pool_id, "left_point": request.left_point, "right_point": request.right_point }));
        self.position = Some(PositionRef { lpt_id, pool_id: request.pool_id, left_point: request.left_point, right_point: request.right_point });
    }

    // ---------- price watch and exit ----------

    /// Anyone may ask the vault to read its pool and remember the highest point seen (for the exit rule).
    pub fn poke(&mut self) -> Promise {
        let pos = self.position.clone().unwrap_or_else(|| env::panic_str("No open position"));
        call(&self.dcl, "get_pool", json!({ "pool_id": pos.pool_id }), ZERO, GAS_VIEW).then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB).on_poke())
    }

    #[private]
    pub fn on_poke(&mut self, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> Option<i32> {
        let Ok(Some(pool)) = pool else { env::panic_str("Could not read the pool from Rhea DCL") };
        self.note_point(pool.current_point);
        self.high_point
    }

    /// Closes the position and pulls both tokens back into the vault's own wallet.
    /// The owner may always do this; the agent only when the owner's exit rule is met
    /// (the pool fell `exit_drop_points` below the highest point the vault saw itself).
    pub fn exit(&mut self) -> Promise {
        let by_owner = env::predecessor_account_id() == self.owner;
        if !by_owner {
            self.assert_agent_and_live();
            if self.policy.exit_drop_points == 0 {
                env::panic_str("The owner has not allowed the agent to exit");
            }
        }
        let pos = self.position.clone().unwrap_or_else(|| env::panic_str("No open position"));
        call(&self.dcl, "get_pool", json!({ "pool_id": pos.pool_id }), ZERO, GAS_VIEW)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB_CHAIN.saturating_mul(2)).on_pool_for_exit(by_owner))
    }

    #[private]
    pub fn on_pool_for_exit(&mut self, by_owner: bool, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> Promise {
        let Ok(Some(pool)) = pool else { env::panic_str("Could not read the pool from Rhea DCL") };
        let high = self.high_point.unwrap_or(pool.current_point).max(pool.current_point);
        let drop = i64::from(high) - i64::from(pool.current_point);
        if !by_owner && drop < i64::from(self.policy.exit_drop_points) {
            env::panic_str("Exit rule not met: the price has not fallen far enough from its high");
        }
        let pos = self.position.clone().unwrap_or_else(|| env::panic_str("No open position"));
        emit("exit_started", json!({ "by": if by_owner { "owner" } else { "agent" }, "lpt_id": pos.lpt_id, "high_point": high, "current_point": pool.current_point }));
        call(&self.dcl, "remove_liquidity", json!({ "lpt_id": pos.lpt_id, "amount": "0", "min_amount_x": "0", "min_amount_y": "0" }), ZERO, GAS_REMOVE)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB).on_removed())
            .then(call(&self.dcl, "withdraw_asset", json!({ "token_id": pool.token_x }), ZERO, GAS_WITHDRAW))
            .then(call(&self.dcl, "withdraw_asset", json!({ "token_id": pool.token_y }), ZERO, GAS_WITHDRAW))
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB).on_exited())
    }

    #[private]
    pub fn on_exited(&mut self) {
        self.high_point = None;
        emit("exited", json!({ "funds": "in the vault wallet; owner sweeps them or sends them to the Zcash exit address" }));
    }

    // ---------- views ----------

    pub fn get_vault(&self) -> VaultView {
        VaultView {
            owner: self.owner.clone(),
            agent: self.agent.clone(),
            dcl: self.dcl.clone(),
            policy: self.policy.clone(),
            frozen: self.frozen,
            rate: self.rate.clone(),
            exit_zec: self.exit_zec.clone(),
            position: self.position.clone(),
            high_point: self.high_point,
        }
    }

    // ---------- helpers ----------

    fn note_point(&mut self, point: i32) {
        self.high_point = Some(self.high_point.map_or(point, |h| h.max(point)));
    }

    fn assert_owner(&self) {
        if env::predecessor_account_id() != self.owner {
            env::panic_str("Only the vault's owner can do this");
        }
    }

    fn assert_agent_and_live(&self) {
        if self.frozen {
            refuse(Refusal::Frozen);
        }
        if self.agent.as_ref() != Some(&env::predecessor_account_id()) {
            refuse(Refusal::NotAgent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    const UA: &str = "u1fasx684eaqjej3rvn7kjnjsymdm8mdv66nuym9uh9xznmf3skz6zz0qt3q3cflw8cgy8cfndran77a4e797q7t32u779q9halyx7djty";
    const POOL: &str = "usdt.fakes.testnet|wrap.testnet|2000";

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }
    fn as_caller(who: &str) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(who)).current_account_id(acc("vault.testnet")).build());
    }
    fn pool_view(point: i32) -> PoolView {
        PoolView { token_x: acc("usdt.fakes.testnet"), token_y: acc("wrap.testnet"), current_point: point }
    }
    fn req(pool: &str, l: i32, r: i32) -> RebalanceRequest {
        RebalanceRequest { pool_id: pool.into(), left_point: l, right_point: r }
    }
    fn vault() -> Vault {
        as_caller("owner.testnet");
        Vault::new(
            acc("owner.testnet"),
            Some(acc("agent.testnet")),
            acc("dclv2.ref-dev.testnet"),
            Policy { allowed_pools: vec![POOL.into()], min_width: 400, max_width: 8_000, max_distance: 400, max_actions: 3, window_secs: 86_400, max_slippage_bps: 100, exit_drop_points: 2_877 },
        )
    }

    #[test]
    fn owner_sets_a_shielded_exit() {
        let mut v = vault();
        v.set_exit_zec(UA.into());
        assert_eq!(v.get_vault().exit_zec.as_deref(), Some(UA));
    }

    #[test]
    #[should_panic(expected = "not private")]
    fn transparent_exit_is_refused() {
        vault().set_exit_zec("t1Q879cLgqaCd7zKRi79wQYuGBenmNX6cKn".into());
    }

    #[test]
    #[should_panic(expected = "Only the vault's owner")]
    fn agent_cannot_change_the_exit_address() {
        let mut v = vault();
        as_caller("agent.testnet");
        v.set_exit_zec(UA.into());
    }

    #[test]
    #[should_panic(expected = "Pool is not in the owner's allowed list")]
    fn agent_cannot_use_another_pool() {
        let mut v = vault();
        as_caller("agent.testnet");
        let _ = v.request_rebalance(req("evil.testnet|wrap.testnet|2000", -1000, 1000));
    }

    #[test]
    #[should_panic(expected = "frozen")]
    fn frozen_vault_refuses_the_agent() {
        let mut v = vault();
        v.freeze();
        as_caller("agent.testnet");
        let _ = v.request_rebalance(req(POOL, -1000, 1000));
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn revoked_agent_is_refused() {
        let mut v = vault();
        v.revoke_agent();
        as_caller("agent.testnet");
        let _ = v.request_rebalance(req(POOL, -1000, 1000));
    }

    #[test]
    #[should_panic(expected = "too far from the pool's current price")]
    fn callback_refuses_a_range_far_from_price() {
        let mut v = vault();
        let _ = v.on_pool_for_rebalance(req(POOL, -1000, 1000), Ok(Some(pool_view(50_000))));
    }

    #[test]
    #[should_panic(expected = "Only the vault's owner can fund it")]
    fn only_the_owner_funds_the_vault() {
        let mut v = vault();
        as_caller("usdt.fakes.testnet");
        let _ = v.ft_on_transfer(acc("stranger.testnet"), U128(5), String::new());
    }

    #[test]
    #[should_panic(expected = "Exit rule not met")]
    fn agent_exit_needs_the_rule() {
        let mut v = vault();
        v.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -400, right_point: 400 });
        v.high_point = Some(1_000);
        let _ = v.on_pool_for_exit(false, Ok(Some(pool_view(0))));
    }

    #[test]
    fn agent_exit_allowed_after_the_drop() {
        let mut v = vault();
        v.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -400, right_point: 400 });
        v.high_point = Some(3_000);
        let _ = v.on_pool_for_exit(false, Ok(Some(pool_view(0))));
    }

    #[test]
    fn min_amounts_follow_the_range_side() {
        assert_eq!(min_amounts(200, 1200, 0, 1_000, 500, 100), (990, 0));
        assert_eq!(min_amounts(-1200, -200, 0, 1_000, 500, 100), (0, 495));
        assert_eq!(min_amounts(-400, 400, 0, 1_000, 500, 100), (0, 0));
    }
}
