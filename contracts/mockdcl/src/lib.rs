//! Test-only stand-in for Rhea DCL (concentrated liquidity), used by the sandbox tests because the
//! testnet DCL rejects deposits since its code changed after 27 Sep 2026 (see DAY1.md) and Rhea's
//! CLMM source is not public. It keeps the same method names and argument shapes the vault uses:
//! storage_deposit, ft_on_transfer("Deposit"), get_pool, add_liquidity, remove_liquidity,
//! list_liquidities, list_user_assets, withdraw_asset. Liquidity math is NOT modelled: a position
//! simply holds the amounts it was given. `set_point` lets tests move the price.

use near_sdk::json_types::U128;
use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise, PromiseOrValue, env, near};
use std::collections::HashMap;

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Users,
    Assets,
    Pools,
    Positions,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Pool {
    pub pool_id: String,
    pub token_x: AccountId,
    pub token_y: AccountId,
    pub fee: u32,
    pub point_delta: i32,
    pub current_point: i32,
    pub state: String,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Position {
    pub lpt_id: String,
    pub owner_id: AccountId,
    pub pool_id: String,
    pub left_point: i32,
    pub right_point: i32,
    pub amount_x: U128,
    pub amount_y: U128,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct MockDcl {
    users: LookupMap<AccountId, bool>,
    /// (user, token) -> internal balance
    assets: IterableMap<String, u128>,
    pools: IterableMap<String, Pool>,
    positions: IterableMap<String, Position>,
    next_id: u64,
}

fn akey(user: &AccountId, token: &AccountId) -> String {
    format!("{user}#{token}")
}

#[near]
impl MockDcl {
    #[init]
    pub fn new() -> Self {
        Self { users: LookupMap::new(Key::Users), assets: IterableMap::new(Key::Assets), pools: IterableMap::new(Key::Pools), positions: IterableMap::new(Key::Positions), next_id: 0 }
    }

    #[payable]
    pub fn storage_deposit(&mut self, account_id: Option<AccountId>, registration_only: Option<bool>) {
        let _ = registration_only;
        if env::attached_deposit() < NearToken::from_millinear(100) {
            env::panic_str("E11: insufficient $NEAR storage deposit");
        }
        self.users.insert(account_id.unwrap_or_else(env::predecessor_account_id), true);
    }

    pub fn create_pool(&mut self, token_a: AccountId, token_b: AccountId, fee: u32, init_point: i32) -> String {
        let (x, y) = if token_a < token_b { (token_a, token_b) } else { (token_b, token_a) };
        let pool_id = format!("{x}|{y}|{fee}");
        self.pools.insert(pool_id.clone(), Pool { pool_id: pool_id.clone(), token_x: x, token_y: y, fee, point_delta: 40, current_point: init_point, state: "Running".into() });
        pool_id
    }

    /// Test hook: move a pool's price.
    pub fn set_point(&mut self, pool_id: String, point: i32) {
        self.pools.get_mut(&pool_id).expect("E403: pool not exist").current_point = point;
    }

    pub fn get_pool(&self, pool_id: String) -> Option<Pool> {
        self.pools.get(&pool_id).cloned()
    }

    /// msg "\"Deposit\"": credits the sender's internal balance of the calling token.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        if msg != "\"Deposit\"" {
            env::panic_str("E1: unsupported msg");
        }
        if !self.users.contains_key(&sender_id) {
            env::panic_str("E100: account not registered");
        }
        let token = env::predecessor_account_id();
        let k = akey(&sender_id, &token);
        let cur = self.assets.get(&k).copied().unwrap_or(0);
        self.assets.insert(k, cur + amount.0);
        PromiseOrValue::Value(U128(0))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_liquidity(&mut self, pool_id: String, left_point: i32, right_point: i32, amount_x: U128, amount_y: U128, min_amount_x: U128, min_amount_y: U128) -> String {
        let user = env::predecessor_account_id();
        let pool = self.pools.get(&pool_id).cloned().unwrap_or_else(|| env::panic_str("E403: pool not exist"));
        if left_point >= right_point || left_point % pool.point_delta != 0 || right_point % pool.point_delta != 0 {
            env::panic_str("E200: invalid params");
        }
        // One-sided ranges use one token only, as in a CLMM.
        let (use_x, use_y) = if left_point > pool.current_point {
            (amount_x.0, 0)
        } else if right_point <= pool.current_point {
            (0, amount_y.0)
        } else {
            (amount_x.0, amount_y.0)
        };
        if use_x < min_amount_x.0 || use_y < min_amount_y.0 {
            env::panic_str("E204: slippage error");
        }
        self.take(&user, &pool.token_x, use_x);
        self.take(&user, &pool.token_y, use_y);
        self.next_id += 1;
        let lpt_id = format!("{pool_id}#{}", self.next_id);
        self.positions.insert(lpt_id.clone(), Position { lpt_id: lpt_id.clone(), owner_id: user, pool_id, left_point, right_point, amount_x: U128(use_x), amount_y: U128(use_y) });
        lpt_id
    }

    pub fn remove_liquidity(&mut self, lpt_id: String, amount: U128, min_amount_x: U128, min_amount_y: U128) -> (U128, U128) {
        let _ = (amount, min_amount_x, min_amount_y);
        let user = env::predecessor_account_id();
        let p = self.positions.get(&lpt_id).cloned().unwrap_or_else(|| env::panic_str("E207: lpt not exist"));
        if p.owner_id != user {
            env::panic_str("E215: not lpt owner");
        }
        let pool = self.pools.get(&p.pool_id).cloned().unwrap();
        self.give(&user, &pool.token_x, p.amount_x.0);
        self.give(&user, &pool.token_y, p.amount_y.0);
        self.positions.remove(&lpt_id);
        (p.amount_x, p.amount_y)
    }

    pub fn list_liquidities(&self, account_id: AccountId) -> Vec<Position> {
        self.positions.values().filter(|p| p.owner_id == account_id).cloned().collect()
    }

    /// Same shape as Rhea DCL: every token with a non-zero internal balance.
    pub fn list_user_assets(&self, account_id: AccountId) -> HashMap<AccountId, U128> {
        let prefix = format!("{account_id}#");
        self.assets
            .iter()
            .filter(|(k, v)| k.starts_with(&prefix) && **v > 0)
            .map(|(k, v)| (k[prefix.len()..].parse().unwrap(), U128(*v)))
            .collect()
    }

    /// Sends the whole internal balance of a token back to the caller.
    pub fn withdraw_asset(&mut self, token_id: AccountId, amount: Option<U128>) -> Promise {
        let user = env::predecessor_account_id();
        let k = akey(&user, &token_id);
        let have = self.assets.get(&k).copied().unwrap_or(0);
        let send = amount.map(|a| a.0).unwrap_or(have);
        if send == 0 || send > have {
            env::panic_str("E101: insufficient balance");
        }
        self.assets.insert(k, have - send);
        Promise::new(token_id).function_call(
            "ft_transfer".to_string(),
            near_sdk::serde_json::json!({ "receiver_id": user, "amount": U128(send) }).to_string().into_bytes(),
            NearToken::from_yoctonear(1),
            Gas::from_tgas(10),
        )
    }

    fn take(&mut self, user: &AccountId, token: &AccountId, amount: u128) {
        if amount == 0 {
            return;
        }
        let k = akey(user, token);
        let have = self.assets.get(&k).copied().unwrap_or(0);
        if have < amount {
            env::panic_str("E101: insufficient balance");
        }
        self.assets.insert(k, have - amount);
    }

    fn give(&mut self, user: &AccountId, token: &AccountId, amount: u128) {
        let k = akey(user, token);
        let have = self.assets.get(&k).copied().unwrap_or(0);
        self.assets.insert(k, have + amount);
    }
}
