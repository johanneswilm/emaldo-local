// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Small shared helpers: base64 (no dep) and salted/iterated SHA-256
// password hashing (ring, already in the tree via rustls).

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(s.len());
    for &c in s.as_bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => continue,
            b'\n' | b'\r' => continue,
            _ => return None,
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let n = chunk.iter().fold(0u32, |a, &v| (a << 6) | v as u32)
            << (6 * (4 - chunk.len()));
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// PBKDF2-HMAC-SHA256, the format the bridge role stores in its user map.
/// Both roles must agree: the operator's password file is one file, and a
/// hash written by one role has to verify in the other.
pub const HASH_ITERATIONS: u32 = 100_000;

pub fn hash_password(pw: &str, salt_b64: &str) -> String {
    let salt = b64_decode(salt_b64).unwrap_or_else(|| salt_b64.as_bytes().to_vec());
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pw.as_bytes(), &salt, HASH_ITERATIONS, &mut out);
    b64_encode(&out)
}

/// Stable identity used by MQTT discovery and the web UI.
///
/// `/root/.device_id` is the id the vendor app itself carries, so the local
/// stack reuses it: Home Assistant keeps exactly one device per unit no matter
/// which role is publishing, and a reinstall does not fork the entity ids.
pub fn device_id(state_dir: &std::path::Path) -> String {
    if let Ok(id) = std::fs::read_to_string("/root/.device_id") {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let p = state_dir.join("device_id");
    if let Ok(id) = std::fs::read_to_string(&p) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let id = format!(
        "emaldo-{}",
        random_bytes(6).iter().map(|x| format!("{x:02x}")).collect::<String>()
    );
    let _ = std::fs::create_dir_all(state_dir);
    let _ = std::fs::write(&p, &id);
    id
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut b = vec![0u8; n];
    let _ = rng.fill(&mut b);
    b
}

pub fn new_salt() -> String {
    b64_encode(&random_bytes(16))
}

pub fn new_token() -> String {
    // URL-safe variant of the session token.
    b64_encode(&random_bytes(24))
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        for data in [&b""[..], &b"f"[..], &b"fo"[..], &b"foo"[..], &b"foob"[..], &b"fooba"[..], &b"foobar"[..]] {
            assert_eq!(b64_decode(&b64_encode(data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn hash_stable() {
        let salt = new_salt();
        assert_eq!(hash_password("secret", &salt), hash_password("secret", &salt));
        assert_ne!(hash_password("secret", &salt), hash_password("other", &salt));
    }
}
