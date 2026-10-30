//! Tax tokens (nearly.trade style, e.g. $KINU): 1% on every transfer into Rhea (sell) and out of Rhea
//! (buy), the tax to the token's own account, `ft_resolve_transfer` reporting net + tax as used. The
//! test token's `new_taxed` copies what mainnet showed on 30 Oct 2026 (contracts/testtoken).
//!
//! Part 1, v1.4 as deployed on mainnet (fixture shared-v1.4.wasm): a deposit is credited what the
//! token reported (100%) while Rhea holds 99%, and a withdrawal of what Rhea holds arrives at 99%
//! while the contract tries to pay 100%: the payout fails and the user is owed what cannot be paid.
//!
//! Part 2, the build of this commit: every token in and out is measured. Deposits credit what Rhea
//! received, payouts send what arrived, a refused payout is owed exactly that, and after everyone
//! leaves the contract holds nothing on Rhea and nothing in its own token wallet. Covers a tax pool
//! with wNEAR and one with two tokens that are not wNEAR (cash_out pays both in one transaction), and
//! claim_fees on real trading fees (onto the ledger, and straight to the wallet).

use near_api::{AccountId, Contract, NearGas, NearToken, NetworkConfig, Signer};
use serde_json::{Value, json};
use std::sync::Arc;

const TAX: &str = "tax.sandbox";
const TKQ: &str = "tkq.sandbox";
const WRAP: &str = "wrap.sandbox";
const DCL: &str = "dcl.sandbox";
const OLD: &str = "old.sandbox"; // v1.4
const G: &str = "guardian.sandbox"; // this build
const POOL_W: &str = "tax.sandbox|wrap.sandbox|10000";
const POOL_Q: &str = "tax.sandbox|tkq.sandbox|10000";
const E24: u128 = 1_000_000_000_000_000_000_000_000;
const E12: u128 = 1_000_000_000_000;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}
fn built(name: &str) -> Vec<u8> {
    let path = format!("{}/../target/near/{name}/{name}.wasm", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|_| panic!("{path} missing: run ./scripts/build.sh first"))
}
fn after_tax(a: u128) -> u128 {
    a - a / 100
}

struct T {
    net: NetworkConfig,
    signer: Arc<Signer>,
}
impl T {
    async fn call(&self, from: &str, to: &str, method: &str, args: Value, deposit: NearToken) -> String {
        let out = Contract(to.parse().unwrap()).call_function(method, args).transaction().gas(NearGas::from_tgas(300)).deposit(deposit).with_signer(from.parse().unwrap(), self.signer.clone()).send_to(&self.net).await;
        match out {
            Ok(r) => {
                let dbg = format!("{r:?}");
                let err = ["ExecutionError(\"", "Smart contract panicked: "].iter().find_map(|m| dbg.find(m).map(|i| dbg[i + m.len()..].split('"').next().unwrap_or("").to_string()));
                let why = "\\\"why\\\":\\\"";
                let refused = dbg.find(why).map(|i| dbg[i + why.len()..].split("\\\"").next().unwrap_or("").to_string());
                match (r.is_success(), err, refused) {
                    (true, None, None) => "ok".into(),
                    (true, None, Some(w)) => format!("ok (event: {w})"),
                    (_, Some(e), _) => format!("ERR {e}"),
                    (false, None, _) => "ERR (failed)".into(),
                }
            }
            Err(e) => format!("ERR {e}").chars().take(200).collect(),
        }
    }
    async fn view(&self, to: &str, method: &str, args: Value) -> Value {
        Contract(to.parse().unwrap()).call_function(method, args).read_only().fetch_from(&self.net).await.map(|r| r.data).unwrap_or(Value::Null)
    }
    async fn ft(&self, token: &str, who: &str) -> u128 {
        self.view(token, "ft_balance_of", json!({ "account_id": who })).await.as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    async fn rhea(&self, who: &str, token: &str) -> u128 {
        self.view(DCL, "list_user_assets", json!({ "account_id": who })).await[token].as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    async fn acct(&self, g: &str, who: &str) -> Value {
        self.view(g, "get_account", json!({ "user": who })).await
    }
    async fn near(&self, id: &str) -> u128 {
        near_api::Tokens::account(id.parse().unwrap()).near_balance().fetch_from(&self.net).await.map(|b| b.total.as_yoctonear()).unwrap_or(0)
    }
}
fn entry(a: &Value, list: &str, token: &str) -> u128 {
    a[list].as_array().into_iter().flatten().find(|e| e[0] == token).and_then(|e| e[1].as_str()).and_then(|s| s.parse().ok()).unwrap_or(0)
}
fn ok(label: &str, r: String) {
    println!("{label:<52} {r}");
    assert_eq!(r, "ok", "{label}");
}
fn show(label: &str, r: String) -> String {
    println!("{label:<52} {r}");
    r
}

#[tokio::test]
async fn tax_tokens_in_and_out() -> testresult::TestResult<()> {
    let sandbox = near_sandbox::Sandbox::start_sandbox().await?;
    let net = NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse()?);
    let signer = Signer::from_secret_key(near_sandbox::config::DEFAULT_GENESIS_ACCOUNT_PRIVATE_KEY.parse()?)?;
    for name in ["owner", "dcl", "wrap", "farm", "tax", "tkq", "lp", "old", "guardian", "erin", "alice", "bob", "carol", "dave"] {
        sandbox.create_account(format!("{name}.sandbox").parse::<AccountId>()?).initial_balance(NearToken::from_near(60)).send().await?;
    }
    let t = T { net: net.clone(), signer: signer.clone() };
    let deploy = |id: &str, code: Vec<u8>, method: &str, init: Value| {
        let (net, signer, id, method) = (net.clone(), signer.clone(), id.to_string(), method.to_string());
        async move {
            Contract::deploy(id.parse().unwrap()).use_code(code).with_init_call(&method, init).unwrap().with_signer(signer).send_to(&net).await.unwrap().assert_success();
        }
    };
    let (yocto, zero, reg) = (NearToken::from_yoctonear(1), NearToken::from_near(0), NearToken::from_millinear(125));
    deploy(WRAP, fixture("wrap-near.wasm"), "new", json!({})).await;
    deploy(DCL, kinu_guardian_tests::dcl_wasm(), "new", json!({ "owner_id": "owner.sandbox", "wnear_id": WRAP, "farming_contract_id": "farm.sandbox" })).await;
    let tax = json!({ "pair": DCL, "buy_bps": 100, "sell_bps": 100 });
    deploy(TAX, built("kinu_guardian_testtoken"), "new_taxed", json!({ "owner_id": "owner.sandbox", "total_supply": (1_000_000_000 * E24).to_string(), "symbol": "TAX", "decimals": 24, "tax": tax })).await;
    deploy(TKQ, built("kinu_guardian_testtoken"), "new", json!({ "owner_id": "owner.sandbox", "total_supply": (1_000_000_000 * E12).to_string(), "symbol": "TKQ", "decimals": 12 })).await;
    println!("{:<52} {}", "TAX get_tax", t.view(TAX, "get_tax", json!({})).await);

    ok("whitelist", t.call("owner.sandbox", DCL, "extend_whitelisted_tokens", json!({ "token_ids": [TAX, TKQ, WRAP] }), yocto).await);
    // TAX and wNEAR both 24 decimals at 1:1 (point 0); TAX/TKQ 1:1 in whole tokens (24 vs 12 decimals).
    ok("pool TAX/wNEAR", t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": TAX, "token_b": WRAP, "fee": 10000, "init_point": 0 }), NearToken::from_near(1)).await);
    ok("pool TAX/TKQ", t.call("owner.sandbox", DCL, "create_pool", json!({ "token_a": TAX, "token_b": TKQ, "fee": 10000, "init_point": -276400 }), NearToken::from_near(1)).await);
    for who in [DCL, "lp.sandbox", "erin.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox", "dave.sandbox"] {
        for tok in [TAX, TKQ, WRAP] {
            t.call("owner.sandbox", tok, "storage_deposit", json!({ "account_id": who, "registration_only": true }), reg).await;
        }
    }
    // Owner -> users is not a pair transfer: no tax.
    for who in ["lp.sandbox", "erin.sandbox", "alice.sandbox", "bob.sandbox", "carol.sandbox", "dave.sandbox"] {
        ok("fund TAX", t.call("owner.sandbox", TAX, "ft_transfer", json!({ "receiver_id": who, "amount": (100_000 * E24).to_string() }), yocto).await);
        ok("fund TKQ", t.call("owner.sandbox", TKQ, "ft_transfer", json!({ "receiver_id": who, "amount": (100_000 * E12).to_string() }), yocto).await);
    }
    assert_eq!(t.ft(TAX, "alice.sandbox").await, 100_000 * E24, "a transfer that is not to or from the pair is not taxed");
    ok("lp registers on Rhea", t.call("lp.sandbox", DCL, "storage_deposit", json!({ "account_id": "lp.sandbox", "registration_only": false }), NearToken::from_millinear(500)).await);
    ok("lp wraps", t.call("lp.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(30)).await);
    for (tok, amt) in [(TAX, 50_000 * E24), (TKQ, 50_000 * E12), (WRAP, 25 * E24)] {
        ok("lp deposits to Rhea", t.call("lp.sandbox", tok, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": amt.to_string(), "msg": "\"Deposit\"" }), yocto).await);
    }
    assert_eq!(t.rhea("lp.sandbox", TAX).await, after_tax(50_000 * E24), "Rhea credits the net amount (sell tax)");
    ok("lp liquidity TAX/wNEAR", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": POOL_W, "left_point": -20000, "right_point": 20000, "amount_x": (20 * E24).to_string(), "amount_y": (20 * E24).to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);
    ok("lp liquidity TAX/TKQ", t.call("lp.sandbox", DCL, "add_liquidity", json!({ "pool_id": POOL_Q, "left_point": -296400, "right_point": -256400, "amount_x": (20_000 * E24).to_string(), "amount_y": (20_000 * E12).to_string(), "min_amount_x": "0", "min_amount_y": "0" }), zero).await);

    let policy = |pool: &str| json!({ "allowed_pools": [pool], "min_width": 1400, "max_width": 5600, "max_distance": 3000, "max_actions": 12, "window_secs": 86400, "max_slippage_bps": 100, "exit_drop_points": 2877, "exit_on_rise": false, "rebalance_buffer_points": 100 });
    let setup = |g: &'static str, code: Vec<u8>| {
        let (t, deploy) = (&t, &deploy);
        async move {
            deploy(g, code, "new", json!({ "dcl": DCL, "wnear": WRAP, "owner": "owner.sandbox" })).await;
            ok("register_dcl", t.call("owner.sandbox", g, "register_dcl", json!({}), NearToken::from_millinear(500)).await);
            for tok in [TAX, TKQ, WRAP] {
                ok("register_token", t.call("owner.sandbox", g, "register_token", json!({ "token": tok }), reg).await);
            }
        }
    };

    // ---------------- Part 1: v1.4 as on mainnet ----------------
    println!("\n--- v1.4 (mainnet) ---");
    setup(OLD, fixture("shared-v1.4.wasm")).await;
    assert_eq!(t.view(OLD, "get_config", json!({})).await["version"], "1.4.0");
    ok("erin joins v1.4 (TAX/wNEAR)", t.call("erin.sandbox", OLD, "join", json!({ "policy": policy(POOL_W), "agent": null }), NearToken::from_millinear(10)).await);
    ok("erin deposits 1,000 TAX", t.call("erin.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": OLD, "amount": (1_000 * E24).to_string(), "msg": "" }), yocto).await);
    let ledger = entry(&t.acct(OLD, "erin.sandbox").await, "ledger", TAX);
    let on_rhea = t.rhea(OLD, TAX).await;
    println!("{:<52} ledger {} / Rhea holds {}", "v1.4 after the deposit", ledger as f64 / 1e24, on_rhea as f64 / 1e24);
    assert_eq!(ledger, 1_000 * E24, "v1.4 credits what the token reported");
    assert_eq!(on_rhea, after_tax(1_000 * E24), "Rhea holds 99%");
    let r = show("erin withdraws her whole ledger (1,000)", t.call("erin.sandbox", OLD, "withdraw", json!({ "token": TAX, "amount": null }), zero).await);
    assert!(r.contains("insufficient balance") && entry(&t.acct(OLD, "erin.sandbox").await, "ledger", TAX) == 1_000 * E24, "Rhea cannot release more than it holds; the ledger keeps 1,000");
    let wallet = t.ft(TAX, "erin.sandbox").await;
    let r = show("erin withdraws what Rhea holds (990)", t.call("erin.sandbox", OLD, "withdraw", json!({ "token": TAX, "amount": on_rhea.to_string() }), zero).await);
    let a = t.acct(OLD, "erin.sandbox").await;
    let got = t.ft(TAX, "erin.sandbox").await - wallet;
    println!("{:<52} {} ; erin got {} ; owed {} ; contract wallet {}", "v1.4 after the payout", r, got as f64 / 1e24, entry(&a, "owed", TAX) as f64 / 1e24, t.ft(TAX, OLD).await as f64 / 1e24);
    assert_eq!(got, 0, "v1.4: the payout of 990 failed (only 980.1 arrived)");
    assert_eq!(entry(&a, "owed", TAX), on_rhea, "v1.4: erin is owed 990");
    assert_eq!(t.ft(TAX, OLD).await, after_tax(on_rhea), "v1.4: the contract holds 980.1");
    let r = show("erin claims", t.call("erin.sandbox", OLD, "claim", json!({ "token": TAX }), zero).await);
    assert_eq!(t.ft(TAX, "erin.sandbox").await, wallet, "v1.4: the claim fails too: erin's tokens are stuck");
    assert!(r.contains("refused") || entry(&t.acct(OLD, "erin.sandbox").await, "owed", TAX) == on_rhea);
    println!("{:<52} ledger left {} TAX that Rhea does not hold", "v1.4 stranded", entry(&t.acct(OLD, "erin.sandbox").await, "ledger", TAX) as f64 / 1e24);

    // ---------------- Part 2: this build ----------------
    println!("\n--- this build ---");
    setup(G, built("kinu_guardian_shared")).await;
    println!("{:<52} {}", "version", t.view(G, "get_config", json!({})).await["version"]);
    for (who, pool) in [("alice.sandbox", POOL_W), ("bob.sandbox", POOL_W), ("carol.sandbox", POOL_W), ("dave.sandbox", POOL_Q)] {
        ok(&format!("{who} joins"), t.call(who, G, "join", json!({ "policy": policy(pool), "agent": null }), NearToken::from_millinear(10)).await);
    }
    // Deposits are measured: credited what Rhea received.
    ok("alice deposits 1,000 TAX", t.call("alice.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": G, "amount": (1_000 * E24).to_string(), "msg": "" }), yocto).await);
    assert_eq!(entry(&t.acct(G, "alice.sandbox").await, "ledger", TAX), after_tax(1_000 * E24), "credited what Rhea received");
    ok("alice wraps", t.call("alice.sandbox", WRAP, "near_deposit", json!({}), NearToken::from_near(3)).await);
    ok("alice deposits 2 wNEAR", t.call("alice.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": G, "amount": (2 * E24).to_string(), "msg": "" }), yocto).await);
    ok("alice opens across the price", t.call("alice.sandbox", G, "open", json!({ "left_point": -800, "right_point": 800 }), zero).await);
    ok("bob deposits 500 TAX", t.call("bob.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": G, "amount": (500 * E24).to_string(), "msg": "" }), yocto).await);
    ok("carol deposits 300 TAX", t.call("carol.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": G, "amount": (300 * E24).to_string(), "msg": "" }), yocto).await);
    ok("dave deposits 1,000 TAX", t.call("dave.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": G, "amount": (1_000 * E24).to_string(), "msg": "" }), yocto).await);
    ok("dave deposits 1,000 TKQ", t.call("dave.sandbox", TKQ, "ft_transfer_call", json!({ "receiver_id": G, "amount": (1_000 * E12).to_string(), "msg": "" }), yocto).await);
    ok("dave opens across the price", t.call("dave.sandbox", G, "open", json!({ "left_point": -277200, "right_point": -275600 }), zero).await);

    // Fees: trades through Alice's range, then she takes the fees without closing (claim_fees).
    let ledgers = |tok: &'static str| {
        let t = &t;
        async move {
            let mut sum = 0;
            for who in ["alice.sandbox", "bob.sandbox", "carol.sandbox", "dave.sandbox"] {
                sum += entry(&t.acct(G, who).await, "ledger", tok);
            }
            sum
        }
    };
    let swap = |pool: &str, out: &str| json!({ "Swap": { "pool_ids": [pool], "output_token": out, "min_output_amount": "0" } }).to_string();
    for _ in 0..3 {
        ok("lp sells 0.5 TAX through alice's range", t.call("lp.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": (E24 / 2).to_string(), "msg": swap(POOL_W, WRAP) }), yocto).await);
        ok("lp buys back with 0.5 wNEAR", t.call("lp.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": (E24 / 2).to_string(), "msg": swap(POOL_W, TAX) }), yocto).await);
    }
    let a0 = t.acct(G, "alice.sandbox").await;
    ok("alice claim_fees(false): fees onto her ledger", t.call("alice.sandbox", G, "claim_fees", json!({ "send": false }), zero).await);
    let a1 = t.acct(G, "alice.sandbox").await;
    let (fee_tax, fee_w) = (entry(&a1, "ledger", TAX) - entry(&a0, "ledger", TAX), entry(&a1, "ledger", WRAP) - entry(&a0, "ledger", WRAP));
    println!("{:<52} TAX +{} ; wNEAR +{}", "alice's fees on her ledger", fee_tax as f64 / 1e24, fee_w as f64 / 1e24);
    assert!(fee_tax > 0 && fee_w > 0, "both sides earned fees");
    assert_eq!(a1["position"], a0["position"], "the position stays");
    assert_eq!(t.rhea(G, TAX).await, ledgers(TAX).await, "Rhea balance == ledgers (TAX)");
    assert_eq!(t.rhea(G, WRAP).await, ledgers(WRAP).await, "Rhea balance == ledgers (wNEAR)");
    for _ in 0..3 {
        ok("lp sells 0.5 TAX", t.call("lp.sandbox", TAX, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": (E24 / 2).to_string(), "msg": swap(POOL_W, WRAP) }), yocto).await);
        ok("lp buys back", t.call("lp.sandbox", WRAP, "ft_transfer_call", json!({ "receiver_id": DCL, "amount": (E24 / 2).to_string(), "msg": swap(POOL_W, TAX) }), yocto).await);
    }
    let (tax_before, near_before) = (t.ft(TAX, "alice.sandbox").await, t.near("alice.sandbox").await);
    ok("alice claim_fees(true): fees to her wallet", t.call("alice.sandbox", G, "claim_fees", json!({ "send": true }), zero).await);
    let a2 = t.acct(G, "alice.sandbox").await;
    let got_tax = t.ft(TAX, "alice.sandbox").await - tax_before;
    println!("{:<52} TAX +{} ; NEAR {:+.6} (wNEAR fees less gas)", "alice's fees in her wallet", got_tax as f64 / 1e24, (t.near("alice.sandbox").await as f64 - near_before as f64) / 1e24);
    assert!(got_tax > 0, "TAX fees arrived (less the buy tax)");
    assert_eq!(a2["ledger"], a1["ledger"], "sent straight on: the ledger is unchanged");
    assert_eq!(a2["position"], a0["position"], "the position stays");
    assert_eq!(t.ft(TAX, G).await, 0, "nothing left in the contract's token wallet");
    assert_eq!(t.rhea(G, TAX).await, ledgers(TAX).await, "Rhea balance == ledgers (TAX)");

    // Bob withdraws his whole ledger: he gets what arrived (99%), nothing stays owed or stranded.
    let held = entry(&t.acct(G, "bob.sandbox").await, "ledger", TAX);
    let before = t.ft(TAX, "bob.sandbox").await;
    ok("bob withdraws all his TAX", t.call("bob.sandbox", G, "withdraw", json!({ "token": TAX, "amount": null }), zero).await);
    let a = t.acct(G, "bob.sandbox").await;
    let got = t.ft(TAX, "bob.sandbox").await - before;
    println!("{:<52} ledger {} -> got {} (buy tax 1%), owed {}", "bob", held as f64 / 1e24, got as f64 / 1e24, entry(&a, "owed", TAX));
    assert_eq!(got, after_tax(held), "bob got exactly what arrived");
    assert_eq!(entry(&a, "ledger", TAX), 0);
    assert_eq!(entry(&a, "owed", TAX), 0, "nothing owed");
    assert_eq!(t.ft(TAX, G).await, 0, "nothing left in the contract's token wallet");
    ok("bob leaves", t.call("bob.sandbox", G, "leave", json!({}), zero).await);

    // Carol's wallet refuses (unregistered): she is owed exactly what arrived, and her claim pays it.
    ok("carol unregisters from TAX", t.call("carol.sandbox", TAX, "storage_unregister", json!({ "force": true }), yocto).await);
    let held = entry(&t.acct(G, "carol.sandbox").await, "ledger", TAX);
    show("carol withdraws (her wallet refuses)", t.call("carol.sandbox", G, "withdraw", json!({ "token": TAX, "amount": null }), zero).await);
    let owed = entry(&t.acct(G, "carol.sandbox").await, "owed", TAX);
    println!("{:<52} ledger {} -> owed {}", "carol", held as f64 / 1e24, owed as f64 / 1e24);
    assert_eq!(owed, after_tax(held), "owed what arrived");
    assert_eq!(t.ft(TAX, G).await, owed, "the contract's token wallet holds exactly what it owes");
    ok("carol registers again", t.call("carol.sandbox", TAX, "storage_deposit", json!({ "account_id": "carol.sandbox", "registration_only": true }), reg).await);
    ok("carol claims", t.call("carol.sandbox", G, "claim", json!({ "token": TAX }), zero).await);
    assert_eq!(t.ft(TAX, "carol.sandbox").await, owed, "the claim paid it in full");
    assert_eq!(t.ft(TAX, G).await, 0);
    ok("carol leaves", t.call("carol.sandbox", G, "leave", json!({}), zero).await);

    // Alice cashes out with an open position (TAX + wNEAR), Dave with TAX + TKQ: one transaction each.
    for (who, other) in [("alice.sandbox", WRAP), ("dave.sandbox", TKQ)] {
        let (tax_before, other_before, near_before) = (t.ft(TAX, who).await, t.ft(other, who).await, t.near(who).await);
        ok(&format!("{who} cash_out(leave) with a position"), t.call(who, G, "cash_out", json!({ "leave": true }), zero).await);
        let (got_tax, got_other) = (t.ft(TAX, who).await - tax_before, t.ft(other, who).await.saturating_sub(other_before));
        let got_near = t.near(who).await.saturating_sub(near_before);
        println!("{:<52} TAX +{} ; {other} +{} ; NEAR +{}", who, got_tax as f64 / 1e24, got_other, got_near as f64 / 1e24);
        assert!(got_tax > 0, "TAX came out");
        assert_eq!(t.acct(G, who).await, Value::Null, "the record is gone (left)");
        assert_eq!(t.ft(TAX, G).await, 0, "nothing left in the contract's TAX wallet");
    }

    // The contract ends empty: no users, nothing on Rhea, nothing in its own token wallets.
    assert_eq!(t.view(G, "get_users", json!({})).await, json!([]));
    let rhea = t.view(DCL, "list_user_assets", json!({ "account_id": G })).await;
    println!("{:<52} {rhea}", "contract on Rhea");
    assert!(rhea.as_object().unwrap().values().all(|v| v == "0"), "nothing left on Rhea");
    assert_eq!(t.view(DCL, "list_liquidities", json!({ "account_id": G })).await, json!([]));
    for tok in [TAX, TKQ] {
        assert_eq!(t.ft(tok, G).await, 0, "nothing left in the contract's {tok} wallet");
    }
    Ok(())
}
