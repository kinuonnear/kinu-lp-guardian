//! Zcash exit addresses: the vault only accepts a mainnet Unified Address (ZIP 316) that contains an
//! Orchard receiver, so a "bunker exit" really lands in the shielded pool. Transparent (t1/t3/tex1)
//! and Sapling-only (zs1, or a UA without Orchard) addresses are refused.
//!
//! Decoding: bech32m (HRP "u", no length limit) -> F4Jumble^-1 -> strip the 16-byte HRP padding ->
//! a list of (typecode, length, bytes) items.

use blake2b_simd::Params;

pub const TYPE_P2PKH: u64 = 0x00;
pub const TYPE_P2SH: u64 = 0x01;
pub const TYPE_SAPLING: u64 = 0x02;
pub const TYPE_ORCHARD: u64 = 0x03;

#[derive(Debug, PartialEq, Eq)]
pub enum ZecAddressError {
    NotUnified,
    BadEncoding,
    BadPadding,
    BadItems,
    NoOrchardReceiver,
}

impl ZecAddressError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::NotUnified => "Exit address must be a Zcash unified address (u1...) with an Orchard receiver; t1/t3/tex1 and zs1 are not private",
            Self::BadEncoding => "Exit address is not valid bech32m",
            Self::BadPadding => "Exit address padding is wrong (not a mainnet unified address)",
            Self::BadItems => "Exit address receivers could not be read",
            Self::NoOrchardReceiver => "Exit address has no Orchard receiver, so the exit would not be fully shielded",
        }
    }
}

/// Typecodes of the receivers in a valid mainnet UA that includes an Orchard receiver.
pub fn validate_orchard_ua(address: &str) -> Result<Vec<u64>, ZecAddressError> {
    let lower = address.to_ascii_lowercase();
    if !lower.starts_with("u1") {
        return Err(ZecAddressError::NotUnified);
    }
    if address != lower && address != address.to_ascii_uppercase() {
        return Err(ZecAddressError::BadEncoding);
    }
    let raw = bech32m_decode("u", &lower).ok_or(ZecAddressError::BadEncoding)?;
    if raw.len() < 48 || raw.len() > 4194368 {
        return Err(ZecAddressError::BadEncoding);
    }
    let plain = f4jumble_inv(&raw);
    let (body, pad) = plain.split_at(plain.len() - 16);
    let mut want = [0u8; 16];
    want[0] = b'u';
    if pad != want {
        return Err(ZecAddressError::BadPadding);
    }
    let types = parse_items(body).ok_or(ZecAddressError::BadItems)?;
    if !types.contains(&TYPE_ORCHARD) {
        return Err(ZecAddressError::NoOrchardReceiver);
    }
    Ok(types)
}

// ---------- bech32m (BIP 350), without the 90-character limit ----------

const CHARSET: &[u8; 32] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
const BECH32M_CONST: u32 = 0x2bc8_30a3;

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a_57b2, 0x2650_8e6d, 0x1ea1_19fa, 0x3d42_33dd, 0x2a14_62b3];
    let mut chk: u32 = 1;
    for v in values {
        let top = chk >> 25;
        chk = ((chk & 0x01ff_ffff) << 5) ^ u32::from(*v);
        for (i, g) in GEN.iter().enumerate() {
            if (top >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn bech32m_decode(hrp: &str, s: &str) -> Option<Vec<u8>> {
    let pos = s.rfind('1')?;
    if &s[..pos] != hrp || s.len() - pos - 1 < 6 {
        return None;
    }
    let mut data = Vec::with_capacity(s.len() - pos - 1);
    for c in s[pos + 1..].bytes() {
        data.push(CHARSET.iter().position(|x| *x == c)? as u8);
    }
    let mut check: Vec<u8> = hrp.bytes().map(|b| b >> 5).collect();
    check.push(0);
    check.extend(hrp.bytes().map(|b| b & 31));
    check.extend_from_slice(&data);
    if polymod(&check) != BECH32M_CONST {
        return None;
    }
    data.truncate(data.len() - 6);
    // 5-bit groups -> bytes; leftover bits must be zero padding (< 5 bits).
    let (mut acc, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for v in data {
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if bits >= 5 || (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

// ---------- F4Jumble (ZIP 316) ----------

fn h_pers(i: u8, u: &[u8], len: usize) -> Vec<u8> {
    let mut p = [0u8; 16];
    p[..13].copy_from_slice(b"UA_F4Jumble_H");
    p[13] = i;
    Params::new().hash_length(len).personal(&p).hash(u).as_bytes().to_vec()
}

fn g_pers(i: u8, u: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64);
    let mut j: u16 = 0;
    while out.len() < len {
        let mut p = [0u8; 16];
        p[..13].copy_from_slice(b"UA_F4Jumble_G");
        p[13] = i;
        p[14..16].copy_from_slice(&j.to_le_bytes());
        out.extend_from_slice(Params::new().hash_length(64).personal(&p).hash(u).as_bytes());
        j += 1;
    }
    out.truncate(len);
    out
}

fn xor(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b).map(|(x, y)| x ^ y).collect()
}

fn f4jumble_inv(m: &[u8]) -> Vec<u8> {
    let l_l = core::cmp::min(64, m.len() / 2);
    let l_r = m.len() - l_l;
    let (c, d) = m.split_at(l_l);
    let y = xor(c, &h_pers(1, d, l_l));
    let x = xor(d, &g_pers(1, &y, l_r));
    let a = xor(&y, &h_pers(0, &x, l_l));
    let b = xor(&x, &g_pers(0, &a, l_r));
    [a, b].concat()
}

#[cfg(test)]
fn f4jumble(m: &[u8]) -> Vec<u8> {
    let l_l = core::cmp::min(64, m.len() / 2);
    let l_r = m.len() - l_l;
    let (a, b) = m.split_at(l_l);
    let x = xor(b, &g_pers(0, a, l_r));
    let y = xor(a, &h_pers(0, &x, l_l));
    let d = xor(&x, &g_pers(1, &y, l_r));
    let c = xor(&y, &h_pers(1, &d, l_l));
    [c, d].concat()
}

// ---------- receiver items ----------

fn compact_size(b: &[u8], at: &mut usize) -> Option<u64> {
    let first = *b.get(*at)?;
    *at += 1;
    let n = match first {
        0..=0xfc => return Some(u64::from(first)),
        0xfd => 2,
        0xfe => 4,
        0xff => 8,
    };
    let bytes = b.get(*at..*at + n)?;
    *at += n;
    let mut v = 0u64;
    for (i, x) in bytes.iter().enumerate() {
        v |= u64::from(*x) << (8 * i);
    }
    Some(v)
}

fn parse_items(body: &[u8]) -> Option<Vec<u64>> {
    let mut at = 0;
    let mut types: Vec<u64> = Vec::new();
    while at < body.len() {
        let t = compact_size(body, &mut at)?;
        let len = compact_size(body, &mut at)? as usize;
        let expected = match t {
            TYPE_P2PKH | TYPE_P2SH => Some(20),
            TYPE_SAPLING | TYPE_ORCHARD => Some(43),
            _ => None,
        };
        if expected.is_some_and(|e| e != len) {
            return None;
        }
        at = at.checked_add(len)?;
        if at > body.len() {
            return None;
        }
        // ZIP 316: typecodes strictly ascending, no duplicates.
        if types.last().is_some_and(|last| *last >= t) {
            return None;
        }
        types.push(t);
    }
    // A UA must have at least one shielded receiver.
    if !types.iter().any(|t| *t == TYPE_SAPLING || *t == TYPE_ORCHARD) {
        return None;
    }
    Some(types)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test strings from the NEAR Intents SDK (intents-sdk validateAddress.spec.ts, Oct 2026).
    const UA_REAL_ORCHARD_ONLY: &str = "u1fasx684eaqjej3rvn7kjnjsymdm8mdv66nuym9uh9xznmf3skz6zz0qt3q3cflw8cgy8cfndran77a4e797q7t32u779q9halyx7djty";
    const UA_ORCHARD_ONLY: &str = "u1mvjnzd2jv4mwpzv0v5ze2p8d0txt4erah3ffx2m68wz74et90nx2wzhvxkv4kdaw2e5lv5c59f8r8j3cnev277jjgljywujnkct3q6kq";
    const UA_SAPLING_ORCHARD: &str = "u1x08faycv384llwet8r8zedp0w8axcjtckhpv03sj0anft683j2ku9lfamp5q60avusdt4xkg2rlf6nq7pxy444jm3lwtequrzwxm5d8dgcy023fjl4hh4j68c8uuy79v6dk4j5w042zl5wk3vgwatvfr8rlky09vwkjw5yltrqreyr3f";
    const UA_P2PKH_SAPLING_ORCHARD: &str = "u1ngljcknkpc3k59rg493pz2fck9u7pd3fhq7yc45rt92s3c97rcm3nxxs659vn9d8u4n4x65yfsav90t0am2yea9huj8fgk8hzmh5w5g0sxdrm4q2dyp4jq839srf435uwxnvjf2xzaa9awprt76zrq45ppy60rzlru6p2t87el9m3syfn36jfdl5ts0kmyd629paqwfcqddnw8s6t3l";
    const UA_SAPLING_ONLY: &str = "u1c4q5lx2m27v5n8m0v8eeehd3zy43a27v8ze7thlns6njkea00f8p5e6an3hhsa3cmzvhdcgxjmyfpw6v8wxq35gh74hsrw7z2c2ze2y7";
    const UA_P2PKH_SAPLING: &str = "u1t9qnfm852emx7a8am0pg5ygh3s4astw5t60lnw6w40043vz8hz6v8a8ew8yk79wfykemjdfw4h7gkpsmyw4nkqud2f826mrx2rmr6xpun05mdn50jd08qvdspqvtnrt32xsn50wyzdt";

    #[test]
    fn f4jumble_round_trip() {
        for len in [48usize, 83, 128, 200] {
            let m: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(f4jumble_inv(&f4jumble(&m)), m);
        }
    }

    #[test]
    fn accepts_uas_with_orchard() {
        assert_eq!(validate_orchard_ua(UA_REAL_ORCHARD_ONLY), Ok(vec![TYPE_ORCHARD]));
        assert_eq!(validate_orchard_ua(UA_ORCHARD_ONLY), Ok(vec![TYPE_ORCHARD]));
        assert_eq!(validate_orchard_ua(UA_SAPLING_ORCHARD), Ok(vec![TYPE_SAPLING, TYPE_ORCHARD]));
        assert_eq!(validate_orchard_ua(UA_P2PKH_SAPLING_ORCHARD), Ok(vec![TYPE_P2PKH, TYPE_SAPLING, TYPE_ORCHARD]));
    }

    #[test]
    fn refuses_uas_without_orchard() {
        assert_eq!(validate_orchard_ua(UA_SAPLING_ONLY), Err(ZecAddressError::NoOrchardReceiver));
        assert_eq!(validate_orchard_ua(UA_P2PKH_SAPLING), Err(ZecAddressError::NoOrchardReceiver));
    }

    #[test]
    fn refuses_transparent_sapling_and_garbage() {
        assert_eq!(validate_orchard_ua("t1Q879cLgqaCd7zKRi79wQYuGBenmNX6cKn"), Err(ZecAddressError::NotUnified));
        assert_eq!(validate_orchard_ua("tex1s2rt77ggv6q989lr49rkgzmh5slsksa9khdgte"), Err(ZecAddressError::NotUnified));
        assert_eq!(validate_orchard_ua("zs13tacfgraukryw9y0jxxxxxx"), Err(ZecAddressError::NotUnified));
        assert_eq!(validate_orchard_ua("u1r2VHBwC5eAnZB22YNFSJg8iFtWgoAEKW000"), Err(ZecAddressError::BadEncoding));
        // One character changed: checksum fails.
        let mut s = UA_REAL_ORCHARD_ONLY.to_string();
        s.replace_range(10..11, if &s[10..11] == "q" { "p" } else { "q" });
        assert_eq!(validate_orchard_ua(&s), Err(ZecAddressError::BadEncoding));
    }
}
