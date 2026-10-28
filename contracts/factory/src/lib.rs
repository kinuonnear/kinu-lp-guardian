//! KINU LP Guardian factory: one vault per call, every vault running the same published code.
//!
//! The vault code is published once; the factory computes its sha256 itself, so the hash it hands
//! out is the hash of the code it published. Two modes, fixed at init:
//! - `global`: the code is a NEP-591 global contract and each vault points at it by hash (cheap
//!   vaults, but publishing ~255 KB burns ~25 NEAR: global storage costs 10x);
//! - `inline`: the factory keeps the code and deploys a copy to each vault (the vault's ~2.6 NEAR of
//!   storage stays locked in the vault and comes back if it is deleted).
//! `create_vault` creates `<name>.<factory>` and initialises it with the caller as owner. Anyone can
//! check a vault with `view_account`: its `code_hash` (inline) or `global_contract_hash` (global)
//! equals `get_config().vault_code_hash`.

use near_sdk::json_types::Base64VecU8;
use near_sdk::serde_json::{Value, json};
use near_sdk::store::LookupMap;
use near_sdk::{AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, Promise, PromiseError, env, near};

/// What a new vault receives: its own state and storage deposits (tokens, 1Click deposit addresses).
pub const MIN_VAULT_DEPOSIT: NearToken = NearToken::from_millinear(300);
/// Inline mode: storage for the vault's own copy of the code, per byte (NEAR's 10^19 yocto/byte).
const STORAGE_PER_BYTE: u128 = 10_000_000_000_000_000_000;
const GAS_INIT: Gas = Gas::from_tgas(30);
const GAS_CB: Gas = Gas::from_tgas(15);

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Vaults,
    Code,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Global,
    Inline,
}

#[near(serializers = [json])]
pub struct Config {
    pub mode: Mode,
    /// Least deposit `create_vault` accepts (yocto).
    pub min_deposit: near_sdk::json_types::U128,
    pub admin: AccountId,
    pub dcl: AccountId,
    /// base58 sha256 of the published vault code (None until published).
    pub vault_code_hash: Option<String>,
    pub vault_count: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Factory {
    mode: Mode,
    /// Inline mode: the published code, deployed to each vault.
    code: near_sdk::store::LazyOption<Vec<u8>>,
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
    pub fn new(admin: AccountId, dcl: AccountId, mode: Option<Mode>) -> Self {
        Self { mode: mode.unwrap_or(Mode::Global), code: near_sdk::store::LazyOption::new(Key::Code, None), admin, dcl, vault_code_hash: None, vaults: LookupMap::new(Key::Vaults), vault_count: 0 }
    }

    fn min_deposit(&self) -> NearToken {
        match self.mode {
            Mode::Global => MIN_VAULT_DEPOSIT,
            Mode::Inline => {
                let code_len = self.code.get().as_ref().map_or(0, |c| c.len()) as u128;
                MIN_VAULT_DEPOSIT.saturating_add(NearToken::from_yoctonear(code_len * STORAGE_PER_BYTE))
            }
        }
    }

    /// Publishes the vault code, once: as a global contract (the caller pays the storage it burns) or,
    /// inline, into the factory's own storage (the factory's balance must cover it).
    #[payable]
    pub fn publish_vault_code(&mut self, code: Base64VecU8) -> Option<Promise> {
        if env::predecessor_account_id() != self.admin {
            env::panic_str("Only the factory admin publishes the vault code");
        }
        if self.vault_code_hash.is_some() {
            env::panic_str("The vault code is already published; a new version needs a new factory");
        }
        let hash: [u8; 32] = env::sha256(&code.0).try_into().unwrap();
        self.vault_code_hash = Some(hash);
        env::log_str(&format!("EVENT_JSON:{}", json!({ "standard": "kinu_guardian_factory", "version": "0.1.0", "event": "vault_code_published", "data": [{ "hash": b58(&hash), "bytes": code.0.len(), "mode": self.mode }] })));
        match self.mode {
            Mode::Global => Some(Promise::new(env::current_account_id()).deploy_global_contract(code.0)),
            Mode::Inline => {
                self.code.set(Some(code.0));
                None
            }
        }
    }

    /// Creates `<name>.<factory>` running the published vault code, owned by the caller.
    #[payable]
    pub fn create_vault(&mut self, name: String, policy: Value, agent: Option<AccountId>, exit_zec: Option<String>) -> Promise {
        let hash = self.vault_code_hash.unwrap_or_else(|| env::panic_str("The vault code is not published yet"));
        if !valid_name(&name) {
            env::panic_str("Vault name: 2-32 characters of a-z, 0-9 and '-'");
        }
        let deposit = env::attached_deposit();
        let min = self.min_deposit();
        if deposit < min {
            env::panic_str(&format!("Attach at least {min}: it becomes the vault's own balance for storage"));
        }
        let owner = env::predecessor_account_id();
        let vault: AccountId = format!("{name}.{}", env::current_account_id()).parse().unwrap_or_else(|_| env::panic_str("Invalid vault account id"));
        let init = json!({ "owner": owner, "agent": agent, "dcl": self.dcl, "policy": policy, "oneclick_key": null, "exit_zec": exit_zec });
        let created = Promise::new(vault.clone()).create_account().transfer(deposit);
        let installed = match self.mode {
            Mode::Global => created.use_global_contract(hash),
            Mode::Inline => created.deploy_contract(self.code.get().clone().unwrap_or_else(|| env::panic_str("The vault code is not published yet"))),
        };
        installed
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
        Config { mode: self.mode, min_deposit: near_sdk::json_types::U128(self.min_deposit().as_yoctonear()), admin: self.admin.clone(), dcl: self.dcl.clone(), vault_code_hash: self.vault_code_hash.map(|h| b58(&h)), vault_count: self.vault_count }
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
