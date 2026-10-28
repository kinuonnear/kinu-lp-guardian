//! KINU LP Guardian vault (first slice, 28 Oct 2026).
//!
//! One vault per owner. The owner sets a policy, an agent account and a shielded exit address; the
//! agent can only ask for actions, and the vault checks every argument against the policy and the
//! pool it reads itself before anything happens. Refusals fail on-chain with a clear reason.
//! Next slices: holding the Rhea DCL position, rebalance/compound execution, Bunker exit via NEAR Intents.

pub mod policy;
pub mod zcash;

use near_sdk::{env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseError};
use policy::{check_rebalance, Policy, RateWindow, RebalanceRequest, Refusal};

const GAS_VIEW: Gas = Gas::from_tgas(10);
const GAS_CALLBACK: Gas = Gas::from_tgas(15);

/// The fields of Rhea DCL `get_pool` the vault needs.
#[near(serializers = [json])]
pub struct PoolView {
    pub current_point: i32,
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
}

fn refuse(r: Refusal) -> ! {
    env::panic_str(r.message())
}

/// NEP-297 event log.
fn emit(event: &str, data: near_sdk::serde_json::Value) {
    let line = near_sdk::serde_json::json!({
        "standard": "kinu_guardian",
        "version": "0.1.0",
        "event": event,
        "data": [data],
    });
    env::log_str(&format!("EVENT_JSON:{line}"));
}

#[near]
impl Vault {
    #[init]
    pub fn new(owner: AccountId, agent: Option<AccountId>, dcl: AccountId, policy: Policy) -> Self {
        policy.validate().unwrap_or_else(|r| refuse(r));
        Self { owner, agent, dcl, policy, frozen: false, rate: RateWindow::default(), exit_zec: None }
    }

    // ---------- owner ----------

    pub fn set_policy(&mut self, policy: Policy) {
        self.assert_owner();
        policy.validate().unwrap_or_else(|r| refuse(r));
        self.policy = policy;
        emit("policy_set", near_sdk::serde_json::json!({ "owner": self.owner }));
    }

    pub fn set_agent(&mut self, agent: AccountId) {
        self.assert_owner();
        emit("agent_set", near_sdk::serde_json::json!({ "agent": agent }));
        self.agent = Some(agent);
    }

    /// Removes the agent at once; nothing it signs afterwards reaches the position.
    pub fn revoke_agent(&mut self) {
        self.assert_owner();
        self.agent = None;
        emit("agent_revoked", near_sdk::serde_json::json!({}));
    }

    pub fn freeze(&mut self) {
        self.assert_owner();
        self.frozen = true;
        emit("frozen", near_sdk::serde_json::json!({}));
    }

    pub fn unfreeze(&mut self) {
        self.assert_owner();
        self.frozen = false;
        emit("unfrozen", near_sdk::serde_json::json!({}));
    }

    /// The only place a Bunker exit can send funds: a Zcash unified address with an Orchard receiver.
    pub fn set_exit_zec(&mut self, address: String) {
        self.assert_owner();
        let receivers = zcash::validate_orchard_ua(&address).unwrap_or_else(|e| env::panic_str(e.message()));
        emit("exit_address_set", near_sdk::serde_json::json!({ "chain": "zcash", "receivers": receivers }));
        self.exit_zec = Some(address);
    }

    // ---------- agent ----------

    /// The agent asks for a new range. The vault reads the pool's current point itself and checks
    /// the request in the callback; every attempt counts towards the rate limit.
    pub fn request_rebalance(&mut self, request: RebalanceRequest) -> Promise {
        self.assert_agent_and_live();
        let now = env::block_timestamp() / 1_000_000_000;
        let policy = self.policy.clone();
        self.rate.take(&policy, now).unwrap_or_else(|r| refuse(r));
        if !self.policy.allowed_pools.iter().any(|p| p == &request.pool_id) {
            refuse(Refusal::PoolNotAllowed);
        }
        let args = near_sdk::serde_json::json!({ "pool_id": request.pool_id }).to_string().into_bytes();
        Promise::new(self.dcl.clone())
            .function_call("get_pool", args, NearToken::from_yoctonear(0), GAS_VIEW)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CALLBACK).on_pool_for_rebalance(request))
    }

    #[private]
    pub fn on_pool_for_rebalance(
        &mut self,
        request: RebalanceRequest,
        #[callback_result] pool: Result<Option<PoolView>, PromiseError>,
    ) -> bool {
        let Ok(Some(pool)) = pool else {
            env::panic_str("Could not read the pool from Rhea DCL");
        };
        if self.frozen {
            refuse(Refusal::Frozen);
        }
        check_rebalance(&self.policy, &request, pool.current_point).unwrap_or_else(|r| refuse(r));
        emit(
            "rebalance_approved",
            near_sdk::serde_json::json!({
                "pool_id": request.pool_id,
                "left_point": request.left_point,
                "right_point": request.right_point,
                "current_point": pool.current_point,
            }),
        );
        true
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
        }
    }

    // ---------- helpers ----------

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

    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }
    fn as_caller(who: &str) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(who)).current_account_id(acc("vault.testnet")).build());
    }
    fn vault() -> Vault {
        as_caller("owner.testnet");
        Vault::new(
            acc("owner.testnet"),
            Some(acc("agent.testnet")),
            acc("dclv2.ref-dev.testnet"),
            Policy {
                allowed_pools: vec!["usdt.fakes.testnet|wrap.testnet|2000".into()],
                min_width: 400,
                max_width: 8_000,
                max_distance: 400,
                max_actions: 3,
                window_secs: 86_400,
                max_slippage_bps: 100,
            },
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
        let mut v = vault();
        v.set_exit_zec("t1Q879cLgqaCd7zKRi79wQYuGBenmNX6cKn".into());
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
        let _ = v.request_rebalance(RebalanceRequest { pool_id: "evil.testnet|wrap.testnet|2000".into(), left_point: -1000, right_point: 1000 });
    }

    #[test]
    #[should_panic(expected = "frozen")]
    fn frozen_vault_refuses_the_agent() {
        let mut v = vault();
        v.freeze();
        as_caller("agent.testnet");
        let _ = v.request_rebalance(RebalanceRequest { pool_id: "usdt.fakes.testnet|wrap.testnet|2000".into(), left_point: -1000, right_point: 1000 });
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn revoked_agent_is_refused() {
        let mut v = vault();
        v.revoke_agent();
        as_caller("agent.testnet");
        let _ = v.request_rebalance(RebalanceRequest { pool_id: "usdt.fakes.testnet|wrap.testnet|2000".into(), left_point: -1000, right_point: 1000 });
    }

    #[test]
    fn callback_checks_range_against_the_pool() {
        let mut v = vault();
        let ok = v.on_pool_for_rebalance(
            RebalanceRequest { pool_id: "usdt.fakes.testnet|wrap.testnet|2000".into(), left_point: -1000, right_point: 1000 },
            Ok(Some(PoolView { current_point: 0 })),
        );
        assert!(ok);
    }

    #[test]
    #[should_panic(expected = "too far from the pool's current price")]
    fn callback_refuses_a_range_far_from_price() {
        let mut v = vault();
        v.on_pool_for_rebalance(
            RebalanceRequest { pool_id: "usdt.fakes.testnet|wrap.testnet|2000".into(), left_point: -1000, right_point: 1000 },
            Ok(Some(PoolView { current_point: 50_000 })),
        );
    }
}
