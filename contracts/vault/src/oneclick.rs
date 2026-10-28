//! Bunker exit: checking a NEAR Intents 1Click quote on-chain before the vault pays its deposit address.
//!
//! 1Click signs every quote with its manager key (ed25519). The signed message is
//! base58(sha256(stable JSON of the request fields + quote fields + timestamp)) — the same rule as
//! 1Click's SDK `verifyQuoteSignature`. The caller passes that exact JSON string and the signature;
//! the vault verifies the signature and then reads the fields it cares about from the same string:
//! the recipient must be the owner's Orchard address, the asset must be native ZEC, refunds must come
//! back to the vault, the quote must be live, and the deposit address must be a NEAR implicit account.
//! A deposit address taken from anywhere else (an agent, a web page) never reaches the vault's funds.

use near_sdk::serde_json::Value;
use near_sdk::{AccountId, env};

/// 1Click's quote-signing key (docs "Verify Quote Signatures"; also in KINU's intentsSignature.js).
pub const ONECLICK_MANAGER_KEY: &str = "reYaWhvwu8Jzo3WUM3zhn6VrhuMEF4eADL17qtRVifc";
/// NEAR Intents asset id of ZEC on the Zcash chain.
pub const ZEC_NATIVE: &str = "nep141:zec.omft.near";

#[derive(Debug, PartialEq)]
pub struct BunkerQuote {
    pub deposit_address: AccountId,
    pub amount_in: u128,
    pub deposit_memo: Option<String>,
    pub deadline_secs: u64,
}

pub struct Expect<'a> {
    pub token: &'a AccountId,
    pub recipient: &'a str,
    pub refund_to: &'a AccountId,
    pub now_secs: u64,
}

pub fn verify_and_read(signed: &str, signature: &str, manager_key: &[u8; 32], expect: &Expect) -> Result<BunkerQuote, &'static str> {
    let sig = b58_decode(signature.strip_prefix("ed25519:").unwrap_or(signature)).ok_or("Signature is not base58")?;
    let sig: [u8; 64] = sig.try_into().map_err(|_| "Signature must be 64 bytes")?;
    let message = b58_encode(&env::sha256(signed.as_bytes()));
    if !env::ed25519_verify(&sig, message.as_bytes(), manager_key) {
        return Err("Quote is not signed by NEAR Intents 1Click");
    }
    let v: Value = near_sdk::serde_json::from_str(signed).map_err(|_| "Signed quote is not JSON")?;
    let s = |k: &str| v.get(k).and_then(Value::as_str);
    if v.get("dry").and_then(Value::as_bool) != Some(false) {
        return Err("Quote is a dry run");
    }
    if s("recipient") != Some(expect.recipient) {
        return Err("Quote recipient is not the owner's Zcash exit address");
    }
    if s("recipientType") != Some("DESTINATION_CHAIN") || s("destinationAsset") != Some(ZEC_NATIVE) {
        return Err("Quote does not pay out native ZEC on Zcash");
    }
    if s("originAsset") != Some(format!("nep141:{}", expect.token).as_str()) || s("depositType") != Some("ORIGIN_CHAIN") {
        return Err("Quote is for another token");
    }
    if s("refundTo") != Some(expect.refund_to.as_str()) || s("refundType") != Some("ORIGIN_CHAIN") {
        return Err("Quote refunds would not come back to the vault");
    }
    let deadline_secs = s("deadline").and_then(iso_to_secs).ok_or("Quote has no valid deadline")?;
    if deadline_secs <= expect.now_secs + 60 {
        return Err("Quote has expired");
    }
    let deposit = s("depositAddress").ok_or("Quote has no deposit address")?;
    if deposit.len() != 64 || !deposit.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err("Deposit address is not a NEAR implicit account");
    }
    let amount_in: u128 = s("amountIn").and_then(|a| a.parse().ok()).ok_or("Quote has no amountIn")?;
    if amount_in == 0 {
        return Err("Quote amount is zero");
    }
    Ok(BunkerQuote {
        deposit_address: deposit.parse().map_err(|_| "Deposit address is not a NEAR account")?,
        amount_in,
        deposit_memo: s("depositMemo").map(str::to_string),
        deadline_secs,
    })
}

pub fn manager_key_bytes(b58: &str) -> Option<[u8; 32]> {
    b58_decode(b58)?.try_into().ok()
}

// ---------- base58 (Bitcoin alphabet) ----------

const B58: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

pub fn b58_encode(bytes: &[u8]) -> String {
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
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    std::iter::repeat_n(b'1', zeros).chain(digits.iter().rev().map(|d| B58[*d as usize])).map(char::from).collect()
}

pub fn b58_decode(s: &str) -> Option<Vec<u8>> {
    let mut bytes: Vec<u8> = Vec::new();
    for c in s.bytes() {
        let mut carry = B58.iter().position(|x| *x == c)? as u32;
        for b in bytes.iter_mut() {
            carry += u32::from(*b) * 58;
            *b = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    let zeros = s.bytes().take_while(|c| *c == b'1').count();
    Some(std::iter::repeat_n(0u8, zeros).chain(bytes.into_iter().rev()).collect())
}

/// "2026-10-31T16:46:01.808Z" -> seconds since 1970 (UTC only, as 1Click sends).
pub fn iso_to_secs(t: &str) -> Option<u64> {
    let b = t.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || !t.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3_600 + mm * 60 + ss).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::test_utils::VMContextBuilder;
    use near_sdk::testing_env;

    const FIXTURE: &str = include_str!("fixtures/oneclick-quote.json");
    const UA: &str = "u1fasx684eaqjej3rvn7kjnjsymdm8mdv66nuym9uh9xznmf3skz6zz0qt3q3cflw8cgy8cfndran77a4e797q7t32u779q9halyx7djty";
    const USDC: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";
    const VAULT: &str = "74890a9ca6436e1da0b9a730a020223756e520c9812b97804b5c4a13be19e6c1";
    // The quote's timestamp (28 Oct 2026 16:16 UTC); its deadline is 31 Oct.
    const NOW: u64 = 1_793_204_164;

    fn fixture() -> (String, String) {
        testing_env!(VMContextBuilder::new().build());
        let v: Value = near_sdk::serde_json::from_str(FIXTURE).unwrap();
        (v["signed"].as_str().unwrap().to_string(), v["signature"].as_str().unwrap().to_string())
    }
    fn key() -> [u8; 32] {
        manager_key_bytes(ONECLICK_MANAGER_KEY).unwrap()
    }
    fn check(signed: &str, sig: &str, recipient: &str, now: u64) -> Result<BunkerQuote, &'static str> {
        let (token, vault): (AccountId, AccountId) = (USDC.parse().unwrap(), VAULT.parse().unwrap());
        verify_and_read(signed, sig, &key(), &Expect { token: &token, recipient, refund_to: &vault, now_secs: now })
    }

    #[test]
    fn real_1click_quote_is_accepted() {
        let (signed, sig) = fixture();
        let q = check(&signed, &sig, UA, NOW).unwrap();
        assert_eq!(q.deposit_address.as_str(), "7ac092ef09708fa74b424218943cc91718635989175ea7472dee460287726680");
        assert_eq!(q.amount_in, 3_000_000);
    }

    #[test]
    fn tampered_quote_is_refused() {
        let (signed, sig) = fixture();
        // An agent swaps in its own deposit address: the signature no longer matches.
        let evil = signed.replace("7ac092ef", "0000beef");
        assert_eq!(check(&evil, &sig, UA, NOW), Err("Quote is not signed by NEAR Intents 1Click"));
    }

    #[test]
    fn quote_for_another_recipient_is_refused() {
        let (signed, sig) = fixture();
        let other = "u1mvjnzd2jv4mwpzv0v5ze2p8d0txt4erah3ffx2m68wz74et90nx2wzhvxkv4kdaw2e5lv5c59f8r8j3cnev277jjgljywujnkct3q6kq";
        assert_eq!(check(&signed, &sig, other, NOW), Err("Quote recipient is not the owner's Zcash exit address"));
    }

    #[test]
    fn expired_quote_is_refused() {
        let (signed, sig) = fixture();
        assert_eq!(check(&signed, &sig, UA, NOW + 4 * 86_400), Err("Quote has expired"));
    }

    #[test]
    fn base58_and_dates() {
        assert_eq!(b58_decode(&b58_encode(&[0, 0, 1, 2, 255])).unwrap(), vec![0, 0, 1, 2, 255]);
        assert_eq!(iso_to_secs("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(iso_to_secs("2026-10-28T16:16:04.811Z"), Some(NOW));
        assert_eq!(iso_to_secs("not a date"), None);
    }
}
