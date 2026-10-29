//! KINU Guardian, shared: one contract holds every user's Rhea DCL position, with a ledger and a
//! policy per user. A user joins once (a small refundable storage deposit), sends one token, and
//! the contract opens the position; an agent (a key born in a TEE) may re-centre it within the
//! user's policy; the user can close and withdraw at any time without the agent.
//!
//! Lessons built in (29 Oct 2026):
//!   · every call into Rhea forwards the transaction's unused gas (`function_call_weight`), so a
//!     Rhea upgrade that needs more gas does not lock anyone's funds;
//!   · Rhea keeps removed tokens and add leftovers on this contract's Rhea balance
//!     (`skip_refund_transfer`, `skip_unwrap_near`): no surprise transfers;
//!   · the ledger is exact: what an add took is this contract's Rhea balance before minus after,
//!     read around the add while every balance-changing step runs one at a time (a global lock);
//!   · callbacks never panic (a panic would undo the unlock or the credit written before it);
//!   · the user's way out (`withdraw`) needs only Rhea's `withdraw_asset`; no agent, no policy;
//!     a payout the user's wallet refuses stays owed to them in this contract (`claim`);
//!   · `cash_out` is the one-approval way out: close, pay every token, optionally leave;
//!   · every callback that follows a step on Rhea gets a static gas floor paying for the whole rest
//!     of its chain: a transaction has enough gas from the start or fails before Rhea is touched
//!     (v1.1; v1.0 once ran out after an add on mainnet and left the position off the user's record);
//!   · nobody else touches a user's ledger. Anyone may pay the contract's Rhea / token storage.
//!     During beta the contract account keeps its own key (KINU): it can upgrade the code, and
//!     `recover_position` / `recover_credit` let it put a stranded position or leftover onto a
//!     user's record (public events).

mod policy;

use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::serde_json::{Value, json};
use near_sdk::store::LookupMap;
use near_sdk::{AccountId, BorshStorageKey, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseError, PromiseOrValue, env, near};
use std::collections::HashMap;
pub use policy::{Policy, RateWindow, RebalanceRequest, Refusal, check_out_of_range, check_rebalance};

// Gas (v1.1). Every callback that follows a step on Rhea gets a static floor that pays for the WHOLE
// rest of its chain (built below from the end of each chain back to its start), so a transaction
// either has enough gas from the start or fails before Rhea is touched (a token transfer then comes
// back whole). Unused gas is still shared out on top (actions weight 1, callbacks weight 2), so a
// Rhea upgrade that needs more gas gets it from what the wallet attaches. Action budgets are what
// the real DCL 2.4.0 needs plus margin (tests/realdcl.rs); a Rhea action that runs short fails on
// its own and its callback, which has its own floor, puts the tokens back.
const T_OWN: u64 = 15; // one callback's own work
const T_VIEW: u64 = 10;
const T_ADD: u64 = 50; // add_liquidity passes at 40 TGas, burns ~12
const T_REMOVE: u64 = 40; // remove_liquidity passes at 30, burns ~9
const T_WITHDRAW: u64 = 50; // withdraw_asset passes at 40, burns ~10
const T_FT_CALL: u64 = 60; // a token into Rhea: ft_transfer_call → ft_on_transfer → resolve
const T_TRANSFER: u64 = 10;
const CB_OWN: u64 = T_OWN; // a callback that schedules nothing more
const CB_ADDED: u64 = T_VIEW + CB_OWN + T_OWN; // reads the balance, then on_measured
const CB_SNAP_ADD: u64 = T_ADD + CB_ADDED + T_OWN;
const SNAPSHOT: u64 = T_VIEW + CB_SNAP_ADD; // what snapshot_then_add schedules
const CB_REMOVED_ADD: u64 = SNAPSHOT + T_OWN;
const CB_LIQ_MOVE: u64 = T_REMOVE + CB_REMOVED_ADD + T_OWN;
const CB_POOL_NEW: u64 = SNAPSHOT + T_OWN; // on_pool_for_move with no position yet
const CB_POOL_HELD: u64 = T_VIEW + CB_LIQ_MOVE + T_OWN; // on_pool_for_move moving a position
const CB_FORWARDED_OPEN: u64 = T_VIEW + CB_POOL_NEW + T_OWN; // a deposit that opens
const CB_LIQ_EXIT: u64 = T_REMOVE + CB_OWN + T_OWN;
const CB_POOL_EXIT: u64 = T_VIEW + CB_LIQ_EXIT + T_OWN;
const CB_WITHDRAWN: u64 = T_TRANSFER + CB_OWN + T_OWN;
/// What cash out's pay_next schedules with `n` payouts left, this one included.
const fn pay_floor(n: u64) -> u64 {
    if n == 0 { 0 } else { T_WITHDRAW + cash_withdrawn_floor(n) }
}
const fn cash_withdrawn_floor(n: u64) -> u64 {
    T_TRANSFER + cash_paid_floor(n - 1) + T_OWN
}
const fn cash_paid_floor(left: u64) -> u64 {
    T_OWN + pay_floor(left)
}
const CB_CASH_CLOSED: u64 = T_OWN + pay_floor(2);
const CB_LIQ_CASH: u64 = T_REMOVE + CB_CASH_CLOSED + T_OWN;
const fn tg(t: u64) -> Gas {
    Gas::from_tgas(t)
}
pub const VERSION: &str = "1.1.0";
const ZERO: NearToken = NearToken::from_yoctonear(0);
const ONE_YOCTO: NearToken = NearToken::from_yoctonear(1);
/// A user's record; returned by `leave`.
pub const JOIN_DEPOSIT: NearToken = NearToken::from_millinear(10);
/// A step that never finished (it ran out of gas) frees the lock after this long.
const BUSY_SECS: u64 = 120;

#[near(serializers = [json])]
#[derive(Clone)]
pub struct PoolView {
    pub token_x: AccountId,
    pub token_y: AccountId,
    pub current_point: i32,
}

#[near(serializers = [json])]
pub struct LiquidityView {
    pub amount: U128,
}

/// Rhea's get_liquidity, the fields recovery checks.
#[near(serializers = [json])]
pub struct RheaPosition {
    pub owner_id: AccountId,
    pub pool_id: String,
    pub left_point: i32,
    pub right_point: i32,
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct PositionRef {
    pub lpt_id: String,
    pub pool_id: String,
    pub left_point: i32,
    pub right_point: i32,
}

/// One user: their policy, agent, open position, and idle tokens on this contract's Rhea balance.
#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Account {
    pub policy: Policy,
    pub agent: Option<AccountId>,
    pub frozen: bool,
    pub rate: RateWindow,
    pub position: Option<PositionRef>,
    pub high_point: Option<i32>,
    pub low_point: Option<i32>,
    /// Idle tokens (raw) this user owns on the contract's Rhea balance: (token, amount).
    pub ledger: Vec<(AccountId, U128)>,
    /// Payouts the user's wallet refused (not registered on the token): held in this contract's own
    /// token wallet until `claim`.
    pub owed: Vec<(AccountId, U128)>,
}

/// Sent in the `msg` of a token transfer to open (or move) the position right after the deposit.
#[derive(Deserialize, Serialize, Clone)]
#[serde(crate = "near_sdk::serde")]
pub struct OpenMsg {
    pub left_point: i32,
    pub right_point: i32,
}

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Accounts,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Guardian {
    dcl: AccountId,
    /// Rhea's wNEAR (wrap.near): withdrawals of it arrive as NEAR.
    wnear: AccountId,
    accounts: LookupMap<AccountId, Account>,
    /// While a step that changes Rhea balances runs (seconds); others wait. Released by every end
    /// of a chain, or by time if a chain ran out of gas.
    busy_until: u64,
}

fn refuse(r: Refusal) -> ! {
    env::panic_str(r.message())
}

fn emit(event: &str, data: Value) {
    env::log_str(&format!("EVENT_JSON:{}", json!({ "standard": "kinu_guardian", "version": "1.0.0", "event": event, "data": [data] })));
}

/// An action in another contract: a static minimum plus a share (weight 1) of the unused gas.
fn call(to: &AccountId, method: &str, args: Value, deposit: NearToken, gas: Gas) -> Promise {
    Promise::new(to.clone()).function_call_weight(method.to_string(), args.to_string().into_bytes(), deposit, gas, GasWeight(1))
}

/// An action with a static budget only, for `cash_out`'s long chain: there every unused unit goes
/// to the callback, so the last payout still has gas. (Measured needs on DCL 2.4.0: remove 30T,
/// withdraw_asset 40T; the 60T given leaves room, and a failure only stops the chain safely.)
fn call_fixed(to: &AccountId, method: &str, args: Value, deposit: NearToken, gas: Gas) -> Promise {
    Promise::new(to.clone()).function_call(method.to_string(), args.to_string().into_bytes(), deposit, gas)
}

/// A view of another contract: a static budget only (nothing to forward).
fn view(to: &AccountId, method: &str, args: Value) -> Promise {
    Promise::new(to.clone()).function_call(method.to_string(), args.to_string().into_bytes(), ZERO, tg(T_VIEW))
}

fn now_secs() -> u64 {
    env::block_timestamp() / 1_000_000_000
}

/// remove_liquidity of a whole position; the tokens stay on this contract's Rhea balance.
fn remove_args(lpt_id: &str, amount: U128) -> Value {
    json!({ "lpt_id": lpt_id, "amount": amount, "min_amount_x": "0", "min_amount_y": "0", "skip_refund_transfer": true, "skip_unwrap_near": true })
}

// Rhea DCL (iZiSwap design): over points [left, right) a position of liquidity L holds
// L*sqrtP(i) of Y and L/sqrtP(i) of X at every point i; these are the closed-form sums.
const SQRT_RATE: f64 = 1.000_049_998_750_062_5; // sqrt(1.0001)
fn sqrt_price(point: f64) -> f64 {
    SQRT_RATE.powf(point)
}
fn amount_y(l: f64, left: f64, right: f64) -> f64 {
    if right <= left { 0.0 } else { l * (sqrt_price(right) - sqrt_price(left)) / (SQRT_RATE - 1.0) }
}
fn amount_x(l: f64, left: f64, right: f64) -> f64 {
    if right <= left { 0.0 } else { l * (1.0 / sqrt_price(left) - 1.0 / sqrt_price(right)) * SQRT_RATE / (SQRT_RATE - 1.0) }
}

/// Minimum amounts demanded from add_liquidity: one-sided ranges must use the deposited side
/// fully minus slippage; in range, the least of each token the position would use anywhere within
/// ±slippage of the price. (Only a guard against price moves: the ledger uses the measured amounts.)
pub fn min_amounts(left: i32, right: i32, current: i32, x: u128, y: u128, slippage_bps: u32) -> (u128, u128) {
    let keep = |v: u128| v - v * u128::from(slippage_bps) / 10_000;
    if left > current {
        return (keep(x), 0);
    }
    if right <= current {
        return (0, keep(y));
    }
    let (ax, ay) = (x as f64, y as f64);
    let (l, r) = (f64::from(left), f64::from(right));
    let used = |point: f64| {
        let split = point.clamp(l, r);
        let (ux, uy) = (amount_x(1e24, split, r), amount_y(1e24, l, split));
        let lx = if ux > 0.0 { ax / ux } else { f64::INFINITY };
        let ly = if uy > 0.0 { ay / uy } else { f64::INFINITY };
        let k = lx.min(ly);
        if k.is_finite() { (k * ux, k * uy) } else { (0.0, 0.0) }
    };
    let sl = (f64::from(slippage_bps) / 10_000.0).min(0.5);
    let log_base = 1.0001f64.ln();
    let up = ((1.0 + sl).ln() / log_base).ceil().max(1.0);
    let down = (-(1.0 - sl).ln() / log_base).ceil().max(1.0);
    let cur = f64::from(current);
    let (mut mx, mut my) = (f64::INFINITY, f64::INFINITY);
    for k in [-down, -down / 2.0, 0.0, up / 2.0, up] {
        let (ux, uy) = used((cur + k).round());
        mx = mx.min(ux);
        my = my.min(uy);
    }
    let floor = |v: f64| if v.is_finite() && v > 0.0 { (v * 0.999).floor() as u128 } else { 0 };
    (floor(mx), floor(my))
}

/// What an add left over: lent minus what this contract's Rhea balance lost during it.
pub fn leftover(lent: u128, before: u128, after: u128) -> u128 {
    let paid = before.saturating_sub(after).min(lent);
    lent - paid
}

impl Account {
    fn held(&self, token: &AccountId) -> u128 {
        self.ledger.iter().find(|(t, _)| t == token).map(|(_, a)| a.0).unwrap_or(0)
    }
    fn credit(&mut self, token: &AccountId, amount: u128) {
        if amount == 0 {
            return;
        }
        match self.ledger.iter_mut().find(|(t, _)| t == token) {
            Some((_, a)) => a.0 += amount,
            None => self.ledger.push((token.clone(), U128(amount))),
        }
    }
    fn debit(&mut self, token: &AccountId, amount: u128) {
        if let Some((_, a)) = self.ledger.iter_mut().find(|(t, _)| t == token) {
            a.0 = a.0.saturating_sub(amount);
        }
        self.ledger.retain(|(_, a)| a.0 > 0);
    }
    fn owe(&mut self, token: &AccountId, amount: u128) {
        match self.owed.iter_mut().find(|(t, _)| t == token) {
            Some((_, a)) => a.0 += amount,
            None => self.owed.push((token.clone(), U128(amount))),
        }
    }
    /// The next ledger token to pay out: other tokens first (their payout may be refused and needs
    /// a callback), wNEAR last (NEAR cannot be refused).
    fn next_payout(&self, wnear: &AccountId) -> Option<(AccountId, u128)> {
        let (x, y) = self.pool_tokens();
        let mut order = [x, y];
        if order[0] == *wnear {
            order.swap(0, 1);
        }
        order.into_iter().map(|t| (self.held(&t), t)).find(|(a, _)| *a > 0).map(|(a, t)| (t, a))
    }
    fn pool_id(&self) -> &str {
        &self.policy.allowed_pools[0]
    }
    fn pool_tokens(&self) -> (AccountId, AccountId) {
        let mut it = self.pool_id().split('|');
        let x: AccountId = it.next().unwrap_or("").parse().unwrap_or_else(|_| env::panic_str("Policy pool id is not token_x|token_y|fee"));
        let y: AccountId = it.next().unwrap_or("").parse().unwrap_or_else(|_| env::panic_str("Policy pool id is not token_x|token_y|fee"));
        (x, y)
    }
    fn note_point(&mut self, point: i32) {
        self.high_point = Some(self.high_point.map_or(point, |h| h.max(point)));
        self.low_point = Some(self.low_point.map_or(point, |l| l.min(point)));
    }
}

#[near]
impl Guardian {
    #[init]
    pub fn new(dcl: AccountId, wnear: AccountId) -> Self {
        Self { dcl, wnear, accounts: LookupMap::new(Key::Accounts), busy_until: 0 }
    }

    // ---------- storage anyone may pay (the contract's own Rhea and token registrations) ----------

    /// Rhea DCL storage for this contract (0.5 NEAR registers it; more NEAR adds position slots).
    #[payable]
    pub fn register_dcl(&mut self) -> Promise {
        call(&self.dcl, "storage_deposit", json!({ "account_id": env::current_account_id(), "registration_only": false }), env::attached_deposit(), tg(T_VIEW))
    }

    /// Registers this contract on a token (so Rhea can pay it out).
    #[payable]
    pub fn register_token(&mut self, token: AccountId) -> Promise {
        call(&token, "storage_deposit", json!({ "account_id": env::current_account_id(), "registration_only": true }), env::attached_deposit(), tg(T_VIEW))
    }

    // ---------- the user ----------

    /// Joins with a policy (one Rhea pool). JOIN_DEPOSIT pays the record; `leave` returns it.
    #[payable]
    pub fn join(&mut self, policy: Policy, agent: Option<AccountId>) {
        let user = env::predecessor_account_id();
        if self.accounts.contains_key(&user) {
            env::panic_str("Already joined: use set_policy");
        }
        if env::attached_deposit() < JOIN_DEPOSIT {
            env::panic_str("Attach the join deposit (0.01 NEAR, returned when you leave)");
        }
        policy.validate().unwrap_or_else(|r| refuse(r));
        if policy.allowed_pools.len() != 1 || policy.allowed_pools[0].split('|').count() != 3 {
            env::panic_str("Policy names exactly one pool, as token_x|token_y|fee");
        }
        let acct = Account { policy, agent, frozen: false, rate: RateWindow::default(), position: None, high_point: None, low_point: None, ledger: Vec::new(), owed: Vec::new() };
        acct.pool_tokens();
        self.accounts.insert(user.clone(), acct);
        emit("joined", json!({ "user": user }));
    }

    /// Leaves with nothing left: no position, empty ledger. The join deposit comes back.
    pub fn leave(&mut self) -> Promise {
        let user = env::predecessor_account_id();
        let acct = self.account(&user);
        if acct.position.is_some() || !acct.ledger.is_empty() || !acct.owed.is_empty() {
            env::panic_str("Close your position and withdraw first");
        }
        // A step in flight could still credit this account.
        self.assert_free();
        self.accounts.remove(&user);
        emit("left", json!({ "user": user }));
        Promise::new(user).transfer(JOIN_DEPOSIT)
    }

    pub fn set_policy(&mut self, policy: Policy) {
        let user = env::predecessor_account_id();
        let mut acct = self.account(&user);
        policy.validate().unwrap_or_else(|r| refuse(r));
        if policy.allowed_pools != acct.policy.allowed_pools {
            env::panic_str("The pool cannot change: leave and join again");
        }
        acct.policy = policy;
        self.accounts.insert(user, acct);
    }

    pub fn set_agent(&mut self, agent: Option<AccountId>) {
        let user = env::predecessor_account_id();
        let mut acct = self.account(&user);
        acct.agent = agent;
        self.accounts.insert(user, acct);
    }

    pub fn set_frozen(&mut self, frozen: bool) {
        let user = env::predecessor_account_id();
        let mut acct = self.account(&user);
        acct.frozen = frozen;
        self.accounts.insert(user, acct);
    }

    // ---------- deposit: one token transfer, optionally opening the position ----------

    /// A pool token arrives for `sender_id`: forwarded onto this contract's Rhea balance and credited
    /// to their ledger. `msg` empty: deposit only; `{"left_point":..,"right_point":..}`: then open
    /// (or move) the position with everything they hold. While another step runs, the whole
    /// transfer goes back to the sender at once (try again in a moment).
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        let acct = self.account(&sender_id);
        let (x, y) = acct.pool_tokens();
        if token != x && token != y {
            env::panic_str("This token is not in your pool");
        }
        let open: Option<OpenMsg> = if msg.trim().is_empty() { None } else { Some(near_sdk::serde_json::from_str(&msg).unwrap_or_else(|_| env::panic_str("msg must be empty or {\"left_point\",\"right_point\"}"))) };
        if let Some(o) = &open {
            if o.left_point >= o.right_point {
                refuse(Refusal::BadRange);
            }
            // Moving a position inside a deposit's chain would need more gas than a token transfer
            // passes on: deposit first, then `open` (the page sends both in one wallet request).
            if acct.position.is_some() {
                env::panic_str("You have a position: deposit without a range, then move it with open");
            }
        }
        if self.is_busy() {
            emit("busy", json!({ "user": sender_id, "token": token, "amount": amount }));
            return PromiseOrValue::Value(amount);
        }
        self.lock();
        PromiseOrValue::Promise(
            call(&token, "ft_transfer_call", json!({ "receiver_id": self.dcl, "amount": amount, "msg": "\"Deposit\"" }), ONE_YOCTO, tg(T_FT_CALL))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(if open.is_some() { CB_FORWARDED_OPEN } else { CB_OWN })).with_unused_gas_weight(2).on_forwarded(sender_id, token, amount, open)),
        )
    }

    /// Credits what Rhea took and returns the rest to the sender; then opens if asked.
    #[private]
    pub fn on_forwarded(&mut self, user: AccountId, token: AccountId, amount: U128, open: Option<OpenMsg>, #[callback_result] used: Result<U128, PromiseError>) -> U128 {
        let taken = used.map(|u| u.0).unwrap_or(0).min(amount.0);
        let mut acct = self.account(&user);
        acct.credit(&token, taken);
        let pool_id = acct.pool_id().to_string();
        self.accounts.insert(user.clone(), acct);
        emit("deposited", json!({ "user": user, "token": token, "amount": U128(taken), "returned": U128(amount.0 - taken) }));
        match open {
            Some(o) if taken > 0 => {
                // Still locked: the move continues the same step.
                self.begin_move(user, RebalanceRequest { pool_id, left_point: o.left_point, right_point: o.right_point }, false).detach();
            }
            _ => self.unlock(),
        }
        U128(amount.0 - taken)
    }

    // ---------- the position ----------

    /// The user places (or moves) their position: any range in their pool.
    pub fn open(&mut self, left_point: i32, right_point: i32) -> Promise {
        let user = env::predecessor_account_id();
        let pool_id = self.account(&user).pool_id().to_string();
        if left_point >= right_point {
            refuse(Refusal::BadRange);
        }
        self.assert_free();
        self.lock();
        self.begin_move(user, RebalanceRequest { pool_id, left_point, right_point }, false)
    }

    /// The user's agent asks for a new range; checked against the user's policy on-chain.
    pub fn request_rebalance(&mut self, user: AccountId, request: RebalanceRequest) -> Promise {
        let mut acct = self.account(&user);
        if acct.frozen {
            refuse(Refusal::Frozen);
        }
        if acct.agent.as_ref() != Some(&env::predecessor_account_id()) {
            refuse(Refusal::NotAgent);
        }
        if request.pool_id != acct.pool_id() {
            refuse(Refusal::PoolNotAllowed);
        }
        if request.left_point >= request.right_point {
            refuse(Refusal::BadRange);
        }
        // The agent only re-centres an open position: idle tokens (after an exit, or a deposit
        // without a range) stay idle until the user opens a range themselves.
        if acct.position.is_none() {
            env::panic_str("No open position: only the user opens one");
        }
        self.assert_free();
        let policy = acct.policy.clone();
        acct.rate.take(&policy, now_secs()).unwrap_or_else(|r| refuse(r));
        self.accounts.insert(user.clone(), acct);
        self.lock();
        self.begin_move(user, request, true)
    }

    /// Records the price for the exit rule (its high, or its low with exit_on_rise), so the rule
    /// measures from the best point seen, not only from the last move. The agent or the user; no
    /// lock (it only writes the marks).
    pub fn poke(&mut self, user: AccountId) -> Promise {
        let acct = self.account(&user);
        let caller = env::predecessor_account_id();
        if caller != user {
            if acct.agent.as_ref() != Some(&caller) {
                refuse(Refusal::NotAgent);
            }
            if acct.frozen {
                refuse(Refusal::Frozen);
            }
        }
        let pos = acct.position.unwrap_or_else(|| env::panic_str("No open position"));
        view(&self.dcl, "get_pool", json!({ "pool_id": pos.pool_id }))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).on_poked(user))
    }

    #[private]
    pub fn on_poked(&mut self, user: AccountId, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> bool {
        let Ok(Some(pool)) = pool else { return false };
        let Some(mut acct) = self.accounts.get(&user).cloned() else { return false };
        if acct.position.is_none() {
            return false;
        }
        acct.note_point(pool.current_point);
        emit("point_noted", json!({ "user": user, "current_point": pool.current_point, "high_point": acct.high_point, "low_point": acct.low_point }));
        self.accounts.insert(user, acct);
        true
    }

    /// Reads the pool; the checks and the move follow in the callback. The caller holds the lock.
    fn begin_move(&mut self, user: AccountId, request: RebalanceRequest, by_agent: bool) -> Promise {
        let held = self.account(&user).position.is_some();
        view(&self.dcl, "get_pool", json!({ "pool_id": request.pool_id }))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(if held { CB_POOL_HELD } else { CB_POOL_NEW })).with_unused_gas_weight(2).on_pool_for_move(user, request, by_agent))
    }

    #[private]
    pub fn on_pool_for_move(&mut self, user: AccountId, request: RebalanceRequest, by_agent: bool, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(pool)) = pool else { return self.bail(&user, "Could not read the pool from Rhea DCL") };
        let mut acct = self.account(&user);
        if by_agent {
            if acct.frozen {
                return self.bail(&user, Refusal::Frozen.message());
            }
            if let Err(r) = check_rebalance(&acct.policy, &request, pool.current_point) {
                return self.bail(&user, r.message());
            }
            if let Some(open) = &acct.position {
                if let Err(r) = check_out_of_range(&acct.policy, open.left_point, open.right_point, pool.current_point) {
                    return self.bail(&user, r.message());
                }
            }
        }
        acct.note_point(pool.current_point);
        self.accounts.insert(user.clone(), acct.clone());
        emit(if by_agent { "rebalance_approved" } else { "range_set_by_user" }, json!({ "user": user, "left_point": request.left_point, "right_point": request.right_point, "current_point": pool.current_point }));
        match acct.position {
            Some(old) => PromiseOrValue::Promise(
                view(&self.dcl, "get_liquidity", json!({ "lpt_id": old.lpt_id }))
                    .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_LIQ_MOVE)).with_unused_gas_weight(2).on_liquidity_for_move(user, request, pool)),
            ),
            None => self.snapshot_then_add(user, request, pool),
        }
    }

    /// Closes the old position with its full liquidity; the add follows.
    #[private]
    pub fn on_liquidity_for_move(&mut self, user: AccountId, request: RebalanceRequest, pool: PoolView, #[callback_result] liq: Result<Option<LiquidityView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(liq)) = liq else { return self.bail(&user, "Could not read the open position from Rhea DCL") };
        let Some(old) = self.account(&user).position else { return self.bail(&user, "No open position") };
        PromiseOrValue::Promise(
            call(&self.dcl, "remove_liquidity", remove_args(&old.lpt_id, liq.amount), ZERO, tg(T_REMOVE))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_REMOVED_ADD)).with_unused_gas_weight(2).on_removed_then_add(user, request, pool)),
        )
    }

    #[private]
    pub fn on_removed_then_add(&mut self, user: AccountId, request: RebalanceRequest, pool: PoolView, #[callback_result] removed: Result<(U128, U128), PromiseError>) -> PromiseOrValue<bool> {
        if !self.credit_removed(&user, &pool, removed) {
            return self.bail(&user, "Rhea DCL refused to close the position; it stays open");
        }
        self.snapshot_then_add(user, request, pool)
    }

    /// Reads this contract's Rhea balance right before the add (the ledger takes the difference).
    fn snapshot_then_add(&mut self, user: AccountId, request: RebalanceRequest, pool: PoolView) -> PromiseOrValue<bool> {
        let acct = self.account(&user);
        if acct.held(&pool.token_x) == 0 && acct.held(&pool.token_y) == 0 {
            return self.bail(&user, "Nothing to place: deposit first");
        }
        PromiseOrValue::Promise(
            view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_SNAP_ADD)).with_unused_gas_weight(2).on_snapshot_then_add(user, request, pool)),
        )
    }

    /// Opens `request` with everything the user holds of both tokens.
    #[private]
    pub fn on_snapshot_then_add(&mut self, user: AccountId, request: RebalanceRequest, pool: PoolView, #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(assets) = assets else { return self.bail(&user, "Could not read the balances on Rhea DCL") };
        let before_x = assets.get(&pool.token_x).map(|a| a.0).unwrap_or(0);
        let before_y = assets.get(&pool.token_y).map(|a| a.0).unwrap_or(0);
        let mut acct = self.account(&user);
        let (x, y) = (acct.held(&pool.token_x), acct.held(&pool.token_y));
        let (min_x, min_y) = min_amounts(request.left_point, request.right_point, pool.current_point, x, y, acct.policy.max_slippage_bps);
        // The ledger lends the position everything; on_measured credits back what the add left.
        acct.debit(&pool.token_x, x);
        acct.debit(&pool.token_y, y);
        self.accounts.insert(user.clone(), acct);
        PromiseOrValue::Promise(
            call(
                &self.dcl,
                "add_liquidity",
                json!({ "pool_id": request.pool_id, "left_point": request.left_point, "right_point": request.right_point, "amount_x": U128(x), "amount_y": U128(y), "min_amount_x": U128(min_x), "min_amount_y": U128(min_y), "skip_refund_transfer": true, "skip_unwrap_near": true }),
                ZERO,
                tg(T_ADD),
            )
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_ADDED)).with_unused_gas_weight(2).on_added(user, request, pool, [U128(x), U128(y), U128(before_x), U128(before_y)])),
        )
    }

    /// `lent_before`: [lent_x, lent_y, before_x, before_y].
    #[private]
    pub fn on_added(&mut self, user: AccountId, request: RebalanceRequest, pool: PoolView, lent_before: [U128; 4], #[callback_result] lpt_id: Result<String, PromiseError>) -> PromiseOrValue<bool> {
        let [lent_x, lent_y, _, _] = lent_before;
        let Ok(lpt_id) = lpt_id else {
            // Rhea refused: nothing left the contract's Rhea balance; the tokens are the user's again.
            let mut acct = self.account(&user);
            acct.credit(&pool.token_x, lent_x.0);
            acct.credit(&pool.token_y, lent_y.0);
            self.accounts.insert(user.clone(), acct);
            return self.bail(&user, "Rhea DCL refused the new position (price moved past the slippage?); your tokens stay on your ledger");
        };
        let mut acct = self.account(&user);
        acct.position = Some(PositionRef { lpt_id: lpt_id.clone(), pool_id: request.pool_id.clone(), left_point: request.left_point, right_point: request.right_point });
        self.accounts.insert(user.clone(), acct);
        emit("position_opened", json!({ "user": user, "lpt_id": lpt_id, "left_point": request.left_point, "right_point": request.right_point }));
        PromiseOrValue::Promise(
            view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_measured(user, pool, lent_before)),
        )
    }

    /// Credits back exactly what the add did not take: lent − (balance before − balance after).
    #[private]
    pub fn on_measured(&mut self, user: AccountId, pool: PoolView, lent_before: [U128; 4], #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> bool {
        let [lent_x, lent_y, before_x, before_y] = lent_before;
        let mut acct = self.account(&user);
        match assets {
            Ok(after) => {
                let after_x = after.get(&pool.token_x).map(|a| a.0).unwrap_or(0);
                let after_y = after.get(&pool.token_y).map(|a| a.0).unwrap_or(0);
                acct.credit(&pool.token_x, leftover(lent_x.0, before_x.0, after_x));
                acct.credit(&pool.token_y, leftover(lent_y.0, before_y.0, after_y));
            }
            // Could not read the balance: keep the leftover unassigned rather than guess (reported).
            Err(_) => emit("leftover_unmeasured", json!({ "user": user })),
        }
        self.accounts.insert(user, acct);
        self.unlock();
        true
    }

    // ---------- exit and withdraw: the user's way out, no agent needed ----------

    /// Closes the position: the tokens land on the user's ledger. The user may always; the agent
    /// only when the user's exit rule is met.
    pub fn exit(&mut self, user: AccountId) -> Promise {
        let caller = env::predecessor_account_id();
        let acct = self.account(&user);
        let by_user = caller == user;
        if !by_user {
            if acct.frozen {
                refuse(Refusal::Frozen);
            }
            if acct.agent.as_ref() != Some(&caller) {
                refuse(Refusal::NotAgent);
            }
            if acct.policy.exit_drop_points == 0 {
                env::panic_str("The user has not allowed the agent to exit");
            }
        }
        let pos = acct.position.unwrap_or_else(|| env::panic_str("No open position"));
        self.assert_free();
        self.lock();
        view(&self.dcl, "get_pool", json!({ "pool_id": pos.pool_id }))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_POOL_EXIT)).with_unused_gas_weight(2).on_pool_for_exit(user, by_user))
    }

    #[private]
    pub fn on_pool_for_exit(&mut self, user: AccountId, by_user: bool, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(pool)) = pool else { return self.bail(&user, "Could not read the pool from Rhea DCL") };
        let acct = self.account(&user);
        let cur = pool.current_point;
        let moved = if acct.policy.exit_on_rise {
            i64::from(cur) - i64::from(acct.low_point.unwrap_or(cur).min(cur))
        } else {
            i64::from(acct.high_point.unwrap_or(cur).max(cur)) - i64::from(cur)
        };
        if !by_user && moved < i64::from(acct.policy.exit_drop_points) {
            return self.bail(&user, "Exit rule not met: the price has not moved far enough against the user");
        }
        let Some(pos) = acct.position else { return self.bail(&user, "No open position") };
        emit("exit_started", json!({ "user": user, "by": if by_user { "user" } else { "agent" }, "lpt_id": pos.lpt_id, "current_point": cur }));
        PromiseOrValue::Promise(
            view(&self.dcl, "get_liquidity", json!({ "lpt_id": pos.lpt_id }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_LIQ_EXIT)).with_unused_gas_weight(2).on_liquidity_for_exit(user, pool)),
        )
    }

    #[private]
    pub fn on_liquidity_for_exit(&mut self, user: AccountId, pool: PoolView, #[callback_result] liq: Result<Option<LiquidityView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(liq)) = liq else { return self.bail(&user, "Could not read the open position from Rhea DCL") };
        let Some(pos) = self.account(&user).position else { return self.bail(&user, "No open position") };
        PromiseOrValue::Promise(
            call(&self.dcl, "remove_liquidity", remove_args(&pos.lpt_id, liq.amount), ZERO, tg(T_REMOVE))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_exited(user, pool)),
        )
    }

    #[private]
    pub fn on_exited(&mut self, user: AccountId, pool: PoolView, #[callback_result] removed: Result<(U128, U128), PromiseError>) -> bool {
        if !self.credit_removed(&user, &pool, removed) {
            let _ = self.bail(&user, "Rhea DCL refused to close the position; it stays open");
            return false;
        }
        self.unlock();
        true
    }

    /// Sends the user `amount` (or all) of a pool token from their ledger: Rhea pays this contract,
    /// this contract pays the user. wNEAR arrives as NEAR. Needs no agent and no policy.
    pub fn withdraw(&mut self, token: AccountId, amount: Option<U128>) -> Promise {
        let user = env::predecessor_account_id();
        let mut acct = self.account(&user);
        let held = acct.held(&token);
        let amount = amount.map(|a| a.0).unwrap_or(held);
        if amount == 0 || amount > held {
            env::panic_str("Nothing to withdraw, or more than you hold");
        }
        self.assert_free();
        self.lock();
        acct.debit(&token, amount);
        self.accounts.insert(user.clone(), acct);
        let wnear = token == self.wnear;
        emit("withdraw_started", json!({ "user": user, "token": token, "amount": U128(amount) }));
        // wNEAR: Rhea unwraps and sends NEAR to this contract; other tokens: it transfers them here.
        call(&self.dcl, "withdraw_asset", json!({ "token_id": token, "amount": U128(amount), "skip_unwrap_near": !wnear }), ZERO, tg(T_WITHDRAW))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_WITHDRAWN)).with_unused_gas_weight(2).on_withdrawn(user, token, U128(amount), wnear))
    }

    #[private]
    pub fn on_withdrawn(&mut self, user: AccountId, token: AccountId, amount: U128, wnear: bool, #[callback_result] paid: Result<Value, PromiseError>) -> PromiseOrValue<bool> {
        if paid.is_err() {
            // Rhea kept the tokens: they are the user's again.
            let mut acct = self.account(&user);
            acct.credit(&token, amount.0);
            self.accounts.insert(user.clone(), acct);
            return self.bail(&user, "Rhea DCL did not release the tokens; they stay on your ledger");
        }
        if wnear {
            // NEAR to an existing account cannot be refused.
            self.unlock();
            emit("withdrawn", json!({ "user": user, "token": token, "amount": amount }));
            return PromiseOrValue::Promise(Promise::new(user).transfer(NearToken::from_yoctonear(amount.0)));
        }
        PromiseOrValue::Promise(
            call(&token, "ft_transfer", json!({ "receiver_id": user, "amount": amount }), ONE_YOCTO, tg(T_TRANSFER))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_paid(user, token, amount)),
        )
    }

    /// The token refused the payout (e.g. the user is not registered on it): the tokens stay in this
    /// contract's token wallet, owed to the user until `claim`. Nothing is lost, no further call.
    #[private]
    pub fn on_paid(&mut self, user: AccountId, token: AccountId, amount: U128, #[callback_result] sent: Result<(), PromiseError>) -> bool {
        self.unlock();
        self.settle_payout(&user, &token, amount, sent.is_ok())
    }

    /// Pays what a refused payout left owed, from this contract's own token wallet (Rhea is not
    /// involved, so no lock). A second refusal keeps it owed.
    pub fn claim(&mut self, token: AccountId) -> Promise {
        let user = env::predecessor_account_id();
        let mut acct = self.account(&user);
        // Locked so the user cannot `leave` before a second refusal is written back.
        self.assert_free();
        self.lock();
        let amount = acct.owed.iter().find(|(t, _)| *t == token).map(|(_, a)| a.0).unwrap_or(0);
        if amount == 0 {
            env::panic_str("Nothing owed in this token");
        }
        acct.owed.retain(|(t, _)| *t != token);
        self.accounts.insert(user.clone(), acct);
        call(&token, "ft_transfer", json!({ "receiver_id": user, "amount": U128(amount) }), ONE_YOCTO, tg(T_TRANSFER))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_claimed(user, token, U128(amount)))
    }

    #[private]
    pub fn on_claimed(&mut self, user: AccountId, token: AccountId, amount: U128, #[callback_result] sent: Result<(), PromiseError>) -> bool {
        self.unlock();
        self.settle_payout(&user, &token, amount, sent.is_ok())
    }

    // ---------- cash out: one approval for the whole way out ----------

    /// Closes the position (if open), sends every pool token on the ledger to the wallet (wNEAR
    /// arrives as NEAR) and, with `leave`, removes the record and returns the join deposit. The
    /// user only. Each step that fails stops the chain with the tokens on the ledger, where `exit`
    /// and `withdraw` (which forward all unused gas) still reach them.
    pub fn cash_out(&mut self, leave: bool) -> PromiseOrValue<bool> {
        let user = env::predecessor_account_id();
        let acct = self.account(&user);
        self.assert_free();
        if acct.position.is_none() && acct.next_payout(&self.wnear).is_none() {
            if !leave {
                env::panic_str("Nothing to cash out");
            }
            return PromiseOrValue::Promise(self.leave());
        }
        self.lock();
        emit("cash_out_started", json!({ "user": user, "leave": leave }));
        let Some(pos) = acct.position else { return self.pay_next(user, leave) };
        PromiseOrValue::Promise(
            view(&self.dcl, "get_liquidity", json!({ "lpt_id": pos.lpt_id }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_LIQ_CASH)).with_unused_gas_weight(1).on_liquidity_for_cash_out(user, leave)),
        )
    }

    #[private]
    pub fn on_liquidity_for_cash_out(&mut self, user: AccountId, leave: bool, #[callback_result] liq: Result<Option<LiquidityView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(liq)) = liq else { return self.bail(&user, "Could not read the open position from Rhea DCL") };
        let Some(pos) = self.account(&user).position else { return self.bail(&user, "No open position") };
        PromiseOrValue::Promise(
            call_fixed(&self.dcl, "remove_liquidity", remove_args(&pos.lpt_id, liq.amount), ZERO, tg(T_REMOVE))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_CASH_CLOSED)).with_unused_gas_weight(1).on_cash_out_closed(user, leave)),
        )
    }

    #[private]
    pub fn on_cash_out_closed(&mut self, user: AccountId, leave: bool, #[callback_result] removed: Result<(U128, U128), PromiseError>) -> PromiseOrValue<bool> {
        let (token_x, token_y) = self.account(&user).pool_tokens();
        let pool = PoolView { token_x, token_y, current_point: 0 };
        if !self.credit_removed(&user, &pool, removed) {
            return self.bail(&user, "Rhea DCL refused to close the position; it stays open");
        }
        self.pay_next(user, leave)
    }

    #[private]
    pub fn on_cash_out_withdrawn(&mut self, user: AccountId, token: AccountId, amount: U128, leave: bool, #[callback_result] paid: Result<Value, PromiseError>) -> PromiseOrValue<bool> {
        if paid.is_err() {
            let mut acct = self.account(&user);
            acct.credit(&token, amount.0);
            self.accounts.insert(user.clone(), acct);
            return self.bail(&user, "Rhea DCL did not release the tokens; they stay on your ledger");
        }
        if token == self.wnear {
            emit("withdrawn", json!({ "user": user, "token": token, "amount": amount }));
            // NEAR to an existing account cannot be refused; the chain goes on beside it.
            let _ = Promise::new(user.clone()).transfer(NearToken::from_yoctonear(amount.0));
            return self.pay_next(user, leave);
        }
        let acct = self.account(&user);
        let (x, y) = acct.pool_tokens();
        let left = [x, y].iter().filter(|t| acct.held(t) > 0).count() as u64;
        PromiseOrValue::Promise(
            call_fixed(&token, "ft_transfer", json!({ "receiver_id": user, "amount": amount }), ONE_YOCTO, tg(T_TRANSFER))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(cash_paid_floor(left))).with_unused_gas_weight(1).on_cash_out_paid(user, token, amount, leave)),
        )
    }

    #[private]
    pub fn on_cash_out_paid(&mut self, user: AccountId, token: AccountId, amount: U128, leave: bool, #[callback_result] sent: Result<(), PromiseError>) -> PromiseOrValue<bool> {
        self.settle_payout(&user, &token, amount, sent.is_ok());
        self.pay_next(user, leave)
    }

    // ---------- recovery: only the contract account itself (its key is KINU's during beta) ----------

    /// Puts a position this contract owns on Rhea onto a user's record, e.g. after a chain ran out of
    /// gas between the add and the record (v1.0). Rhea says who owns it and where it sits; it must be
    /// this contract's, in the user's pool, and the user must have no position. A public event.
    pub fn recover_position(&mut self, user: AccountId, lpt_id: String) -> Promise {
        self.assert_self();
        if self.account(&user).position.is_some() {
            env::panic_str("The user already has a position");
        }
        view(&self.dcl, "get_liquidity", json!({ "lpt_id": lpt_id })).then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).on_recover_checked(user, lpt_id))
    }

    #[private]
    pub fn on_recover_checked(&mut self, user: AccountId, lpt_id: String, #[callback_result] found: Result<Option<RheaPosition>, PromiseError>) -> bool {
        let Ok(Some(l)) = found else {
            emit("recover_refused", json!({ "user": user, "lpt_id": lpt_id, "why": "Rhea has no such position" }));
            return false;
        };
        let Some(mut acct) = self.accounts.get(&user).cloned() else { return false };
        if l.owner_id != env::current_account_id() || l.pool_id != acct.pool_id() || acct.position.is_some() {
            emit("recover_refused", json!({ "user": user, "lpt_id": lpt_id, "why": "not this contract's, not the user's pool, or the user has a position" }));
            return false;
        }
        acct.position = Some(PositionRef { lpt_id: lpt_id.clone(), pool_id: l.pool_id, left_point: l.left_point, right_point: l.right_point });
        self.accounts.insert(user.clone(), acct);
        emit("recovered_position", json!({ "user": user, "lpt_id": lpt_id, "left_point": l.left_point, "right_point": l.right_point }));
        true
    }

    /// Credits a user with tokens on this contract's Rhea balance that no record holds (an add's
    /// leftover a chain could not measure). Only the contract account itself; a public event.
    pub fn recover_credit(&mut self, user: AccountId, token: AccountId, amount: U128) {
        self.assert_self();
        let mut acct = self.account(&user);
        let (x, y) = acct.pool_tokens();
        if token != x && token != y {
            env::panic_str("This token is not in the user's pool");
        }
        acct.credit(&token, amount.0);
        self.accounts.insert(user.clone(), acct);
        emit("recovered_credit", json!({ "user": user, "token": token, "amount": amount }));
    }

    fn assert_self(&self) {
        if env::predecessor_account_id() != env::current_account_id() {
            env::panic_str("Only the contract account itself can recover");
        }
    }

    // ---------- views ----------

    pub fn get_account(&self, user: AccountId) -> Option<Account> {
        self.accounts.get(&user).cloned()
    }

    pub fn get_config(&self) -> Value {
        json!({ "version": VERSION, "dcl": self.dcl, "wnear": self.wnear, "join_deposit": JOIN_DEPOSIT, "busy": self.is_busy() })
    }

    // ---------- helpers ----------

    fn account(&self, user: &AccountId) -> Account {
        self.accounts.get(user).cloned().unwrap_or_else(|| env::panic_str("This account has not joined"))
    }

    fn is_busy(&self) -> bool {
        self.busy_until > now_secs()
    }

    fn assert_free(&self) {
        if self.is_busy() {
            env::panic_str("Another step is running; try again in a moment");
        }
    }

    fn lock(&mut self) {
        self.busy_until = now_secs() + BUSY_SECS;
    }

    fn unlock(&mut self) {
        self.busy_until = 0;
    }

    /// A step that cannot go on: release the lock, say why (the event is the only trace a wallet
    /// gets, since the transaction itself succeeds), and end the chain.
    fn bail(&mut self, user: &AccountId, why: &str) -> PromiseOrValue<bool> {
        self.unlock();
        emit("refused", json!({ "user": user, "why": why }));
        PromiseOrValue::Value(false)
    }

    /// The end of one payout: sent, or refused and owed to the user (see `claim`).
    fn settle_payout(&mut self, user: &AccountId, token: &AccountId, amount: U128, sent: bool) -> bool {
        if sent {
            emit("withdrawn", json!({ "user": user, "token": token, "amount": amount }));
            return true;
        }
        let mut acct = self.account(user);
        acct.owe(token, amount.0);
        self.accounts.insert(user.clone(), acct);
        emit("payout_refused", json!({ "user": user, "token": token, "amount": amount, "why": "Your wallet refused the token (register on it), use claim" }));
        false
    }

    /// Cash out's next step: withdraw the next ledger token from Rhea, or, with the ledger empty,
    /// end the chain (and leave, if asked and nothing is owed).
    fn pay_next(&mut self, user: AccountId, leave: bool) -> PromiseOrValue<bool> {
        let mut acct = self.account(&user);
        let Some((token, amount)) = acct.next_payout(&self.wnear) else {
            self.unlock();
            emit("cash_out_done", json!({ "user": user }));
            if leave && acct.position.is_none() && acct.owed.is_empty() {
                self.accounts.remove(&user);
                emit("left", json!({ "user": user }));
                return PromiseOrValue::Promise(Promise::new(user).transfer(JOIN_DEPOSIT));
            }
            return PromiseOrValue::Value(true);
        };
        let (x, y) = acct.pool_tokens();
        let n = [x, y].iter().filter(|t| acct.held(t) > 0).count() as u64;
        acct.debit(&token, amount);
        self.accounts.insert(user.clone(), acct);
        let wnear = token == self.wnear;
        emit("withdraw_started", json!({ "user": user, "token": token, "amount": U128(amount) }));
        PromiseOrValue::Promise(
            call_fixed(&self.dcl, "withdraw_asset", json!({ "token_id": token, "amount": U128(amount), "skip_unwrap_near": !wnear }), ZERO, tg(T_WITHDRAW))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(cash_withdrawn_floor(n))).with_unused_gas_weight(1).on_cash_out_withdrawn(user, token, U128(amount), leave)),
        )
    }

    /// After remove_liquidity: the position is gone and the removed amounts are the user's again.
    fn credit_removed(&mut self, user: &AccountId, pool: &PoolView, removed: Result<(U128, U128), PromiseError>) -> bool {
        let Ok((rx, ry)) = removed else { return false };
        let mut acct = self.account(user);
        acct.credit(&pool.token_x, rx.0);
        acct.credit(&pool.token_y, ry.0);
        let was = acct.position.take();
        acct.high_point = None;
        acct.low_point = None;
        self.accounts.insert(user.clone(), acct);
        emit("position_closed", json!({ "user": user, "lpt_id": was.map(|p| p.lpt_id), "amount_x": rx, "amount_y": ry }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    const POOL: &str = "tkq.testnet|wrap.testnet|10000";
    fn acc(s: &str) -> AccountId {
        s.parse().unwrap()
    }
    fn ctx(who: &str, deposit: NearToken) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(who)).current_account_id(acc("guardian.testnet")).attached_deposit(deposit).build());
    }
    fn policy() -> Policy {
        Policy { allowed_pools: vec![POOL.into()], min_width: 1400, max_width: 5600, max_distance: 3000, max_actions: 12, window_secs: 86_400, max_slippage_bps: 100, exit_drop_points: 0, exit_on_rise: false, rebalance_buffer_points: 100 }
    }
    fn joined() -> Guardian {
        let mut g = Guardian::new(acc("dcl.testnet"), acc("wrap.testnet"));
        ctx("alice.testnet", JOIN_DEPOSIT);
        g.join(policy(), Some(acc("agent.testnet")));
        g
    }
    fn held(g: &Guardian, who: &str, token: &str) -> u128 {
        g.get_account(acc(who)).unwrap().held(&acc(token))
    }

    #[test]
    #[should_panic(expected = "Attach the join deposit")]
    fn join_needs_the_deposit() {
        let mut g = Guardian::new(acc("dcl.testnet"), acc("wrap.testnet"));
        ctx("alice.testnet", ZERO);
        g.join(policy(), None);
    }

    #[test]
    #[should_panic(expected = "exactly one pool")]
    fn join_needs_one_pool() {
        let mut g = Guardian::new(acc("dcl.testnet"), acc("wrap.testnet"));
        ctx("alice.testnet", JOIN_DEPOSIT);
        let mut p = policy();
        p.allowed_pools.push("a|b|100".into());
        g.join(p, None);
    }

    #[test]
    #[should_panic(expected = "not in your pool")]
    fn deposit_of_another_token_is_refused() {
        let mut g = joined();
        ctx("usdt.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("alice.testnet"), U128(5), String::new());
    }

    #[test]
    #[should_panic(expected = "has not joined")]
    fn deposit_from_a_stranger_is_refused() {
        let mut g = joined();
        ctx("tkq.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("bob.testnet"), U128(5), String::new());
    }

    #[test]
    fn a_busy_contract_returns_the_deposit_whole() {
        let mut g = joined();
        g.lock();
        ctx("tkq.testnet", ZERO);
        match g.ft_on_transfer(acc("alice.testnet"), U128(5), String::new()) {
            PromiseOrValue::Value(v) => assert_eq!(v, U128(5)),
            _ => panic!("expected the whole amount back"),
        }
    }

    #[test]
    fn deposits_credit_what_rhea_took_and_return_the_rest() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        assert_eq!(g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(5), None, Ok(U128(5))), U128(0));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 5);
        // Rhea took only part: that part is credited, the rest goes back to the sender.
        assert_eq!(g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(7), None, Ok(U128(3))), U128(4));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 8);
        // Rhea failed: nothing credited, all of it back.
        assert_eq!(g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(9), None, Err(PromiseError::Failed)), U128(9));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 8);
        assert!(!g.is_busy(), "deposit-only steps release the lock");
    }

    #[test]
    fn leftover_is_exact() {
        // Lent 100, the contract's Rhea balance fell by 73: 27 back.
        assert_eq!(leftover(100, 1_000, 927), 27);
        // A balance that rose meanwhile cannot credit more than was lent.
        assert_eq!(leftover(100, 1_000, 1_050), 100);
        // Took everything.
        assert_eq!(leftover(100, 1_000, 900), 0);
        // Never more than lent is counted as paid.
        assert_eq!(leftover(100, 1_000, 800), 0);
    }

    #[test]
    fn a_refused_add_returns_the_tokens_and_the_lock() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        let pool = PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: 0 };
        let req = RebalanceRequest { pool_id: POOL.into(), left_point: -1400, right_point: 1400 };
        let r = g.on_added(acc("alice.testnet"), req, pool, [U128(10), U128(20), U128(0), U128(0)], Err(PromiseError::Failed));
        assert!(matches!(r, PromiseOrValue::Value(false)));
        assert_eq!((held(&g, "alice.testnet", "tkq.testnet"), held(&g, "alice.testnet", "wrap.testnet")), (10, 20));
        assert!(!g.is_busy());
    }

    #[test]
    fn measured_leftovers_go_back_to_the_ledger() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        let pool = PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: 0 };
        let after: HashMap<AccountId, U128> = [(acc("tkq.testnet"), U128(1_000)), (acc("wrap.testnet"), U128(5_015))].into_iter().collect();
        // Lent 100 x and 200 y; before 1_100 x and 5_200 y: the add took 100 x and 185 y.
        g.on_measured(acc("alice.testnet"), pool, [U128(100), U128(200), U128(1_100), U128(5_200)], Ok(after));
        assert_eq!((held(&g, "alice.testnet", "tkq.testnet"), held(&g, "alice.testnet", "wrap.testnet")), (0, 15));
        assert!(!g.is_busy());
    }

    #[test]
    fn a_refused_payout_is_owed_and_claimable() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert!(!g.on_paid(acc("alice.testnet"), acc("tkq.testnet"), U128(42), Err(PromiseError::Failed)));
        assert_eq!(g.get_account(acc("alice.testnet")).unwrap().owed, vec![(acc("tkq.testnet"), U128(42))]);
        assert!(!g.is_busy());
        ctx("alice.testnet", ZERO);
        // A claim takes it off; a second refusal puts it back.
        let _ = g.claim(acc("tkq.testnet"));
        assert!(g.get_account(acc("alice.testnet")).unwrap().owed.is_empty());
        assert!(g.is_busy(), "a claim holds the lock until its callback");
        ctx("guardian.testnet", ZERO);
        assert!(!g.on_claimed(acc("alice.testnet"), acc("tkq.testnet"), U128(42), Err(PromiseError::Failed)));
        assert_eq!(g.get_account(acc("alice.testnet")).unwrap().owed, vec![(acc("tkq.testnet"), U128(42))]);
        assert!(!g.is_busy());
    }

    #[test]
    #[should_panic(expected = "Close your position and withdraw first")]
    fn leave_waits_for_what_is_owed() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.on_paid(acc("alice.testnet"), acc("tkq.testnet"), U128(42), Err(PromiseError::Failed));
        ctx("alice.testnet", ZERO);
        let _ = g.leave();
    }

    #[test]
    #[should_panic(expected = "Nothing to cash out")]
    fn cash_out_needs_something() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        let _ = g.cash_out(false);
    }

    #[test]
    fn cash_out_with_nothing_and_leave_just_leaves() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        assert!(matches!(g.cash_out(true), PromiseOrValue::Promise(_)));
        assert!(g.get_account(acc("alice.testnet")).is_none());
    }

    #[test]
    fn cash_out_pays_other_tokens_before_wnear() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.on_forwarded(acc("alice.testnet"), acc("wrap.testnet"), U128(9), None, Ok(U128(9)));
        g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(5), None, Ok(U128(5)));
        let a = g.get_account(acc("alice.testnet")).unwrap();
        assert_eq!(a.next_payout(&acc("wrap.testnet")), Some((acc("tkq.testnet"), 5)));
        // The chain: tkq withdrawn and paid, then wNEAR, then the end (and leave).
        ctx("alice.testnet", ZERO);
        assert!(matches!(g.cash_out(true), PromiseOrValue::Promise(_)));
        assert!(g.is_busy());
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 0, "debited when its withdraw starts");
        ctx("guardian.testnet", ZERO);
        let _ = g.on_cash_out_withdrawn(acc("alice.testnet"), acc("tkq.testnet"), U128(5), true, Ok(Value::Null));
        // The wallet refused tkq: owed, and the chain goes on to wNEAR.
        assert!(matches!(g.on_cash_out_paid(acc("alice.testnet"), acc("tkq.testnet"), U128(5), true, Err(PromiseError::Failed)), PromiseOrValue::Promise(_)));
        assert_eq!(held(&g, "alice.testnet", "wrap.testnet"), 0);
        let _ = g.on_cash_out_withdrawn(acc("alice.testnet"), acc("wrap.testnet"), U128(9), true, Ok(Value::Null));
        // Ledger empty: unlocked, but something is owed, so the record stays (no leave).
        assert!(!g.is_busy());
        let a = g.get_account(acc("alice.testnet")).expect("kept while owed");
        assert!(a.ledger.is_empty());
        assert_eq!(a.owed, vec![(acc("tkq.testnet"), U128(5))]);
    }

    #[test]
    fn a_refused_cash_out_withdraw_stops_with_the_tokens_on_the_ledger() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(5), None, Ok(U128(5)));
        ctx("alice.testnet", ZERO);
        let _ = g.cash_out(true);
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.on_cash_out_withdrawn(acc("alice.testnet"), acc("tkq.testnet"), U128(5), true, Err(PromiseError::Failed)), PromiseOrValue::Value(false)));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 5);
        assert!(!g.is_busy());
        assert!(g.get_account(acc("alice.testnet")).is_some());
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn only_the_agent_rebalances() {
        let mut g = joined();
        ctx("bob.testnet", ZERO);
        let _ = g.request_rebalance(acc("alice.testnet"), RebalanceRequest { pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
    }

    #[test]
    #[should_panic(expected = "only the user opens one")]
    fn the_agent_never_opens_from_idle_tokens() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(5), None, Ok(U128(5)));
        ctx("agent.testnet", ZERO);
        let _ = g.request_rebalance(acc("alice.testnet"), RebalanceRequest { pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn only_the_agent_or_the_user_pokes() {
        let mut g = joined();
        ctx("bob.testnet", ZERO);
        let _ = g.poke(acc("alice.testnet"));
    }

    #[test]
    fn a_poke_notes_the_best_point() {
        let mut g = joined();
        let mut a = g.get_account(acc("alice.testnet")).unwrap();
        a.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
        a.note_point(10);
        g.accounts.insert(acc("alice.testnet"), a);
        ctx("guardian.testnet", ZERO);
        let pool = |p| Ok(Some(PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: p }));
        assert!(g.on_poked(acc("alice.testnet"), pool(50)));
        assert!(g.on_poked(acc("alice.testnet"), pool(-30)));
        let a = g.get_account(acc("alice.testnet")).unwrap();
        assert_eq!((a.high_point, a.low_point), (Some(50), Some(-30)));
    }

    #[test]
    fn gas_floors_cover_each_chain_within_300_tgas() {
        // Each entry point: what it schedules, plus its own work, must fit a wallet's 300 TGas
        // (a token transfer passes ~270 of it on to ft_on_transfer).
        let deposit_open = T_FT_CALL + CB_FORWARDED_OPEN + T_OWN;
        let agent_move = T_VIEW + CB_POOL_HELD + T_OWN;
        let cash_out = T_VIEW + CB_LIQ_CASH + T_OWN;
        let exit = T_VIEW + CB_POOL_EXIT + T_OWN;
        let withdraw = T_WITHDRAW + CB_WITHDRAWN + T_OWN;
        println!("deposit+open {deposit_open}, move {agent_move}, cash_out {cash_out}, exit {exit}, withdraw {withdraw}");
        assert!(deposit_open <= 265 && agent_move <= 290 && cash_out <= 290 && exit <= 290 && withdraw <= 290);
        // The chain behind an add always has its measurement step paid for.
        assert!(CB_ADDED >= T_VIEW + CB_OWN && CB_SNAP_ADD >= T_ADD + CB_ADDED);
    }

    #[test]
    #[should_panic(expected = "deposit without a range, then move it with open")]
    fn a_deposit_never_moves_an_open_position() {
        let mut g = joined();
        let mut a = g.get_account(acc("alice.testnet")).unwrap();
        a.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
        g.accounts.insert(acc("alice.testnet"), a);
        ctx("tkq.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("alice.testnet"), U128(5), r#"{"left_point":0,"right_point":1400}"#.into());
    }

    #[test]
    #[should_panic(expected = "Only the contract account itself")]
    fn only_the_contract_itself_recovers_credit() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        g.recover_credit(acc("alice.testnet"), acc("tkq.testnet"), U128(5));
    }

    #[test]
    fn the_contract_itself_recovers_a_position_and_credit() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.recover_credit(acc("alice.testnet"), acc("tkq.testnet"), U128(5));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 5);
        let _ = g.recover_position(acc("alice.testnet"), "tkq.testnet|wrap.testnet|10000#9".into());
        let found = RheaPosition { owner_id: acc("guardian.testnet"), pool_id: POOL.into(), left_point: -1400, right_point: 1400 };
        assert!(g.on_recover_checked(acc("alice.testnet"), "tkq.testnet|wrap.testnet|10000#9".into(), Ok(Some(found))));
        assert_eq!(g.get_account(acc("alice.testnet")).unwrap().position.unwrap().left_point, -1400);
        // Someone else's position, or a second one, is refused.
        let theirs = RheaPosition { owner_id: acc("bob.testnet"), pool_id: POOL.into(), left_point: 0, right_point: 1400 };
        assert!(!g.on_recover_checked(acc("alice.testnet"), "x#1".into(), Ok(Some(theirs))));
    }

    #[test]
    #[should_panic(expected = "Nothing to withdraw")]
    fn withdraw_is_bounded_by_the_ledger() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        let _ = g.withdraw(acc("tkq.testnet"), Some(U128(1)));
    }

    #[test]
    #[should_panic(expected = "Close your position and withdraw first")]
    fn leave_needs_an_empty_ledger() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.on_forwarded(acc("alice.testnet"), acc("tkq.testnet"), U128(5), None, Ok(U128(5)));
        ctx("alice.testnet", ZERO);
        let _ = g.leave();
    }

    #[test]
    fn the_lock_expires() {
        let mut g = joined();
        g.lock();
        assert!(g.is_busy());
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("alice.testnet")).current_account_id(acc("guardian.testnet")).block_timestamp((BUSY_SECS + 1) * 1_000_000_000).build());
        assert!(!g.is_busy());
    }
}
