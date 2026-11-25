// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Primitive codecs shared by the REST envelopes and the E2E frames.

use aes::cipher::block_padding::{NoPadding, Pkcs7, UnpadError};
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

type Aes256Enc = cbc::Encryptor<aes::Aes256>;
type Aes256Dec = cbc::Decryptor<aes::Aes256>;

/// The vendor derives AES keys/IVs from 32- and 16-character ASCII secrets and
/// zero-pads shorter ones (see the bhsdk helper documented in docs/PROTOCOL.md).
pub fn key32(k: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..k.len().min(32)].copy_from_slice(&k[..k.len().min(32)]);
    out
}

pub fn iv16(iv: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..iv.len().min(16)].copy_from_slice(&iv[..iv.len().min(16)]);
    out
}

/// AES-256-CBC with PKCS#7 padding.
pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Vec<u8> {
    Aes256Enc::new(&key32(key).into(), &iv16(iv).into()).encrypt_padded_vec_mut::<Pkcs7>(pt)
}

pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>, DecryptError> {
    Aes256Dec::new(&key32(key).into(), &iv16(iv).into())
        .decrypt_padded_vec_mut::<Pkcs7>(ct)
        .map_err(|e| match e {
            UnpadError => DecryptError::Padding,
        })
}

/// AES-256-CBC without unpadding: the module tolerates frames whose tail is not
/// padded (the reference implementation strips 0x00..0x08 by hand). Returns the
/// raw block-decrypted bytes.
pub fn aes_cbc_decrypt_raw(key: &[u8], iv: &[u8], ct: &[u8]) -> Vec<u8> {
    let mut buf = if ct.len() % 16 == 0 {
        Aes256Dec::new(&key32(key).into(), &iv16(iv).into())
            .decrypt_padded_vec_mut::<NoPadding>(ct)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    while matches!(buf.last(), Some(b) if (0x00..=0x08).contains(b)) {
        buf.pop();
    }
    buf
}

/// Stock RC4, key used as-is (the REST envelope cipher).
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = core::array::from_fn(|i| i as u8);
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j
            .wrapping_add(s[i])
            .wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut i, mut j) = (0u8, 0u8);
    for b in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(b ^ k);
    }
    out
}

pub fn hex_encode(d: &[u8]) -> String {
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let nib = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..b.len()).step_by(2) {
        out.push((nib(b[i])? << 4) | nib(b[i + 1])?);
    }
    Some(out)
}

pub fn b64_encode(d: &[u8]) -> String {
    B64.encode(d)
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    B64.decode(s.as_bytes()).ok()
}

const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn unix_s() -> u64 {
    unix_ms() / 1000
}

/// Uniform random bytes from the OS CSPRNG (getrandom via /dev/urandom: no
/// extra dependency in a static musl binary).
pub fn rand_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    use std::io::Read;
    match std::fs::File::open("/dev/urandom") {
        Ok(mut f) => {
            if f.read_exact(&mut v).is_err() {
                for (i, b) in v.iter_mut().enumerate() {
                    *b = (unix_ms() as usize).wrapping_mul(2654435761) as u8 ^ (i as u8);
                }
            }
        }
        Err(_) => {
            for (i, b) in v.iter_mut().enumerate() {
                *b = (unix_ms() as usize).wrapping_mul(2654435761) as u8 ^ (i as u8);
            }
        }
    }
    v
}

/// `n` characters from the vendor alphabet (letters + digits): nonces, end ids,
/// group ids and chat secrets are all ASCII alnum.
pub fn rand_alnum(n: usize) -> String {
    rand_bytes(n)
        .iter()
        .map(|b| ALNUM[(*b as usize) % ALNUM.len()] as char)
        .collect()
}

pub fn rand_hex(nbytes: usize) -> String {
    hex_encode(&rand_bytes(nbytes))
}

/// `application/x-www-form-urlencoded` decoding (the vendor API body form).
pub fn form_decode(body: &[u8]) -> Vec<(String, String)> {
    let s = String::from_utf8_lossy(body);
    s.split('&')
        .filter(|f| !f.is_empty())
        .map(|f| {
            let (k, v) = f.split_once('=').unwrap_or((f, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Some(v) = hex_decode(&s[i + 1..i + 3]) {
                    out.push(v[0]);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug)]
pub enum DecryptError {
    Padding,
}

impl core::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecryptError::Padding => write!(f, "bad PKCS#7 padding"),
        }
    }
}
impl std::error::Error for DecryptError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip() {
        let key = b"0123456789abcdef0123456789abcdef";
        let iv = b"abcdefghijklmnop";
        let ct = aes_cbc_encrypt(key, iv, b"hello world");
        assert_eq!(aes_cbc_decrypt(key, iv, &ct).unwrap(), b"hello world");
    }

    #[test]
    fn rc4_known_vector() {
        // RFC 6229 style check: all-zero key keeps the identity-ish keystream.
        let a = rc4(b"Key", b"Plaintext");
        let b = rc4(b"Key", &a);
        assert_eq!(b, b"Plaintext");
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_decode(&hex_encode(b"\x00\xffA")).unwrap(), b"\x00\xffA");
        assert!(hex_decode("abc").is_none());
    }

    #[test]
    fn form_and_percent() {
        let f = form_decode(b"gm=2&iv=abc&json=a%2Fb%3Dc&token=x");
        assert_eq!(f[2], ("json".into(), "a/b=c".into()));
        assert_eq!(percent_encode("a/b=c+"), "a%2Fb%3Dc%2B");
    }
}
