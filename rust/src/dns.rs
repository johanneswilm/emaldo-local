// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Minimal DNS A/AAAA resolver. The driver ships as a fully static binary;
// static glibc cannot reliably use NSS (getaddrinfo dlopens libnss at
// runtime), so broker hostnames are resolved with a tiny hand-rolled UDP
// DNS client against the servers in /etc/resolv.conf. IP literals bypass
// DNS entirely. std's resolver remains as a last-resort fallback.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

fn nameservers() -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("nameserver") {
                if let Some(ip) = rest.trim().split_whitespace().next() {
                    if let Ok(a) = ip.parse::<IpAddr>() {
                        out.push(a);
                    }
                }
            }
        }
    }
    if out.is_empty() {
        // systemd-resolved stub is the common default; fall back to it.
        out.push(IpAddr::from([127, 0, 0, 53]));
    }
    out
}

fn encode_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(name.len() + 18);
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // RD
    q.extend_from_slice(&[0x00, 0x01]); // QDCOUNT
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR
    for label in name.trim_end_matches('.').split('.') {
        let l = label.as_bytes();
        q.push(l.len().min(63) as u8);
        q.extend_from_slice(&l[..l.len().min(63)]);
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&[0x00, 0x01]); // IN
    q
}

fn skip_name(msg: &[u8], mut i: usize) -> usize {
    while i < msg.len() {
        let l = msg[i] as usize;
        if l == 0 {
            return i + 1;
        }
        if l & 0xC0 == 0xC0 {
            return i + 2; // compression pointer ends the name
        }
        i += 1 + l;
    }
    i
}

fn parse_answers(msg: &[u8], want_v6: bool) -> Option<IpAddr> {
    if msg.len() < 12 {
        return None;
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]) as usize;
    let an = u16::from_be_bytes([msg[6], msg[7]]) as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(msg, i);
        i += 4;
    }
    for _ in 0..an {
        if i >= msg.len() {
            return None;
        }
        i = skip_name(msg, i);
        if i + 10 > msg.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([msg[i], msg[i + 1]]);
        let rdlen = u16::from_be_bytes([msg[i + 8], msg[i + 9]]) as usize;
        i += 10;
        if i + rdlen > msg.len() {
            return None;
        }
        if rtype == 1 && rdlen == 4 && !want_v6 {
            return Some(IpAddr::from([msg[i], msg[i + 1], msg[i + 2], msg[i + 3]]));
        }
        if rtype == 28 && rdlen == 16 && want_v6 {
            let mut b = [0u8; 16];
            b.copy_from_slice(&msg[i..i + 16]);
            return Some(IpAddr::from(b));
        }
        i += rdlen;
    }
    None
}

fn query(server: IpAddr, name: &str, qtype: u16) -> io::Result<Option<IpAddr>> {
    let sock = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let id = (std::process::id() as u16) ^ 0x5A5A;
    let q = encode_query(id, name, qtype);
    let port = if server.is_ipv4() { 53 } else { 53 };
    sock.send_to(&q, SocketAddr::new(server, port))?;
    let mut buf = [0u8; 512];
    match sock.recv(&mut buf) {
        Ok(n) => Ok(parse_answers(&buf[..n], qtype == 28)),
        Err(e) => Err(e),
    }
}

/// Resolve `host` to an IP. IP literals pass through. Tries our own UDP
/// resolver first, then std's getaddrinfo as a fallback.
pub fn resolve(host: &str) -> io::Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    for ns in nameservers() {
        match query(ns, host, 1) {
            Ok(Some(ip)) => return Ok(ip),
            _ => {}
        }
        match query(ns, host, 28) {
            Ok(Some(ip)) => return Ok(ip),
            _ => {}
        }
    }
    use std::net::ToSocketAddrs;
    (host, 0)
        .to_socket_addrs()?
        .next()
        .map(|a| a.ip())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {host}")))
}
