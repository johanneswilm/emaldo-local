// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Minimal self-contained MQTT 3.1.1 client: CONNECT/PUBLISH/SUBSCRIBE,
// QoS 0 and 1 (fire-and-forget on ack timeout), plain TCP or TLS,
// auto-reconnect with backoff. Single I/O thread with a buffered decoder,
// so partial reads across timeouts never corrupt framing. Port of the Go
// reference (internal/mqtt/client.go).

use crate::dns;
use crate::tls;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Message {
    pub topic: String,
    pub payload: Vec<u8>,
}

pub type Handler = Arc<dyn Fn(Message) + Send + Sync>;

struct Pkt {
    topic: String,
    data: Vec<u8>,
    qos: u8,
    retain: bool,
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Transport {
    fn set_read_timeout(&self, d: Option<Duration>) {
        match self {
            Transport::Plain(s) => {
                let _ = s.set_read_timeout(d);
            }
            Transport::Tls(s) => {
                let _ = s.get_ref().set_read_timeout(d);
            }
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

pub struct Client {
    pub url: String,
    pub username: String,
    pub password: String,
    pub client_id: String,
    pub tls_insecure: bool,
    /// Last will and testament: published by the broker if we vanish without
    /// DISCONNECT, which is what makes Home Assistant's availability work.
    pub will: Option<(String, Vec<u8>)>,
    /// Runs after every successful CONNACK (initial connect and every
    /// reconnect) so retained discovery configs are re-published when the
    /// broker or Home Assistant restarts.
    on_connect: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    on_message: Mutex<Option<Handler>>,
    out: Mutex<Sender<Pkt>>,
    out_rx: Mutex<Option<Receiver<Pkt>>>,
    subs: Mutex<HashMap<String, Handler>>,
    closed: AtomicBool,
}

impl Client {
    pub fn new(
        url: &str,
        username: &str,
        password: &str,
        client_id: &str,
        tls_insecure: bool,
        will: Option<(&str, &[u8])>,
    ) -> Arc<Client> {
        let (tx, rx) = channel();
        let c = Arc::new(Client {
            url: url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
            client_id: client_id.to_string(),
            tls_insecure,
            will: will.map(|(t, p)| (t.to_string(), p.to_vec())),
            on_connect: Mutex::new(None),
            on_message: Mutex::new(None),
            out: Mutex::new(tx),
            out_rx: Mutex::new(Some(rx)),
            subs: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });
        let rx = c.out_rx.lock().unwrap().take().unwrap();
        let c2 = c.clone();
        std::thread::Builder::new()
            .name("mqtt".into())
            .spawn(move || c2.run(rx))
            .expect("spawn mqtt thread");
        c
    }

    pub fn set_on_message(&self, h: Handler) {
        *self.on_message.lock().unwrap() = Some(h);
    }

    pub fn set_on_connect(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.on_connect.lock().unwrap() = Some(f);
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn publish(&self, topic: &str, payload: &[u8], qos: u8) {
        let _ = self.out.lock().unwrap().send(Pkt {
            topic: topic.to_string(),
            data: payload.to_vec(),
            qos,
            retain: false,
        });
    }

    /// QoS 1 + retain: what Home Assistant expects for discovery configs and
    /// for availability, so a broker/HA restart does not lose the device.
    pub fn publish_retained(&self, topic: &str, payload: &[u8]) {
        let _ = self.out.lock().unwrap().send(Pkt {
            topic: topic.to_string(),
            data: payload.to_vec(),
            qos: 1,
            retain: true,
        });
    }

    pub fn subscribe(&self, topic: &str, h: Handler) {
        self.subs.lock().unwrap().insert(topic.to_string(), h);
        // Re-subscription happens on (re)connect; if currently connected the
        // next reconnect picks it up. For immediate effect we push a marker
        // through the out queue.
        let _ = self.out.lock().unwrap().send(Pkt {
            topic: format!("__subscribe__{topic}"),
            data: Vec::new(),
            qos: 0,
            retain: false,
        });
    }

    fn run(&self, rx: Receiver<Pkt>) {
        let mut backoff = Duration::from_secs(1);
        while !self.closed.load(Ordering::SeqCst) {
            match self.connect_once(&rx) {
                Ok(()) => return, // closed
                Err(e) => {
                    eprintln!("mqtt: {e}");
                }
            }
            let mut slept = Duration::ZERO;
            while slept < backoff && !self.closed.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                slept += Duration::from_millis(100);
            }
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    fn connect_once(&self, rx: &Receiver<Pkt>) -> Result<(), String> {
        let (host, port, use_tls) = parse_url(&self.url)?;
        let ip = dns::resolve(&host).map_err(|e| format!("resolve {host}: {e}"))?;
        let addr = SocketAddr::new(ip, port);
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .map_err(|e| format!("connect {addr}: {e}"))?;
        let _ = sock.set_nodelay(true);

        let mut t = if use_tls {
            let cfg = tls::client_config(self.tls_insecure);
            let name = tls::server_name(&host)?;
            let conn = rustls::ClientConnection::new(cfg, name)
                .map_err(|e| format!("tls: {e}"))?;
            Transport::Tls(Box::new(rustls::StreamOwned::new(conn, sock)))
        } else {
            Transport::Plain(sock)
        };

        // CONNECT + CONNACK with a hard deadline.
        t.set_read_timeout(Some(Duration::from_secs(10)));
        let connect_pkt = self.encode_connect();
        write_packet(&mut t, 0x10, &connect_pkt).map_err(|e| format!("write connect: {e}"))?;
        let mut rxbuf: Vec<u8> = Vec::new();
        read_packet_buffered(&mut t, &mut rxbuf).map_err(|e| format!("connack: {e}"))?;

        // Resubscribe.
        let subs: Vec<String> = self.subs.lock().unwrap().keys().cloned().collect();
        let mut mid: u16 = 0;
        for topic in &subs {
            mid = mid.wrapping_add(1);
            let mut body = encode_str(topic);
            body.push(0); // QoS 0
            let mut full = mid.to_be_bytes().to_vec();
            full.extend_from_slice(&body);
            write_packet(&mut t, 0x82, &full).map_err(|e| format!("subscribe: {e}"))?;
        }

        // Re-publish discovery configs and availability on every connect, so a
        // broker or Home Assistant restart does not silently drop the device.
        if let Some(f) = self.on_connect.lock().unwrap().clone() {
            f();
        }

        t.set_read_timeout(Some(Duration::from_millis(500)));
        let mut last_ping = Instant::now();
        let mut pending_qos1: HashMap<u16, Instant> = HashMap::new();

        loop {
            if self.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            // Drain outgoing queue.
            while let Ok(p) = rx.try_recv() {
                if let Some(topic) = p.topic.strip_prefix("__subscribe__") {
                    mid = mid.wrapping_add(1);
                    let mut body = encode_str(topic);
                    body.push(0);
                    let mut full = mid.to_be_bytes().to_vec();
                    full.extend_from_slice(&body);
                    write_packet(&mut t, 0x82, &full).map_err(|e| format!("subscribe: {e}"))?;
                    continue;
                }
                let hdr = 0x30u8
                    | ((p.qos & 0x03) << 1)
                    | if p.retain { 1 } else { 0 };
                let mut body = encode_str(&p.topic);
                if p.qos == 1 {
                    mid = mid.wrapping_add(1);
                    body.extend_from_slice(&mid.to_be_bytes());
                }
                body.extend_from_slice(&p.data);
                write_packet(&mut t, hdr, &body).map_err(|e| format!("publish: {e}"))?;
                if p.qos == 1 {
                    pending_qos1.insert(mid, Instant::now());
                }
            }
            pending_qos1.retain(|_, at| at.elapsed() < Duration::from_secs(5));
            if last_ping.elapsed() > Duration::from_secs(30) {
                write_packet(&mut t, 0xC0, &[]).map_err(|e| format!("ping: {e}"))?;
                last_ping = Instant::now();
            }

            // Read whatever is available; decode complete packets.
            let mut tmp = [0u8; 4096];
            match t.read(&mut tmp) {
                Ok(0) => return Err("connection closed by broker".into()),
                Ok(n) => rxbuf.extend_from_slice(&tmp[..n]),
                Err(e) if is_timeout(&e) => continue,
                Err(e) => return Err(format!("read: {e}")),
            }
            while let Some((hdr, payload)) = take_packet(&mut rxbuf) {
                match hdr >> 4 {
                    3 => {
                        if payload.len() < 2 {
                            continue;
                        }
                        let tl = u16::from_be_bytes([payload[0], payload[1]]) as usize;
                        if payload.len() < 2 + tl {
                            continue;
                        }
                        let topic = String::from_utf8_lossy(&payload[2..2 + tl]).to_string();
                        let mut rest = &payload[2 + tl..];
                        if hdr & 0x06 != 0 {
                            if rest.len() < 2 {
                                continue;
                            }
                            let pmid = u16::from_be_bytes([rest[0], rest[1]]);
                            rest = &rest[2..];
                            let _ = write_packet(&mut t, 0x40, &pmid.to_be_bytes());
                        }
                        let h = {
                            let subs = self.subs.lock().unwrap();
                            subs.iter()
                                .find(|(pat, _)| topic_matches(pat, &topic))
                                .map(|(_, h)| h.clone())
                        }
                        .or_else(|| self.on_message.lock().unwrap().clone());
                        if let Some(h) = h {
                            h(Message {
                                topic,
                                payload: rest.to_vec(),
                            });
                        }
                    }
                    4 => {
                        if payload.len() >= 2 {
                            let pmid = u16::from_be_bytes([payload[0], payload[1]]);
                            pending_qos1.remove(&pmid);
                        }
                    }
                    13 => {} // PINGRESP
                    _ => {}
                }
            }
        }
    }

    fn encode_connect(&self) -> Vec<u8> {
        let mut vh = Vec::new();
        // The protocol name is a length-prefixed string, like every other
        // MQTT UTF-8 string: 0x00 0x04 "MQTT".
        vh.extend_from_slice(&encode_str("MQTT"));
        vh.push(4); // level
        let mut flags = 0x02u8; // clean session
        if let Some((topic, _)) = &self.will {
            if !topic.is_empty() {
                flags |= 0x04; // Will Flag
                flags |= 0x20; // Will Retain: the offline notice must be kept
                flags |= 0x08; // Will QoS 1 (bits 3-4)
            }
        }
        if !self.username.is_empty() {
            flags |= 0x80;
            if !self.password.is_empty() {
                flags |= 0x40;
            }
        }
        vh.push(flags);
        vh.extend_from_slice(&[0x00, 0x3C]); // keepalive 60
        let mut payload = encode_str(&self.client_id);
        // Will topic and will message come before the credentials, in that order.
        if flags & 0x04 != 0 {
            if let Some((topic, msg)) = &self.will {
                payload.extend_from_slice(&encode_str(topic));
                payload.extend_from_slice(&encode_bytes(msg));
            }
        }
        if flags & 0x80 != 0 {
            payload.extend_from_slice(&encode_str(&self.username));
            if flags & 0x40 != 0 {
                payload.extend_from_slice(&encode_str(&self.password));
            }
        }
        vh.extend_from_slice(&payload);
        vh
    }
}

fn parse_url(url: &str) -> Result<(String, u16, bool), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("invalid broker URL {url:?} (want mqtt://host:port)"))?;
    let use_tls = match scheme {
        "mqtt" | "tcp" => false,
        "mqtts" => true,
        _ => return Err(format!("invalid broker scheme {scheme:?}")),
    };
    let default_port = if use_tls { 8883 } else { 1883 };
    // Strip any path; split host:port (no IPv6 literal support needed here).
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>().map_err(|_| format!("bad port in {url:?}"))?,
        ),
        None => (hostport.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(format!("empty host in {url:?}"));
    }
    Ok((host, port, use_tls))
}

fn topic_matches(pat: &str, topic: &str) -> bool {
    if pat == topic {
        return true;
    }
    if let Some(pre) = pat.strip_suffix('#') {
        return topic.starts_with(pre);
    }
    if let Some(i) = pat.find('+') {
        return topic.starts_with(&pat[..i]) && topic.ends_with(&pat[i + 1..]);
    }
    false
}

fn encode_str(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + s.len());
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    out
}

/// Same 2-byte length prefix as a UTF-8 string, for will payloads (which are
/// arbitrary bytes, not necessarily valid UTF-8).
fn encode_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + b.len());
    out.extend_from_slice(&(b.len() as u16).to_be_bytes());
    out.extend_from_slice(b);
    out
}

fn encode_rem_len(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut d = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            d |= 0x80;
        }
        out.push(d);
        if n == 0 {
            return out;
        }
    }
}

fn write_packet(t: &mut Transport, hdr: u8, body: &[u8]) -> std::io::Result<()> {
    let mut frame = vec![hdr];
    frame.extend_from_slice(&encode_rem_len(body.len()));
    frame.extend_from_slice(body);
    t.write_all(&frame)?;
    t.flush()
}

/// Read one full packet from the transport into `buf`-style streaming.
/// Used for the CONNACK where a blocking read is fine.
fn read_packet_buffered(t: &mut Transport, rxbuf: &mut Vec<u8>) -> Result<(), String> {
    loop {
        if let Some(_pkt) = take_packet(rxbuf) {
            return Ok(());
        }
        let mut tmp = [0u8; 2048];
        match t.read(&mut tmp) {
            Ok(0) => return Err("eof".into()),
            Ok(n) => rxbuf.extend_from_slice(&tmp[..n]),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Try to extract one complete MQTT packet from the buffer.
fn take_packet(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if buf.len() < 2 {
        return None;
    }
    let hdr = buf[0];
    let mut mult = 1usize;
    let mut rem = 0usize;
    let mut i = 1usize;
    loop {
        if i >= buf.len() {
            return None;
        }
        let b = buf[i];
        rem += (b & 0x7F) as usize * mult;
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
        mult *= 128;
        if mult > 128 * 128 * 128 {
            buf.clear(); // garbage; resync by dropping
            return None;
        }
    }
    if buf.len() < i + rem {
        return None;
    }
    let payload = buf[i..i + rem].to_vec();
    buf.drain(..i + rem);
    Some((hdr, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rem_len_roundtrip() {
        for n in [0usize, 1, 127, 128, 16383, 16384] {
            let enc = encode_rem_len(n);
            let mut buf = vec![0x30u8];
            buf.extend_from_slice(&enc);
            buf.extend_from_slice(&vec![0u8; n]);
            let (hdr, payload) = take_packet(&mut buf).unwrap();
            assert_eq!(hdr, 0x30);
            assert_eq!(payload.len(), n);
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn partial_packet_waits() {
        let mut buf = vec![0x30u8, 10, 1, 2, 3]; // says 10 bytes, gives 3
        assert!(take_packet(&mut buf).is_none());
        assert_eq!(buf.len(), 5); // nothing consumed
    }

    #[test]
    fn topic_match() {
        assert!(topic_matches("a/b/#", "a/b/c/d"));
        assert!(topic_matches("a/+/c", "a/b/c"));
        assert!(!topic_matches("a/+/c", "a/b/d"));
        assert!(topic_matches("a/b", "a/b"));
    }

    #[test]
    fn url_parse() {
        assert_eq!(parse_url("mqtt://10.0.0.2:1883").unwrap(), ("10.0.0.2".into(), 1883, false));
        assert_eq!(parse_url("mqtts://broker.local").unwrap(), ("broker.local".into(), 8883, true));
        assert!(parse_url("http://x").is_err());
    }
}

#[cfg(test)]
mod conn_wire_tests {
    use super::*;

    /// Mock broker: capture the CONNECT frame, answer with a CONNACK.
    #[test]
    fn connect_frame_matches_mqtt311() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = sock.set_read_timeout(Some(Duration::from_millis(400)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break, // read timeout: the frame is complete
                }
            }
            let _ = sock.write_all(&[0x20, 0x02, 0x00, 0x00]);
            let _ = sock.flush();
            std::thread::sleep(Duration::from_millis(300));
            buf
        });
        let c = Client::new(
            &format!("mqtt://127.0.0.1:{port}"),
            "user",
            "pass",
            "HMvSUfAafuYCokvv",
            false,
            Some(("emaldo/powercore/status", b"offline".as_slice())),
        );
        std::thread::sleep(Duration::from_millis(500));
        c.close();
        let buf = handle.join().unwrap();
        eprintln!("captured {} bytes: {:02x?}", buf.len(), &buf[..buf.len().min(64)]);
        assert_eq!(buf[0], 0x10, "CONNECT fixed header");
        // variable header: length-prefixed name, level, flags, keepalive
        assert_eq!(&buf[2..4], &[0x00, 0x04], "protocol name length");
        assert_eq!(&buf[4..8], b"MQTT", "protocol name");
        assert_eq!(buf[8], 4, "protocol level");
        // clean session + will + will QoS1 + will retain + username + password
        assert_eq!(buf[9], 0xEE, "connect flags");
        assert_eq!(&buf[10..12], &[0x00, 0x3C], "keepalive 60");
        let clen = u16::from_be_bytes([buf[12], buf[13]]) as usize;
        assert_eq!(&buf[14..14 + clen], b"HMvSUfAafuYCokvv", "client id");
        let mut p = 14 + clen;
        let wlen = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
        assert_eq!(&buf[p + 2..p + 2 + wlen], b"emaldo/powercore/status", "will topic");
        p += 2 + wlen;
        let mlen = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
        assert_eq!(&buf[p + 2..p + 2 + mlen], b"offline", "will message");
        p += 2 + mlen;
        let ulen = u16::from_be_bytes([buf[p], buf[p + 1]]) as usize;
        assert_eq!(&buf[p + 2..p + 2 + ulen], b"user", "username");
        let rem = encode_rem_len(buf.len() - 2);
        assert_eq!(buf[1], rem[0], "remaining length");
    }
}
