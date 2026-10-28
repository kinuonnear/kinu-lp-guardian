//! KINU LP Guardian factory: one vault per call, every vault running the same published code.
//!
//! The vault code is published once as a NEP-591 global contract; the factory computes its sha256
//! itself, so the hash it hands out is the hash of the code it published. `create_vault` creates
//! `<name>.<factory>`, points it at that code by hash (no per-vault code storage) and initialises it
//! with the caller as owner. Anyone (the agent, a judge, the KINU page) can check a vault with
//! `view_account`: its `global_contract_hash` equals `get_config().vault_code_hash`.

use near_sdk::json_types::Base64VecU8;
use near_sdk::serde_json::{Value, json};
use near_sdk::store::LookupMap;
use near_sdk::{AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise, PromiseError, env, near};

/// What a new vault receives: its own state and storage deposits (tokens, 1Click deposit addresses).
pub const MIN_VAULT_DEPOSIT: NearToken = NearToken::from_millinear(300);
const GAS_INIT: Gas = Gas::from_tgas(30);
const GAS_CB: Gas = Gas::from_tgas(15);

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Vaults,
}

#[near(serializers = [json])]
pub struct Config {
    pub admin: AccountId,
    pub dcl: AccountId,
    /// base58 sha256 of the published vault code (None until published).
    pub vault_code_hash: Option<String>,
    pub vault_count: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Factory {
    admin: AccountId,
    dcl: AccountId,
    vault_code_hash: Option<[u8; 32]>,
    vaults: LookupMap<AccountId, Vec<AccountId>>,
    vault_count: u64,
}

fn b58(bytes: &[u8]) -> String {
    let mut digits: Vec<u8> = Vec::new();
    for &b in bytes {
        let mut carry = u32::from(b);
        for d in digits.iter_mut() {
            carry += u32::from(*d) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    const A: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    std::iter::repeat_n('1', zeros).chain(digits.iter().rev().map(|d| A[*d as usize] as char)).collect()
}

/// 2–32 characters of a-z, 0-9 and '-', not starting or ending with '-'.
fn valid_name(name: &str) -> bool {
    (2..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

#[near]
impl Factory {
    #[init]
    pub fn new(admin: AccountId, dcl: AccountId) -> Self {
        Self { admin, dcl, vault_code_hash: None, vaults: LookupMap::new(Key::Vaults), vault_count: 0 }
    }

    /// Publishes the vault code as a global contract, once. The caller pays the storage it costs.
    #[payable]
    pub fn publish_vault_code(&mut self, code: Base64VecU8) -> Promise {
        if env::predecessor_account_id() != self.admin {
            env::panic_str("Only the factory admin publishes the vault code");
        }
        if self.vault_code_hash.is_some() {
            env::panic_str("The vault code is already published; a new version needs a new factory");
        }
        let hash: [u8; 32] = env::sha256(&code.0).try_into().unwrap();
        self.vault_code_hash = Some(hash);
        env::log_str(&format!("EVENT_JSON:{}", json!({ "standard": "kinu_guardian_factory", "version": "0.1.0", "event": "vault_code_published", "data": [{ "hash": b58(&hash), "bytes": code.0.len() }] })));
        Promise::new(env::current_account_id()).deploy_global_contract(code.0)
    }

    /// Creates `<name>.<factory>` running the published vault code, owned by the caller.
    #[payable]
    pub fn create_vault(&mut self, name: String, policy: Value, agent: Option<AccountId>, exit_zec: Option<String>) -> Promise {
        let hash = self.vault_code_hash.unwrap_or_else(|| env::panic_str("The vault code is not published yet"));
        if !valid_name(&name) {
            env::panic_str("Vault name: 2-32 characters of a-z, 0-9 and '-'");
        }
        let deposit = env::attached_deposit();
        if deposit < MIN_VAULT_DEPOSIT {
            env::panic_str("Attach at least 0.3 NEAR: it becomes the vault's own balance for storage");
        }
        let owner = env::predecessor_account_id();
        let vault: AccountId = format!("{name}.{}", env::current_account_id()).parse().unwrap_or_else(|_| env::panic_str("Invalid vault account id"));
        let init = json!({ "owner": owner, "agent": agent, "dcl": self.dcl, "policy": policy, "oneclick_key": null, "exit_zec": exit_zec });
        Promise::new(vault.clone())
            .create_account()
            .transfer(deposit)
            .use_global_contract(hash)
            .function_call("new".to_string(), init.to_string().into_bytes(), NearToken::from_yoctonear(0), GAS_INIT)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_CB).on_vault_created(owner, vault, deposit))
    }

    #[private]
    pub fn on_vault_created(&mut self, owner: AccountId, vault: AccountId, deposit: NearToken, #[callback_result] created: Result<(), PromiseError>) -> Option<AccountId> {
        if created.is_err() {
            // The account was not created (name taken, bad policy, bad exit address...): the deposit
            // came back to the factory, so it goes back to the caller.
            env::log_str(&format!("EVENT_JSON:{}", json!({ "standard": "kinu_guardian_factory", "version": "0.1.0", "event": "vault_failed", "data": [{ "owner": owner, "vault": vault }] })));
            Promise::new(owner).transfer(deposit).detach();
            return None;
        }
        let mut list = self.vaults.get(&owner).cloned().unwrap_or_default();
        list.push(vault.clone());
        self.vaults.insert(owner.clone(), list);
        self.vault_count += 1;
        env::log_str(&format!("EVENT_JSON:{}", json!({ "standard": "kinu_guardian_factory", "version": "0.1.0", "event": "vault_created", "data": [{ "owner": owner, "vault": vault }] })));
        Some(vault)
    }

    pub fn get_config(&self) -> Config {
        Config { admin: self.admin.clone(), dcl: self.dcl.clone(), vault_code_hash: self.vault_code_hash.map(|h| b58(&h)), vault_count: self.vault_count }
    }

    pub fn get_vaults(&self, owner: AccountId) -> Vec<AccountId> {
        self.vaults.get(&owner).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_base58() {
        assert!(valid_name("alice-lp"));
        assert!(!valid_name("A"));
        assert!(!valid_name("-x"));
        assert!(!valid_name("evil.near"));
        assert_eq!(b58(&[0, 0, 1]), "112");
    }
}
