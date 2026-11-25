// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// MSCT / E2E frame codec.
//
// A frame is one UDP datagram:
//
//   [head][option...]  [<last option>]  [ciphertext body]
//
// Each option is `[lead][code][value]`, where `lead = 0x80 | len` for every
// option except the last, whose lead is the bare length. Everything that
// follows the last option is the encrypted body (it is *not* an option): the
// last option is the content type (`application/byte` / `application/json`)
// and the AES-CBC(chat_secret, iv) ciphertext follows it directly.
//
// The head byte is `(msgType << 4) | (channel << 1) | hasOptions`:
// 0xD9 request, 0xE3 relay/peer response, 0xD3 vendor-server push.
//
// Response matching in the vendor clients only looks at `head & 1`, then walks
// options by code, so option *order* is free — which is exactly how the
// `A0 = "SERVER"` message-level router switch works (docs/PROTOCOL.md): a
// second END_ID option carrying the literal `SERVER` makes the module route
// the frame to its cloud router instead of the user router.

use crate::codec::rand_alnum;

pub const HEAD_REQUEST: u8 = 0xD9;
pub const HEAD_RESPONSE: u8 = 0xE3;
pub const HEAD_SERVER_PUSH: u8 = 0xD3;

pub const OPT_END_ID: u8 = 0xA0;
pub const OPT_GROUP: u8 = 0xA1;
pub const OPT_RECEIVER: u8 = 0xA2;
pub const OPT_IV: u8 = 0xA3;
pub const OPT_PROXY: u8 = 0xF1;
pub const OPT_APP_ID: u8 = 0xB5;
pub const OPT_METHOD: u8 = 0xF5;
pub const OPT_MSGID: u8 = 0xF6;
pub const OPT_CTYPE: u8 = 0xB7;
pub const OPT_STATUS: u8 = 0xC0;
pub const OPT_LAST: u8 = 0xC1;

/// Two-byte option tags as they appear on the wire (length lead + code). The
/// reference relay locates fields by scanning for these instead of walking the
/// TLV chain, because a few frames — the module's `0x1a` override write among
/// them — carry length leads the strict walk cannot follow.
pub const TAG_END: [u8; 2] = [0xa0, OPT_END_ID];
pub const TAG_GROUP: [u8; 2] = [0xa0, OPT_GROUP];
pub const TAG_RECEIVER: [u8; 2] = [0xa0, OPT_RECEIVER];
pub const TAG_IV_LONG: [u8; 2] = [0x90, OPT_IV];
pub const TAG_IV_LAST: [u8; 2] = [0x10, OPT_IV];
pub const TAG_CMD: [u8; 2] = [0x82, OPT_METHOD];
pub const TAG_MSGID: [u8; 2] = [0x9b, OPT_MSGID];
pub const TAG_MSGID_ALT: [u8; 2] = [0x8c, OPT_MSGID];

/// Vendor method names carried as ASCII in the METHOD option.
pub const M_ALIVE: &[u8] = b"alive";
pub const M_HEARTBEAT: &[u8] = b"heartbeat";
pub const M_WAKE: &[u8] = b"wake";
pub const M_ESTAB: &[u8] = b"estab";
pub const M_DC_ACT: &[u8] = b"dc_act";
pub const M_DC_DEACT: &[u8] = b"dc_deact";

#[derive(Debug, Clone, PartialEq)]
pub struct Opt {
    pub code: u8,
    pub val: Vec<u8>,
}

impl Opt {
    pub fn new(code: u8, val: impl Into<Vec<u8>>) -> Self {
        Opt { code, val: val.into() }
    }
    pub fn ascii(code: u8, s: &str) -> Self {
        Opt { code, val: s.as_bytes().to_vec() }
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub head: u8,
    pub opts: Vec<Opt>,
    /// Ciphertext (or plaintext, for a few unencrypted control frames) after the
    /// last option.
    pub body: Vec<u8>,
    /// Original datagram, kept for verbatim forwarding.
    pub raw: Vec<u8>,
}

impl Frame {
    pub fn parse(d: &[u8]) -> Frame {
        let mut f = Frame {
            head: d.first().copied().unwrap_or(0),
            opts: Vec::new(),
            body: Vec::new(),
            raw: d.to_vec(),
        };
        let mut i = 1usize;
        while i + 2 <= d.len() {
            let lead = d[i];
            let len = (lead & 0x7f) as usize;
            let code = d[i + 1];
            let start = i + 2;
            let end = start + len;
            if end > d.len() {
                // Truncated option: treat the remainder as the body.
                break;
            }
            f.opts.push(Opt {
                code,
                val: d[start..end].to_vec(),
            });
            i = end;
            if lead & 0x80 == 0 {
                break; // last option
            }
        }
        f.body = d[i.min(d.len())..].to_vec();
        f
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = encode(self.head, &self.opts);
        out.extend_from_slice(&self.body);
        out
    }

    pub fn get(&self, code: u8) -> Option<&[u8]> {
        self.opts
            .iter()
            .find(|o| o.code == code)
            .map(|o| o.val.as_slice())
    }

    /// Byte-scan for a two-byte option tag and return the `n` bytes that
    /// follow it. The set-override frame (and a few module frames) carry an
    /// option length that breaks the strict TLV walk, so the reference relay
    /// locates fields by tag scan; the same fallbacks are used here.
    pub fn tag_after(&self, tag: &[u8; 2], n: usize) -> Option<&[u8]> {
        let i = self.raw.windows(2).position(|w| w == *tag)?;
        let start = i + 2;
        if start + n <= self.raw.len() {
            Some(&self.raw[start..start + n])
        } else {
            None
        }
    }

    /// Byte-scan for an option tag and return the printable run after it.
    pub fn ascii_after(&self, tag: &[u8; 2]) -> Option<&[u8]> {
        let i = self.raw.windows(2).position(|w| w == *tag)?;
        let start = i + 2;
        let mut e = start;
        while e < self.raw.len() && (0x20..=0x7e).contains(&self.raw[e]) {
            e += 1;
        }
        if e > start {
            Some(&self.raw[start..e])
        } else {
            None
        }
    }

    pub fn get_all(&self, code: u8) -> Vec<&[u8]> {
        self.opts
            .iter()
            .filter(|o| o.code == code)
            .map(|o| o.val.as_slice())
            .collect()
    }

    /// The END_ID option the module actually uses as the sender name: the last
    /// one wins (`ShouldGet` returns the final duplicate), which is what makes
    /// the `SERVER` trick work.
    pub fn sender_name(&self) -> Option<&[u8]> {
        self.get_all(OPT_END_ID).last().copied()
    }

    pub fn sender(&self) -> Option<&[u8]> {
        self.get(OPT_END_ID)
            .or_else(|| self.tag_after(&TAG_END, 32))
    }
    pub fn group(&self) -> Option<&[u8]> {
        self.get(OPT_GROUP).or_else(|| self.tag_after(&TAG_GROUP, 32))
    }
    pub fn receiver(&self) -> Option<&[u8]> {
        self.get(OPT_RECEIVER).or_else(|| self.tag_after(&TAG_RECEIVER, 32))
    }
    pub fn iv(&self) -> Option<&[u8]> {
        self.get(OPT_IV)
            .or_else(|| self.tag_after(&TAG_IV_LONG, 16))
            .or_else(|| self.tag_after(&TAG_IV_LAST, 16))
    }
    /// Message id, falling back to the printable run after the msgid tag.
    pub fn msgid(&self) -> Option<&[u8]> {
        self.get(OPT_MSGID)
            .or_else(|| self.ascii_after(&TAG_MSGID))
            .or_else(|| self.ascii_after(&TAG_MSGID_ALT))
    }
    pub fn ctype(&self) -> Option<&[u8]> {
        self.get(OPT_CTYPE)
    }
    pub fn has_proxy(&self) -> bool {
        self.get(OPT_PROXY).is_some()
    }

    /// METHOD option: ASCII method name, or a 2-byte `[type, mode]` command.
    pub fn method(&self) -> Option<&[u8]> {
        self.opts.iter().find(|o| o.code == OPT_METHOD).map(|o| o.val.as_slice())
    }

    /// `[type, mode]` for numeric commands (METHOD option of length 2).
    pub fn cmd(&self) -> Option<[u8; 2]> {
        self.method()
            .and_then(|m| if m.len() == 2 { Some([m[0], m[1]]) } else { None })
            .or_else(|| self.tag_after(&TAG_CMD, 2).and_then(|v| <[u8; 2]>::try_from(v).ok()))
    }

    /// Replace the first option of `code` whose length matches `val`.
    pub fn set_first(&mut self, code: u8, val: &[u8]) -> bool {
        for o in self.opts.iter_mut() {
            if o.code == code && o.val.len() == val.len() {
                o.val = val.to_vec();
                return true;
            }
        }
        false
    }

    pub fn set_all(&mut self, code: u8, val: &[u8]) -> usize {
        let mut n = 0;
        for o in self.opts.iter_mut() {
            if o.code == code && o.val.len() == val.len() {
                o.val = val.to_vec();
                n += 1;
            }
        }
        n
    }
}

/// Serialize options (continuation bit set on all but the last).
pub fn encode(head: u8, opts: &[Opt]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.push(head);
    for (i, o) in opts.iter().enumerate() {
        let last = i + 1 == opts.len();
        let len = o.val.len().min(0x7f);
        let lead = if last { len as u8 } else { 0x80 | len as u8 };
        out.push(lead);
        out.push(o.code);
        out.extend_from_slice(&o.val[..len]);
    }
    out
}

/// Build a full frame: options + encrypted body.
pub fn build(head: u8, opts: &[Opt], body: &[u8]) -> Vec<u8> {
    let mut out = encode(head, opts);
    out.extend_from_slice(body);
    out
}

/// `[type, mode]` command code as used in the METHOD option: the wire order is
/// `[type, mode]` while the documented code is `(mode << 8) | type`.
pub fn cmd_opt(code: u16) -> Opt {
    Opt::new(OPT_METHOD, vec![(code & 0xff) as u8, (code >> 8) as u8])
}

pub fn cmd_code(t: u8, mode: u8) -> u16 {
    ((mode as u16) << 8) | t as u16
}

// ---------------------------------------------------------------------------
// Client-side frames (what the module and the HA client send)
// ---------------------------------------------------------------------------

/// The credentials one side of an E2E session needs to build frames.
#[derive(Debug, Clone)]
pub struct Creds {
    pub end_id: String,
    pub group_id: String,
    /// Per-end secret: encrypts `alive` (and the login handshake).
    pub end_secret: String,
    /// Per-session secret: encrypts everything else.
    pub chat_secret: String,
    pub app_id: String,
}

fn time_body() -> Vec<u8> {
    format!("{{\"__time\":{}}}", crate::codec::unix_s()).into_bytes()
}

/// `alive` — proof of possession of `end_secret`, and the frame that registers
/// the sender's source address with the relay.
pub fn alive(c: &Creds) -> Vec<u8> {
    alive_with(c, &rand_alnum(16), &app_msgid())
}

/// `alive` with the session nonce and message id supplied, so the golden
/// vectors (which pin both) can be checked byte-for-byte.
pub fn alive_with(c: &Creds, nonce: &str, msgid: &str) -> Vec<u8> {
    let body = crate::codec::aes_cbc_encrypt(c.end_secret.as_bytes(), nonce.as_bytes(), &time_body());
    build(
        HEAD_REQUEST,
        &[
            Opt::ascii(OPT_END_ID, &c.end_id),
            Opt::ascii(OPT_GROUP, &c.group_id),
            Opt::ascii(OPT_IV, nonce),
            Opt::new(OPT_METHOD, M_ALIVE),
            Opt::ascii(OPT_MSGID, msgid),
            Opt::ascii(OPT_CTYPE, "application/json"),
        ],
        &body,
    )
}

/// `heartbeat` — opens/refreshes the session with the relay. The session nonce
/// doubles as the AES IV for the rest of the session.
pub fn heartbeat(c: &Creds, recipient: &str, session_nonce: &str, msgid: &str) -> Vec<u8> {
    let body = crate::codec::aes_cbc_encrypt(
        c.chat_secret.as_bytes(),
        session_nonce.as_bytes(),
        &time_body(),
    );
    build(
        HEAD_REQUEST,
        &[
            Opt::ascii(OPT_END_ID, &c.end_id),
            Opt::ascii(OPT_GROUP, &c.group_id),
            Opt::ascii(OPT_RECEIVER, recipient),
            Opt::ascii(OPT_IV, session_nonce),
            Opt::new(OPT_PROXY, [0x01u8]),
            Opt::new(OPT_METHOD, M_HEARTBEAT),
            Opt::ascii(OPT_APP_ID, &c.app_id),
            Opt::ascii(OPT_MSGID, msgid),
            Opt::ascii(OPT_CTYPE, "application/json"),
        ],
        &body,
    )
}

/// `wake` — fire-and-forget nudge that refreshes the relay's routing entry for
/// the recipient. Sent by both clients before a command burst.
pub fn wake(c: &Creds, recipient: &str, session_nonce: &str, msgid: &str) -> Vec<u8> {
    let body = crate::codec::aes_cbc_encrypt(
        c.chat_secret.as_bytes(),
        session_nonce.as_bytes(),
        &time_body(),
    );
    build(
        HEAD_REQUEST,
        &[
            Opt::ascii(OPT_END_ID, &c.end_id),
            Opt::ascii(OPT_GROUP, &c.group_id),
            Opt::ascii(OPT_RECEIVER, recipient),
            Opt::ascii(OPT_IV, session_nonce),
            Opt::new(OPT_PROXY, [0x01u8]),
            Opt::new(OPT_METHOD, M_WAKE),
            Opt::ascii(OPT_APP_ID, &c.app_id),
            Opt::ascii(OPT_MSGID, msgid),
        ],
        &body,
    )
}

/// Subscribe (`mode 0xA0`, state the relay holds) or request (`mode 0x10`,
/// forwarded to the device) a command type.
pub fn subscribe(
    c: &Creds,
    recipient: &str,
    cmd_type: u8,
    mode: u8,
    session_nonce: &str,
    msgid: &str,
    payload: &[u8],
) -> Vec<u8> {
    let body = crate::codec::aes_cbc_encrypt(c.chat_secret.as_bytes(), session_nonce.as_bytes(), payload);
    build(
        HEAD_REQUEST,
        &[
            Opt::ascii(OPT_END_ID, &c.end_id),
            Opt::ascii(OPT_GROUP, &c.group_id),
            Opt::ascii(OPT_RECEIVER, recipient),
            Opt::ascii(OPT_IV, session_nonce),
            Opt::new(OPT_PROXY, [0x01u8]),
            Opt::ascii(OPT_APP_ID, &c.app_id),
            Opt::new(OPT_METHOD, [cmd_type, mode]),
            Opt::ascii(OPT_MSGID, msgid),
            Opt::ascii(OPT_CTYPE, "application/byte"),
        ],
        &body,
    )
}

/// `0x1a` — write the override (schedule) table.
///
/// The module's own frame layout is reproduced byte-for-byte (option order and
/// the 4-byte PROXY), because the module's frame parser is what has to accept
/// the reply flow; unlike `subscribe`, METHOD here is a single byte and the
/// msgid option carries a 32-byte length lead.
pub fn set_override(
    c: &Creds,
    recipient: &str,
    session_nonce: &str,
    msgid: &str,
    payload: &[u8],
) -> Vec<u8> {
    let body = crate::codec::aes_cbc_encrypt(c.chat_secret.as_bytes(), session_nonce.as_bytes(), payload);
    build(
        HEAD_REQUEST,
        &[
            Opt::ascii(OPT_END_ID, &c.end_id),
            Opt::ascii(OPT_GROUP, &c.group_id),
            Opt::new(OPT_PROXY, [0x00u8, 0x00, 0x00, 0x01]),
            Opt::ascii(OPT_RECEIVER, recipient),
            Opt::ascii(OPT_IV, session_nonce),
            Opt::ascii(OPT_APP_ID, &c.app_id),
            // METHOD is a two-byte option here as well: [0x1a, 0xa0].
            Opt::new(OPT_METHOD, [0x1Au8, 0xA0]),
            Opt::ascii(OPT_MSGID, msgid),
            Opt::ascii(OPT_CTYPE, "application/byte"),
        ],
        &body,
    )
}

// ---------------------------------------------------------------------------
// Relay ACKs
// ---------------------------------------------------------------------------

/// Two byte-level ACK shapes, both observed to be accepted by the module and
/// by the community HA client:
///
/// `Vendor` — the shape captured from the real vendor relay: well-formed TLV,
/// echoes the request msgid, closes with the `C1` zero-length option.
/// `Legacy` — byte-for-byte what the reference Python relay emits as its
/// synthetic ACK (including its off-by-one length leads), kept because it is
/// the shape proven against the live battery in the takeover stack.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AckStyle {
    Vendor,
    Legacy,
}

pub fn ack(frame: &Frame, style: AckStyle) -> Vec<u8> {
    match style {
        AckStyle::Vendor => ack_vendor(frame),
        AckStyle::Legacy => ack_legacy(frame),
    }
}

fn ack_vendor(frame: &Frame) -> Vec<u8> {
    let mut opts: Vec<Opt> = Vec::new();
    opts.push(Opt::new(OPT_END_ID, frame.sender().unwrap_or(b"").to_vec()));
    opts.push(Opt::new(OPT_GROUP, frame.group().unwrap_or(b"").to_vec()));
    if frame.get(OPT_APP_ID).is_some() {
        opts.push(Opt::new(OPT_APP_ID, frame.get(OPT_APP_ID).unwrap().to_vec()));
    }
    opts.push(Opt::new(OPT_IV, frame.iv().unwrap_or(b"").to_vec()));
    if let Some(m) = frame.method() {
        opts.push(Opt::new(OPT_METHOD, m.to_vec()));
    }
    let msgid = frame
        .msgid()
        .map(|m| m.to_vec())
        .unwrap_or_else(|| format!("hs_{}", crate::codec::unix_ms() % 100_000).into_bytes());
    opts.push(Opt::new(OPT_MSGID, msgid));
    opts.push(Opt::new(OPT_STATUS, vec![0u8, 0u8]));
    opts.push(Opt::new(OPT_LAST, Vec::new()));
    build(HEAD_RESPONSE, &opts, &[])
}

fn ack_legacy(frame: &Frame) -> Vec<u8> {
    let mut d = Vec::with_capacity(128);
    d.push(HEAD_RESPONSE);
    let push = |d: &mut Vec<u8>, code: u8, val: &[u8]| {
        d.push(0x80 | val.len() as u8);
        d.push(code);
        d.extend_from_slice(val);
    };
    push(&mut d, OPT_END_ID, frame.sender().unwrap_or(b""));
    push(&mut d, OPT_GROUP, frame.group().unwrap_or(b""));
    if frame.raw.windows(2).any(|w| w == [0x80, OPT_APP_ID]) {
        d.push(0x80);
        d.push(OPT_APP_ID);
    }
    if let Some(iv) = frame.iv() {
        push(&mut d, OPT_IV, iv);
    }
    match frame.method() {
        Some(m) if m == M_ALIVE => push(&mut d, OPT_METHOD, M_ALIVE),
        Some(m) if m == M_HEARTBEAT => push(&mut d, OPT_METHOD, M_HEARTBEAT),
        Some(m) if m == M_WAKE => push(&mut d, OPT_METHOD, M_WAKE),
        Some(m) => push(&mut d, OPT_METHOD, m),
        None => {}
    }
    let mid = format!("hs_{}", (crate::codec::unix_ms() / 10) % 1_000_000);
    d.push(0x86);
    d.push(OPT_MSGID);
    d.extend_from_slice(mid.as_bytes());
    d.push(0x82);
    d.push(OPT_STATUS);
    d.extend_from_slice(&[0u8, 0u8, 0u8]);
    d.push(0x80);
    d.extend_from_slice(&[0u8; 15]);
    d
}

/// Patch a captured ACK datagram (byte-exact vendor relay template) with the
/// fields of `frame`, the way the reference relay does: sender, group, IV,
/// msgid (only when the length matches) and the 18-byte status tail.
pub fn ack_from_template(tpl: &[u8], frame: &Frame) -> Option<Vec<u8>> {
    fn find(d: &[u8], pat: &[u8]) -> Option<usize> {
        d.windows(pat.len()).position(|w| w == pat)
    }
    let mut d = tpl.to_vec();
    let sender = frame.sender()?;
    if let Some(i) = find(&d, &[0xa0, OPT_END_ID]) {
        if i + 2 + 32 <= d.len() {
            d[i + 2..i + 34].copy_from_slice(&sender[..32]);
        }
    }
    if let (Some(g), Some(i)) = (frame.group(), find(&d, &[0xa0, OPT_GROUP])) {
        if i + 2 + 32 <= d.len() {
            d[i + 2..i + 34].copy_from_slice(&g[..32.min(g.len())]);
        }
    }
    if let (Some(iv), Some(i)) = (
        frame.iv(),
        find(&d, &[0x90, OPT_IV]).or_else(|| find(&d, &[0x10, OPT_IV])),
    ) {
        if i + 2 + 16 <= d.len() {
            d[i + 2..i + 18].copy_from_slice(&iv[..16.min(iv.len())]);
        }
    }
    if let (Some(i), Some(req)) = (find(&d, &[0x86, OPT_MSGID]), find(&frame.raw, &[0x86, OPT_MSGID])) {
        let ascii_end = |b: &[u8], mut j: usize| {
            while j < b.len() && (0x20..=0x7e).contains(&b[j]) {
                j += 1;
            }
            j
        };
        let tlen = ascii_end(&d, i + 2) - (i + 2);
        let rlen = ascii_end(&frame.raw, req + 2) - (req + 2);
        if rlen > 0 && rlen == tlen {
            d[i + 2..i + 2 + tlen].copy_from_slice(&frame.raw[req + 2..req + 2 + rlen]);
        }
    }
    if let (Some(t), Some(u)) = (find(&frame.raw, &[0x82, OPT_STATUS]), find(&d, &[0x82, OPT_STATUS])) {
        if frame.raw.len() >= t + 18 && d.len() >= u + 18 {
            d[u..u + 18].copy_from_slice(&frame.raw[t..t + 18]);
        }
    }
    Some(d)
}

/// Fresh 16-char session nonce.
pub fn nonce() -> String {
    rand_alnum(16)
}

/// Vendor msgid formats: app clients use `and_` + 23 alnum, the module uses
/// `hps_` + digits, the relay uses `hs_` + digits.
pub fn app_msgid() -> String {
    format!("and_{}", rand_alnum(23))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_command() -> Vec<u8> {
        let iv = nonce();
        let opts = vec![
            Opt::ascii(OPT_END_ID, &rand_alnum(32)),
            Opt::ascii(OPT_GROUP, &format!("{}@0", rand_alnum(31))),
            Opt::ascii(OPT_RECEIVER, &rand_alnum(32)),
            Opt::ascii(OPT_IV, &iv),
            Opt::new(OPT_PROXY, vec![1u8, 0u8]),
            Opt::ascii(OPT_APP_ID, crate::params::APP_ID),
            cmd_opt(cmd_code(0x30, 0xA0)),
            Opt::ascii(OPT_MSGID, &app_msgid()),
            Opt::ascii(OPT_CTYPE, "application/byte"),
        ];
        build(HEAD_REQUEST, &opts, &[7u8; 16])
    }

    #[test]
    fn parse_walks_every_option() {
        let d = sample_command();
        let f = Frame::parse(&d);
        assert_eq!(f.head, HEAD_REQUEST);
        assert_eq!(f.cmd(), Some([0x30, 0xA0]));
        assert_eq!(f.iv().unwrap().len(), 16);
        assert_eq!(f.msgid().unwrap().len(), 27);
        assert_eq!(f.ctype().unwrap(), b"application/byte");
        assert_eq!(f.body.len(), 16);
        assert!(f.has_proxy());
    }

    #[test]
    fn roundtrip_preserves_bytes() {
        let d = sample_command();
        let f = Frame::parse(&d);
        assert_eq!(f.to_bytes(), d);
    }

    #[test]
    fn receiver_rewrite() {
        let d = sample_command();
        let mut f = Frame::parse(&d);
        let new = rand_alnum(32);
        assert!(f.set_first(OPT_RECEIVER, new.as_bytes()));
        assert_eq!(Frame::parse(&f.to_bytes()).receiver().unwrap(), new.as_bytes());
    }

    #[test]
    fn server_header_selects_cloud_router() {
        let mut opts = vec![
            Opt::ascii(OPT_END_ID, &rand_alnum(32)),
            Opt::ascii(OPT_METHOD, "dc_act"),
            Opt::ascii(OPT_END_ID, "SERVER"),
        ];
        let d = build(HEAD_REQUEST, &opts, b"{}");
        let f = Frame::parse(&d);
        assert_eq!(f.sender_name().unwrap(), b"SERVER");
        opts.pop();
        let f2 = Frame::parse(&build(HEAD_REQUEST, &opts, b"{}"));
        assert_ne!(f2.sender_name().unwrap(), b"SERVER");
    }

    #[test]
    fn acks_echo_identity_and_msgid() {
        let d = sample_command();
        let f = Frame::parse(&d);
        for style in [AckStyle::Vendor, AckStyle::Legacy] {
            let a = Frame::parse(&ack(&f, style));
            assert_eq!(a.sender(), f.sender());
            assert_eq!(a.group(), f.group());
            assert_eq!(a.iv(), f.iv());
            assert_eq!(a.cmd(), f.cmd());
            if style == AckStyle::Vendor {
                assert_eq!(a.msgid(), f.msgid());
            }
        }
    }

    /// Golden frames produced by the community client's Python builders
    /// (`emaldo.e2e.build_*`) with fixed ids: the option region must match
    /// byte-for-byte, because the module's and the relay's parsers are strict
    /// about option order and lengths (a wrong PROXY length is answered with
    /// status 0x52D4).
    #[test]
    fn client_frames_match_python_reference() {
        let nonce = "N".repeat(16);
        let mid = "and_MMMMMMMMMM1795611755054";
        let c = Creds {
            end_id: "a".repeat(32),
            group_id: "g".repeat(32),
            end_secret: "e".repeat(32),
            chat_secret: "c".repeat(32),
            app_id: "A".repeat(32),
        };
        let key_end = "e".repeat(32);
        let key_chat = "c".repeat(32);
        let device = "d".repeat(32);
        // (name, frame, region length, body key, expected plaintext). The three
        // session frames carry the time body; the command frames carry exactly
        // the payload handed to the builder.
        let slots96 = [0x80u8; 96];
        let cases: [(&str, Vec<u8>, usize, &[u8], &[u8]); 6] = [
            ("alive", alive_with(&c, &nonce, mid), 141, key_end.as_bytes(), b"{\"__time\":"),
            ("heartbeat", heartbeat(&c, &device, &nonce, mid), 216, key_chat.as_bytes(), b"{\"__time\":"),
            ("wake", wake(&c, &device, &nonce, mid), 193, key_chat.as_bytes(), b"{\"__time\":"),
            ("subscribe-0x30", subscribe(&c, &device, 0x30, 0xA0, &nonce, mid, b"\x01"), 209, key_chat.as_bytes(), b"\x01"),
            ("request-0x06", subscribe(&c, &device, 0x06, 0x10, &nonce, mid, b"\x00"), 209, key_chat.as_bytes(), b"\x00"),
            ("set-override", set_override(&c, &device, &nonce, mid, &slots96), 212, key_chat.as_bytes(), &slots96),
        ];
        // Option regions exactly as emitted by the Python builders.
        let goldens: [(&str, &str); 6] = [
            ("alive", concat!(
                "d9a0a06161616161616161616161616161616161616161616161616161616161616161",
                "a0a16767676767676767676767676767676767676767676767676767676767676767",
                "90a34e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e",
                "85f5616c697665",
                "9bf6616e645f4d4d4d4d4d4d4d4d4d4d31373935363131373535303534",
                "10b76170706c69636174696f6e2f6a736f6e")),
            ("heartbeat", concat!(
                "d9a0a06161616161616161616161616161616161616161616161616161616161616161",
                "a0a16767676767676767676767676767676767676767676767676767676767676767",
                "a0a26464646464646464646464646464646464646464646464646464646464646464",
                "90a34e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e",
                "81f101",
                "89f5686561727462656174",
                "a0b54141414141414141414141414141414141414141414141414141414141414141",
                "9bf6616e645f4d4d4d4d4d4d4d4d4d4d31373935363131373535303534",
                "10b76170706c69636174696f6e2f6a736f6e")),
            ("wake", concat!(
                "d9a0a06161616161616161616161616161616161616161616161616161616161616161",
                "a0a16767676767676767676767676767676767676767676767676767676767676767",
                "a0a26464646464646464646464646464646464646464646464646464646464646464",
                "90a34e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e",
                "81f101",
                "84f577616b65",
                "a0b54141414141414141414141414141414141414141414141414141414141414141",
                "1bf6616e645f4d4d4d4d4d4d4d4d4d4d31373935363131373535303534")),
            ("subscribe-0x30", concat!(
                "d9a0a06161616161616161616161616161616161616161616161616161616161616161",
                "a0a16767676767676767676767676767676767676767676767676767676767676767",
                "a0a26464646464646464646464646464646464646464646464646464646464646464",
                "90a34e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e",
                "81f101",
                "a0b54141414141414141414141414141414141414141414141414141414141414141",
                "82f530a0",
                "9bf6616e645f4d4d4d4d4d4d4d4d4d4d31373935363131373535303534",
                "10b76170706c69636174696f6e2f62797465")),
            ("request-0x06", concat!(
                "d9a0a06161616161616161616161616161616161616161616161616161616161616161",
                "a0a16767676767676767676767676767676767676767676767676767676767676767",
                "a0a26464646464646464646464646464646464646464646464646464646464646464",
                "90a34e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e",
                "81f101",
                "a0b54141414141414141414141414141414141414141414141414141414141414141",
                "82f50610",
                "9bf6616e645f4d4d4d4d4d4d4d4d4d4d31373935363131373535303534",
                "10b76170706c69636174696f6e2f62797465")),
            ("set-override", concat!(
                "d9a0a06161616161616161616161616161616161616161616161616161616161616161",
                "a0a16767676767676767676767676767676767676767676767676767676767676767",
                "84f100000001",
                "a0a26464646464646464646464646464646464646464646464646464646464646464",
                "90a34e4e4e4e4e4e4e4e4e4e4e4e4e4e4e4e",
                "a0b54141414141414141414141414141414141414141414141414141414141414141",
                "82f51aa0",
                "9bf6616e645f4d4d4d4d4d4d4d4d4d4d31373935363131373535303534",
                "10b76170706c69636174696f6e2f62797465")),
        ];
        for (name, frame, region_len, key, expect) in cases {
            let golden = goldens.iter().find(|(n, _)| *n == name).unwrap().1;
            let want = crate::codec::hex_decode(golden).unwrap();
            assert_eq!(region_len, want.len(), "{name}: golden region length");
            let got = &frame[..want.len()];
            if got != want.as_slice() {
                let d = got
                    .iter()
                    .zip(want.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or(got.len().min(want.len()));
                panic!(
                    "{name}: option region differs at byte {d}: ours {:?}, reference {:?}",
                    &got[d..(d + 14).min(got.len())],
                    &want[d..(d + 14).min(want.len())]
                );
            }
            // The body must decrypt under the right secret to what was built:
            // a time body for the session frames, the payload for commands.
            let pt = crate::codec::aes_cbc_decrypt(key, nonce.as_bytes(), &frame[want.len()..])
                .unwrap_or_else(|e| panic!("{name}: body decrypt: {e:?}"));
            if expect.starts_with(b"{") {
                let txt = String::from_utf8_lossy(&pt).into_owned();
                assert!(txt.starts_with("{\"__time\":"), "{name}: body = {txt}");
            } else {
                assert_eq!(pt.as_slice(), expect, "{name}: body payload");
            }
            // Round-trip through the parser: identity, cmd and msgid all found.
            let f = Frame::parse(&frame);
            assert_eq!(f.sender().unwrap(), c.end_id.as_bytes(), "{name}: sender");
            assert_eq!(f.group().unwrap(), c.group_id.as_bytes(), "{name}: group");
            assert_eq!(f.iv().unwrap(), nonce.as_bytes(), "{name}: iv");
            assert_eq!(f.msgid().unwrap(), mid.as_bytes(), "{name}: msgid");
        }
        // Command codes as the relay sees them.
        assert_eq!(
            Frame::parse(&subscribe(&c, &device, 0x30, 0xA0, &nonce, mid, b"\x01")).cmd(),
            Some([0x30, 0xA0])
        );
        assert_eq!(
            Frame::parse(&set_override(&c, &device, &nonce, mid, &[0x80u8; 96])).cmd(),
            Some([0x1A, 0xA0])
        );
    }
}
