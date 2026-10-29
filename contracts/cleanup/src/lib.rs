//! Retiring a Guardian account: deployed over the factory (or a test vault) by its own full-access
//! key, it removes the given storage keys so the account is small enough to delete (NEAR refuses to
//! delete accounts holding more than ~10 kB). Only the account itself may call it. Its methods never
//! read the old contract's state (they take no `self`), so any previous state can be removed.

use near_sdk::json_types::Base64VecU8;
use near_sdk::{env, near};

#[near(contract_state)]
#[derive(Default)]
pub struct Cleanup {}

#[near]
impl Cleanup {
    /// Removes each key; returns how many existed. Callable only by the account itself.
    pub fn clean(keys: Vec<Base64VecU8>) -> u32 {
        if env::predecessor_account_id() != env::current_account_id() {
            env::panic_str("Only the account itself can clean its storage");
        }
        keys.iter().filter(|k| env::storage_remove(&k.0)).count() as u32
    }

    /// Bytes the account still uses (code, keys and state).
    pub fn storage_usage() -> u64 {
        env::storage_usage()
    }
}
