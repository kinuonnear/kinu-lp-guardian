//! KINU Sentinel: reads a wall's queue and stops what looks stolen. Pure rules, no model: every call
//! can be replayed from the chain, and `RULES` is the whole list of them.
//!
//! The wall lets it do two things only, cancel and freeze, so the worst a wrong call does is delay an
//! honest payment. The canceller vouches for an honest request or dismisses the sentinel.
//!
//! Trust model: a run's input only names walls. The sentinel acts on a wall only if that wall runs the
//! published code (hash pinned in this build) and names this sentinel, and it decides from what it
//! reads on chain.
//!
//! A flagged request is left alone for the first half of its wait, so a person can see the alarm and
//! vouch for it. After that it is stopped: one request is cancelled, two or more freeze the wall.

use serde::Deserialize;

/// Two flagged requests due at once means someone is working the wall, not a mistake.
pub const FREEZE_AT: usize = 2;
pub const MAX_REASON: usize = 280;

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Limit {
    pub token: Option<String>,
    pub per_day: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct WallConfig {
    pub sentinel: Option<String>,
    pub home: String,
    pub delay_secs: u64,
    pub frozen: bool,
    pub agent: Option<String>,
    pub receivers: Vec<String>,
    pub limits: Vec<Limit>,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub enum WallAction {
    Send { token: Option<String>, to: String, amount: String },
    Guardian { method: String, args: String, deposit: String },
    SetHome { home: String },
    SetCanceller { canceller: String },
    SetSentinel { sentinel: Option<String> },
    SetDelay { delay_secs: u64 },
    SetAgent { agent: Option<String>, receivers: Vec<String>, limits: Vec<Limit> },
    RemoveTripwire { public_key: String },
    Unfreeze,
    Close,
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Request {
    pub id: u64,
    pub action: WallAction,
    pub unlock_secs: u64,
    pub by_agent: bool,
    pub vouched: bool,
}

/// What one request gets. `Alarm` is for what the wall will not let the sentinel cancel.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    Leave,
    Stop(&'static str),
    Alarm(&'static str),
}

/// Every rule, in order. A rule change that only narrows what can happen is left alone.
pub const RULES: [&str; 10] = [
    "Pays an account that is not on the wall's list",
    "The agent asks past its daily limit",
    "Changes the home account",
    "Changes the canceller",
    "Removes a bait key",
    "Closes the wall",
    "Shortens the wait",
    "Widens what the agent may do",
    "Changes Guardian rules",
    "Only a person can stop this",
];

fn parse(amount: &str) -> Option<u128> {
    amount.parse().ok()
}

/// True when the new agent settings can only do less than today's.
fn narrows(c: &WallConfig, agent: &Option<String>, receivers: &[String], limits: &[Limit]) -> bool {
    if agent.is_none() {
        return true;
    }
    agent == &c.agent
        && receivers.iter().all(|r| c.receivers.contains(r))
        && limits.iter().all(|l| {
            let was = c.limits.iter().find(|o| o.token == l.token);
            matches!(was.and_then(|o| parse(&o.per_day)).zip(parse(&l.per_day)), Some((old, new)) if new <= old)
        })
}

/// The rules for one request. A vouched request is the canceller's call, so it is left alone.
pub fn judge(c: &WallConfig, r: &Request) -> Call {
    if r.vouched {
        return Call::Leave;
    }
    match &r.action {
        WallAction::Send { to, .. } => {
            if *to != c.home && !c.receivers.contains(to) {
                Call::Stop(RULES[0])
            } else if r.by_agent {
                Call::Stop(RULES[1])
            } else {
                Call::Leave
            }
        }
        WallAction::SetHome { .. } => Call::Stop(RULES[2]),
        WallAction::SetCanceller { .. } => Call::Stop(RULES[3]),
        WallAction::RemoveTripwire { .. } => Call::Stop(RULES[4]),
        WallAction::Close => Call::Stop(RULES[5]),
        WallAction::SetDelay { delay_secs } if *delay_secs < c.delay_secs => Call::Stop(RULES[6]),
        WallAction::SetDelay { .. } => Call::Leave,
        WallAction::SetAgent { agent, receivers, limits } if !narrows(c, agent, receivers, limits) => Call::Stop(RULES[7]),
        WallAction::SetAgent { .. } => Call::Leave,
        WallAction::Guardian { .. } => Call::Stop(RULES[8]),
        WallAction::SetSentinel { .. } | WallAction::Unfreeze => Call::Alarm(RULES[9]),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Move {
    Nothing,
    Cancel { id: u64, reason: String },
    Freeze { reason: String },
}

/// One flagged request, for the alarm: `stop_at` is 0 when the sentinel cannot stop it.
#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub id: u64,
    pub why: &'static str,
    pub unlock_secs: u64,
    pub stop_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub act: Move,
    pub flags: Vec<Flag>,
}

/// When a flagged request stops being a person's to answer: half way through its wait.
pub fn stop_at(c: &WallConfig, r: &Request) -> u64 {
    r.unlock_secs.saturating_sub(c.delay_secs / 2)
}

/// What to do about a wall right now. At most one move, so a run has a known cost.
pub fn plan(c: &WallConfig, pending: &[Request], now_secs: u64) -> Plan {
    let mut flags = Vec::new();
    let mut due = Vec::new();
    for r in pending {
        match judge(c, r) {
            Call::Leave => {}
            Call::Alarm(why) => flags.push(Flag { id: r.id, why, unlock_secs: r.unlock_secs, stop_at: 0 }),
            Call::Stop(why) => {
                let at = stop_at(c, r);
                flags.push(Flag { id: r.id, why, unlock_secs: r.unlock_secs, stop_at: at });
                if now_secs >= at {
                    due.push((r.id, why));
                }
            }
        }
    }
    let act = if c.frozen || due.is_empty() {
        Move::Nothing
    } else if due.len() >= FREEZE_AT {
        Move::Freeze { reason: format!("{} flagged requests at once", due.len()) }
    } else {
        Move::Cancel { id: due[0].0, reason: format!("Sentinel: {}", due[0].1) }
    };
    Plan { act, flags }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;

    fn wall() -> WallConfig {
        WallConfig {
            sentinel: Some("s.near".into()),
            home: "home.near".into(),
            delay_secs: 24 * HOUR,
            frozen: false,
            agent: Some("a.near".into()),
            receivers: vec!["shop.near".into()],
            limits: vec![Limit { token: None, per_day: "100".into() }],
        }
    }
    fn req(id: u64, action: WallAction) -> Request {
        Request { id, action, unlock_secs: 100 * HOUR, by_agent: false, vouched: false }
    }
    fn send(to: &str) -> WallAction {
        WallAction::Send { token: None, to: to.into(), amount: "1".into() }
    }
    fn agent(agent: Option<&str>, receivers: &[&str], per_day: &str) -> WallAction {
        WallAction::SetAgent {
            agent: agent.map(String::from),
            receivers: receivers.iter().map(|s| s.to_string()).collect(),
            limits: vec![Limit { token: None, per_day: per_day.into() }],
        }
    }

    #[test]
    fn every_action_gets_its_rule() {
        let c = wall();
        let stops = [
            (send("thief.near"), 0),
            (WallAction::SetHome { home: "x.near".into() }, 2),
            (WallAction::SetCanceller { canceller: "x.near".into() }, 3),
            (WallAction::RemoveTripwire { public_key: "ed25519:x".into() }, 4),
            (WallAction::Close, 5),
            (WallAction::SetDelay { delay_secs: HOUR }, 6),
            (agent(Some("b.near"), &[], "1"), 7),
            (agent(Some("a.near"), &["shop.near", "new.near"], "1"), 7),
            (agent(Some("a.near"), &["shop.near"], "101"), 7),
            (WallAction::Guardian { method: "set_agent".into(), args: "{}".into(), deposit: "0".into() }, 8),
        ];
        for (action, rule) in stops {
            assert_eq!(judge(&c, &req(1, action.clone())), Call::Stop(RULES[rule]), "{action:?}");
        }
        for action in [send("shop.near"), send("home.near"), WallAction::SetDelay { delay_secs: 48 * HOUR }, agent(None, &[], "0"), agent(Some("a.near"), &["shop.near"], "50")] {
            assert_eq!(judge(&c, &req(1, action.clone())), Call::Leave, "{action:?}");
        }
        for action in [WallAction::Unfreeze, WallAction::SetSentinel { sentinel: None }] {
            assert_eq!(judge(&c, &req(1, action)), Call::Alarm(RULES[9]));
        }
    }

    #[test]
    fn the_agent_past_its_limit_is_stopped_even_to_a_listed_receiver() {
        let r = Request { by_agent: true, ..req(1, send("shop.near")) };
        assert_eq!(judge(&wall(), &r), Call::Stop(RULES[1]));
    }

    #[test]
    fn a_new_agent_limit_for_an_unknown_token_widens() {
        let a = WallAction::SetAgent { agent: Some("a.near".into()), receivers: vec![], limits: vec![Limit { token: Some("t.near".into()), per_day: "1".into() }] };
        assert_eq!(judge(&wall(), &req(1, a)), Call::Stop(RULES[7]));
    }

    #[test]
    fn a_vouched_request_is_left_alone() {
        let r = Request { vouched: true, ..req(1, send("thief.near")) };
        assert_eq!(judge(&wall(), &r), Call::Leave);
        assert_eq!(plan(&wall(), &[r], 100 * HOUR).act, Move::Nothing);
    }

    #[test]
    fn a_person_gets_the_first_half_of_the_wait() {
        let c = wall();
        let r = [req(1, send("thief.near"))];
        let half = 88 * HOUR; // unlocks at 100 h after a 24 h wait
        let early = plan(&c, &r, half - 1);
        assert_eq!(early.act, Move::Nothing);
        assert_eq!(early.flags[0].stop_at, half, "the alarm is out before the stop");
        assert_eq!(plan(&c, &r, half).act, Move::Cancel { id: 1, reason: format!("Sentinel: {}", RULES[0]) });
    }

    #[test]
    fn a_request_past_its_wait_is_still_cancelled() {
        assert!(matches!(plan(&wall(), &[req(1, send("thief.near"))], 200 * HOUR).act, Move::Cancel { id: 1, .. }));
    }

    #[test]
    fn two_due_at_once_freeze_and_a_later_one_does_not_count() {
        let c = wall();
        let two = [req(1, send("thief.near")), req(2, WallAction::SetHome { home: "x.near".into() })];
        assert!(matches!(plan(&c, &two, 100 * HOUR).act, Move::Freeze { .. }));
        let later = [req(1, send("thief.near")), Request { unlock_secs: 500 * HOUR, ..req(2, WallAction::Close) }];
        assert!(matches!(plan(&c, &later, 100 * HOUR).act, Move::Cancel { id: 1, .. }));
    }

    #[test]
    fn a_frozen_wall_needs_no_move_but_keeps_its_alarm() {
        let c = WallConfig { frozen: true, ..wall() };
        let p = plan(&c, &[req(1, WallAction::Unfreeze)], 100 * HOUR);
        assert_eq!(p.act, Move::Nothing);
        assert_eq!(p.flags.len(), 1);
        assert_eq!(p.flags[0].stop_at, 0);
    }

    #[test]
    fn honest_requests_make_no_noise() {
        let p = plan(&wall(), &[req(1, send("shop.near")), req(2, send("home.near"))], 100 * HOUR);
        assert_eq!(p, Plan { act: Move::Nothing, flags: vec![] });
    }

    #[test]
    fn reasons_fit_the_wall() {
        assert!(RULES.iter().all(|r| r.len() + "Sentinel: ".len() <= MAX_REASON));
    }

    #[test]
    fn reads_the_wall_view() {
        let v = r#"{"id":3,"action":{"Send":{"token":null,"to":"t.near","amount":"5"}},"unlock_secs":9,"by_agent":false,"vouched":true}"#;
        let r: Request = serde_json::from_str(v).unwrap();
        assert!(r.vouched);
        let u: Request = serde_json::from_str(r#"{"id":1,"action":"Unfreeze","unlock_secs":9,"by_agent":false,"vouched":false}"#).unwrap();
        assert_eq!(u.action, WallAction::Unfreeze);
        let c = r#"{"owner":"o","canceller":"k","sentinel":null,"home":"h","guardian":"g","delay_secs":3600,"frozen":false,"agent":null,"receivers":[],"limits":[{"token":null,"per_day":"7"}],"tripwires":1}"#;
        let c: WallConfig = serde_json::from_str(c).unwrap();
        assert_eq!(c.limits[0].per_day, "7");
    }
}
