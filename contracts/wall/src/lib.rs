//! KINU Outer Wall: one account per person, standing in front of Guardian.
//!
//! Guardian guards the money from the agent. It obeys the owner, so a stolen owner key can cash out.
//! The wall is the Guardian user instead of the wallet, so every payout lands here, and money leaves
//! only by these roads:
//!   - to `home`, at once, even while frozen: the owner's way out;
//!   - to anyone else, after `delay_secs`, in public, while the canceller can stop it;
//!   - by the agent, to its listed receivers within its daily limit; over the limit it waits too.
//! A bait key is a function-call key on the wall itself, so it can only ever call the wall: its first
//! signature freezes the wall and the agent on Guardian, and clears the queue. The canceller can stop
//! and freeze, and send only to `home`. Owner, canceller and home are three different accounts.
//! The sentinel is an optional watching agent: it can only stop (cancel, freeze), so the worst a fooled
//! one does is delay an honest payment, and the canceller can dismiss it at once.
//! No upgrade method, and `new` removes the key that deployed it: nobody holds the wall's own account.

use near_sdk::json_types::U128;
use near_sdk::{AccountId, Allowance, Gas, GasWeight, NearToken, PanicOnDefault, Promise, PublicKey, env, near, require, serde_json::json};

const MIN_DELAY_SECS: u64 = 3_600;
const MAX_DELAY_SECS: u64 = 7 * 86_400;
/// The owner alone can lift a freeze only after this long, so a lost canceller never strands money.
#[cfg(not(feature = "short-delay"))]
const OWNER_UNFREEZE_SECS: u64 = 7 * 86_400;
#[cfg(feature = "short-delay")]
const OWNER_UNFREEZE_SECS: u64 = 7_200;
const DAY_SECS: u64 = 86_400;
const MAX_PENDING: usize = 16;
const MAX_AGENT_PENDING: usize = 4;
const MAX_TRIPWIRES: usize = 16;
const MAX_LIST: usize = 16;
/// Every gated call needs this much, so nobody can spring a bait key with too little gas to freeze.
const MIN_GAS: Gas = Gas::from_tgas(60);
const FREEZE_GAS: Gas = Gas::from_tgas(12);
const FT_GAS: Gas = Gas::from_tgas(30);
const VIEW_GAS: Gas = Gas::from_tgas(5);
const CLOSE_GAS: Gas = Gas::from_tgas(10);
const MAX_ARGS: usize = 2_048;
const MAX_REASON: usize = 280;
/// What a bait key may burn. At 0.25 NEAR the protocol refused a 300 TGas call before the wall saw it,
/// so a thief at full gas would not have sprung the bait.
const BAIT_ALLOWANCE: NearToken = NearToken::from_millinear(500);
const SLOTS: [u8; 3] = [0, 1, 2];

/// Guardian methods that run at once: none can widen what the agent may do or send money elsewhere.
/// `open` is not here: with a stolen key it places all the funds at a price the thief just pushed.
const AT_ONCE: [&str; 8] = ["claim_fees", "set_take_profit", "set_frozen", "exit", "withdraw", "claim", "cash_out", "leave"];
/// The ways out: they still run while frozen, and every one pays into the wall.
const WAYS_OUT: [&str; 6] = ["claim_fees", "exit", "withdraw", "claim", "cash_out", "leave"];
/// Guardian methods that change the rules: these wait like a payment.
const DELAYED: [&str; 4] = ["join", "set_policy", "set_agent", "open"];

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub enum Action {
    Send { token: Option<AccountId>, to: AccountId, amount: U128 },
    Guardian { method: String, args: String, deposit: NearToken },
    SetHome { home: AccountId },
    SetCanceller { canceller: AccountId },
    SetSentinel { sentinel: Option<AccountId> },
    SetDelay { delay_secs: u64 },
    SetAgent { agent: Option<AccountId>, receivers: Vec<AccountId>, limits: Vec<Limit> },
    RemoveTripwire { public_key: PublicKey },
    Unfreeze,
    /// Deletes the wall, all its NEAR (storage too) to `home`. Tokens still here would be lost: send them first.
    Close,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Limit {
    pub token: Option<AccountId>,
    pub per_day: U128,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Request {
    pub id: u64,
    pub action: Action,
    pub unlock_secs: u64,
    pub by_agent: bool,
    /// The canceller's word that this one is honest: the sentinel then leaves it alone.
    pub vouched: bool,
}

#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct Spent {
    pub token: Option<AccountId>,
    pub amount: U128,
    pub start_secs: u64,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct Wall {
    owner: AccountId,
    canceller: AccountId,
    sentinel: Option<AccountId>,
    home: AccountId,
    guardian: AccountId,
    delay_secs: u64,
    frozen: bool,
    agent: Option<AccountId>,
    receivers: Vec<AccountId>,
    limits: Vec<Limit>,
    spent: Vec<Spent>,
    tripwires: Vec<Vec<u8>>,
    pending: Vec<Request>,
    next_id: u64,
}

fn now_secs() -> u64 {
    env::block_timestamp() / 1_000_000_000
}

fn pay(token: Option<AccountId>, to: AccountId, amount: u128) -> Promise {
    match token {
        None => Promise::new(to).transfer(NearToken::from_yoctonear(amount)),
        Some(t) => Promise::new(t).function_call(
            "ft_transfer".to_string(),
            json!({ "receiver_id": to, "amount": U128(amount) }).to_string().into_bytes(),
            NearToken::from_yoctonear(1),
            FT_GAS,
        ),
    }
}

fn forward(receiver: AccountId, method: &str, args: String, deposit: NearToken) -> Promise {
    Promise::new(receiver).function_call_weight(method.to_string(), args.into_bytes(), deposit, Gas::from_gas(0), GasWeight(1))
}

#[near]
impl Wall {
    /// Private: deploy and init are one transaction, or anyone could init first. The key that did it
    /// is removed in the same breath.
    #[init]
    #[private]
    pub fn new(owner: AccountId, canceller: AccountId, sentinel: Option<AccountId>, home: AccountId, guardian: AccountId, delay_secs: u64) -> Self {
        Self::check_roles(&owner, &canceller, &home);
        Self::check_delay(delay_secs);
        let _ = Promise::new(env::current_account_id()).delete_key(env::signer_account_pk());
        Self {
            owner,
            canceller,
            sentinel,
            home,
            guardian,
            delay_secs,
            frozen: false,
            agent: None,
            receivers: vec![],
            limits: vec![],
            spent: vec![],
            tripwires: vec![],
            pending: vec![],
            next_id: 0,
        }
    }

    // ------------------------------------------------------------ the owner

    /// Queues an action; it runs `delay_secs` later unless cancelled. Returns its id.
    pub fn request(&mut self, action: Action) -> Option<u64> {
        if self.tripped() {
            return None;
        }
        self.only_owner();
        let wait = match &action {
            Action::Unfreeze => {
                require!(self.frozen, "Not frozen");
                OWNER_UNFREEZE_SECS
            }
            _ => {
                require!(!self.frozen, "The wall is frozen");
                self.delay_secs
            }
        };
        match &action {
            Action::Guardian { method, args, .. } => {
                require!(DELAYED.contains(&method.as_str()), "Not a rule change");
                require!(args.len() <= MAX_ARGS, "Arguments too long");
            }
            Action::SetHome { home } => Self::check_roles(&self.owner, &self.canceller, home),
            Action::SetCanceller { canceller } => Self::check_roles(&self.owner, canceller, &self.home),
            Action::SetDelay { delay_secs } => Self::check_delay(*delay_secs),
            Action::SetAgent { receivers, limits, .. } => require!(receivers.len() <= MAX_LIST && limits.len() <= MAX_LIST, "List too long"),
            _ => {}
        }
        Some(self.queue(action, wait, false))
    }

    /// Everything goes home at once, frozen or not. The canceller may do this too, and only this.
    pub fn send_home(&mut self, token: Option<AccountId>, amount: U128) {
        if self.tripped() && env::predecessor_account_id() != self.canceller {
            return;
        }
        let who = env::predecessor_account_id();
        require!(who == self.owner || who == self.canceller, "Only the owner or the canceller");
        let _ = pay(token, self.home.clone(), amount.0);
    }

    /// One Guardian call that cannot widen anything; while frozen, only the ways out.
    pub fn guardian_call(&mut self, method: String, args: String) {
        if self.tripped() {
            return;
        }
        self.only_owner();
        require!(AT_ONCE.contains(&method.as_str()), "This call must wait: use request");
        require!(!self.frozen || WAYS_OUT.contains(&method.as_str()), "The wall is frozen");
        let _ = forward(self.guardian.clone(), &method, args, NearToken::from_near(0));
    }

    /// Tokens from the wall into Guardian; `msg` names the slot.
    pub fn guardian_deposit(&mut self, token: AccountId, amount: U128, msg: String) {
        if self.tripped() {
            return;
        }
        self.only_owner();
        require!(!self.frozen, "The wall is frozen");
        let args = json!({ "receiver_id": self.guardian, "amount": amount, "msg": msg }).to_string();
        let _ = forward(token, "ft_transfer_call", args, NearToken::from_yoctonear(1));
    }

    /// Puts a bait key on the wall: a function-call key for `agent_send`, as an agent's would look. Any
    /// use of it reaches the wall. Adding is never delayed; removing is, so a thief cannot clear it first.
    pub fn add_tripwire(&mut self, public_key: PublicKey) {
        if self.tripped() {
            return;
        }
        self.only_owner();
        require!(self.tripwires.len() < MAX_TRIPWIRES, "Too many tripwires");
        let hash = env::sha256(public_key.as_bytes());
        require!(!self.tripwires.contains(&hash), "Already planted");
        self.tripwires.push(hash);
        let me = env::current_account_id();
        let _ = Promise::new(me.clone()).add_access_key_allowance(public_key, Allowance::limited(BAIT_ALLOWANCE).unwrap(), me, "agent_send".to_string());
    }

    // ------------------------------------------------------------ the agent

    /// Pays a listed receiver: at once within the daily limit, otherwise it waits like a payment.
    pub fn agent_send(&mut self, token: Option<AccountId>, to: AccountId, amount: U128) -> Option<u64> {
        if self.tripped() {
            return None;
        }
        require!(Some(env::predecessor_account_id()) == self.agent, "Only the agent");
        require!(!self.frozen, "The wall is frozen");
        require!(self.receivers.contains(&to), "Receiver not on the owner's list");
        let per_day = self.limits.iter().find(|l| l.token == token).map(|l| l.per_day.0);
        let per_day = per_day.unwrap_or_else(|| env::panic_str("No limit for this token"));
        if self.spend(&token, amount.0, per_day) {
            let _ = pay(token, to, amount.0);
            return None;
        }
        let waiting = self.pending.iter().filter(|r| r.by_agent).count();
        require!(waiting < MAX_AGENT_PENDING, "Too many agent requests waiting");
        Some(self.queue(Action::Send { token, to, amount }, self.delay_secs, true))
    }

    // ------------------------------------------------------------ anyone, the canceller

    /// Runs a request whose wait is over.
    pub fn execute(&mut self, id: u64) {
        if self.tripped() {
            return;
        }
        let i = self.pending.iter().position(|r| r.id == id).unwrap_or_else(|| env::panic_str("No such request"));
        require!(now_secs() >= self.pending[i].unlock_secs, "Still waiting");
        let request = self.pending.remove(i);
        require!(!self.frozen || matches!(request.action, Action::Unfreeze), "The wall is frozen");
        match request.action {
            Action::Send { token, to, amount } => {
                let _ = pay(token, to, amount.0);
            }
            Action::Guardian { method, args, deposit } => {
                let _ = forward(self.guardian.clone(), &method, args, deposit);
            }
            Action::SetHome { home } => self.home = home,
            Action::SetCanceller { canceller } => self.canceller = canceller,
            Action::SetSentinel { sentinel } => self.sentinel = sentinel,
            Action::SetDelay { delay_secs } => self.delay_secs = delay_secs,
            Action::SetAgent { agent, receivers, limits } => {
                self.agent = agent;
                self.receivers = receivers;
                self.limits = limits;
                self.spent.clear();
            }
            Action::RemoveTripwire { public_key } => {
                let hash = env::sha256(public_key.as_bytes());
                self.tripwires.retain(|h| *h != hash);
                let _ = Promise::new(env::current_account_id()).delete_key(public_key);
            }
            Action::Unfreeze => self.frozen = false,
            Action::Close => {
                let me = env::current_account_id();
                let read = |suffix: &str| {
                    let args = json!({ "user": format!("{me}{suffix}") }).to_string().into_bytes();
                    Promise::new(self.guardian.clone()).function_call("get_account".to_string(), args, NearToken::from_near(0), VIEW_GAS)
                };
                let _ = read("").and(read("#1")).and(read("#2")).then(Promise::new(me.clone()).function_call(
                    "on_close_read".to_string(),
                    vec![],
                    NearToken::from_near(0),
                    CLOSE_GAS,
                ));
            }
        }
    }

    /// The undo. The owner may cancel too: a thief holding that key gains nothing by it. The sentinel
    /// may not cancel what would replace it or lift a freeze, or a fooled one could never be removed.
    pub fn cancel(&mut self, id: u64, reason: Option<String>) {
        let who = env::predecessor_account_id();
        let watching = Some(&who) == self.sentinel.as_ref();
        require!(who == self.canceller || who == self.owner || watching, "Only the owner, the canceller or the sentinel");
        let i = self.pending.iter().position(|r| r.id == id).unwrap_or_else(|| env::panic_str("No such request"));
        if watching {
            require!(!matches!(self.pending[i].action, Action::SetSentinel { .. } | Action::Unfreeze), "Not for the sentinel");
            require!(!self.pending[i].vouched, "The canceller vouched for this one");
        }
        self.pending.remove(i);
        let reason = reason.unwrap_or_default();
        require!(reason.len() <= MAX_REASON, "Reason too long");
        env::log_str(&format!("wall: request {id} cancelled by {who}: {reason}"));
    }

    /// Lets an honest request past the sentinel. It gives the canceller nothing new: it can dismiss
    /// the sentinel outright, and a vouched request can still be cancelled by the owner or the canceller.
    pub fn vouch(&mut self, id: u64) {
        require!(env::predecessor_account_id() == self.canceller, "Only the canceller");
        let request = self.pending.iter_mut().find(|r| r.id == id).unwrap_or_else(|| env::panic_str("No such request"));
        request.vouched = true;
    }

    /// Stops every road but the way home, here and for the agent on Guardian.
    pub fn freeze(&mut self) {
        let who = env::predecessor_account_id();
        require!(who == self.canceller || who == self.owner || Some(&who) == self.sentinel.as_ref(), "Only the owner, the canceller or the sentinel");
        self.set_frozen();
    }

    /// A misbehaving sentinel goes at once, frozen or not.
    pub fn dismiss_sentinel(&mut self) {
        require!(env::predecessor_account_id() == self.canceller, "Only the canceller");
        self.sentinel = None;
    }

    /// The canceller lifts a freeze at once; the owner alone waits a week (`request(Unfreeze)`).
    pub fn unfreeze(&mut self) {
        require!(env::predecessor_account_id() == self.canceller, "Only the canceller");
        self.frozen = false;
    }

    /// Closes only when Guardian holds no record of this wall: a position left there would be lost.
    #[private]
    pub fn on_close_read(&mut self) {
        // Anything but a plain `null`, a failed read or an oversized record included, keeps the wall up.
        let clear = (0..env::promise_results_count()).all(|i| env::promise_result_checked(i, 16).is_ok_and(|v| v == b"null"));
        if !clear {
            env::log_str("wall: not closed, Guardian still holds a record");
            return;
        }
        let _ = Promise::new(env::current_account_id()).delete_account(self.home.clone());
    }

    // ------------------------------------------------------------ views

    pub fn get_config(&self) -> near_sdk::serde_json::Value {
        json!({
            "owner": self.owner, "canceller": self.canceller, "sentinel": self.sentinel, "home": self.home, "guardian": self.guardian,
            "delay_secs": self.delay_secs, "frozen": self.frozen, "agent": self.agent,
            "receivers": self.receivers, "limits": self.limits, "tripwires": self.tripwires.len()
        })
    }

    pub fn get_pending(&self) -> Vec<Request> {
        self.pending.clone()
    }
}

impl Wall {
    /// A stolen owner key must not reach home, nor a stolen canceller key, nor can home be the wall.
    fn check_roles(owner: &AccountId, canceller: &AccountId, home: &AccountId) {
        require!(canceller != owner, "The canceller must be another account");
        require!(home != owner && home != canceller, "Home must be a third account");
        require!(*home != env::current_account_id(), "Home must be outside the wall");
    }

    fn check_delay(delay_secs: u64) {
        require!((MIN_DELAY_SECS..=MAX_DELAY_SECS).contains(&delay_secs), "Delay must be 1 hour to 7 days");
    }

    fn only_owner(&self) {
        require!(env::predecessor_account_id() == self.owner, "Only the owner");
    }

    fn queue(&mut self, action: Action, wait_secs: u64, by_agent: bool) -> u64 {
        require!(self.pending.len() < MAX_PENDING, "Too many requests waiting");
        let id = self.next_id;
        self.next_id += 1;
        let unlock_secs = now_secs() + wait_secs;
        self.pending.push(Request { id, action, unlock_secs, by_agent, vouched: false });
        env::log_str(&format!("wall: request {id} waits until {unlock_secs}"));
        id
    }

    /// Adds to today's spending when it fits under the limit.
    fn spend(&mut self, token: &Option<AccountId>, amount: u128, per_day: u128) -> bool {
        let now = now_secs();
        let i = match self.spent.iter().position(|s| s.token == *token) {
            Some(i) => i,
            None => {
                self.spent.push(Spent { token: token.clone(), amount: U128(0), start_secs: now });
                self.spent.len() - 1
            }
        };
        let s = &mut self.spent[i];
        if now >= s.start_secs + DAY_SECS {
            s.amount = U128(0);
            s.start_secs = now;
        }
        match s.amount.0.checked_add(amount) {
            Some(total) if total <= per_day => {
                s.amount = U128(total);
                true
            }
            _ => false,
        }
    }

    /// Also empties the queue: a request that waited through a freeze must not run the moment it lifts.
    fn set_frozen(&mut self) {
        self.frozen = true;
        self.pending.clear();
        for slot in SLOTS {
            let args = json!({ "frozen": true, "slot": slot }).to_string().into_bytes();
            let _ = Promise::new(self.guardian.clone()).function_call("set_frozen".to_string(), args, NearToken::from_near(0), FREEZE_GAS);
        }
        env::log_str("wall: frozen");
    }

    /// True when a bait key signed this transaction: it freezes, and the call does nothing more.
    /// It must not panic, or the freeze would be undone with the call.
    fn tripped(&mut self) -> bool {
        require!(env::prepaid_gas() >= MIN_GAS, "Attach at least 60 TGas");
        let hash = env::sha256(env::signer_account_pk().as_bytes());
        if !self.tripwires.contains(&hash) {
            return false;
        }
        env::log_str("wall: tripwire");
        if !self.frozen {
            self.set_frozen();
        }
        true
    }
}
