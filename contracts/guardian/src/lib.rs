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
//!   · every user is listed on-chain (`get_users`, v1.2), so the agent's scheduler finds them all
//!     without an indexer;
//!   · nobody else touches a user's ledger. Anyone may pay the contract's Rhea / token storage.
//!
//! v1.3 (hackathon, Nov 2026):
//!   · no key holds the users' funds: every admin power (a code upgrade, recovering a stranded
//!     position or leftover, a new owner) is proposed by the owner (a Sputnik DAO), published as an
//!     event, and can run only after ADMIN_DELAY_SECS; anyone may run it then. Users see it coming
//!     and can cash out first. The contract account's own key is deleted once v1.3 is deployed.
//!   · new code is staged first (anyone, paying its storage); the owner proposes its sha256, so the
//!     hash a user checks against the public build is the code that runs.
//!   · the agent cannot act on one moment's price: a re-centre or an exit needs the rule already met
//!     at a price this contract read itself (`poke`) 3 to 60 minutes earlier, so a price pushed by a
//!     large swap for one block is not enough.
//!
//! v1.5 (review 30 Oct 2026):
//!   · the exit rule's best price (high_point, or low_point with exit_on_rise) moves only on a
//!     confirmed read: a price that held at two pokes 3 to 60 minutes apart. One trade's spike, or a
//!     price pushed for one block, no longer becomes the "best price" a fall is measured from (N1).
//!   · a deposit is measured: this contract's Rhea balance of the token is read before and after the
//!     forward, and the user is credited the smaller of that and what the token reports. A token
//!     that over-reports can no longer inflate a ledger into other users' balance (M2). Measuring
//!     takes two reads, so a deposit no longer opens a range in the same transfer: the page sends
//!     `open` as its own transaction in the same wallet request.
//!   · a payout is measured too: tax tokens (nearly.trade, $KINU) keep 1% of every transfer out of
//!     Rhea, so what reaches this contract can be less than Rhea released. The user is paid what
//!     arrived: this contract's own token balance less what it owes users (`owed_total`, kept per
//!     token), capped at what Rhea released. A refused payout is owed exactly that, and `claim`
//!     pays at most what the contract holds beyond what it owes others. Before, a tax-token payout
//!     asked for 100%, failed, and left the user owed what could never be paid.
//!   · cash out moves both tokens side by side (withdraw, read, transfer), so the extra reads fit
//!     in one transaction.
//!   · `claim_fees(send)`: the user takes the fees their position earned without closing it (Rhea's
//!     remove_liquidity with amount 0), onto the ledger or, with `send`, on to the wallet.
//!   · Rhea's answers are read by hand, so a Rhea upgrade that changes their shape never makes a
//!     callback panic. A step Rhea carried out that this contract cannot measure (an unreadable
//!     answer or balance, a payout that never arrived) marks the user `unsettled`; `settle`, which
//!     anyone may run, credits them what this contract's Rhea balance holds beyond all ledgers
//!     (`ledger_total`). Until then nothing new starts; every way out works. No owner, no delay.

mod policy;

use near_sdk::json_types::U128;
use near_sdk::serde_json::{Value, json};
use near_sdk::store::{IterableMap, IterableSet, LookupMap};
use near_sdk::{AccountId, BorshStorageKey, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PromiseError, PromiseOrValue, env, near};
use std::collections::HashMap;
pub use policy::{Policy, RateWindow, RebalanceRequest, Refusal, SEEN_MAX_AGE_SECS, SEEN_MIN_AGE_SECS, check_out_of_range, check_rebalance, check_seen_age, moved_against, take_profit_met, TakeProfit};

/// A pool id as Rhea writes it: two account ids and a whole-number fee. The join deposit pays for a
/// record of ordinary size, so a pool id of any other shape or length never reaches storage (v1.6).
fn pool_id_ok(id: &str) -> bool {
    let parts: Vec<&str> = id.split('|').collect();
    parts.len() == 3 && parts[0].len() <= 64 && parts[1].len() <= 64 && parts[2].len() <= 7 && parts[2].parse::<u32>().is_ok()
}

/// Positions one wallet can run (v1.6).
pub const MAX_SLOTS: u8 = 3;

/// One position's record: the wallet, or `wallet#1`, `wallet#2` for its other positions (v1.6).
/// A plain string in JSON and borsh, so slot 0 reads exactly like the account id it always was:
/// old agents, events, the unsettled list and stored proposals all stay valid. `#` cannot occur in
/// an account id. Only `#1` and `#2` are read as slots, so a record has exactly one spelling.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Who(String);

impl Who {
    pub fn of(account: &AccountId, slot: u8) -> Self {
        if slot == 0 { Self(account.to_string()) } else { Self(format!("{account}#{slot}")) }
    }

    /// The caller's own record, always: nobody names another wallet's record here.
    fn mine(slot: Option<u8>) -> Self {
        let slot = slot.unwrap_or(0);
        if slot >= MAX_SLOTS {
            env::panic_str("A wallet has positions 0, 1 and 2");
        }
        Self::of(&env::predecessor_account_id(), slot)
    }

    fn parts(&self) -> Option<(AccountId, u8)> {
        let (account, slot) = match self.0.split_once('#') {
            None => (self.0.as_str(), 0),
            Some((account, "1")) => (account, 1),
            Some((account, "2")) => (account, 2),
            Some(_) => return None,
        };
        account.parse().ok().map(|a| (a, slot))
    }

    /// The wallet behind this record: every payout goes here.
    pub fn account(&self) -> AccountId {
        self.parts().map(|(a, _)| a).unwrap_or_else(|| env::panic_str("Not a position"))
    }
}

#[cfg(test)]
impl PartialEq<AccountId> for Who {
    fn eq(&self, other: &AccountId) -> bool {
        self.0 == other.as_str()
    }
}

impl From<AccountId> for Who {
    fn from(account: AccountId) -> Self {
        Self(account.to_string())
    }
}

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
// v1.5 deposit: read the Rhea balance, forward the tokens, read it again, credit the difference.
const CB_DEP_MEASURED: u64 = CB_OWN;
const CB_FORWARDED: u64 = T_VIEW + CB_DEP_MEASURED + T_OWN; // reads the balance after the forward
const CB_DEP_BEFORE: u64 = T_FT_CALL + CB_FORWARDED + T_OWN; // forwards, then on_forwarded
const CB_LIQ_EXIT: u64 = T_REMOVE + CB_OWN + T_OWN;
const CB_POOL_EXIT: u64 = T_VIEW + CB_LIQ_EXIT + T_OWN;
// v1.5 payouts: the tokens of one payout move side by side (withdraw each from Rhea, read where
// each landed, transfer each), `ft` of them not wNEAR (wNEAR arrives as NEAR and needs no read).
const CB_OUT_PAID: u64 = CB_OWN;
const fn cb_out_arrived(ft: u64) -> u64 {
    T_OWN + ft * T_TRANSFER + CB_OUT_PAID
}
const fn cb_out_withdrawn(ft: u64) -> u64 {
    if ft == 0 { CB_OWN } else { T_OWN + ft * T_VIEW + cb_out_arrived(ft) }
}
const fn out_floor(legs: u64, ft: u64) -> u64 {
    legs * T_WITHDRAW + cb_out_withdrawn(ft)
}
const CB_CLAIM_MEASURED: u64 = T_TRANSFER + CB_OWN + T_OWN;
const CB_CASH_CLOSED: u64 = T_OWN + out_floor(2, 2);
const CB_LIQ_CASH: u64 = T_REMOVE + CB_CASH_CLOSED + T_OWN;
// v1.5 claim_fees: remove_liquidity with amount 0 takes only the fees; with `send`, both go out.
const CB_FEES: u64 = T_OWN + out_floor(2, 2);
const fn tg(t: u64) -> Gas {
    Gas::from_tgas(t)
}
pub const VERSION: &str = "1.6.0";
/// How long an admin action waits after it is proposed (v1.3): long enough for every user to see
/// the event and cash out.
#[cfg(not(feature = "short-delay"))]
pub const ADMIN_DELAY_SECS: u64 = 48 * 3_600;
/// The sandbox test build only (feature short-delay): skipping 48 hours of blocks takes the sandbox
/// over an hour. Everything else is the same code; this build is never deployed.
#[cfg(feature = "short-delay")]
pub const ADMIN_DELAY_SECS: u64 = 600;
/// Raw storage key of staged code (kept out of the state struct so normal calls never load it).
const STAGED_CODE: &[u8] = b"staged_code";
/// Gas for the new code's `migrate`, run in the same receipt as its deploy.
const T_MIGRATE: u64 = 50;
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

/// A price this contract read itself for a user's pool (v1.3): what the agent's next action is
/// checked against.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Seen {
    pub point: i32,
    pub at_secs: u64,
}

/// What the owner can do, each only after ADMIN_DELAY_SECS (v1.3).
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub enum AdminAction {
    /// Deploy the staged code; `code_hash` is its sha256, base58 (as `near` CLI and explorers show).
    Upgrade { code_hash: String },
    /// Put a position this contract owns on Rhea onto a user's record (e.g. a chain that ran out of
    /// gas between the add and the record).
    RecoverPosition { user: Who, lpt_id: String },
    /// Credit a user with tokens on this contract's Rhea balance that no record holds.
    RecoverCredit { user: Who, token: AccountId, amount: U128 },
    /// Hand the owner role to another account (e.g. a new DAO).
    SetOwner { owner: AccountId },
}

#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub action: AdminAction,
    pub proposed_at_secs: u64,
    pub ready_at_secs: u64,
}

/// Code waiting to be proposed and deployed: who staged it pays its storage and gets that back.
#[near(serializers = [borsh, json])]
#[derive(Clone, Debug, PartialEq)]
pub struct Staged {
    pub code_hash: String,
    pub bytes: u64,
    pub by: AccountId,
    pub deposit: NearToken,
}

#[derive(BorshStorageKey)]
#[near]
enum Key {
    Accounts,
    Users,
    Pending,
    Seen,
    TakeProfit,
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
    /// Every account with a record (v1.2): joins add, leaving removes.
    users: IterableSet<AccountId>,
    /// v1.3: proposes admin actions (a Sputnik DAO); it cannot run them before the delay.
    owner: AccountId,
    pending: IterableMap<u32, Pending>,
    next_pending: u32,
    staged: Option<Staged>,
    /// v1.3: the last price `poke` read for each user's pool.
    seen: LookupMap<AccountId, Seen>,
    /// v1.3: emergency pause by the owner, at once (no delay): no joins, deposits, new ranges or agent
    /// re-centres. Every way out stays open: exit (the user's, or the agent's by the user's rule),
    /// withdraw, cash_out, claim, leave.
    paused: bool,
    /// v1.4: take profit, set by the user only: the pool point at which the agent may close the
    /// position (the price risen to it; fallen to it with exit_on_rise). Kept apart from the policy
    /// so v1.3 records need no migration. Cleared when the position closes.
    take_profit: LookupMap<AccountId, TakeProfit>,
}

/// The state as v1.3 stored it, read once by `migrate`.
#[near(serializers = [borsh])]
struct GuardianV13 {
    dcl: AccountId,
    wnear: AccountId,
    accounts: LookupMap<AccountId, Account>,
    busy_until: u64,
    users: IterableSet<AccountId>,
    owner: AccountId,
    pending: IterableMap<u32, Pending>,
    next_pending: u32,
    staged: Option<Staged>,
    seen: LookupMap<AccountId, Seen>,
    paused: bool,
}

/// The state as v1.2 stored it, read once by `migrate`.
#[near(serializers = [borsh])]
struct GuardianV12 {
    dcl: AccountId,
    wnear: AccountId,
    accounts: LookupMap<AccountId, Account>,
    busy_until: u64,
    users: IterableSet<AccountId>,
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

/// Per-token totals (v1.5), under raw keys beside the state so the v1.4 state needs no new layout;
/// `migrate` rebuilds both from the records.
///   · owed_total: payouts users' wallets refused, held in this contract's own token wallet until
///     `claim`;
///   · ledger_total: what all ledgers hold on this contract's Rhea balance. Whatever that balance
///     holds beyond it belongs to no ledger, and `settle` gives it to the user whose step Rhea
///     carried out but this contract could not measure.
fn total_key(kind: &str, token: &AccountId) -> Vec<u8> {
    [kind.as_bytes(), b":", token.as_bytes()].concat()
}
fn read_total(kind: &str, token: &AccountId) -> u128 {
    env::storage_read(&total_key(kind, token)).and_then(|v| <[u8; 16]>::try_from(v.as_slice()).ok()).map(u128::from_le_bytes).unwrap_or(0)
}
fn write_total(kind: &str, token: &AccountId, total: u128) {
    if total == 0 {
        env::storage_remove(&total_key(kind, token));
    } else {
        env::storage_write(&total_key(kind, token), &total.to_le_bytes());
    }
}
fn owed_total(token: &AccountId) -> u128 {
    read_total("owed_total", token)
}
fn set_owed_total(token: &AccountId, total: u128) {
    write_total("owed_total", token, total);
}
fn ledger_total(token: &AccountId) -> u128 {
    read_total("ledger_total", token)
}
fn set_ledger_total(token: &AccountId, total: u128) {
    write_total("ledger_total", token, total);
}

/// Users with a step Rhea carried out that this contract could not measure (v1.5): until `settle`
/// runs, nothing new starts (deposits come back, no opens or agent moves); every way out works.
const UNSETTLED: &[u8] = b"unsettled";
fn unsettled() -> Vec<Who> {
    env::storage_read(UNSETTLED).and_then(|v| near_sdk::serde_json::from_slice(&v).ok()).unwrap_or_default()
}
fn set_unsettled(list: &[Who]) {
    if list.is_empty() {
        env::storage_remove(UNSETTLED);
    } else {
        env::storage_write(UNSETTLED, &near_sdk::serde_json::to_vec(list).unwrap_or_default());
    }
}
fn mark_unsettled(user: &Who, why: &str) {
    let mut list = unsettled();
    if !list.contains(user) {
        list.push(user.clone());
    }
    set_unsettled(&list);
    emit("unsettled", json!({ "user": user, "why": why }));
}

/// What a remove_liquidity call came back with (v1.5): the amounts, a refusal, or a success whose
/// answer this code does not recognise (a Rhea upgrade changed it). Read by hand, so an answer in a
/// new shape never makes a callback panic.
pub enum Removed {
    Amounts(u128, u128),
    Refused,
    Unknown,
}
fn json_u128(v: &Value) -> Option<u128> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(u128::from),
        _ => None,
    }
}
pub fn parse_amounts(bytes: &[u8]) -> Option<(u128, u128)> {
    let v: Value = near_sdk::serde_json::from_slice(bytes).ok()?;
    match &v {
        Value::Array(a) if a.len() == 2 => Some((json_u128(&a[0])?, json_u128(&a[1])?)),
        Value::Object(o) => Some((json_u128(o.get("amount_x")?)?, json_u128(o.get("amount_y")?)?)),
        _ => None,
    }
}
fn removed_result() -> Removed {
    match env::promise_result_checked(0, 512) {
        Err(PromiseError::Failed) => Removed::Refused,
        Err(_) => Removed::Unknown,
        Ok(bytes) => parse_amounts(&bytes).map_or(Removed::Unknown, |(x, y)| Removed::Amounts(x, y)),
    }
}
/// What add_liquidity came back with: the new position's id, a refusal, or an id this code cannot read.
pub enum Added {
    Id(String),
    Refused,
    Unknown,
}
pub fn parse_lpt_id(bytes: &[u8]) -> Option<String> {
    let v: Value = near_sdk::serde_json::from_slice(bytes).ok()?;
    let id = match &v {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("lpt_id")?.as_str()?.to_string(),
        _ => return None,
    };
    if id.is_empty() || id.len() > 256 { None } else { Some(id) }
}
fn added_result() -> Added {
    match env::promise_result_checked(0, 1024) {
        Err(PromiseError::Failed) => Added::Refused,
        Err(_) => Added::Unknown,
        Ok(bytes) => parse_lpt_id(&bytes).map_or(Added::Unknown, Added::Id),
    }
}
/// How a close went: credited, closed but not measured (see `settle`), or refused (still open).
#[derive(PartialEq, Debug)]
pub enum Close {
    Closed,
    Unmeasured,
    Refused,
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
        set_ledger_total(token, ledger_total(token) + amount);
    }
    fn debit(&mut self, token: &AccountId, amount: u128) {
        if let Some((_, a)) = self.ledger.iter_mut().find(|(t, _)| t == token) {
            let taken = amount.min(a.0);
            a.0 -= taken;
            set_ledger_total(token, ledger_total(token).saturating_sub(taken));
        }
        self.ledger.retain(|(_, a)| a.0 > 0);
    }
    fn owe(&mut self, token: &AccountId, amount: u128) {
        match self.owed.iter_mut().find(|(t, _)| t == token) {
            Some((_, a)) => a.0 += amount,
            None => self.owed.push((token.clone(), U128(amount))),
        }
    }
    fn owed_of(&self, token: &AccountId) -> u128 {
        self.owed.iter().find(|(t, _)| t == token).map(|(_, a)| a.0).unwrap_or(0)
    }
    /// The pool tokens on the ledger, for cash out (both move side by side).
    fn payout_legs(&self) -> Vec<(AccountId, U128)> {
        let (x, y) = self.pool_tokens();
        [x, y].into_iter().map(|t| (self.held(&t), t)).filter(|(a, _)| *a > 0).map(|(a, t)| (t, U128(a))).collect()
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
    /// v1.5: the marks move only as far as the price held at two reads (`then`, 3 to 60 minutes
    /// before `now`): the high to at most the lower of the two, the low to at least the higher.
    fn note_confirmed(&mut self, then: i32, now: i32) {
        let (held_high, held_low) = (then.min(now), then.max(now));
        self.high_point = Some(self.high_point.map_or(held_high, |h| h.max(held_high)));
        self.low_point = Some(self.low_point.map_or(held_low, |l| l.min(held_low)));
    }
}

#[near]
impl Guardian {
    #[init]
    pub fn new(dcl: AccountId, wnear: AccountId, owner: AccountId) -> Self {
        Self {
            dcl,
            wnear,
            accounts: LookupMap::new(Key::Accounts),
            busy_until: 0,
            users: IterableSet::new(Key::Users),
            owner,
            pending: IterableMap::new(Key::Pending),
            next_pending: 0,
            staged: None,
            seen: LookupMap::new(Key::Seen),
            paused: false,
            take_profit: LookupMap::new(Key::TakeProfit),
        }
    }

    /// Runs in the same receipt as a deploy of this code (if it panics, the deploy is undone too).
    /// Private. From v1.4 (a redeploy): the state as it is. From v1.3 (the upgrade through
    /// `execute`): everything kept (owner, queue, prices, pause), plus an empty take-profit table.
    /// From v1.2: the records, plus `owner`, an empty queue and no prices seen yet.
    /// Every path (v1.5): the per-token totals of what is owed are rebuilt from the records.
    #[private]
    #[init(ignore_state)]
    pub fn migrate(owner: AccountId) -> Self {
        let g = Self::migrated(owner);
        g.seed_owed_totals();
        g
    }

    fn migrated(owner: AccountId) -> Self {
        let raw = env::storage_read(b"STATE").unwrap_or_else(|| env::panic_str("No state to migrate"));
        if let Ok(current) = near_sdk::borsh::from_slice::<Guardian>(&raw) {
            return current;
        }
        if let Ok(v13) = near_sdk::borsh::from_slice::<GuardianV13>(&raw) {
            return Self {
                dcl: v13.dcl,
                wnear: v13.wnear,
                accounts: v13.accounts,
                busy_until: v13.busy_until,
                users: v13.users,
                owner: v13.owner,
                pending: v13.pending,
                next_pending: v13.next_pending,
                staged: v13.staged,
                seen: v13.seen,
                paused: v13.paused,
                take_profit: LookupMap::new(Key::TakeProfit),
            };
        }
        let old: GuardianV12 = near_sdk::borsh::from_slice(&raw).unwrap_or_else(|_| env::panic_str("State is neither v1.2, v1.3 nor v1.4"));
        Self {
            dcl: old.dcl,
            wnear: old.wnear,
            accounts: old.accounts,
            busy_until: old.busy_until,
            users: old.users,
            owner,
            pending: IterableMap::new(Key::Pending),
            next_pending: 0,
            staged: None,
            seen: LookupMap::new(Key::Seen),
            paused: false,
            take_profit: LookupMap::new(Key::TakeProfit),
        }
    }

    // ---------- admin: proposed by the owner, run by anyone after the delay ----------

    /// Stages new code (the raw wasm is this call's whole input, not JSON). Anyone may stage into an
    /// empty slot; staged code is replaced only by whoever staged it or by the owner, and never
    /// while a pending upgrade names it. The attached NEAR must pay the WHOLE code's storage (not a
    /// difference: a replacement must never be paid for by the contract's own balance) and is
    /// returned when the code is deployed, replaced or cleared.
    #[payable]
    pub fn stage_code(&mut self) {
        let code = env::input().unwrap_or_else(|| env::panic_str("Send the wasm as the call's input"));
        if code.len() < 1_000 {
            env::panic_str("That is not a contract");
        }
        let caller = env::predecessor_account_id();
        if let Some(old) = &self.staged {
            if self.upgrade_pending_for(&old.code_hash) {
                env::panic_str("The staged code has a pending upgrade; cancel it first");
            }
            if caller != old.by && caller != self.owner {
                env::panic_str("Code is already staged: only its stager or the owner can replace it");
            }
        }
        // The code itself plus its record in the state, with margin.
        let needed = (code.len() as u128 + 300) * env::storage_byte_cost().as_yoctonear();
        if env::attached_deposit().as_yoctonear() < needed {
            env::panic_str("Attach enough NEAR for the whole code's storage (about 1 NEAR per 100 kB)");
        }
        let previous = self.staged.take();
        env::storage_remove(STAGED_CODE);
        env::storage_write(STAGED_CODE, &code);
        let code_hash = near_sdk::bs58::encode(env::sha256(&code)).into_string();
        self.staged = Some(Staged { code_hash: code_hash.clone(), bytes: code.len() as u64, by: caller.clone(), deposit: env::attached_deposit() });
        if let Some(p) = previous {
            Promise::new(p.by).transfer(p.deposit).detach();
        }
        emit("code_staged", json!({ "code_hash": code_hash, "bytes": code.len(), "by": caller }));
    }

    /// The owner removes staged code no pending upgrade names (e.g. junk someone staged to block
    /// the slot); the stager gets the deposit back. At once: it moves no user funds.
    pub fn clear_staged(&mut self) {
        self.assert_owner();
        let hash = self.staged.as_ref().map(|s| s.code_hash.clone()).unwrap_or_else(|| env::panic_str("Nothing is staged"));
        if self.upgrade_pending_for(&hash) {
            env::panic_str("The staged code has a pending upgrade; cancel it first");
        }
        let staged = self.staged.take().unwrap_or_else(|| env::panic_str("Nothing is staged"));
        env::storage_remove(STAGED_CODE);
        Promise::new(staged.by.clone()).transfer(staged.deposit).detach();
        emit("code_cleared", json!({ "code_hash": staged.code_hash, "by": staged.by }));
    }

    fn upgrade_pending_for(&self, code_hash: &str) -> bool {
        self.pending.values().any(|p| matches!(&p.action, AdminAction::Upgrade { code_hash: h } if h == code_hash))
    }

    /// The owner proposes an admin action. It becomes runnable ADMIN_DELAY_SECS later.
    pub fn propose(&mut self, action: AdminAction) -> u32 {
        self.assert_owner();
        if let AdminAction::Upgrade { code_hash } = &action {
            if self.staged.as_ref().map(|s| &s.code_hash) != Some(code_hash) {
                env::panic_str("Stage that code first: its hash must match the staged code");
            }
        }
        let id = self.next_pending;
        self.next_pending += 1;
        let now = now_secs();
        let pending = Pending { action: action.clone(), proposed_at_secs: now, ready_at_secs: now + ADMIN_DELAY_SECS };
        self.pending.insert(id, pending.clone());
        emit("admin_proposed", json!({ "id": id, "action": action, "ready_at_secs": pending.ready_at_secs }));
        id
    }

    /// The owner withdraws a proposal before it runs.
    pub fn cancel(&mut self, id: u32) {
        self.assert_owner();
        let p = self.pending.remove(&id).unwrap_or_else(|| env::panic_str("No such proposal"));
        emit("admin_cancelled", json!({ "id": id, "action": p.action }));
    }

    /// Anyone runs a proposal whose delay has passed.
    pub fn execute(&mut self, id: u32) -> PromiseOrValue<bool> {
        let p = self.pending.get(&id).cloned().unwrap_or_else(|| env::panic_str("No such proposal"));
        if now_secs() < p.ready_at_secs {
            env::panic_str("Too early: the delay has not passed");
        }
        // Every check before any change.
        if let AdminAction::Upgrade { code_hash } = &p.action {
            if self.staged.as_ref().map(|s| &s.code_hash) != Some(code_hash) {
                env::panic_str("The staged code is not the proposed one");
            }
        }
        if let AdminAction::RecoverPosition { user, .. } = &p.action {
            if self.account(user).position.is_some() {
                env::panic_str("The user already has a position");
            }
        }
        self.pending.remove(&id);
        emit("admin_executed", json!({ "id": id, "action": p.action }));
        match p.action {
            AdminAction::Upgrade { code_hash } => {
                let staged = self.staged.take().unwrap_or_else(|| env::panic_str("No staged code"));
                if staged.code_hash != code_hash {
                    env::panic_str("The staged code is not the proposed one");
                }
                let code = env::storage_read(STAGED_CODE).unwrap_or_else(|| env::panic_str("No staged code"));
                env::storage_remove(STAGED_CODE);
                Promise::new(staged.by).transfer(staged.deposit).detach();
                // The new code's `migrate` takes the owner, so a version that drops or renames it
                // must say so in its own migrate.
                PromiseOrValue::Promise(
                    Promise::new(env::current_account_id())
                        .deploy_contract(code)
                        .function_call("migrate".to_string(), json!({ "owner": self.owner }).to_string().into_bytes(), ZERO, tg(T_MIGRATE)),
                )
            }
            AdminAction::RecoverPosition { user, lpt_id } => {
                if self.account(&user).position.is_some() {
                    env::panic_str("The user already has a position");
                }
                PromiseOrValue::Promise(view(&self.dcl, "get_liquidity", json!({ "lpt_id": lpt_id })).then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).on_recover_checked(user, lpt_id)))
            }
            AdminAction::RecoverCredit { user, token, amount } => {
                let mut acct = self.account(&user);
                let (x, y) = acct.pool_tokens();
                if token != x && token != y {
                    env::panic_str("This token is not in the user's pool");
                }
                acct.credit(&token, amount.0);
                self.put(&user, acct);
                emit("recovered_credit", json!({ "user": user, "token": token, "amount": amount }));
                PromiseOrValue::Value(true)
            }
            AdminAction::SetOwner { owner } => {
                emit("owner_changed", json!({ "from": self.owner, "to": owner }));
                self.owner = owner;
                PromiseOrValue::Value(true)
            }
        }
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
    pub fn join(&mut self, policy: Policy, agent: Option<AccountId>, slot: Option<u8>) {
        self.assert_running();
        let user = Who::mine(slot);
        if self.rec(&user).is_some() {
            env::panic_str("Already joined: use set_policy");
        }
        if env::attached_deposit() < JOIN_DEPOSIT {
            env::panic_str("Attach the join deposit (0.01 NEAR, returned when you leave)");
        }
        policy.validate().unwrap_or_else(|r| refuse(r));
        if policy.allowed_pools.len() != 1 || !pool_id_ok(&policy.allowed_pools[0]) {
            env::panic_str("Policy names exactly one pool, as token_x|token_y|fee");
        }
        let acct = Account { policy, agent, frozen: false, rate: RateWindow::default(), position: None, high_point: None, low_point: None, ledger: Vec::new(), owed: Vec::new() };
        acct.pool_tokens();
        self.put(&user, acct);
        self.users.insert(user.account());
        emit("joined", json!({ "user": user }));
    }

    /// Leaves with nothing left: no position, empty ledger. The join deposit comes back.
    pub fn leave(&mut self, slot: Option<u8>) -> Promise {
        let user = Who::mine(slot);
        let acct = self.account(&user);
        if acct.position.is_some() || !acct.ledger.is_empty() || !acct.owed.is_empty() {
            env::panic_str("Close your position and withdraw first");
        }
        if unsettled().contains(&user) {
            env::panic_str("Run settle first: Rhea holds tokens of yours this contract could not measure");
        }
        // A step in flight could still credit this account.
        self.assert_free();
        self.forget(&user);
        emit("left", json!({ "user": user }));
        Promise::new(user.account()).transfer(JOIN_DEPOSIT)
    }

    pub fn set_policy(&mut self, policy: Policy, slot: Option<u8>) {
        let user = Who::mine(slot);
        let mut acct = self.account(&user);
        policy.validate().unwrap_or_else(|r| refuse(r));
        if policy.allowed_pools != acct.policy.allowed_pools {
            env::panic_str("The pool cannot change: leave and join again");
        }
        acct.policy = policy;
        self.put(&user, acct);
    }

    pub fn set_agent(&mut self, agent: Option<AccountId>, slot: Option<u8>) {
        let user = Who::mine(slot);
        let mut acct = self.account(&user);
        acct.agent = agent;
        self.put(&user, acct);
    }

    pub fn set_frozen(&mut self, frozen: bool, slot: Option<u8>) {
        let user = Who::mine(slot);
        let mut acct = self.account(&user);
        acct.frozen = frozen;
        self.put(&user, acct);
    }

    // ---------- deposit: one token transfer, measured ----------

    /// A pool token arrives for `sender_id`: forwarded onto this contract's Rhea balance and credited
    /// to their ledger with what that balance actually grew by (v1.5). `msg` must be empty: to place
    /// the tokens, call `open` next (the page sends both in one wallet request). While another step
    /// runs, or while paused, the whole transfer goes back to the sender at once.
    pub fn ft_on_transfer(&mut self, sender_id: AccountId, amount: U128, msg: String) -> PromiseOrValue<U128> {
        let token = env::predecessor_account_id();
        // The msg names the position: empty for the first, "1" or "2" for the others (v1.6).
        let slot = match msg.trim() {
            "" => 0,
            "1" => 1,
            "2" => 2,
            _ => env::panic_str("Deposit with an empty msg, or 1 or 2 for your other positions, then call open: a deposit no longer opens a range itself (v1.5)"),
        };
        let sender_id = Who::of(&sender_id, slot);
        let acct = self.account(&sender_id);
        let (x, y) = acct.pool_tokens();
        if token != x && token != y {
            env::panic_str("This token is not in your pool");
        }
        if self.paused {
            emit("paused_refund", json!({ "user": sender_id, "token": token, "amount": amount }));
            return PromiseOrValue::Value(amount);
        }
        if self.is_busy() {
            emit("busy", json!({ "user": sender_id, "token": token, "amount": amount }));
            return PromiseOrValue::Value(amount);
        }
        if !unsettled().is_empty() {
            emit("unsettled_refund", json!({ "user": sender_id, "token": token, "amount": amount }));
            return PromiseOrValue::Value(amount);
        }
        self.lock();
        PromiseOrValue::Promise(
            view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_DEP_BEFORE)).with_unused_gas_weight(2).on_deposit_before(sender_id, token, amount)),
        )
    }

    /// The Rhea balance before the forward is known: forward the tokens.
    #[private]
    pub fn on_deposit_before(&mut self, user: Who, token: AccountId, amount: U128, #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> PromiseOrValue<U128> {
        let Ok(assets) = assets else {
            // Nothing was forwarded: the whole transfer goes back.
            emit("deposit_refused", json!({ "user": user, "token": token, "amount": amount, "why": "could not read the balance on Rhea DCL" }));
            self.unlock();
            return PromiseOrValue::Value(amount);
        };
        let before = assets.get(&token).map(|a| a.0).unwrap_or(0);
        PromiseOrValue::Promise(
            call(&token, "ft_transfer_call", json!({ "receiver_id": self.dcl, "amount": amount, "msg": "\"Deposit\"" }), ONE_YOCTO, tg(T_FT_CALL))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_FORWARDED)).with_unused_gas_weight(2).on_forwarded(user, token, amount, U128(before))),
        )
    }

    /// The token reports what Rhea took; read the Rhea balance again before crediting anything.
    #[private]
    pub fn on_forwarded(&mut self, user: Who, token: AccountId, amount: U128, before: U128, #[callback_result] used: Result<U128, PromiseError>) -> PromiseOrValue<U128> {
        let reported = used.map(|u| u.0).unwrap_or(0).min(amount.0);
        if reported == 0 {
            emit("deposited", json!({ "user": user, "token": token, "amount": U128(0), "returned": amount }));
            self.unlock();
            return PromiseOrValue::Value(amount);
        }
        PromiseOrValue::Promise(
            view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_DEP_MEASURED)).with_unused_gas_weight(2).on_deposit_measured(user, token, amount, before, U128(reported))),
        )
    }

    /// Credits the smaller of what the Rhea balance grew by and what the token reported; returns to
    /// the sender what the token says came back.
    #[private]
    pub fn on_deposit_measured(&mut self, user: Who, token: AccountId, amount: U128, before: U128, reported: U128, #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> U128 {
        let mut acct = self.account(&user);
        match assets {
            Ok(after) => {
                let grew = after.get(&token).map(|a| a.0).unwrap_or(0).saturating_sub(before.0);
                let taken = grew.min(reported.0);
                if taken < reported.0 {
                    emit("deposit_short", json!({ "user": user, "token": token, "reported": reported, "measured": U128(grew) }));
                }
                acct.credit(&token, taken);
                emit("deposited", json!({ "user": user, "token": token, "amount": U128(taken), "returned": U128(amount.0 - reported.0) }));
            }
            // Could not read the balance: nothing is trusted from the token; settle credits what Rhea got.
            Err(_) => mark_unsettled(&user, "the deposit reached Rhea but could not be measured"),
        }
        self.put(&user, acct);
        self.unlock();
        U128(amount.0 - reported.0)
    }

    // ---------- the position ----------

    /// The user places (or moves) their position: any range in their pool.
    pub fn open(&mut self, left_point: i32, right_point: i32, slot: Option<u8>) -> Promise {
        self.assert_running();
        self.assert_settled();
        let user = Who::mine(slot);
        let pool_id = self.account(&user).pool_id().to_string();
        if left_point >= right_point {
            refuse(Refusal::BadRange);
        }
        self.assert_free();
        self.lock();
        self.begin_move(user, RebalanceRequest { pool_id, left_point, right_point }, false)
    }

    /// The user's agent asks for a new range; checked against the user's policy on-chain.
    pub fn request_rebalance(&mut self, user: Who, request: RebalanceRequest) -> Promise {
        self.assert_running();
        self.assert_settled();
        let acct = self.account(&user);
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
        // Only checked here; an action is counted once it passes every check (on_pool_for_move), so
        // a compromised agent cannot use up the user's daily actions with requests that are refused.
        acct.rate.clone().take(&acct.policy, now_secs()).unwrap_or_else(|r| refuse(r));
        self.lock();
        self.begin_move(user, request, true)
    }

    /// Records the price for the exit rule (its high, or its low with exit_on_rise), so the rule
    /// measures from the best point seen, not only from the last move. The agent or the user; no
    /// lock (it only writes the marks).
    pub fn poke(&mut self, user: Who) -> Promise {
        let acct = self.account(&user);
        let caller = env::predecessor_account_id();
        if caller != user.account() {
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
    pub fn on_poked(&mut self, user: Who, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> bool {
        let Ok(Some(pool)) = pool else { return false };
        let Some(mut acct) = self.rec(&user) else { return false };
        if acct.position.is_none() {
            return false;
        }
        let now = now_secs();
        let cur = pool.current_point;
        let prev = self.seen_of(&user);
        // v1.5: a read younger than SEEN_MIN_AGE_SECS stays as it is, so quick re-pokes neither
        // restart the confirmation clock nor move the marks.
        if prev.as_ref().is_some_and(|p| now < p.at_secs.saturating_add(SEEN_MIN_AGE_SECS)) {
            return false;
        }
        // The best price moves only as far as the price held at this read and one 3–60 min earlier.
        if let Some(p) = prev.filter(|p| check_seen_age(Some(p.at_secs), now).is_ok()) {
            acct.note_confirmed(p.point, cur);
        }
        emit("point_noted", json!({ "user": user, "current_point": cur, "high_point": acct.high_point, "low_point": acct.low_point }));
        self.put(&user, acct);
        self.put_seen(&user, Seen { point: cur, at_secs: now });
        true
    }

    /// Reads the pool; the checks and the move follow in the callback. The caller holds the lock.
    fn begin_move(&mut self, user: Who, request: RebalanceRequest, by_agent: bool) -> Promise {
        let held = self.account(&user).position.is_some();
        view(&self.dcl, "get_pool", json!({ "pool_id": request.pool_id }))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(if held { CB_POOL_HELD } else { CB_POOL_NEW })).with_unused_gas_weight(2).on_pool_for_move(user, request, by_agent))
    }

    #[private]
    pub fn on_pool_for_move(&mut self, user: Who, request: RebalanceRequest, by_agent: bool, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> PromiseOrValue<bool> {
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
            // v1.3: the same checks must already have passed at a price read 3–60 minutes earlier,
            // so one block's pushed price cannot move the user.
            let seen = self.seen_of(&user);
            if let Err(r) = check_seen_age(seen.as_ref().map(|s| s.at_secs), now_secs()) {
                return self.bail(&user, r.message());
            }
            let then = seen.map(|s| s.point).unwrap_or(pool.current_point);
            let confirmed = check_rebalance(&acct.policy, &request, then).is_ok()
                && acct.position.as_ref().is_none_or(|open| check_out_of_range(&acct.policy, open.left_point, open.right_point, then).is_ok());
            if !confirmed {
                return self.bail(&user, Refusal::NotConfirmed.message());
            }
            // Approved: now it counts against the user's actions per window (checked again here,
            // the lock keeps anything else from counting in between).
            let policy = acct.policy.clone();
            if let Err(r) = acct.rate.take(&policy, now_secs()) {
                return self.bail(&user, r.message());
            }
            // A fresh read is needed before the next action.
            self.del_seen(&user);
        }
        acct.note_point(pool.current_point);
        self.put(&user, acct.clone());
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
    pub fn on_liquidity_for_move(&mut self, user: Who, request: RebalanceRequest, pool: PoolView, #[callback_result] liq: Result<Option<LiquidityView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(liq)) = liq else { return self.bail(&user, "Could not read the open position from Rhea DCL") };
        let Some(old) = self.account(&user).position else { return self.bail(&user, "No open position") };
        PromiseOrValue::Promise(
            call(&self.dcl, "remove_liquidity", remove_args(&old.lpt_id, liq.amount), ZERO, tg(T_REMOVE))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_REMOVED_ADD)).with_unused_gas_weight(2).on_removed_then_add(user, request, pool)),
        )
    }

    #[private]
    pub fn on_removed_then_add(&mut self, user: Who, request: RebalanceRequest, pool: PoolView) -> PromiseOrValue<bool> {
        match self.credit_removed(&user, &pool, removed_result()) {
            Close::Refused => self.bail(&user, "Rhea DCL refused to close the position; it stays open"),
            Close::Unmeasured => self.bail(&user, "The old range closed but Rhea's answer could not be read: run settle, then open again"),
            Close::Closed => self.snapshot_then_add(user, request, pool),
        }
    }

    /// Reads this contract's Rhea balance right before the add (the ledger takes the difference).
    fn snapshot_then_add(&mut self, user: Who, request: RebalanceRequest, pool: PoolView) -> PromiseOrValue<bool> {
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
    pub fn on_snapshot_then_add(&mut self, user: Who, request: RebalanceRequest, pool: PoolView, #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(assets) = assets else { return self.bail(&user, "Could not read the balances on Rhea DCL") };
        let before_x = assets.get(&pool.token_x).map(|a| a.0).unwrap_or(0);
        let before_y = assets.get(&pool.token_y).map(|a| a.0).unwrap_or(0);
        let mut acct = self.account(&user);
        let (x, y) = (acct.held(&pool.token_x), acct.held(&pool.token_y));
        let (min_x, min_y) = min_amounts(request.left_point, request.right_point, pool.current_point, x, y, acct.policy.max_slippage_bps);
        // The ledger lends the position everything; on_measured credits back what the add left.
        acct.debit(&pool.token_x, x);
        acct.debit(&pool.token_y, y);
        self.put(&user, acct);
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
    pub fn on_added(&mut self, user: Who, request: RebalanceRequest, pool: PoolView, lent_before: [U128; 4]) -> PromiseOrValue<bool> {
        self.added(user, request, pool, lent_before, added_result())
    }

    fn added(&mut self, user: Who, request: RebalanceRequest, pool: PoolView, lent_before: [U128; 4], added: Added) -> PromiseOrValue<bool> {
        let [lent_x, lent_y, _, _] = lent_before;
        if let Added::Unknown = added {
            // Rhea opened a position whose id this code cannot read: the leftovers are still measured
            // back onto the ledger; the position itself needs RecoverPosition (reported).
            emit("position_unrecorded", json!({ "user": user, "pool_id": request.pool_id, "left_point": request.left_point, "right_point": request.right_point }));
            return PromiseOrValue::Promise(
                view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
                    .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_measured(user, pool, lent_before)),
            );
        }
        let Added::Id(lpt_id) = added else {
            // Rhea refused: nothing left the contract's Rhea balance; the tokens are the user's again.
            let mut acct = self.account(&user);
            acct.credit(&pool.token_x, lent_x.0);
            acct.credit(&pool.token_y, lent_y.0);
            self.put(&user, acct);
            return self.bail(&user, "Rhea DCL refused the new position (price moved past the slippage?); your tokens stay on your ledger");
        };
        let mut acct = self.account(&user);
        acct.position = Some(PositionRef { lpt_id: lpt_id.clone(), pool_id: request.pool_id.clone(), left_point: request.left_point, right_point: request.right_point });
        self.put(&user, acct);
        emit("position_opened", json!({ "user": user, "lpt_id": lpt_id, "left_point": request.left_point, "right_point": request.right_point }));
        PromiseOrValue::Promise(
            view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_measured(user, pool, lent_before)),
        )
    }

    /// Credits back exactly what the add did not take: lent − (balance before − balance after).
    #[private]
    pub fn on_measured(&mut self, user: Who, pool: PoolView, lent_before: [U128; 4], #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> bool {
        let [lent_x, lent_y, before_x, before_y] = lent_before;
        let mut acct = self.account(&user);
        match assets {
            Ok(after) => {
                let after_x = after.get(&pool.token_x).map(|a| a.0).unwrap_or(0);
                let after_y = after.get(&pool.token_y).map(|a| a.0).unwrap_or(0);
                acct.credit(&pool.token_x, leftover(lent_x.0, before_x.0, after_x));
                acct.credit(&pool.token_y, leftover(lent_y.0, before_y.0, after_y));
            }
            // Could not read the balance: nothing is guessed; settle credits what the add left.
            Err(_) => mark_unsettled(&user, "the add's leftover could not be measured"),
        }
        self.put(&user, acct);
        self.unlock();
        true
    }

    // ---------- exit and withdraw: the user's way out, no agent needed ----------

    /// Closes the position: the tokens land on the user's ledger. The user may always; the agent
    /// only when one of the user's own rules is met: the exit rule (a fall from the best price) or
    /// take profit (v1.4: the price reached the user's target).
    pub fn exit(&mut self, user: Who) -> Promise {
        let caller = env::predecessor_account_id();
        let acct = self.account(&user);
        let by_user = caller == user.account();
        if !by_user {
            if acct.frozen {
                refuse(Refusal::Frozen);
            }
            if acct.agent.as_ref() != Some(&caller) {
                refuse(Refusal::NotAgent);
            }
            if acct.policy.exit_drop_points == 0 && self.tp_of(&user).is_none() {
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
    pub fn on_pool_for_exit(&mut self, user: Who, by_user: bool, #[callback_result] pool: Result<Option<PoolView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(pool)) = pool else { return self.bail(&user, "Could not read the pool from Rhea DCL") };
        let acct = self.account(&user);
        let cur = pool.current_point;
        let mut reason = "user";
        if !by_user {
            // Which of the user's rules is met: the exit rule, or take profit (v1.4).
            let target = self.tp_of(&user);
            let stop_at = |point: i32| acct.policy.exit_drop_points > 0 && moved_against(&acct.policy, acct.high_point, acct.low_point, point) >= i64::from(acct.policy.exit_drop_points);
            let profit_at = |point: i32| target.as_ref().is_some_and(|t| take_profit_met(t, point));
            if !stop_at(cur) && !profit_at(cur) {
                return self.bail(&user, "Exit rule not met: the price has not moved far enough against the user, nor reached the take-profit target");
            }
            // v1.3: the same rule must already have been met at a price read 3–60 minutes earlier.
            let seen = self.seen_of(&user);
            if let Err(r) = check_seen_age(seen.as_ref().map(|s| s.at_secs), now_secs()) {
                return self.bail(&user, r.message());
            }
            let then = seen.map(|s| s.point).unwrap_or(cur);
            reason = if stop_at(cur) && stop_at(then) {
                "exit_rule"
            } else if profit_at(cur) && profit_at(then) {
                "take_profit"
            } else {
                return self.bail(&user, Refusal::NotConfirmed.message());
            };
            self.del_seen(&user);
        }
        let Some(pos) = acct.position else { return self.bail(&user, "No open position") };
        emit("exit_started", json!({ "user": user, "by": if by_user { "user" } else { "agent" }, "reason": reason, "lpt_id": pos.lpt_id, "current_point": cur }));
        PromiseOrValue::Promise(
            view(&self.dcl, "get_liquidity", json!({ "lpt_id": pos.lpt_id }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_LIQ_EXIT)).with_unused_gas_weight(2).on_liquidity_for_exit(user, pool)),
        )
    }

    #[private]
    pub fn on_liquidity_for_exit(&mut self, user: Who, pool: PoolView, #[callback_result] liq: Result<Option<LiquidityView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(liq)) = liq else { return self.bail(&user, "Could not read the open position from Rhea DCL") };
        let Some(pos) = self.account(&user).position else { return self.bail(&user, "No open position") };
        PromiseOrValue::Promise(
            call(&self.dcl, "remove_liquidity", remove_args(&pos.lpt_id, liq.amount), ZERO, tg(T_REMOVE))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_exited(user, pool)),
        )
    }

    #[private]
    pub fn on_exited(&mut self, user: Who, pool: PoolView) -> bool {
        self.exited(user, pool, removed_result())
    }

    fn exited(&mut self, user: Who, pool: PoolView, removed: Removed) -> bool {
        let close = self.credit_removed(&user, &pool, removed);
        if close == Close::Refused {
            let _ = self.bail(&user, "Rhea DCL refused to close the position; it stays open");
            return false;
        }
        // The position it was set for is closed: a take-profit target never outlives it.
        self.del_tp(&user);
        self.unlock();
        close == Close::Closed
    }

    /// v1.4, the user only: the target at which the agent may close the position for profit,
    /// with its direction (above: the point rising to it). None removes it. Allowed while paused:
    /// it only ever leads to a way out.
    /// The user takes the fees their position earned without closing it (v1.5): Rhea's
    /// remove_liquidity with amount 0 moves only the fees onto this contract's Rhea balance, and they
    /// are credited to the user's ledger. With `send`, they go on to the wallet in the same
    /// transaction (measured like every payout). The position, its range, the exit rule's marks and
    /// the take-profit target stay as they are. The user only; works while paused.
    pub fn claim_fees(&mut self, send: bool, slot: Option<u8>) -> Promise {
        let user = Who::mine(slot);
        let Some(pos) = self.account(&user).position else { env::panic_str("No open position") };
        self.assert_free();
        self.lock();
        emit("fees_claim_started", json!({ "user": user, "lpt_id": pos.lpt_id, "send": send }));
        call(&self.dcl, "remove_liquidity", remove_args(&pos.lpt_id, U128(0)), ZERO, tg(T_REMOVE))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_FEES)).with_unused_gas_weight(2).on_fees_claimed(user, send))
    }

    #[private]
    pub fn on_fees_claimed(&mut self, user: Who, send: bool) -> PromiseOrValue<bool> {
        self.fees_claimed(user, send, removed_result())
    }

    pub fn set_take_profit(&mut self, target: Option<TakeProfit>, slot: Option<u8>) {
        let user = Who::mine(slot);
        let acct = self.account(&user);
        match &target {
            Some(t) => {
                if acct.position.is_none() {
                    env::panic_str("Open a position first: take profit closes it");
                }
                self.put_tp(&user, t.clone());
            }
            None => {
                self.del_tp(&user);
            }
        }
        emit("take_profit_set", json!({ "user": user, "target": target }));
    }

    pub fn get_take_profit(&self, user: Who) -> Option<TakeProfit> {
        self.tp_of(&user)
    }

    /// Sends the user `amount` (or all) of a pool token from their ledger: Rhea pays this contract,
    /// this contract pays the user what arrived (v1.5: a tax token keeps part of it on the way out
    /// of Rhea). wNEAR arrives as NEAR. Needs no agent and no policy.
    pub fn withdraw(&mut self, token: AccountId, amount: Option<U128>, slot: Option<u8>) -> PromiseOrValue<bool> {
        let user = Who::mine(slot);
        let mut acct = self.account(&user);
        let held = acct.held(&token);
        let amount = amount.map(|a| a.0).unwrap_or(held);
        if amount == 0 || amount > held {
            env::panic_str("Nothing to withdraw, or more than you hold");
        }
        self.assert_free();
        self.lock();
        acct.debit(&token, amount);
        self.put(&user, acct);
        self.pay_out(user, vec![(token, U128(amount))], None)
    }

    /// Pays what a refused payout left owed, from this contract's own token wallet (Rhea is not
    /// involved). v1.5: reads that wallet first and pays at most what it holds beyond what it owes
    /// others; the claim clears the debt either way (a shortfall is reported), so nothing owed can
    /// keep a user from leaving. A second refusal keeps what was sent owed.
    pub fn claim(&mut self, token: AccountId, slot: Option<u8>) -> Promise {
        let user = Who::mine(slot);
        let acct = self.account(&user);
        // Locked so the user cannot `leave` before a second refusal is written back.
        self.assert_free();
        if acct.owed_of(&token) == 0 {
            env::panic_str("Nothing owed in this token");
        }
        self.lock();
        view(&token, "ft_balance_of", json!({ "account_id": env::current_account_id() }))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_CLAIM_MEASURED)).with_unused_gas_weight(2).on_claim_measured(user, token))
    }

    #[private]
    pub fn on_claim_measured(&mut self, user: Who, token: AccountId, #[callback_result] balance: Result<U128, PromiseError>) -> PromiseOrValue<bool> {
        self.claim_measured(user, token, balance.ok().map(|b| b.0))
    }

    #[private]
    pub fn on_claimed(&mut self, user: Who, token: AccountId, amount: U128, #[callback_result] sent: Result<(), PromiseError>) -> bool {
        self.unlock();
        self.settle_payout(&user, &token, amount, sent.is_ok())
    }

    // ---------- payouts (v1.5): every token measured where it lands ----------

    /// After the withdrawals: each token Rhea released is read where it landed; one it kept goes
    /// back on the ledger.
    #[private]
    pub fn on_out_withdrawn(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>) -> PromiseOrValue<bool> {
        let released = (0..legs.len()).map(|i| match env::promise_result_checked(i as u64, 16) {
            Ok(v) => v != b"false",
            Err(PromiseError::Failed) => false,
            // Succeeded with a longer answer than a bool.
            Err(_) => true,
        });
        let released = released.collect();
        self.out_withdrawn(user, legs, cash, released)
    }

    /// After the reads: each user is paid what arrived.
    #[private]
    pub fn on_out_arrived(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>) -> PromiseOrValue<bool> {
        let balances = (0..legs.len()).map(|i| env::promise_result_checked(i as u64, 64).ok().and_then(|v| near_sdk::serde_json::from_slice::<U128>(&v).ok()).map(|b| b.0));
        let balances = balances.collect();
        self.out_arrived(user, legs, cash, balances)
    }

    /// After the transfers: a refused one is owed (see `claim`).
    #[private]
    pub fn on_out_paid(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>) -> PromiseOrValue<bool> {
        let sent = (0..legs.len()).map(|i| !matches!(env::promise_result_checked(i as u64, 0), Err(PromiseError::Failed))).collect();
        self.out_paid(user, legs, cash, sent)
    }

    // ---------- cash out: one approval for the whole way out ----------

    /// Closes the position (if open), sends every pool token on the ledger to the wallet (wNEAR
    /// arrives as NEAR) and, with `leave`, removes the record and returns the join deposit. The
    /// user only. A step that fails leaves the tokens on the ledger, where `exit` and `withdraw`
    /// (which forward all unused gas) still reach them.
    pub fn cash_out(&mut self, leave: bool, slot: Option<u8>) -> PromiseOrValue<bool> {
        let user = Who::mine(slot);
        let acct = self.account(&user);
        self.assert_free();
        if acct.position.is_none() && acct.payout_legs().is_empty() {
            if !leave {
                env::panic_str("Nothing to cash out");
            }
            return PromiseOrValue::Promise(self.leave(slot));
        }
        self.lock();
        emit("cash_out_started", json!({ "user": user, "leave": leave }));
        let Some(pos) = acct.position else { return self.pay_all(user, leave) };
        PromiseOrValue::Promise(
            view(&self.dcl, "get_liquidity", json!({ "lpt_id": pos.lpt_id }))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_LIQ_CASH)).with_unused_gas_weight(1).on_liquidity_for_cash_out(user, leave)),
        )
    }

    #[private]
    pub fn on_liquidity_for_cash_out(&mut self, user: Who, leave: bool, #[callback_result] liq: Result<Option<LiquidityView>, PromiseError>) -> PromiseOrValue<bool> {
        let Ok(Some(liq)) = liq else { return self.bail(&user, "Could not read the open position from Rhea DCL") };
        let Some(pos) = self.account(&user).position else { return self.bail(&user, "No open position") };
        PromiseOrValue::Promise(
            call_fixed(&self.dcl, "remove_liquidity", remove_args(&pos.lpt_id, liq.amount), ZERO, tg(T_REMOVE))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_CASH_CLOSED)).with_unused_gas_weight(1).on_cash_out_closed(user, leave)),
        )
    }

    #[private]
    pub fn on_cash_out_closed(&mut self, user: Who, leave: bool) -> PromiseOrValue<bool> {
        self.cash_out_closed(user, leave, removed_result())
    }

    fn cash_out_closed(&mut self, user: Who, leave: bool, removed: Removed) -> PromiseOrValue<bool> {
        let (token_x, token_y) = self.account(&user).pool_tokens();
        let pool = PoolView { token_x, token_y, current_point: 0 };
        match self.credit_removed(&user, &pool, removed) {
            Close::Refused => self.bail(&user, "Rhea DCL refused to close the position; it stays open"),
            Close::Unmeasured => {
                self.del_tp(&user);
                self.bail(&user, "The position closed but Rhea's answer could not be read: run settle, then cash out again")
            }
            Close::Closed => {
                // The position closed: its take-profit target goes with it.
                self.del_tp(&user);
                self.pay_all(user, leave)
            }
        }
    }

    // ---------- recovery: an admin action (owner proposes, delay, anyone runs) ----------

    /// The end of `AdminAction::RecoverPosition`: Rhea says who owns the position and where it sits;
    /// it must be this contract's, in the user's pool, and the user must have no position.
    #[private]
    pub fn on_recover_checked(&mut self, user: Who, lpt_id: String, #[callback_result] found: Result<Option<RheaPosition>, PromiseError>) -> bool {
        let Ok(Some(l)) = found else {
            emit("recover_refused", json!({ "user": user, "lpt_id": lpt_id, "why": "Rhea has no such position" }));
            return false;
        };
        let Some(mut acct) = self.rec(&user) else { return false };
        if l.owner_id != env::current_account_id() || l.pool_id != acct.pool_id() || acct.position.is_some() {
            emit("recover_refused", json!({ "user": user, "lpt_id": lpt_id, "why": "not this contract's, not the user's pool, or the user has a position" }));
            return false;
        }
        acct.position = Some(PositionRef { lpt_id: lpt_id.clone(), pool_id: l.pool_id, left_point: l.left_point, right_point: l.right_point });
        self.put(&user, acct);
        emit("recovered_position", json!({ "user": user, "lpt_id": lpt_id, "left_point": l.left_point, "right_point": l.right_point }));
        true
    }

    /// Emergency pause: the owner stops new risk at once (see `paused`); resuming is at once too.
    /// It only takes powers away and never moves funds, so it needs no delay.
    pub fn set_paused(&mut self, paused: bool) {
        self.assert_owner();
        if self.paused != paused {
            self.paused = paused;
            emit(if paused { "paused" } else { "resumed" }, json!({ "by": env::predecessor_account_id() }));
        }
    }

    fn assert_settled(&self) {
        if !unsettled().is_empty() {
            env::panic_str("A step could not be measured: run settle first (anyone can); ways out still work");
        }
    }

    fn assert_running(&self) {
        if self.paused {
            env::panic_str("Guardian is paused: nothing new can start; every way out still works");
        }
    }

    fn assert_owner(&self) {
        if env::predecessor_account_id() != self.owner {
            env::panic_str("Only the owner can propose, cancel or pause");
        }
    }

    // ---------- views ----------

    /// Accounts with a record, `limit` (at most 100) from `from_index`.
    pub fn get_users(&self, from_index: Option<u32>, limit: Option<u32>) -> Vec<AccountId> {
        self.users.iter().skip(from_index.unwrap_or(0) as usize).take(limit.unwrap_or(100).min(100) as usize).cloned().collect()
    }

    /// The positions this wallet has records for (v1.6).
    pub fn get_slots(&self, user: AccountId) -> Vec<u8> {
        (0..MAX_SLOTS).filter(|s| self.rec(&Who::of(&user, *s)).is_some()).collect()
    }

    pub fn get_user_count(&self) -> u32 {
        self.users.len()
    }

    pub fn get_account(&self, user: Who) -> Option<Account> {
        self.rec(&user)
    }

    pub fn get_config(&self) -> Value {
        json!({
            "version": VERSION,
            "dcl": self.dcl,
            "wnear": self.wnear,
            "join_deposit": JOIN_DEPOSIT,
            "busy": self.is_busy(),
            "users": self.users.len(),
            "owner": self.owner,
            "admin_delay_secs": ADMIN_DELAY_SECS,
            "pending": self.pending.len(),
            "staged": self.staged,
            "seen_age_secs": [SEEN_MIN_AGE_SECS, SEEN_MAX_AGE_SECS],
            "paused": self.paused,
            "unsettled": unsettled()
        })
    }

    /// Admin actions waiting out (or past) their delay, by id.
    pub fn get_pending(&self) -> Vec<(u32, Pending)> {
        self.pending.iter().map(|(id, p)| (*id, p.clone())).collect()
    }

    /// The last price `poke` read for a user's pool.
    pub fn get_seen(&self, user: Who) -> Option<Seen> {
        self.seen_of(&user)
    }

    /// What all ledgers hold of `token` on this contract's Rhea balance (v1.5).
    pub fn get_ledger_total(&self, token: AccountId) -> U128 {
        U128(ledger_total(&token))
    }

    /// Users a step could not measure for, waiting for `settle` (v1.5).
    pub fn get_unsettled(&self) -> Vec<Who> {
        unsettled()
    }

    /// What this contract owes users in `token`, held in its own token wallet (v1.5).
    pub fn get_owed_total(&self, token: AccountId) -> U128 {
        U128(owed_total(&token))
    }

    // ---------- helpers ----------

    // Slot 0 lives where it always has; slots 1 and 2 beside the state, under raw keys (v1.6).
    fn raw_key(kind: &str, who: &Who) -> Vec<u8> {
        format!("{kind}:{}", who.0).into_bytes()
    }

    fn raw_read<T: near_sdk::borsh::BorshDeserialize>(kind: &str, who: &Who) -> Option<T> {
        // Unreadable must stop, not read as "no record": a new join would then write over the old one.
        env::storage_read(&Self::raw_key(kind, who)).map(|v| near_sdk::borsh::from_slice(&v).unwrap_or_else(|_| env::panic_str("A stored position could not be read")))
    }

    fn raw_write<T: near_sdk::borsh::BorshSerialize>(kind: &str, who: &Who, value: &T) {
        env::storage_write(&Self::raw_key(kind, who), &near_sdk::borsh::to_vec(value).unwrap_or_else(|_| env::panic_str("Could not store the record")));
    }

    fn rec(&self, who: &Who) -> Option<Account> {
        match who.parts()? {
            (account, 0) => self.accounts.get(&account).cloned(),
            _ => Self::raw_read("s", who),
        }
    }

    fn put(&mut self, who: &Who, acct: Account) {
        match who.parts() {
            Some((account, 0)) => {
                self.accounts.insert(account, acct);
            }
            Some(_) => Self::raw_write("s", who, &acct),
            None => env::panic_str("Not a position"),
        }
    }

    fn seen_of(&self, who: &Who) -> Option<Seen> {
        match who.parts()? {
            (account, 0) => self.seen.get(&account).cloned(),
            _ => Self::raw_read("s_seen", who),
        }
    }

    fn put_seen(&mut self, who: &Who, seen: Seen) {
        match who.parts() {
            Some((account, 0)) => {
                self.seen.insert(account, seen);
            }
            Some(_) => Self::raw_write("s_seen", who, &seen),
            None => env::panic_str("Not a position"),
        }
    }

    fn del_seen(&mut self, who: &Who) {
        match who.parts() {
            Some((account, 0)) => {
                self.seen.remove(&account);
            }
            Some(_) => {
                env::storage_remove(&Self::raw_key("s_seen", who));
            }
            None => {}
        }
    }

    fn tp_of(&self, who: &Who) -> Option<TakeProfit> {
        match who.parts()? {
            (account, 0) => self.take_profit.get(&account).cloned(),
            _ => Self::raw_read("s_tp", who),
        }
    }

    fn put_tp(&mut self, who: &Who, target: TakeProfit) {
        match who.parts() {
            Some((account, 0)) => {
                self.take_profit.insert(account, target);
            }
            Some(_) => Self::raw_write("s_tp", who, &target),
            None => env::panic_str("Not a position"),
        }
    }

    fn del_tp(&mut self, who: &Who) {
        match who.parts() {
            Some((account, 0)) => {
                self.take_profit.remove(&account);
            }
            Some(_) => {
                env::storage_remove(&Self::raw_key("s_tp", who));
            }
            None => {}
        }
    }

    /// Removes one position's record; the wallet leaves `users` with its last one.
    fn forget(&mut self, who: &Who) {
        let Some((account, slot)) = who.parts() else { return };
        if slot == 0 {
            self.accounts.remove(&account);
        } else {
            env::storage_remove(&Self::raw_key("s", who));
        }
        self.del_seen(who);
        self.del_tp(who);
        if self.get_slots(account.clone()).is_empty() {
            self.users.remove(&account);
        }
    }

    fn account(&self, user: &Who) -> Account {
        self.rec(user).unwrap_or_else(|| env::panic_str("This account has not joined"))
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
    fn bail(&mut self, user: &Who, why: &str) -> PromiseOrValue<bool> {
        self.unlock();
        emit("refused", json!({ "user": user, "why": why }));
        PromiseOrValue::Value(false)
    }

    /// The end of one payout: sent, or refused and owed to the user (see `claim`). What is owed stays
    /// in this contract's own token wallet and is counted in its `owed_total`.
    fn settle_payout(&mut self, user: &Who, token: &AccountId, amount: U128, sent: bool) -> bool {
        if sent {
            emit("withdrawn", json!({ "user": user, "token": token, "amount": amount }));
            return true;
        }
        let mut acct = self.account(user);
        acct.owe(token, amount.0);
        self.put(&user, acct);
        set_owed_total(token, owed_total(token) + amount.0);
        emit("payout_refused", json!({ "user": user, "token": token, "amount": amount, "why": "Your wallet refused the token (register on it), use claim" }));
        false
    }

    /// The fees Rhea moved onto this contract's balance are the user's; with `send`, they go out.
    fn fees_claimed(&mut self, user: Who, send: bool, fees: Removed) -> PromiseOrValue<bool> {
        let (fx, fy) = match fees {
            Removed::Amounts(x, y) => (U128(x), U128(y)),
            Removed::Refused => return self.bail(&user, "Rhea DCL did not release the fees; the position is unchanged"),
            Removed::Unknown => {
                mark_unsettled(&user, "Rhea released the fees but its answer could not be read");
                return self.bail(&user, "The fees left the position but Rhea's answer could not be read: run settle");
            }
        };
        let mut acct = self.account(&user);
        let (x, y) = acct.pool_tokens();
        acct.credit(&x, fx.0);
        acct.credit(&y, fy.0);
        emit("fees_claimed", json!({ "user": user, "lpt_id": acct.position.as_ref().map(|p| &p.lpt_id), "amount_x": fx, "amount_y": fy }));
        let legs: Vec<(AccountId, U128)> = [(x, fx), (y, fy)].into_iter().filter(|(_, a)| send && a.0 > 0).collect();
        for (token, amount) in &legs {
            acct.debit(token, amount.0);
        }
        self.put(&user, acct);
        if legs.is_empty() {
            self.unlock();
            return PromiseOrValue::Value(true);
        }
        self.pay_out(user, legs, None)
    }

    /// Cash out's payout: every pool token on the ledger, side by side.
    fn pay_all(&mut self, user: Who, leave: bool) -> PromiseOrValue<bool> {
        let mut acct = self.account(&user);
        let legs = acct.payout_legs();
        for (token, amount) in &legs {
            acct.debit(token, amount.0);
        }
        self.put(&user, acct);
        self.pay_out(user, legs, Some(leave))
    }

    /// Withdraws each leg (already off the ledger) from Rhea, side by side. `cash` is None for a
    /// withdraw and Some(leave) for a cash out.
    fn pay_out(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>) -> PromiseOrValue<bool> {
        let ft = legs.iter().filter(|(t, _)| *t != self.wnear).count() as u64;
        // A withdraw forwards its unused gas to Rhea; cash out keeps fixed budgets (see call_fixed).
        let rhea = if cash.is_none() { call } else { call_fixed };
        let mut out: Option<Promise> = None;
        for (token, amount) in &legs {
            emit("withdraw_started", json!({ "user": user, "token": token, "amount": amount }));
            // wNEAR: Rhea unwraps and sends NEAR to this contract; other tokens: it transfers them here.
            let p = rhea(&self.dcl, "withdraw_asset", json!({ "token_id": token, "amount": amount, "skip_unwrap_near": *token != self.wnear }), ZERO, tg(T_WITHDRAW));
            out = Some(match out {
                Some(o) => o.and(p),
                None => p,
            });
        }
        let Some(out) = out else { return self.out_done(user, cash, true) };
        PromiseOrValue::Promise(out.then(Self::ext(env::current_account_id()).with_static_gas(tg(cb_out_withdrawn(ft))).with_unused_gas_weight(2).on_out_withdrawn(user, legs, cash)))
    }

    fn out_withdrawn(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>, released: Vec<bool>) -> PromiseOrValue<bool> {
        let mut acct = self.account(&user);
        let (mut arrived, mut all) = (Vec::new(), true);
        for ((token, amount), released) in legs.into_iter().zip(released) {
            if !released {
                // Rhea kept the tokens: they are the user's again.
                acct.credit(&token, amount.0);
                all = false;
                emit("refused", json!({ "user": user, "token": token, "why": "Rhea DCL did not release the tokens; they stay on your ledger" }));
            } else if token == self.wnear {
                // NEAR to an existing account cannot be refused; it goes out beside the rest.
                emit("withdrawn", json!({ "user": user, "token": token, "amount": amount }));
                let _ = Promise::new(user.account()).transfer(NearToken::from_yoctonear(amount.0));
            } else {
                arrived.push((token, amount));
            }
        }
        self.put(&user, acct);
        let me = env::current_account_id();
        let mut reads: Option<Promise> = None;
        for (token, _) in &arrived {
            let p = view(token, "ft_balance_of", json!({ "account_id": me }));
            reads = Some(match reads {
                Some(r) => r.and(p),
                None => p,
            });
        }
        let Some(reads) = reads else { return self.out_done(user, cash, all) };
        let ft = arrived.len() as u64;
        PromiseOrValue::Promise(reads.then(Self::ext(me).with_static_gas(tg(cb_out_arrived(ft))).with_unused_gas_weight(2).on_out_arrived(user, arrived, cash)))
    }

    /// What arrived is everything in this contract's token wallet beyond what it owes users, and
    /// never more than Rhea released. A balance that could not be read is owed in full: `claim`
    /// reads again and never pays out of what others are owed.
    fn out_arrived(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>, balances: Vec<Option<u128>>) -> PromiseOrValue<bool> {
        let mut acct = self.account(&user);
        let mut sends = Vec::new();
        for ((token, amount), balance) in legs.into_iter().zip(balances) {
            let owed = owed_total(&token);
            let Some(balance) = balance else {
                acct.owe(&token, amount.0);
                set_owed_total(&token, owed + amount.0);
                emit("payout_unmeasured", json!({ "user": user, "token": token, "amount": amount, "why": "Could not read what arrived; use claim" }));
                continue;
            };
            let got = balance.saturating_sub(owed).min(amount.0);
            if got == 0 {
                // Rhea said it released them, yet nothing arrived: whatever is still on Rhea goes back
                // to this user through settle (it is off every ledger now).
                mark_unsettled(&user, "Rhea reported a payout but nothing arrived");
                continue;
            }
            if got < amount.0 {
                emit("payout_short", json!({ "user": user, "token": token, "released": amount, "arrived": U128(got) }));
            }
            if got > 0 {
                sends.push((token, U128(got)));
            }
        }
        self.put(&user, acct);
        let mut transfers: Option<Promise> = None;
        for (token, got) in &sends {
            let p = call(token, "ft_transfer", json!({ "receiver_id": user.account(), "amount": got }), ONE_YOCTO, tg(T_TRANSFER));
            transfers = Some(match transfers {
                Some(t) => t.and(p),
                None => p,
            });
        }
        let Some(transfers) = transfers else { return self.out_done(user, cash, false) };
        PromiseOrValue::Promise(transfers.then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OUT_PAID)).with_unused_gas_weight(2).on_out_paid(user, sends, cash)))
    }

    fn out_paid(&mut self, user: Who, legs: Vec<(AccountId, U128)>, cash: Option<bool>, sent: Vec<bool>) -> PromiseOrValue<bool> {
        let mut all = true;
        for ((token, amount), sent) in legs.into_iter().zip(sent) {
            all &= self.settle_payout(&user, &token, amount, sent);
        }
        self.out_done(user, cash, all)
    }

    /// The end of a payout. A cash out with `leave` removes the record once nothing is left (no
    /// position, empty ledger, nothing owed) and returns the join deposit.
    fn out_done(&mut self, user: Who, cash: Option<bool>, all: bool) -> PromiseOrValue<bool> {
        self.unlock();
        let Some(leave) = cash else { return PromiseOrValue::Value(all) };
        emit("cash_out_done", json!({ "user": user }));
        let acct = self.account(&user);
        if leave && acct.position.is_none() && acct.ledger.is_empty() && acct.owed.is_empty() && !unsettled().contains(&user) {
            self.forget(&user);
            emit("left", json!({ "user": user }));
            return PromiseOrValue::Promise(Promise::new(user.account()).transfer(JOIN_DEPOSIT));
        }
        PromiseOrValue::Value(all)
    }

    /// A claim, once this contract's token balance is known: pays the smaller of what is owed and
    /// what the wallet holds beyond what others are owed, and clears the debt.
    fn claim_measured(&mut self, user: Who, token: AccountId, balance: Option<u128>) -> PromiseOrValue<bool> {
        let Some(balance) = balance else { return self.bail(&user, "Could not read this contract's token balance; nothing changed, try again") };
        let mut acct = self.account(&user);
        let owed = acct.owed_of(&token);
        let others = owed_total(&token).saturating_sub(owed);
        let pay = owed.min(balance.saturating_sub(others));
        acct.owed.retain(|(t, _)| *t != token);
        self.put(&user, acct);
        set_owed_total(&token, others);
        if pay < owed {
            emit("claim_short", json!({ "user": user, "token": token, "owed": U128(owed), "paid": U128(pay) }));
        }
        if pay == 0 {
            self.unlock();
            return PromiseOrValue::Value(false);
        }
        PromiseOrValue::Promise(
            call(&token, "ft_transfer", json!({ "receiver_id": user.account(), "amount": U128(pay) }), ONE_YOCTO, tg(T_TRANSFER))
                .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_claimed(user, token, U128(pay))),
        )
    }

    /// Rebuilds `owed_total` and `ledger_total` from the records (run by `migrate`): for every pool
    /// token of every user, the sums of what users are owed and hold in it.
    fn seed_owed_totals(&self) {
        let mut owed: HashMap<AccountId, u128> = HashMap::new();
        let mut held: HashMap<AccountId, u128> = HashMap::new();
        for user in self.users.iter() {
            for slot in 0..MAX_SLOTS {
                let Some(a) = self.rec(&Who::of(user, slot)) else { continue };
                let (x, y) = a.pool_tokens();
                for t in [x, y] {
                    owed.entry(t.clone()).or_default();
                    held.entry(t).or_default();
                }
                for (token, amount) in &a.owed {
                    *owed.entry(token.clone()).or_default() += amount.0;
                }
                for (token, amount) in &a.ledger {
                    *held.entry(token.clone()).or_default() += amount.0;
                }
            }
        }
        for (token, total) in owed {
            set_owed_total(&token, total);
        }
        for (token, total) in held {
            set_ledger_total(&token, total);
        }
    }

    /// After remove_liquidity: the position is gone and the removed amounts are the user's again.
    /// When Rhea's answer cannot be read, the position is still gone from Rhea: the record says so,
    /// nothing is guessed, and the user is marked for `settle`.
    fn credit_removed(&mut self, user: &Who, pool: &PoolView, removed: Removed) -> Close {
        let (rx, ry, close) = match removed {
            Removed::Refused => return Close::Refused,
            Removed::Amounts(x, y) => (x, y, Close::Closed),
            Removed::Unknown => (0, 0, Close::Unmeasured),
        };
        let mut acct = self.account(user);
        acct.credit(&pool.token_x, rx);
        acct.credit(&pool.token_y, ry);
        let was = acct.position.take();
        acct.high_point = None;
        acct.low_point = None;
        self.put(&user, acct);
        if close == Close::Unmeasured {
            mark_unsettled(user, "Rhea closed the position but its answer could not be read");
        }
        emit("position_closed", json!({ "user": user, "lpt_id": was.map(|p| p.lpt_id), "amount_x": U128(rx), "amount_y": U128(ry), "measured": close == Close::Closed }));
        close
    }

    // ---------- settle (v1.5): anyone may run it ----------

    /// Credits the one unsettled user with what this contract's Rhea balance holds beyond all ledgers,
    /// for each of their pool tokens. That surplus can only come from the step that marked them (every
    /// other step is measured and the lock runs them one at a time). With more than one user marked,
    /// the owner sorts it out through RecoverCredit.
    pub fn settle(&mut self) -> Promise {
        let list = unsettled();
        if list.is_empty() {
            env::panic_str("Nothing to settle");
        }
        if list.len() > 1 {
            env::panic_str("More than one unmeasured step: the owner settles them (RecoverCredit)");
        }
        self.assert_free();
        self.lock();
        let user = list[0].clone();
        view(&self.dcl, "list_user_assets", json!({ "account_id": env::current_account_id() }))
            .then(Self::ext(env::current_account_id()).with_static_gas(tg(CB_OWN)).with_unused_gas_weight(2).on_settle_read(user))
    }

    #[private]
    pub fn on_settle_read(&mut self, user: Who, #[callback_result] assets: Result<HashMap<AccountId, U128>, PromiseError>) -> bool {
        let Ok(assets) = assets else {
            let _ = self.bail(&user, "Could not read the balances on Rhea DCL; nothing changed, run settle again");
            return false;
        };
        self.settled(user, &assets)
    }

    fn settled(&mut self, user: Who, assets: &HashMap<AccountId, U128>) -> bool {
        self.unlock();
        let mut acct = self.account(&user);
        let (x, y) = acct.pool_tokens();
        let mut credited = Vec::new();
        for token in [x, y] {
            let on_rhea = assets.get(&token).map(|a| a.0).unwrap_or(0);
            let surplus = on_rhea.saturating_sub(ledger_total(&token));
            acct.credit(&token, surplus);
            credited.push((token, U128(surplus)));
        }
        self.put(&user, acct);
        set_unsettled(&unsettled().into_iter().filter(|u| *u != user).collect::<Vec<_>>());
        emit("settled", json!({ "user": user, "credited": credited }));
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
    fn fresh() -> Guardian {
        Guardian::new(acc("dcl.testnet"), acc("wrap.testnet"), acc("dao.testnet"))
    }
    fn at(who: &str, secs: u64) {
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc(who)).current_account_id(acc("guardian.testnet")).block_timestamp(secs * 1_000_000_000).build());
    }
    fn joined() -> Guardian {
        let mut g = fresh();
        ctx("alice.testnet", JOIN_DEPOSIT);
        g.join(policy(), Some(acc("agent.testnet")), None);
        g
    }
    fn held(g: &Guardian, who: &str, token: &str) -> u128 {
        g.get_account(acc(who).into()).unwrap().held(&acc(token))
    }
    fn assets(token: &str, amount: u128) -> Result<HashMap<AccountId, U128>, PromiseError> {
        Ok(HashMap::from([(acc(token), U128(amount))]))
    }
    /// A measured deposit (v1.5) that Rhea took whole: the Rhea balance grew by exactly `amount`.
    fn deposited(g: &mut Guardian, who: &str, token: &str, amount: u128) {
        ctx("guardian.testnet", ZERO);
        assert_eq!(g.on_deposit_measured(acc(who).into(), acc(token), U128(amount), U128(1_000), U128(amount), assets(token, 1_000 + amount)), U128(0));
    }

    #[test]
    #[should_panic(expected = "Attach the join deposit")]
    fn join_needs_the_deposit() {
        let mut g = fresh();
        ctx("alice.testnet", ZERO);
        g.join(policy(), None, None);
    }

    #[test]
    #[should_panic(expected = "exactly one pool")]
    fn join_needs_one_pool() {
        let mut g = fresh();
        ctx("alice.testnet", JOIN_DEPOSIT);
        let mut p = policy();
        p.allowed_pools.push("a|b|100".into());
        g.join(p, None, None);
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
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 5);
        // Rhea took only part: that part is credited, the rest goes back to the sender.
        ctx("guardian.testnet", ZERO);
        assert_eq!(g.on_deposit_measured(acc("alice.testnet").into(), acc("tkq.testnet"), U128(7), U128(100), U128(3), assets("tkq.testnet", 103)), U128(4));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 8);
        // Rhea failed: nothing credited, all of it back, and no second read.
        g.lock();
        assert!(matches!(g.on_forwarded(acc("alice.testnet").into(), acc("tkq.testnet"), U128(9), U128(100), Err(PromiseError::Failed)), PromiseOrValue::Value(U128(9))));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 8);
        assert!(!g.is_busy(), "deposit steps release the lock");
    }

    #[test]
    fn a_token_that_over_reports_is_credited_only_what_rhea_got() {
        // M2 (review 30 Oct): the token says Rhea took 10, Rhea's balance grew by 2.
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert_eq!(g.on_deposit_measured(acc("alice.testnet").into(), acc("tkq.testnet"), U128(10), U128(500), U128(10), assets("tkq.testnet", 502)), U128(0));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 2);
        assert!(!g.is_busy());
        // A balance that did not grow at all (or shrank) credits nothing.
        assert_eq!(g.on_deposit_measured(acc("alice.testnet").into(), acc("tkq.testnet"), U128(10), U128(500), U128(10), assets("tkq.testnet", 400)), U128(0));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 2);
    }

    #[test]
    fn an_unreadable_balance_credits_nothing_and_unlocks() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert_eq!(g.on_deposit_measured(acc("alice.testnet").into(), acc("tkq.testnet"), U128(5), U128(0), U128(5), Err(PromiseError::Failed)), U128(0));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 0);
        assert!(!g.is_busy());
        // The first read failed: nothing was forwarded, the whole transfer goes back.
        g.lock();
        assert!(matches!(g.on_deposit_before(acc("alice.testnet").into(), acc("tkq.testnet"), U128(5), Err(PromiseError::Failed)), PromiseOrValue::Value(U128(5))));
        assert!(!g.is_busy());
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
        let r = g.added(acc("alice.testnet").into(), req, pool, [U128(10), U128(20), U128(0), U128(0)], Added::Refused);
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
        g.on_measured(acc("alice.testnet").into(), pool, [U128(100), U128(200), U128(1_100), U128(5_200)], Ok(after));
        assert_eq!((held(&g, "alice.testnet", "tkq.testnet"), held(&g, "alice.testnet", "wrap.testnet")), (0, 15));
        assert!(!g.is_busy());
    }

    fn owed(g: &Guardian, who: &str) -> Vec<(AccountId, U128)> {
        g.get_account(acc(who).into()).unwrap().owed
    }
    fn refused_payout(g: &mut Guardian, who: &str, token: &str, amount: u128) {
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert!(matches!(g.out_paid(acc(who).into(), vec![(acc(token), U128(amount))], None, vec![false]), PromiseOrValue::Value(false)));
    }

    #[test]
    fn a_refused_payout_is_owed_and_claimable() {
        let mut g = joined();
        refused_payout(&mut g, "alice.testnet", "tkq.testnet", 42);
        assert_eq!(owed(&g, "alice.testnet"), vec![(acc("tkq.testnet"), U128(42))]);
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(42), "counted in the total");
        assert!(!g.is_busy());
        ctx("alice.testnet", ZERO);
        // A claim reads the wallet first; the debt moves only once the balance is known.
        let _ = g.claim(acc("tkq.testnet"), None);
        assert!(g.is_busy(), "a claim holds the lock until its callback");
        assert_eq!(owed(&g, "alice.testnet"), vec![(acc("tkq.testnet"), U128(42))]);
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.claim_measured(acc("alice.testnet").into(), acc("tkq.testnet"), Some(42)), PromiseOrValue::Promise(_)));
        assert!(owed(&g, "alice.testnet").is_empty());
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(0));
        // A second refusal puts it back.
        assert!(!g.on_claimed(acc("alice.testnet").into(), acc("tkq.testnet"), U128(42), Err(PromiseError::Failed)));
        assert_eq!(owed(&g, "alice.testnet"), vec![(acc("tkq.testnet"), U128(42))]);
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(42));
        assert!(!g.is_busy());
    }

    #[test]
    fn an_unreadable_balance_leaves_a_claim_unchanged() {
        let mut g = joined();
        refused_payout(&mut g, "alice.testnet", "tkq.testnet", 42);
        ctx("alice.testnet", ZERO);
        let _ = g.claim(acc("tkq.testnet"), None);
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.claim_measured(acc("alice.testnet").into(), acc("tkq.testnet"), None), PromiseOrValue::Value(false)));
        assert_eq!(owed(&g, "alice.testnet"), vec![(acc("tkq.testnet"), U128(42))]);
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(42));
        assert!(!g.is_busy());
    }

    #[test]
    fn a_tax_token_payout_sends_what_arrived_never_what_others_are_owed() {
        let mut g = joined();
        // Bob is owed 7 tkq already (his wallet refused a payout): it sits in the contract's wallet.
        ctx("bob.testnet", JOIN_DEPOSIT);
        g.join(policy(), None, None);
        refused_payout(&mut g, "bob.testnet", "tkq.testnet", 7);
        g.unlock();
        deposited(&mut g, "alice.testnet", "tkq.testnet", 100);
        ctx("alice.testnet", ZERO);
        assert!(matches!(g.withdraw(acc("tkq.testnet"), None, None), PromiseOrValue::Promise(_)));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 0, "off the ledger when the withdraw starts");
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.out_withdrawn(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(100))], None, vec![true]), PromiseOrValue::Promise(_)), "released: read where it landed");
        // 1% tax on the way out of Rhea: 99 arrived, beside Bob's 7.
        assert!(matches!(g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(100))], None, vec![Some(7 + 99)]), PromiseOrValue::Promise(_)));
        assert!(near_sdk::test_utils::get_logs().iter().any(|l| l.contains("payout_short") && l.contains("\"arrived\":\"99\"")), "pays 99, not 100");
        // Her wallet refuses: she is owed exactly the 99.
        assert!(matches!(g.out_paid(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(99))], None, vec![false]), PromiseOrValue::Value(false)));
        assert_eq!(owed(&g, "alice.testnet"), vec![(acc("tkq.testnet"), U128(99))]);
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(106));
        assert!(!g.is_busy());
        // Less arrived than even that (the wallet holds only Bob's 7 and 50 more): never Bob's share.
        deposited(&mut g, "alice.testnet", "tkq.testnet", 100);
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert!(matches!(g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(100))], None, vec![Some(106 + 50)]), PromiseOrValue::Promise(_)));
        assert!(near_sdk::test_utils::get_logs().iter().any(|l| l.contains("\"arrived\":\"50\"")));
        // Nothing arrived at all: nothing sent, the chain ends unlocked.
        assert!(matches!(g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(100))], None, vec![Some(106)]), PromiseOrValue::Value(false)));
        assert!(!g.is_busy());
    }

    #[test]
    fn an_unread_payout_is_owed_and_the_claim_pays_only_what_is_there() {
        let mut g = joined();
        ctx("bob.testnet", JOIN_DEPOSIT);
        g.join(policy(), None, None);
        refused_payout(&mut g, "bob.testnet", "tkq.testnet", 7);
        // Alice's 100 were released but the balance could not be read: owed in full, for claim.
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(100))], None, vec![None]), PromiseOrValue::Value(false)));
        assert_eq!(owed(&g, "alice.testnet"), vec![(acc("tkq.testnet"), U128(100))]);
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(107));
        // Only 99 had arrived (tax): the claim pays 99, leaves Bob's 7, and clears her debt.
        ctx("alice.testnet", ZERO);
        let _ = g.claim(acc("tkq.testnet"), None);
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.claim_measured(acc("alice.testnet").into(), acc("tkq.testnet"), Some(7 + 99)), PromiseOrValue::Promise(_)));
        assert!(near_sdk::test_utils::get_logs().iter().any(|l| l.contains("claim_short") && l.contains("\"paid\":\"99\"")));
        assert!(owed(&g, "alice.testnet").is_empty(), "nothing owed keeps her from leaving");
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(7), "Bob's share untouched");
    }

    #[test]
    fn claim_fees_credits_the_fees_and_keeps_the_position() {
        let mut g = joined();
        with_position(&mut g);
        let before = g.get_account(acc("alice.testnet").into()).unwrap();
        let (hx, hy) = (before.held(&acc("tkq.testnet")), before.held(&acc("wrap.testnet")));
        ctx("alice.testnet", ZERO);
        let _ = g.claim_fees(false, None);
        assert!(g.is_busy());
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.fees_claimed(acc("alice.testnet").into(), false, Removed::Amounts(3, 4)), PromiseOrValue::Value(true)));
        let a = g.get_account(acc("alice.testnet").into()).unwrap();
        assert_eq!((a.held(&acc("tkq.testnet")), a.held(&acc("wrap.testnet"))), (hx + 3, hy + 4), "fees on the ledger");
        assert_eq!(a.position, before.position, "the position stays");
        assert_eq!((a.high_point, a.low_point), (before.high_point, before.low_point), "the exit rule's marks stay");
        assert!(!g.is_busy());
    }

    #[test]
    fn claim_fees_can_send_them_on_and_a_refusal_changes_nothing() {
        let mut g = joined();
        with_position(&mut g);
        let before = g.get_account(acc("alice.testnet").into()).unwrap();
        ctx("alice.testnet", ZERO);
        let _ = g.claim_fees(true, None);
        ctx("guardian.testnet", ZERO);
        // Sent: the fees go out by the measured payout (only the token that earned any).
        assert!(matches!(g.fees_claimed(acc("alice.testnet").into(), true, Removed::Amounts(0, 4)), PromiseOrValue::Promise(_)));
        assert!(g.is_busy(), "locked until the payout ends");
        assert_eq!(g.get_account(acc("alice.testnet").into()).unwrap().ledger, before.ledger, "the fees went straight out");
        g.unlock();
        // Rhea refused: nothing changes, unlocked.
        ctx("alice.testnet", ZERO);
        let _ = g.claim_fees(true, None);
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.fees_claimed(acc("alice.testnet").into(), true, Removed::Refused), PromiseOrValue::Value(false)));
        assert_eq!(g.get_account(acc("alice.testnet").into()).unwrap().ledger, before.ledger);
        assert!(!g.is_busy());
    }

    #[test]
    #[should_panic(expected = "No open position")]
    fn claim_fees_needs_a_position() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        let _ = g.claim_fees(false, None);
    }

    fn assets2(x: u128, y: u128) -> HashMap<AccountId, U128> {
        HashMap::from([(acc("tkq.testnet"), U128(x)), (acc("wrap.testnet"), U128(y))])
    }
    fn tkq_pool() -> PoolView {
        PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: 0 }
    }

    #[test]
    fn ledger_total_follows_every_credit_and_debit() {
        let mut g = joined();
        deposited(&mut g, "alice.testnet", "tkq.testnet", 100);
        assert_eq!(g.get_ledger_total(acc("tkq.testnet")), U128(100));
        ctx("alice.testnet", ZERO);
        let _ = g.withdraw(acc("tkq.testnet"), Some(U128(40)), None);
        assert_eq!(g.get_ledger_total(acc("tkq.testnet")), U128(60), "debited when the withdraw starts");
        // A debit larger than the ledger only takes what is there.
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.debit(&acc("tkq.testnet"), 1_000);
        assert_eq!(g.get_ledger_total(acc("tkq.testnet")), U128(0));
    }

    #[test]
    fn rhea_answers_are_read_in_any_shape_they_come_in() {
        assert_eq!(parse_amounts(br#"["3","4"]"#), Some((3, 4)));
        assert_eq!(parse_amounts(b"[3,4]"), Some((3, 4)));
        assert_eq!(parse_amounts(br#"{"amount_x":"5","amount_y":"6","fees":"1"}"#), Some((5, 6)));
        assert_eq!(parse_amounts(br#""x""#), None);
        assert_eq!(parse_amounts(b"[1]"), None);
        assert_eq!(parse_amounts(b"not json"), None);
        assert_eq!(parse_lpt_id(br#""tkq|wrap|10000#3""#), Some("tkq|wrap|10000#3".into()));
        assert_eq!(parse_lpt_id(br#"{"lpt_id":"z#1"}"#), Some("z#1".into()));
        assert_eq!(parse_lpt_id(b"5"), None);
        assert_eq!(parse_lpt_id(br#""""#), None);
    }

    #[test]
    fn an_unreadable_close_marks_the_user_and_settle_credits_what_rhea_released() {
        let mut g = joined();
        with_position(&mut g);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        // Bob holds 9 tkq on the same Rhea balance: settle must never give them to Alice.
        ctx("bob.testnet", JOIN_DEPOSIT);
        g.join(policy(), None, None);
        deposited(&mut g, "bob.testnet", "tkq.testnet", 9);
        ctx("guardian.testnet", ZERO);
        g.lock();
        // Rhea closed the position but answered in a shape this code does not know.
        assert!(!g.exited(acc("alice.testnet").into(), tkq_pool(), Removed::Unknown));
        assert!(g.get_account(acc("alice.testnet").into()).unwrap().position.is_none(), "the record says the position is gone");
        assert_eq!(g.get_unsettled(), vec![acc("alice.testnet")]);
        assert!(!g.is_busy());
        // Nothing new starts; a deposit comes back whole.
        ctx("tkq.testnet", ZERO);
        assert!(matches!(g.ft_on_transfer(acc("bob.testnet"), U128(7), String::new()), PromiseOrValue::Value(U128(7))));
        ctx("alice.testnet", ZERO);
        assert!(panics(|| drop(g.open(-1400, 1400, None))));
        // Alice cannot leave before her tokens are settled.
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.debit(&acc("tkq.testnet"), 5);
        g.accounts.insert(acc("alice.testnet"), a);
        assert!(panics(|| drop(g.leave(None))));
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        // Anyone runs settle: Rhea holds 5 + 9 of Alice's and Bob's, plus 30 tkq and 12 wNEAR from the close.
        ctx("stranger.testnet", ZERO);
        let _ = g.settle();
        assert!(g.is_busy());
        ctx("guardian.testnet", ZERO);
        assert!(g.settled(acc("alice.testnet").into(), &assets2(5 + 9 + 30, 12)));
        assert_eq!((held(&g, "alice.testnet", "tkq.testnet"), held(&g, "alice.testnet", "wrap.testnet")), (35, 12));
        assert_eq!(held(&g, "bob.testnet", "tkq.testnet"), 9, "Bob's share untouched");
        assert!(g.get_unsettled().is_empty());
        assert_eq!(g.get_ledger_total(acc("tkq.testnet")), U128(44));
        assert!(!g.is_busy());
    }

    #[test]
    fn settle_with_nothing_or_two_users_marked_is_refused() {
        let mut g = joined();
        ctx("stranger.testnet", ZERO);
        assert!(panics(|| drop(g.settle())), "nothing to settle");
        mark_unsettled(&acc("alice.testnet").into(), "test");
        mark_unsettled(&acc("bob.testnet").into(), "test");
        assert!(panics(|| drop(g.settle())), "two users: the owner sorts it out");
    }

    #[test]
    fn a_payout_where_nothing_arrived_is_not_paid_and_waits_for_settle() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert!(matches!(g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(100))], None, vec![Some(0)]), PromiseOrValue::Value(false)));
        assert_eq!(g.get_unsettled(), vec![acc("alice.testnet")]);
        assert!(!g.is_busy());
    }

    #[test]
    fn unmeasured_deposits_and_leftovers_wait_for_settle() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.lock();
        let _ = g.on_deposit_measured(acc("alice.testnet").into(), acc("tkq.testnet"), U128(10), U128(1_000), U128(10), Err(PromiseError::Failed));
        assert_eq!(g.get_unsettled(), vec![acc("alice.testnet")]);
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 0, "nothing trusted from the token");
        set_unsettled(&[]);
        g.lock();
        let _ = g.on_measured(acc("alice.testnet").into(), tkq_pool(), [U128(10), U128(20), U128(0), U128(0)], Err(PromiseError::Failed));
        assert_eq!(g.get_unsettled(), vec![acc("alice.testnet")]);
        assert!(!g.is_busy());
    }

    #[test]
    fn unreadable_fees_or_position_ids_never_panic() {
        let mut g = joined();
        with_position(&mut g);
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert!(matches!(g.fees_claimed(acc("alice.testnet").into(), true, Removed::Unknown), PromiseOrValue::Value(false)));
        assert!(g.get_account(acc("alice.testnet").into()).unwrap().position.is_some(), "claiming fees never closes the position");
        assert_eq!(g.get_unsettled(), vec![acc("alice.testnet")]);
        assert!(!g.is_busy());
        // An add whose position id cannot be read: the leftovers are still measured; no record of it.
        set_unsettled(&[]);
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.position = None;
        g.accounts.insert(acc("alice.testnet"), a);
        g.lock();
        let req = RebalanceRequest { pool_id: POOL.into(), left_point: -1400, right_point: 1400 };
        assert!(matches!(g.added(acc("alice.testnet").into(), req, tkq_pool(), [U128(10), U128(20), U128(0), U128(0)], Added::Unknown), PromiseOrValue::Promise(_)));
        assert!(g.get_account(acc("alice.testnet").into()).unwrap().position.is_none());
    }

    #[test]
    fn an_unreadable_cash_out_close_stops_before_paying() {
        let mut g = joined();
        with_position(&mut g);
        ctx("guardian.testnet", ZERO);
        g.lock();
        assert!(matches!(g.cash_out_closed(acc("alice.testnet").into(), true, Removed::Unknown), PromiseOrValue::Value(false)));
        let a = g.get_account(acc("alice.testnet").into()).expect("no leaving before settle");
        assert!(a.position.is_none());
        assert_eq!(g.get_unsettled(), vec![acc("alice.testnet")]);
        assert!(!g.is_busy());
    }

    #[test]
    fn migrate_rebuilds_the_owed_totals() {
        let mut g = joined();
        refused_payout(&mut g, "alice.testnet", "tkq.testnet", 42);
        set_owed_total(&acc("tkq.testnet"), 999);
        set_owed_total(&acc("wrap.testnet"), 5);
        deposited(&mut g, "alice.testnet", "wrap.testnet", 8);
        set_ledger_total(&acc("wrap.testnet"), 999);
        g.seed_owed_totals();
        assert_eq!(g.get_owed_total(acc("tkq.testnet")), U128(42));
        assert_eq!(g.get_ledger_total(acc("wrap.testnet")), U128(8), "ledger total rebuilt from the records");
        assert_eq!(g.get_owed_total(acc("wrap.testnet")), U128(0), "a pool token nobody is owed is cleared");
    }

    #[test]
    #[should_panic(expected = "Close your position and withdraw first")]
    fn leave_waits_for_what_is_owed() {
        let mut g = joined();
        refused_payout(&mut g, "alice.testnet", "tkq.testnet", 42);
        g.unlock();
        ctx("alice.testnet", ZERO);
        let _ = g.leave(None);
    }

    #[test]
    #[should_panic(expected = "Nothing to cash out")]
    fn cash_out_needs_something() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        let _ = g.cash_out(false, None);
    }

    #[test]
    fn cash_out_with_nothing_and_leave_just_leaves() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        assert!(matches!(g.cash_out(true, None), PromiseOrValue::Promise(_)));
        assert!(g.get_account(acc("alice.testnet").into()).is_none());
    }

    #[test]
    fn cash_out_moves_both_tokens_side_by_side() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        deposited(&mut g, "alice.testnet", "wrap.testnet", 9);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        ctx("alice.testnet", ZERO);
        assert!(matches!(g.cash_out(true, None), PromiseOrValue::Promise(_)));
        assert!(g.is_busy());
        assert!(g.get_account(acc("alice.testnet").into()).unwrap().ledger.is_empty(), "both off the ledger when the withdrawals start");
        ctx("guardian.testnet", ZERO);
        // Both released: wNEAR goes out as NEAR, tkq is read where it landed.
        let legs = vec![(acc("tkq.testnet"), U128(5)), (acc("wrap.testnet"), U128(9))];
        assert!(matches!(g.out_withdrawn(acc("alice.testnet").into(), legs, Some(true), vec![true, true]), PromiseOrValue::Promise(_)));
        assert!(matches!(g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(5))], Some(true), vec![Some(5)]), PromiseOrValue::Promise(_)));
        // The wallet refused tkq: owed, so the record stays (no leave), unlocked.
        assert!(matches!(g.out_paid(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(5))], Some(true), vec![false]), PromiseOrValue::Value(false)));
        assert!(!g.is_busy());
        let a = g.get_account(acc("alice.testnet").into()).expect("kept while owed");
        assert!(a.ledger.is_empty());
        assert_eq!(a.owed, vec![(acc("tkq.testnet"), U128(5))]);
    }

    #[test]
    fn cash_out_leaves_when_everything_went_out() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        ctx("alice.testnet", ZERO);
        let _ = g.cash_out(true, None);
        ctx("guardian.testnet", ZERO);
        let _ = g.out_withdrawn(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(5))], Some(true), vec![true]);
        let _ = g.out_arrived(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(5))], Some(true), vec![Some(5)]);
        assert!(matches!(g.out_paid(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(5))], Some(true), vec![true]), PromiseOrValue::Promise(_)), "the join deposit comes back");
        assert!(g.get_account(acc("alice.testnet").into()).is_none());
        assert!(!g.is_busy());
    }

    #[test]
    fn a_refused_cash_out_withdraw_stops_with_the_tokens_on_the_ledger() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        ctx("alice.testnet", ZERO);
        let _ = g.cash_out(true, None);
        ctx("guardian.testnet", ZERO);
        assert!(matches!(g.out_withdrawn(acc("alice.testnet").into(), vec![(acc("tkq.testnet"), U128(5))], Some(true), vec![false]), PromiseOrValue::Value(false)));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 5);
        assert!(!g.is_busy());
        assert!(g.get_account(acc("alice.testnet").into()).is_some());
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn only_the_agent_rebalances() {
        let mut g = joined();
        ctx("bob.testnet", ZERO);
        let _ = g.request_rebalance(acc("alice.testnet").into(), RebalanceRequest { pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
    }

    #[test]
    #[should_panic(expected = "only the user opens one")]
    fn the_agent_never_opens_from_idle_tokens() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        ctx("agent.testnet", ZERO);
        let _ = g.request_rebalance(acc("alice.testnet").into(), RebalanceRequest { pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn only_the_agent_or_the_user_pokes() {
        let mut g = joined();
        ctx("bob.testnet", ZERO);
        let _ = g.poke(acc("alice.testnet").into());
    }

    #[test]
    fn the_best_price_moves_only_when_it_held_at_two_reads() {
        // N1 (review 30 Oct): one read's spike never becomes the best price the exit rule measures from.
        let mut g = joined();
        let alice = || acc("alice.testnet");
        let mut a = g.get_account(alice().into()).unwrap();
        a.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
        a.note_point(10);
        g.accounts.insert(alice(), a);
        let pool = |p| Ok(Some(PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: p }));
        let marks = |g: &Guardian| {
            let a = g.get_account(acc("alice.testnet").into()).unwrap();
            (a.high_point, a.low_point)
        };
        // A spike to 500: recorded as the price to confirm against, the marks stay.
        at("guardian.testnet", 1_000);
        assert!(g.on_poked(alice().into(), pool(500)));
        assert_eq!(marks(&g), (Some(10), Some(10)));
        // A re-poke within 3 minutes changes nothing, not even the recorded price.
        at("guardian.testnet", 1_060);
        assert!(!g.on_poked(alice().into(), pool(900)));
        assert_eq!(g.get_seen(alice().into()), Some(Seen { point: 500, at_secs: 1_000 }));
        // 5 minutes later the price is 40: the spike never held, so the high rises only to 40.
        at("guardian.testnet", 1_300);
        assert!(g.on_poked(alice().into(), pool(40)));
        assert_eq!(marks(&g), (Some(40), Some(10)));
        // 60 then 70: the high takes the lower of each pair of reads.
        at("guardian.testnet", 1_600);
        assert!(g.on_poked(alice().into(), pool(60)));
        assert_eq!(marks(&g), (Some(40), Some(10)));
        at("guardian.testnet", 1_900);
        assert!(g.on_poked(alice().into(), pool(70)));
        assert_eq!(marks(&g), (Some(60), Some(10)));
        // A fall lowers the low the same way: only once it held.
        at("guardian.testnet", 2_200);
        assert!(g.on_poked(alice().into(), pool(-30)));
        assert_eq!(marks(&g), (Some(60), Some(10)));
        at("guardian.testnet", 2_500);
        assert!(g.on_poked(alice().into(), pool(-40)));
        assert_eq!(marks(&g), (Some(60), Some(-30)));
        // A read more than an hour old confirms nothing.
        at("guardian.testnet", 2_500 + 3_700);
        assert!(g.on_poked(alice().into(), pool(1_000)));
        assert_eq!(marks(&g), (Some(60), Some(-30)));
    }

    #[test]
    fn a_pushed_mark_cannot_confirm_an_exit_later() {
        // N1 attack: one pushed read at a poke, then fair reads; the exit rule stays unmet.
        let mut g = joined();
        let alice = || acc("alice.testnet");
        let mut a = g.get_account(alice().into()).unwrap();
        a.policy.exit_drop_points = 2_877;
        a.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
        a.note_point(0);
        g.accounts.insert(alice(), a);
        let pool = |p| Ok(Some(PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: p }));
        at("guardian.testnet", 1_000);
        g.on_poked(alice().into(), pool(5_000)); // pushed
        at("guardian.testnet", 1_300);
        g.on_poked(alice().into(), pool(0)); // back to fair
        at("guardian.testnet", 1_600);
        g.on_poked(alice().into(), pool(0));
        let a = g.get_account(alice().into()).unwrap();
        assert_eq!(a.high_point, Some(0), "the pushed read never became the best price");
        assert_eq!(moved_against(&a.policy, a.high_point, a.low_point, 0), 0);
    }

    #[test]
    fn gas_floors_cover_each_chain_within_300_tgas() {
        // Each entry point: what it schedules, plus its own work, must fit a wallet's 300 TGas
        // (a token transfer passes ~270 of it on to ft_on_transfer).
        // v1.5: a deposit is read, forwarded and read again; the page attaches 200 TGas to it (a token
        // passes ~170 on). Opening is its own call (`open`, 300 TGas).
        let deposit = T_VIEW + CB_DEP_BEFORE + T_OWN;
        let open_new = T_VIEW + CB_POOL_NEW + T_OWN;
        let agent_move = T_VIEW + CB_POOL_HELD + T_OWN;
        let cash_out = T_VIEW + CB_LIQ_CASH + T_OWN;
        let exit = T_VIEW + CB_POOL_EXIT + T_OWN;
        // v1.5: a payout reads where each token landed; cash out moves both tokens side by side.
        let withdraw = out_floor(1, 1) + T_OWN;
        let claim = T_VIEW + CB_CLAIM_MEASURED + T_OWN;
        let claim_fees = T_REMOVE + CB_FEES + T_OWN;
        assert!(claim_fees <= 290, "claim_fees and send both tokens: {claim_fees}");
        println!("deposit {deposit}, open {open_new}, move {agent_move}, cash_out {cash_out}, exit {exit}, withdraw {withdraw}, claim {claim}");
        assert!(deposit <= 160 && open_new <= 290 && agent_move <= 290 && cash_out <= 290 && exit <= 290 && withdraw <= 290 && claim <= 290);
        assert!(CB_CASH_CLOSED >= T_OWN + out_floor(2, 2), "cash out pays for two tokens that are not wNEAR");
        // The chain behind an add always has its measurement step paid for.
        assert!(CB_ADDED >= T_VIEW + CB_OWN && CB_SNAP_ADD >= T_ADD + CB_ADDED);
    }

    #[test]
    #[should_panic(expected = "Deposit with an empty msg")]
    fn a_deposit_never_opens_or_moves_a_position() {
        let mut g = joined();
        ctx("tkq.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("alice.testnet"), U128(5), r#"{"left_point":0,"right_point":1400}"#.into());
    }

    #[test]
    #[should_panic(expected = "Only the owner can propose")]
    fn only_the_owner_proposes() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        g.propose(AdminAction::RecoverCredit { user: acc("alice.testnet").into(), token: acc("tkq.testnet"), amount: U128(5) });
    }

    #[test]
    fn admin_actions_wait_out_the_delay_then_anyone_runs_them() {
        let mut g = joined();
        at("dao.testnet", 1_000);
        let id = g.propose(AdminAction::RecoverCredit { user: acc("alice.testnet").into(), token: acc("tkq.testnet"), amount: U128(5) });
        assert_eq!(g.get_pending()[0].1.ready_at_secs, 1_000 + ADMIN_DELAY_SECS);
        // A minute before the delay: refused, for anyone, the owner included.
        at("dao.testnet", 1_000 + ADMIN_DELAY_SECS - 60);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.execute(id))).is_err());
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 0);
        // After it: anyone.
        at("stranger.testnet", 1_000 + ADMIN_DELAY_SECS);
        assert!(matches!(g.execute(id), PromiseOrValue::Value(true)));
        assert_eq!(held(&g, "alice.testnet", "tkq.testnet"), 5);
        assert!(g.get_pending().is_empty(), "runs once");
    }

    #[test]
    fn the_owner_can_cancel_and_hand_over() {
        let mut g = joined();
        at("dao.testnet", 0);
        let id = g.propose(AdminAction::SetOwner { owner: acc("newdao.testnet") });
        g.cancel(id);
        assert!(g.get_pending().is_empty());
        let id = g.propose(AdminAction::SetOwner { owner: acc("newdao.testnet") });
        at("anyone.testnet", ADMIN_DELAY_SECS);
        let _ = g.execute(id);
        assert_eq!(g.get_config()["owner"], "newdao.testnet");
    }

    #[test]
    #[should_panic(expected = "Stage that code first")]
    fn an_upgrade_needs_the_staged_code() {
        let mut g = joined();
        at("dao.testnet", 0);
        g.propose(AdminAction::Upgrade { code_hash: "11111111111111111111111111111111".into() });
    }

    fn with_position(g: &mut Guardian) {
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.position = Some(PositionRef { lpt_id: "1".into(), pool_id: POOL.into(), left_point: -1400, right_point: 1400 });
        a.policy.exit_drop_points = 2_000;
        a.high_point = Some(0);
        a.low_point = Some(0);
        g.accounts.insert(acc("alice.testnet"), a);
    }
    fn pool_at(p: i32) -> Result<Option<PoolView>, PromiseError> {
        Ok(Some(PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: p }))
    }
    fn refused_why() -> String {
        near_sdk::test_utils::get_logs().into_iter().filter(|l| l.contains("\"refused\"")).collect()
    }

    #[test]
    fn the_agent_cannot_move_on_one_moments_price() {
        let mut g = joined();
        with_position(&mut g);
        let req = RebalanceRequest { pool_id: POOL.into(), left_point: 2_000, right_point: 4_000 };
        // No earlier price: refused.
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_move(acc("alice.testnet").into(), req.clone(), true, pool_at(3_000)), PromiseOrValue::Value(false)));
        assert!(refused_why().contains("3 to 60 minutes"));
        // An earlier price only a minute old: refused.
        g.seen.insert(acc("alice.testnet"), Seen { point: 3_000, at_secs: 9_940 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_move(acc("alice.testnet").into(), req.clone(), true, pool_at(3_000)), PromiseOrValue::Value(false)));
        // Five minutes old, but the price was still inside the range then (a one-block push now).
        g.seen.insert(acc("alice.testnet"), Seen { point: 100, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_move(acc("alice.testnet").into(), req.clone(), true, pool_at(3_000)), PromiseOrValue::Value(false)));
        assert!(refused_why().contains("one moment's price"));
        // Five minutes old and already outside then: the move goes ahead, and needs a fresh read next.
        g.seen.insert(acc("alice.testnet"), Seen { point: 2_900, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_move(acc("alice.testnet").into(), req, true, pool_at(3_000)), PromiseOrValue::Promise(_)));
        assert!(g.get_seen(acc("alice.testnet").into()).is_none());
    }

    #[test]
    fn the_agent_exit_needs_the_rule_met_earlier_too() {
        let mut g = joined();
        with_position(&mut g);
        // Now 2_500 below the high (rule: 2_000), but 3 minutes ago only 500 below: refused.
        g.seen.insert(acc("alice.testnet"), Seen { point: -500, at_secs: 9_800 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(-2_500)), PromiseOrValue::Value(false)));
        // Already 2_100 below then: the exit goes ahead.
        g.seen.insert(acc("alice.testnet"), Seen { point: -2_100, at_secs: 9_800 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(-2_500)), PromiseOrValue::Promise(_)));
        // The user's own exit never needs it.
        let mut g = joined();
        with_position(&mut g);
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), true, pool_at(0)), PromiseOrValue::Promise(_)));
    }

    #[test]
    fn a_poke_records_the_price_for_the_agent() {
        let mut g = joined();
        with_position(&mut g);
        at("guardian.testnet", 5_000);
        assert!(g.on_poked(acc("alice.testnet").into(), pool_at(42)));
        assert_eq!(g.get_seen(acc("alice.testnet").into()), Some(Seen { point: 42, at_secs: 5_000 }));
    }

    #[test]
    fn a_recovered_position_must_be_this_contracts() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        let found = RheaPosition { owner_id: acc("guardian.testnet"), pool_id: POOL.into(), left_point: -1400, right_point: 1400 };
        assert!(g.on_recover_checked(acc("alice.testnet").into(), "tkq.testnet|wrap.testnet|10000#9".into(), Ok(Some(found))));
        assert_eq!(g.get_account(acc("alice.testnet").into()).unwrap().position.unwrap().left_point, -1400);
        // Someone else's position, or a second one, is refused.
        let theirs = RheaPosition { owner_id: acc("bob.testnet"), pool_id: POOL.into(), left_point: 0, right_point: 1400 };
        assert!(!g.on_recover_checked(acc("alice.testnet").into(), "x#1".into(), Ok(Some(theirs))));
    }

    #[test]
    fn users_are_listed_while_they_have_a_record() {
        let mut g = joined();
        ctx("bob.testnet", JOIN_DEPOSIT);
        g.join(policy(), None, None);
        assert_eq!(g.get_user_count(), 2);
        assert_eq!(g.get_users(Some(1), Some(5)), vec![acc("bob.testnet")]);
        ctx("alice.testnet", ZERO);
        let _ = g.leave(None);
        assert_eq!(g.get_users(None, None), vec![acc("bob.testnet")]);
        // cash_out(leave) with nothing held leaves too.
        ctx("bob.testnet", ZERO);
        let _ = g.cash_out(true, None);
        assert_eq!(g.get_user_count(), 0);
    }

    #[test]
    #[should_panic(expected = "Nothing to withdraw")]
    fn withdraw_is_bounded_by_the_ledger() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        let _ = g.withdraw(acc("tkq.testnet"), Some(U128(1)), None);
    }

    #[test]
    #[should_panic(expected = "Close your position and withdraw first")]
    fn leave_needs_an_empty_ledger() {
        let mut g = joined();
        ctx("guardian.testnet", ZERO);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        ctx("alice.testnet", ZERO);
        let _ = g.leave(None);
    }

    #[test]
    fn the_lock_expires() {
        let mut g = joined();
        g.lock();
        assert!(g.is_busy());
        testing_env!(VMContextBuilder::new().predecessor_account_id(acc("alice.testnet")).current_account_id(acc("guardian.testnet")).block_timestamp((BUSY_SECS + 1) * 1_000_000_000).build());
        assert!(!g.is_busy());
    }

    fn panics(f: impl FnOnce()) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
    }

    #[test]
    fn only_the_owner_pauses() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        assert!(panics(|| g.set_paused(true)));
        ctx("dao.testnet", ZERO);
        g.set_paused(true);
        assert_eq!(g.get_config()["paused"], true);
    }

    #[test]
    fn a_pause_stops_new_risk_at_once_and_every_way_out_still_works() {
        let mut g = joined();
        with_position(&mut g);
        ctx("guardian.testnet", ZERO);
        deposited(&mut g, "alice.testnet", "tkq.testnet", 5);
        ctx("dao.testnet", ZERO);
        g.set_paused(true);
        // Nothing new: no join, no range, no agent re-centre; a deposit comes back whole.
        ctx("bob.testnet", JOIN_DEPOSIT);
        assert!(panics(|| g.join(policy(), None, None)));
        ctx("alice.testnet", ZERO);
        assert!(panics(|| drop(g.open(-1400, 1400, None))));
        ctx("agent.testnet", ZERO);
        assert!(panics(|| drop(g.request_rebalance(acc("alice.testnet").into(), RebalanceRequest { pool_id: POOL.into(), left_point: 2_000, right_point: 4_000 }))));
        ctx("tkq.testnet", ZERO);
        assert!(matches!(g.ft_on_transfer(acc("alice.testnet"), U128(7), String::new()), PromiseOrValue::Value(U128(7))));
        // Every way out: the user's exit, a withdraw, a poke for the exit rule.
        ctx("alice.testnet", ZERO);
        let _ = g.exit(acc("alice.testnet").into());
        g.unlock();
        let _ = g.withdraw(acc("tkq.testnet"), None, None);
        g.unlock();
        ctx("agent.testnet", ZERO);
        let _ = g.poke(acc("alice.testnet").into());
        // Resumed: joins work again.
        ctx("dao.testnet", ZERO);
        g.set_paused(false);
        ctx("bob.testnet", JOIN_DEPOSIT);
        g.join(policy(), None, None);
    }

    #[test]
    fn refused_agent_requests_do_not_use_up_the_users_actions() {
        let mut g = joined();
        with_position(&mut g);
        let req = RebalanceRequest { pool_id: POOL.into(), left_point: 2_000, right_point: 4_000 };
        // Refused in the callback (no earlier price, then a pushed one): nothing counted.
        at("guardian.testnet", 10_000);
        let _ = g.on_pool_for_move(acc("alice.testnet").into(), req.clone(), true, pool_at(3_000));
        g.seen.insert(acc("alice.testnet"), Seen { point: 100, at_secs: 9_700 });
        let _ = g.on_pool_for_move(acc("alice.testnet").into(), req.clone(), true, pool_at(3_000));
        assert_eq!(g.get_account(acc("alice.testnet").into()).unwrap().rate.used, 0);
        // Approved: counted once.
        g.seen.insert(acc("alice.testnet"), Seen { point: 2_900, at_secs: 9_700 });
        assert!(matches!(g.on_pool_for_move(acc("alice.testnet").into(), req, true, pool_at(3_000)), PromiseOrValue::Promise(_)));
        assert_eq!(g.get_account(acc("alice.testnet").into()).unwrap().rate.used, 1);
    }

    #[test]
    fn a_full_window_refuses_at_the_door_and_in_the_callback() {
        let mut g = joined();
        with_position(&mut g);
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.rate = RateWindow { start_secs: 10_000, used: a.policy.max_actions };
        g.accounts.insert(acc("alice.testnet"), a);
        at("agent.testnet", 10_000);
        let req = RebalanceRequest { pool_id: POOL.into(), left_point: 2_000, right_point: 4_000 };
        assert!(panics(|| drop(g.request_rebalance(acc("alice.testnet").into(), req.clone()))));
        g.seen.insert(acc("alice.testnet"), Seen { point: 2_900, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_move(acc("alice.testnet").into(), req, true, pool_at(3_000)), PromiseOrValue::Value(false)));
        assert!(refused_why().contains("Too many agent actions"));
    }

    fn stage(g: &mut Guardian, who: &str, code: &[u8], deposit: NearToken) {
        let mut c = VMContextBuilder::new().predecessor_account_id(acc(who)).current_account_id(acc("guardian.testnet")).attached_deposit(deposit).build();
        c.input = std::rc::Rc::from(code.to_vec());
        testing_env!(c);
        g.stage_code();
    }

    #[test]
    fn staged_code_is_paid_in_full_and_only_its_stager_or_the_owner_replaces_it() {
        let mut g = joined();
        let code = vec![7u8; 50_000];
        let full = NearToken::from_yoctonear((50_000 + 300) * 10u128.pow(19));
        // Less than the whole code's storage: refused.
        assert!(panics(|| stage(&mut g, "kinu.testnet", &code, NearToken::from_near(0))));
        stage(&mut g, "kinu.testnet", &code, full);
        assert_eq!(g.staged.as_ref().unwrap().by, acc("kinu.testnet"));
        // A stranger cannot swap it, even paying in full.
        let other = vec![9u8; 50_000];
        assert!(panics(|| stage(&mut g, "mallory.testnet", &other, full)));
        // A replacement by its stager pays in full again (never from the contract's balance).
        assert!(panics(|| stage(&mut g, "kinu.testnet", &other, NearToken::from_near(0))));
        stage(&mut g, "kinu.testnet", &other, full);
        // The owner clears it at once (no pending upgrade names it); a stranger cannot.
        ctx("mallory.testnet", ZERO);
        assert!(panics(|| g.clear_staged()));
        ctx("dao.testnet", ZERO);
        g.clear_staged();
        assert!(g.staged.is_none());
        // Staged and proposed: locked for everyone, the owner's clear included, until cancelled.
        stage(&mut g, "kinu.testnet", &code, full);
        let hash = g.staged.as_ref().unwrap().code_hash.clone();
        at("dao.testnet", 0);
        let id = g.propose(AdminAction::Upgrade { code_hash: hash });
        assert!(panics(|| stage(&mut g, "kinu.testnet", &other, full)));
        at("dao.testnet", 0);
        assert!(panics(|| g.clear_staged()));
        g.cancel(id);
        g.clear_staged();
    }

    fn exit_logs() -> String {
        near_sdk::test_utils::get_logs().into_iter().filter(|l| l.contains("exit_started") || l.contains("\"refused\"")).collect()
    }

    #[test]
    fn take_profit_is_the_users_and_needs_a_position() {
        let mut g = joined();
        ctx("alice.testnet", ZERO);
        assert!(panics(|| g.set_take_profit(Some(TakeProfit { point: 5_000, above: true }), None)), "no position yet");
        with_position(&mut g);
        ctx("alice.testnet", ZERO);
        g.set_take_profit(Some(TakeProfit { point: 5_000, above: true }), None);
        assert_eq!(g.get_take_profit(acc("alice.testnet").into()), Some(TakeProfit { point: 5_000, above: true }));
        // The agent sets nothing for her: set_take_profit always acts on the caller's own record.
        ctx("agent.testnet", ZERO);
        assert!(panics(|| g.set_take_profit(Some(TakeProfit { point: 1, above: true }), None)));
        assert_eq!(g.get_take_profit(acc("alice.testnet").into()), Some(TakeProfit { point: 5_000, above: true }));
        ctx("alice.testnet", ZERO);
        g.set_take_profit(None, None);
        assert_eq!(g.get_take_profit(acc("alice.testnet").into()), None);
    }

    #[test]
    fn the_agent_takes_profit_only_on_a_target_reached_now_and_minutes_ago() {
        let mut g = joined();
        with_position(&mut g);
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.policy.exit_drop_points = 0; // no exit rule: take profit is the only reason to exit
        g.accounts.insert(acc("alice.testnet"), a);
        // Without a target the agent may not even start.
        ctx("agent.testnet", ZERO);
        assert!(panics(|| drop(g.exit(acc("alice.testnet").into()))));
        ctx("alice.testnet", ZERO);
        g.set_take_profit(Some(TakeProfit { point: 5_000, above: true }), None);
        ctx("agent.testnet", ZERO);
        let _ = g.exit(acc("alice.testnet").into());
        g.unlock();
        // Below the target now: refused.
        g.seen.insert(acc("alice.testnet"), Seen { point: 5_200, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(4_000)), PromiseOrValue::Value(false)));
        // Reached now but not 5 minutes ago (a pushed price): refused.
        g.seen.insert(acc("alice.testnet"), Seen { point: 4_000, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(5_400)), PromiseOrValue::Value(false)));
        assert!(exit_logs().contains("one moment's price"));
        // Reached now and 5 minutes ago: the agent closes it, for profit.
        g.seen.insert(acc("alice.testnet"), Seen { point: 5_100, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(5_400)), PromiseOrValue::Promise(_)));
        assert!(exit_logs().contains("\"reason\":\"take_profit\""));
        // Once closed, the target is gone with the position.
        assert!(g.exited(acc("alice.testnet").into(), PoolView { token_x: acc("tkq.testnet"), token_y: acc("wrap.testnet"), current_point: 5_400 }, Removed::Amounts(10, 20)));
        assert_eq!(g.get_take_profit(acc("alice.testnet").into()), None);
        assert!(g.get_account(acc("alice.testnet").into()).unwrap().position.is_none());
    }

    #[test]
    fn take_profit_keeps_its_own_direction_without_an_exit_rule() {
        // A pool whose token X is the quote: the user's token rising is the point falling. No exit
        // rule (so exit_on_rise is false); the target's own direction decides.
        let mut g = joined();
        with_position(&mut g);
        let mut a = g.get_account(acc("alice.testnet").into()).unwrap();
        a.policy.exit_drop_points = 0;
        a.policy.exit_on_rise = false;
        g.accounts.insert(acc("alice.testnet"), a);
        ctx("alice.testnet", ZERO);
        g.set_take_profit(Some(TakeProfit { point: -5_000, above: false }), None);
        // The point rising is not her profit: refused.
        g.seen.insert(acc("alice.testnet"), Seen { point: 6_000, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(6_000)), PromiseOrValue::Value(false)));
        // The point down to the target, now and 5 minutes ago: taken.
        g.seen.insert(acc("alice.testnet"), Seen { point: -5_100, at_secs: 9_700 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(-5_400)), PromiseOrValue::Promise(_)));
        assert!(exit_logs().contains("take_profit"));
    }

    #[test]
    fn the_exit_rule_still_comes_first() {
        let mut g = joined();
        with_position(&mut g); // exit rule: 2_000 points below the high of 0
        ctx("alice.testnet", ZERO);
        g.set_take_profit(Some(TakeProfit { point: 5_000, above: true }), None);
        g.seen.insert(acc("alice.testnet"), Seen { point: -2_100, at_secs: 9_800 });
        at("guardian.testnet", 10_000);
        assert!(matches!(g.on_pool_for_exit(acc("alice.testnet").into(), false, pool_at(-2_500)), PromiseOrValue::Promise(_)));
        assert!(exit_logs().contains("\"reason\":\"exit_rule\""));
    }

    #[test]
    fn take_profit_goes_when_the_user_leaves_or_cashes_out() {
        let mut g = joined();
        with_position(&mut g);
        ctx("alice.testnet", ZERO);
        g.set_take_profit(Some(TakeProfit { point: 5_000, above: true }), None);
        ctx("guardian.testnet", ZERO);
        g.lock();
        let _ = g.cash_out_closed(acc("alice.testnet").into(), false, Removed::Amounts(0, 0));
        assert_eq!(g.get_take_profit(acc("alice.testnet").into()), None);
    }

    // ---------- v1.6: positions (slots) ----------

    fn w(who: &str, slot: u8) -> Who {
        Who::of(&acc(who), slot)
    }
    fn join_slot(g: &mut Guardian, who: &str, agent: Option<&str>, slot: u8) {
        ctx(who, JOIN_DEPOSIT);
        g.join(policy(), agent.map(acc), Some(slot));
    }
    fn deposited_to(g: &mut Guardian, who: Who, token: &str, amount: u128) {
        ctx("guardian.testnet", ZERO);
        assert_eq!(g.on_deposit_measured(who, acc(token), U128(amount), U128(1_000), U128(amount), assets(token, 1_000 + amount)), U128(0));
    }
    fn holds(g: &Guardian, who: Who, token: &str) -> u128 {
        g.get_account(who).unwrap().held(&acc(token))
    }

    #[test]
    fn a_pool_id_has_rheas_shape() {
        assert!(pool_id_ok(POOL));
        assert!(pool_id_ok(&format!("{}|{}|100", "a".repeat(64), "b".repeat(64))));
        for bad in ["a|b", "a|b|c|d", "a|b|", "a|b|x", "a|b|-1", "a|b|12345678"] {
            assert!(!pool_id_ok(bad), "{bad}");
        }
        assert!(!pool_id_ok(&format!("{}|b|100", "a".repeat(65))));
    }

    #[test]
    #[should_panic(expected = "exactly one pool")]
    fn a_huge_pool_id_never_reaches_storage() {
        let mut g = fresh();
        ctx("alice.testnet", JOIN_DEPOSIT);
        let mut p = policy();
        p.allowed_pools = vec![format!("tkq.testnet|wrap.testnet|{}", "9".repeat(100_000))];
        g.join(p, None, None);
    }

    #[test]
    fn a_stored_position_that_cannot_be_read_stops_instead_of_reading_as_empty() {
        let g = joined();
        let who = w("alice.testnet", 1);
        near_sdk::env::storage_write(&Guardian::raw_key("s", &who), b"not a record");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.rec(&who)));
        assert!(r.is_err());
    }

    #[test]
    fn a_record_has_exactly_one_spelling() {
        assert_eq!(Who("alice.testnet".into()).parts(), Some((acc("alice.testnet"), 0)));
        assert_eq!(Who("alice.testnet#1".into()).parts(), Some((acc("alice.testnet"), 1)));
        assert_eq!(Who("alice.testnet#2".into()).parts(), Some((acc("alice.testnet"), 2)));
        for bad in ["alice.testnet#0", "alice.testnet#3", "alice.testnet#01", "alice.testnet#", "alice.testnet#1#2", "#1", "ALICE#1", "a b"] {
            assert_eq!(Who(bad.into()).parts(), None, "{bad}");
        }
        assert_eq!(w("alice.testnet", 0), Who("alice.testnet".into()));
    }

    #[test]
    fn slot_zero_is_stored_and_sent_exactly_like_an_account_id() {
        let id = acc("alice.testnet");
        let who = w("alice.testnet", 0);
        assert_eq!(near_sdk::borsh::to_vec(&who).unwrap(), near_sdk::borsh::to_vec(&id).unwrap());
        assert_eq!(near_sdk::serde_json::to_string(&who).unwrap(), near_sdk::serde_json::to_string(&id).unwrap());
        let old: Vec<Who> = near_sdk::serde_json::from_str(r#"["alice.testnet"]"#).unwrap();
        assert_eq!(old, vec![who.clone()]);
        // A proposal stored by v1.5 still reads.
        #[near(serializers = [borsh])]
        enum OldAction {
            Upgrade { code_hash: String },
            RecoverPosition { user: AccountId, lpt_id: String },
        }
        let raw = near_sdk::borsh::to_vec(&OldAction::RecoverPosition { user: id, lpt_id: "x#1".into() }).unwrap();
        let _ = OldAction::Upgrade { code_hash: String::new() };
        assert_eq!(near_sdk::borsh::from_slice::<AdminAction>(&raw).unwrap(), AdminAction::RecoverPosition { user: who, lpt_id: "x#1".into() });
    }

    #[test]
    fn slots_keep_their_ledgers_apart_even_in_the_same_token() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        join_slot(&mut g, "alice.testnet", None, 2);
        join_slot(&mut g, "bob.testnet", None, 1);
        deposited_to(&mut g, w("alice.testnet", 0), "tkq.testnet", 5);
        deposited_to(&mut g, w("alice.testnet", 1), "tkq.testnet", 7);
        deposited_to(&mut g, w("alice.testnet", 2), "tkq.testnet", 11);
        deposited_to(&mut g, w("bob.testnet", 1), "tkq.testnet", 13);
        assert_eq!(holds(&g, w("alice.testnet", 0), "tkq.testnet"), 5);
        assert_eq!(holds(&g, w("alice.testnet", 1), "tkq.testnet"), 7);
        assert_eq!(holds(&g, w("alice.testnet", 2), "tkq.testnet"), 11);
        assert_eq!(holds(&g, w("bob.testnet", 1), "tkq.testnet"), 13);
        assert_eq!(g.get_ledger_total(acc("tkq.testnet")).0, 36);
        assert_eq!(g.get_slots(acc("alice.testnet")), vec![0, 1, 2]);
        assert_eq!(g.get_slots(acc("bob.testnet")), vec![1]);
        assert_eq!(g.get_user_count(), 2);
    }

    #[test]
    fn the_totals_rebuild_counts_every_slot() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 2);
        deposited_to(&mut g, w("alice.testnet", 0), "tkq.testnet", 5);
        deposited_to(&mut g, w("alice.testnet", 2), "tkq.testnet", 7);
        set_ledger_total(&acc("tkq.testnet"), 0);
        g.seed_owed_totals();
        assert_eq!(g.get_ledger_total(acc("tkq.testnet")).0, 12);
    }

    #[test]
    #[should_panic(expected = "positions 0, 1 and 2")]
    fn there_are_three_positions() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 3);
    }

    #[test]
    #[should_panic(expected = "Already joined")]
    fn a_slot_joins_once() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        join_slot(&mut g, "alice.testnet", None, 1);
    }

    #[test]
    #[should_panic(expected = "has not joined")]
    fn a_deposit_for_a_slot_not_joined_comes_back() {
        let mut g = joined();
        ctx("tkq.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("alice.testnet"), U128(5), "1".into());
    }

    #[test]
    #[should_panic(expected = "Deposit with an empty msg")]
    fn a_deposit_with_any_other_msg_comes_back() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        ctx("tkq.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("alice.testnet"), U128(5), "alice.testnet#1".into());
    }

    #[test]
    fn a_deposit_msg_names_the_slot() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 2);
        ctx("tkq.testnet", ZERO);
        let _ = g.ft_on_transfer(acc("alice.testnet"), U128(5), "2".into());
        assert!(g.is_busy(), "the deposit started");
    }

    #[test]
    #[should_panic(expected = "Only the vault's agent")]
    fn an_agent_has_no_power_on_another_slot_of_the_wallet() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", Some("thief.testnet"), 1);
        ctx("guardian.testnet", ZERO);
        g.lock();
        let mut a = g.account(&w("alice.testnet", 0));
        a.position = Some(PositionRef { lpt_id: "t#1".into(), pool_id: POOL.into(), left_point: 0, right_point: 100 });
        g.put(&w("alice.testnet", 0), a);
        g.unlock();
        ctx("thief.testnet", ZERO);
        let _ = g.exit(w("alice.testnet", 0));
    }

    #[test]
    #[should_panic(expected = "has not joined")]
    fn a_stranger_only_ever_reaches_its_own_slot() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        deposited_to(&mut g, w("alice.testnet", 1), "tkq.testnet", 5);
        ctx("bob.testnet", ZERO);
        let _ = g.withdraw(acc("tkq.testnet"), None, Some(1));
    }

    #[test]
    fn a_wallet_stays_listed_until_its_last_slot_leaves() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        ctx("alice.testnet", ZERO);
        let _ = g.leave(None);
        assert_eq!(g.get_slots(acc("alice.testnet")), vec![1]);
        assert_eq!(g.get_users(None, None), vec![acc("alice.testnet")]);
        assert!(g.get_account(w("alice.testnet", 0)).is_none());
        assert!(g.get_account(w("alice.testnet", 1)).is_some(), "works with no slot 0");
        let _ = g.leave(Some(1));
        assert!(g.get_users(None, None).is_empty());
        assert!(g.get_slots(acc("alice.testnet")).is_empty());
    }

    #[test]
    fn settle_credits_only_the_unsettled_slot() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        deposited_to(&mut g, w("alice.testnet", 0), "tkq.testnet", 5);
        ctx("guardian.testnet", ZERO);
        mark_unsettled(&w("alice.testnet", 1), "test");
        assert_eq!(g.get_unsettled(), vec![w("alice.testnet", 1)]);
        g.lock();
        assert!(g.settled(w("alice.testnet", 1), &HashMap::from([(acc("tkq.testnet"), U128(9))])));
        assert_eq!(holds(&g, w("alice.testnet", 1), "tkq.testnet"), 4, "what Rhea holds beyond all ledgers");
        assert_eq!(holds(&g, w("alice.testnet", 0), "tkq.testnet"), 5);
        assert!(g.get_unsettled().is_empty());
    }

    #[test]
    fn seen_and_take_profit_belong_to_one_slot() {
        let mut g = joined();
        join_slot(&mut g, "alice.testnet", None, 1);
        let (a, b) = (w("alice.testnet", 0), w("alice.testnet", 1));
        g.put_seen(&b, Seen { point: 7, at_secs: 9 });
        g.put_tp(&b, TakeProfit { point: 7, above: true });
        assert_eq!(g.get_seen(a.clone()), None);
        assert_eq!(g.get_take_profit(a), None);
        assert_eq!(g.get_seen(b.clone()), Some(Seen { point: 7, at_secs: 9 }));
        g.forget(&b);
        assert_eq!(g.get_seen(b.clone()), None);
        assert_eq!(g.get_take_profit(b), None);
    }
}
