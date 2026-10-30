//! Testnet-only NEP-141 token. The whole supply is minted to the owner at init.
//!
//! `new_taxed` makes a tax token that behaves like the nearly.trade tokens on mainnet ($KINU,
//! kinu-3.nearlytrade.near, read 30 Oct 2026): a transfer INTO the pair (the Rhea DCL contract) loses
//! `sell_bps`, a transfer OUT of it loses `buy_bps`, the tax goes to the token's own account (memo
//! "tax"), the receiver of `ft_transfer_call` is told only the net amount, and `ft_resolve_transfer`
//! reports the net amount used plus the tax (mainnet tx AbbFkBwY…: 13,251,913.62 sent, 13,119,394.49
//! received by Rhea, 13,251,913.62 reported used; tx GoViKeFd…: Rhea withdrew 30,638.61, the wallet
//! got 30,332.22).
use near_contract_standards::fungible_token::events::FtTransfer;
use near_contract_standards::fungible_token::metadata::{FT_METADATA_SPEC, FungibleTokenMetadata, FungibleTokenMetadataProvider};
use near_contract_standards::fungible_token::receiver::ext_ft_receiver;
use near_contract_standards::fungible_token::{FungibleToken, FungibleTokenCore, FungibleTokenResolver};
use near_contract_standards::storage_management::{StorageBalance, StorageBalanceBounds, StorageManagement};
use near_sdk::json_types::U128;
use near_sdk::serde_json::{Value, json};
use near_sdk::{AccountId, BorshStorageKey, Gas, NearToken, PanicOnDefault, PromiseOrValue, PromiseResult, assert_one_yocto, env, near, require};

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Token,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Tax {
    pub pair: AccountId,
    pub buy_bps: u16,
    pub sell_bps: u16,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Token {
    token: FungibleToken,
    symbol: String,
    decimals: u8,
    tax: Option<Tax>,
}

const GAS_FOR_RESOLVE: Gas = Gas::from_tgas(10);
const GAS_FOR_CALL: Gas = Gas::from_tgas(35);

#[near]
impl Token {
    #[init]
    pub fn new(owner_id: AccountId, total_supply: U128, symbol: String, decimals: u8) -> Self {
        let mut token = FungibleToken::new(Key::Token);
        token.internal_register_account(&owner_id);
        token.internal_deposit(&owner_id, total_supply.0);
        Self { token, symbol, decimals, tax: None }
    }

    #[init]
    pub fn new_taxed(owner_id: AccountId, total_supply: U128, symbol: String, decimals: u8, tax: Tax) -> Self {
        let mut this = Self::new(owner_id, total_supply, symbol, decimals);
        this.token.internal_register_account(&env::current_account_id());
        this.tax = Some(tax);
        this
    }

    /// Same shape as the nearly.trade tokens' `get_tax`.
    pub fn get_tax(&self) -> Value {
        match &self.tax {
            Some(t) => json!({ "tax": { "buy_bps": t.buy_bps, "sell_bps": t.sell_bps, "pairs": [t.pair], "exempt": [] }, "pending": "0" }),
            None => json!({ "tax": null }),
        }
    }

    fn tax_on(&self, from: &AccountId, to: &AccountId, amount: u128) -> u128 {
        let Some(t) = &self.tax else { return 0 };
        let bps = if *to == t.pair {
            t.sell_bps
        } else if *from == t.pair {
            t.buy_bps
        } else {
            0
        };
        amount * bps as u128 / 10_000
    }

    /// Moves `amount` from `from`: `to` gets it less the tax, the token's own account gets the tax.
    /// Returns (net, tax).
    fn taxed_transfer(&mut self, from: &AccountId, to: &AccountId, amount: u128, memo: Option<String>) -> (u128, u128) {
        require!(from != to, "Sender and receiver should be different");
        require!(amount > 0, "The amount should be a positive number");
        let tax = self.tax_on(from, to, amount);
        let net = amount - tax;
        self.token.internal_withdraw(from, amount);
        self.token.internal_deposit(to, net);
        FtTransfer { old_owner_id: from, new_owner_id: to, amount: U128(net), memo: memo.as_deref() }.emit();
        if tax > 0 {
            let me = env::current_account_id();
            self.token.internal_deposit(&me, tax);
            FtTransfer { old_owner_id: from, new_owner_id: &me, amount: U128(tax), memo: Some("tax") }.emit();
        }
        (net, tax)
    }

    #[private]
    pub fn ft_resolve_taxed(&mut self, sender_id: AccountId, receiver_id: AccountId, amount: U128, tax: U128) -> U128 {
        let unused = match env::promise_result(0) {
            PromiseResult::Successful(v) => near_sdk::serde_json::from_slice::<U128>(&v).map(|u| u.0.min(amount.0)).unwrap_or(amount.0),
            _ => amount.0,
        };
        let refund = unused.min(self.token.ft_balance_of(receiver_id.clone()).0);
        if refund > 0 {
            self.token.internal_withdraw(&receiver_id, refund);
            self.token.internal_deposit(&sender_id, refund);
            FtTransfer { old_owner_id: &receiver_id, new_owner_id: &sender_id, amount: U128(refund), memo: Some("refund") }.emit();
        }
        U128(amount.0 - refund + tax.0)
    }
}

#[near]
impl FungibleTokenCore for Token {
    #[payable]
    fn ft_transfer(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>) {
        if self.tax.is_none() {
            return self.token.ft_transfer(receiver_id, amount, memo);
        }
        assert_one_yocto();
        self.taxed_transfer(&env::predecessor_account_id(), &receiver_id, amount.0, memo);
    }
    #[payable]
    fn ft_transfer_call(&mut self, receiver_id: AccountId, amount: U128, memo: Option<String>, msg: String) -> PromiseOrValue<U128> {
        if self.tax.is_none() {
            return self.token.ft_transfer_call(receiver_id, amount, memo, msg);
        }
        assert_one_yocto();
        let sender = env::predecessor_account_id();
        let (net, tax) = self.taxed_transfer(&sender, &receiver_id, amount.0, memo);
        let left = env::prepaid_gas().saturating_sub(env::used_gas()).saturating_sub(GAS_FOR_RESOLVE).saturating_sub(Gas::from_tgas(5));
        require!(left >= GAS_FOR_CALL, "More gas is required");
        ext_ft_receiver::ext(receiver_id.clone())
            .with_static_gas(left)
            .ft_on_transfer(sender.clone(), U128(net), msg)
            .then(Self::ext(env::current_account_id()).with_static_gas(GAS_FOR_RESOLVE).ft_resolve_taxed(sender, receiver_id, U128(net), U128(tax)))
            .into()
    }
    fn ft_total_supply(&self) -> U128 {
        self.token.ft_total_supply()
    }
    fn ft_balance_of(&self, account_id: AccountId) -> U128 {
        self.token.ft_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenResolver for Token {
    #[private]
    fn ft_resolve_transfer(&mut self, sender_id: AccountId, receiver_id: AccountId, amount: U128) -> U128 {
        let (used, _burned) = self.token.internal_ft_resolve_transfer(&sender_id, receiver_id, amount);
        used.into()
    }
}

#[near]
impl StorageManagement for Token {
    #[payable]
    fn storage_deposit(&mut self, account_id: Option<AccountId>, registration_only: Option<bool>) -> StorageBalance {
        self.token.storage_deposit(account_id, registration_only)
    }
    #[payable]
    fn storage_withdraw(&mut self, amount: Option<NearToken>) -> StorageBalance {
        self.token.storage_withdraw(amount)
    }
    #[payable]
    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        self.token.internal_storage_unregister(force).is_some()
    }
    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        self.token.storage_balance_bounds()
    }
    fn storage_balance_of(&self, account_id: AccountId) -> Option<StorageBalance> {
        self.token.storage_balance_of(account_id)
    }
}

#[near]
impl FungibleTokenMetadataProvider for Token {
    fn ft_metadata(&self) -> FungibleTokenMetadata {
        FungibleTokenMetadata { spec: FT_METADATA_SPEC.to_string(), name: format!("Guardian test {}", self.symbol), symbol: self.symbol.clone(), icon: None, reference: None, reference_hash: None, decimals: self.decimals }
    }
}
