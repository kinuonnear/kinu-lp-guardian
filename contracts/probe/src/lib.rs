//! Day-1 go/no-go probe: does Rhea DCL treat a *contract* caller exactly like a user?
//! The probe registers on the DCL, deposits tokens it holds, opens a position, and the test then
//! reads `list_liquidities({account_id: <probe>})`. Owner-only; testnet only.

use near_sdk::json_types::U128;
use near_sdk::{env, near, AccountId, Gas, NearToken, PanicOnDefault, Promise, PromiseOrValue};

const GAS_STORAGE: Gas = Gas::from_tgas(10);
const GAS_FT_CALL: Gas = Gas::from_tgas(180);
const GAS_ADD: Gas = Gas::from_tgas(150);
const GAS_REMOVE: Gas = Gas::from_tgas(100);
const GAS_WITHDRAW: Gas = Gas::from_tgas(60);
const GAS_TRANSFER: Gas = Gas::from_tgas(10);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Probe {
    owner: AccountId,
    dcl: AccountId,
}

fn call(to: &AccountId, method: &str, args: near_sdk::serde_json::Value, deposit: NearToken, gas: Gas) -> Promise {
    Promise::new(to.clone()).function_call(method.to_string(), args.to_string().into_bytes(), deposit, gas)
}

#[near]
impl Probe {
    #[init]
    pub fn new(owner: AccountId, dcl: AccountId) -> Self {
        Self { owner, dcl }
    }

    /// storage_deposit on the DCL for this contract (attach the NEAR to pay it, e.g. 0.5).
    #[payable]
    pub fn register_dcl(&mut self) -> Promise {
        self.only_owner();
        let me = env::current_account_id();
        call(&self.dcl, "storage_deposit", near_sdk::serde_json::json!({ "account_id": me, "registration_only": false }), env::attached_deposit(), GAS_STORAGE)
    }

    /// storage_deposit on a token for this contract (attach e.g. 0.00125 NEAR).
    #[payable]
    pub fn register_token(&mut self, token: AccountId) -> Promise {
        self.only_owner();
        let me = env::current_account_id();
        call(&token, "storage_deposit", near_sdk::serde_json::json!({ "account_id": me, "registration_only": true }), env::attached_deposit(), GAS_STORAGE)
    }

    /// Moves `amount` of a token this contract holds into its DCL balance (msg "Deposit").
    pub fn deposit_to_dcl(&mut self, token: AccountId, amount: U128) -> Promise {
        self.only_owner();
        call(&token, "ft_transfer_call", near_sdk::serde_json::json!({ "receiver_id": self.dcl, "amount": amount, "msg": "\"Deposit\"" }), ONE_YOCTO, GAS_FT_CALL)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_liquidity(&mut self, pool_id: String, left_point: i32, right_point: i32, amount_x: U128, amount_y: U128, min_amount_x: U128, min_amount_y: U128) -> Promise {
        self.only_owner();
        call(
            &self.dcl,
            "add_liquidity",
            near_sdk::serde_json::json!({ "pool_id": pool_id, "left_point": left_point, "right_point": right_point, "amount_x": amount_x, "amount_y": amount_y, "min_amount_x": min_amount_x, "min_amount_y": min_amount_y }),
            NearToken::from_yoctonear(0),
            GAS_ADD,
        )
    }

    pub fn remove_liquidity(&mut self, lpt_id: String) -> Promise {
        self.only_owner();
        call(&self.dcl, "remove_liquidity", near_sdk::serde_json::json!({ "lpt_id": lpt_id, "amount": "0", "min_amount_x": "0", "min_amount_y": "0" }), NearToken::from_yoctonear(0), GAS_REMOVE)
    }

    pub fn withdraw_asset(&mut self, token: AccountId) -> Promise {
        self.only_owner();
        call(&self.dcl, "withdraw_asset", near_sdk::serde_json::json!({ "token_id": token }), NearToken::from_yoctonear(0), GAS_WITHDRAW)
    }

    /// Sends a token held by this contract back to the owner.
    pub fn send_to_owner(&mut self, token: AccountId, amount: U128) -> Promise {
        self.only_owner();
        call(&token, "ft_transfer", near_sdk::serde_json::json!({ "receiver_id": self.owner, "amount": amount }), ONE_YOCTO, GAS_TRANSFER)
    }

    /// Accepts tokens sent by the owner (keeps all of them).
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let _ = (amount, msg);
        if sender_id != self.owner {
            env::panic_str("Probe only accepts tokens from its owner");
        }
        PromiseOrValue::Value(U128(0))
    }

    fn only_owner(&self) {
        if env::predecessor_account_id() != self.owner {
            env::panic_str("owner only");
        }
    }
}
