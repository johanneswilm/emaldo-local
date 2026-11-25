// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// REST request/response envelopes of the vendor cloud API.
//
// Two client families share one wire style:
//
//   gm1 (Android app / community SDK, used by Home Assistant)
//     POST https://host/<path>/<APP_ID>
//     body: gm=1&json=<hex(RC4(APP_SECRET, json))>&token=<hex(RC4(APP_SECRET, "<token>_<gmtime>"))>
//     answer: {"Status":1,...,"Result":"<hex(RC4(APP_SECRET, rsp-json))>"}
//     (the real server snappy-compresses before RC4; plain RC4 is accepted too)
//
//   gm2 (the module binary, bhsdk)
//     POST https://host/<path>/<reg_token>
//     body: gm=2&iv=<16 alnum>&json=<urlencode(b64(AES-CBC(APP_SECRET, iv, json)))>
//           &token=<same encoding of the reg JWT>
//     answer: plaintext envelope, Result = b64(AES-CBC(APP_SECRET, **the request IV**, rsp-json))
//
// `Status != 1` is a business error; `Status == -12` means "session expired" and
// makes the client re-login. The bridge never validates the caller token: it
// answers `Status: 1` for everything it serves, so sessions never expire.

use serde_json::Value;

use crate::codec::*;
use crate::params::*;

#[derive(Debug, Clone)]
pub struct Request {
    /// 1 = app/HA client, 2 = module client, 0 = unrecognised.
    pub gm: u8,
    /// gm2 request IV, reused for the answer.
    pub iv: Option<String>,
    /// Inner JSON, decrypted. `None` when the payload was not decodable — the
    /// metrics endpoints carry a second, separately keyed layer.
    pub json: Option<String>,
    /// Decoded caller token (never validated, kept for the request log).
    pub token: Option<String>,
    pub gm_raw: String,
}

/// A JSON object, decrypted where possible. `None` = opaque payload layer.
pub fn parse_request(body: &[u8]) -> Request {
    let fields = form_decode(body);
    let mut req = Request {
        gm: 0,
        iv: None,
        json: None,
        token: None,
        gm_raw: String::new(),
    };
    for (k, v) in &fields {
        match k.as_str() {
            "gm" => {
                req.gm_raw = v.clone();
                req.gm = v.as_str().parse().unwrap_or(0);
            }
            "iv" => req.iv = Some(v.clone()),
            "json" => {
                req.json = decode_field(req.gm, req.iv.as_deref(), v).ok();
            }
            "token" => {
                req.token = decode_field(req.gm, req.iv.as_deref(), v).ok();
            }
            _ => {}
        }
    }
    req
}

fn decode_field(gm: u8, iv: Option<&str>, v: &str) -> Result<String, ()> {
    match gm {
        2 => {
            let iv = iv.ok_or(())?;
            let ct = b64_decode(v).ok_or(())?;
            let pt = aes_cbc_decrypt(APP_SECRET, iv.as_bytes(), &ct).unwrap_or_default();
            Ok(String::from_utf8_lossy(&pt).into_owned())
        }
        1 => {
            let raw = hex_decode(v).ok_or(())?;
            Ok(String::from_utf8_lossy(&rc4(APP_SECRET, &raw)).into_owned())
        }
        _ => Err(()),
    }
}

/// Re-encode a request field the way the clients expect (used by the optional
/// vendor leg, which replays module requests verbatim and therefore needs no
/// re-encoding; kept for completeness/tests).
pub fn encode_field(gm: u8, iv: Option<&str>, plain: &str) -> String {
    match gm {
        2 => percent_encode(&b64_encode(&aes_cbc_encrypt(
            APP_SECRET,
            iv.unwrap_or("0000000000000000").as_bytes(),
            plain.as_bytes(),
        ))),
        _ => hex_encode(&rc4(APP_SECRET, plain.as_bytes())),
    }
}

fn envelope(status: i64, result: String, iv: Option<&str>) -> Value {
    let mut e = serde_json::Map::new();
    e.insert("Status".into(), serde_json::json!(status));
    e.insert("Action".into(), serde_json::json!(""));
    e.insert("Cmd".into(), serde_json::json!(""));
    e.insert("ErrorMessage".into(), serde_json::json!(""));
    e.insert("MessageId".into(), serde_json::json!(""));
    e.insert("Result".into(), serde_json::json!(result));
    if let Some(iv) = iv {
        e.insert("iv".into(), serde_json::json!(iv));
    }
    Value::Object(e)
}

/// gm2 answer: AES-CBC with the *request* IV, base64, plaintext envelope.
pub fn answer_gm2(rsp: &Value, iv: Option<&str>) -> Vec<u8> {
    let iv = iv.map(|s| s.to_string()).unwrap_or_else(|| rand_alnum(16));
    let plain = serde_json::to_string(rsp).unwrap_or_else(|_| "{}".into());
    let res = b64_encode(&aes_cbc_encrypt(APP_SECRET, iv.as_bytes(), plain.as_bytes()));
    serde_json::to_vec(&envelope(1, res, None)).unwrap()
}

/// gm1 answer: RC4 hex of the compact result JSON (no snappy layer needed —
/// clients fall back when decompression fails).
pub fn answer_gm1(rsp: &Value) -> Vec<u8> {
    let plain = serde_json::to_string(rsp).unwrap_or_else(|_| "{}".into());
    let res = hex_encode(&rc4(APP_SECRET, plain.as_bytes()));
    serde_json::to_vec(&envelope(1, res, None)).unwrap()
}

/// Envelope-level failure (the client raises an error carrying `ErrorMessage`).
pub fn answer_error(status: i64, msg: &str) -> Vec<u8> {
    let mut e = envelope(status, String::new(), None);
    if let Some(m) = e.as_object_mut() {
        m.insert("ErrorMessage".into(), serde_json::json!(msg));
    }
    serde_json::to_vec(&e).unwrap()
}

/// Build the JSON body a gm1 client would send (used by the vendor leg when it
/// replays HA-side calls, and by the self-test).
pub fn gm1_body(json: &Value, token: Option<&str>) -> String {
    let j = encode_field(1, None, &serde_json::to_string(json).unwrap());
    match token {
        Some(t) => {
            let tk = encode_field(
                1,
                None,
                &format!("{}_{}", t, gmtime()),
            );
            format!("gm=1&json={j}&token={tk}")
        }
        None => format!("gm=1&json={j}"),
    }
}

/// Parse the `Result` field of a vendor (gm1) answer back into JSON — the
/// vendor leg and the corpus need this.
pub fn gm1_result(envelope: &Value) -> Option<Value> {
    let res = envelope.get("Result")?.as_str()?;
    let raw = hex_decode(res)?;
    let pt = rc4(APP_SECRET, &raw);
    serde_json::from_slice(&pt).ok()
}

/// Parse the `Result` field of a vendor (gm2) answer.
pub fn gm2_result(envelope: &Value, iv: &str) -> Option<Value> {
    let res = envelope.get("Result")?.as_str()?;
    let ct = b64_decode(res)?;
    let pt = aes_cbc_decrypt(APP_SECRET, iv.as_bytes(), &ct).ok()?;
    serde_json::from_slice(&pt).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gm2_roundtrip_reuses_request_iv() {
        let iv = rand_alnum(16);
        let inner = serde_json::json!({"home_id": "x", "token": "jwt"});
        let body = format!(
            "gm=2&iv={iv}&json={}&token=zz",
            encode_field(2, Some(&iv), &serde_json::to_string(&inner).unwrap())
        );
        let req = parse_request(body.as_bytes());
        assert_eq!(req.gm, 2);
        assert_eq!(req.iv.as_deref(), Some(iv.as_str()));
        assert_eq!(req.json.as_deref().unwrap(), serde_json::to_string(&inner).unwrap());

        let ans = answer_gm2(&serde_json::json!({"end_id": "abc"}), req.iv.as_deref());
        let env: Value = serde_json::from_slice(&ans).unwrap();
        assert_eq!(env["Status"], 1);
        assert_eq!(gm2_result(&env, &iv).unwrap()["end_id"], "abc");
    }

    #[test]
    fn gm1_roundtrip() {
        let body = gm1_body(&serde_json::json!({"email": "a@b"}), None);
        let req = parse_request(body.as_bytes());
        assert_eq!(req.gm, 1);
        assert_eq!(req.json.as_deref().unwrap(), "{\"email\":\"a@b\"}");
        let ans = answer_gm1(&serde_json::json!({"token": "t"}));
        let env: Value = serde_json::from_slice(&ans).unwrap();
        assert_eq!(gm1_result(&env).unwrap()["token"], "t");
    }
}
