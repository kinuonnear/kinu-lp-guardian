//! Sandbox only, never deployed: the smallest contract that can stand as a Guardian user. It proves
//! that a contract, not a wallet, can join, deposit, open, freeze the agent and cash out.

use near_sdk::{AccountId, Gas, GasWeight, NearToken, PanicOnDefault, Promise, env, near, require};

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Probe {
    owner: AccountId,
}

#[near]
impl Probe {
    #[init]
    pub fn new(owner: AccountId) -> Self {
        Self { owner }
    }

    /// Forwards one call with all the gas left, as the wall will.
    #[payable]
    pub fn forward(&mut self, receiver: AccountId, method: String, args: String, deposit: NearToken) -> Promise {
        require!(env::predecessor_account_id() == self.owner, "Only the owner");
        Promise::new(receiver).function_call_weight(method, args.into_bytes(), deposit, Gas::from_gas(0), GasWeight(1))
    }
}
