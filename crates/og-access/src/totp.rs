//! TOTP (RFC 6238: HMAC-SHA1, 30 s steps, 6 digits) and base32 for authenticator apps.

use hmac::{Hmac, Mac};

const B32: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for b in data {
        buf = (buf << 8) | *b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let v = B32.iter().position(|x| *x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

pub fn code_at(secret: &[u8], step: u64) -> u32 {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(&step.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let bin = ((h[off] as u32 & 0x7f) << 24) | ((h[off + 1] as u32) << 16) | ((h[off + 2] as u32) << 8) | h[off + 3] as u32;
    bin % 1_000_000
}

/// Accepts the current step and one either side (clock drift). Returns the matching step,
/// so the caller can refuse a code that was already used.
pub fn verify(secret: &[u8], code: &str, unix_secs: u64) -> Option<u64> {
    let code: u32 = code.trim().replace(' ', "").parse().ok()?;
    let step = unix_secs / 30;
    (step.saturating_sub(1)..=step + 1).find(|s| code_at(secret, *s) == code)
}

pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let enc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
    format!("otpauth://totp/{}:{}?secret={secret_b32}&issuer={}&algorithm=SHA1&digits=6&period=30", enc(issuer), enc(account), enc(issuer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B, SHA1 secret "12345678901234567890", 8-digit codes truncated to 6
        let secret = b"12345678901234567890";
        assert_eq!(code_at(secret, 59 / 30), 287082);
        assert_eq!(code_at(secret, 1111111109 / 30), 81804);
        assert_eq!(code_at(secret, 1234567890 / 30), 5924);
        assert_eq!(verify(secret, "287082", 59), Some(1));
        assert_eq!(verify(secret, "287082", 59 + 30), Some(1)); // one step late is fine
        assert_eq!(verify(secret, "287082", 59 + 90), None);
        let b = base32_encode(secret);
        assert_eq!(b, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
        assert_eq!(base32_decode(&b).unwrap(), secret);
    }
}
