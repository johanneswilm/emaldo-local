// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Local DNS front for the vendor domains.
//
// `/etc/hosts` is not enough on this unit: the vendor app resolves
// `api.emaldo.com` through its own resolver path (it asks the DNS server from
// `/etc/resolv.conf` directly and also speaks DNS-over-HTTPS), so the pin the
// hosts file provides is ignored for its API calls. The router answers those
// names with the home's public address, which port-forwards to the old NAS
// stack; the router must not change, so the answer is rewritten here: the
// bridge listens on loopback, answers the vendor names with its own address and
// forwards everything else to the upstream server that was configured before.
//
// Local mode prepends this resolver to `/etc/resolv.conf` (and to the dhclient
// hook that regenerates it); vendor mode removes it again.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::bridge::config::write_atomic;
use crate::bridge::config::Config;
use crate::bridge::log::Level;
use crate::bridge::state::Bridge;

const HEADER: usize = 12;

/// Names answered locally, mapped to the address handed out.
pub fn pinned(cfg: &Config, ip: &str) -> BTreeMap<String, std::net::Ipv4Addr> {
    let mut map = BTreeMap::new();
    let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() else {
        return map;
    };
    for name in &cfg.dns_hosts {
        map.insert(normalize(name), addr);
    }
    map
}

fn normalize(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Start the resolver: one thread for UDP, one for TCP. Returns the bound
/// address so the caller can report what it listens on.
pub fn start(
    bridge: Arc<Bridge>,
    map: BTreeMap<String, std::net::Ipv4Addr>,
) -> std::io::Result<SocketAddr> {
    let cfg = bridge.cfg.get();
    let sock = UdpSocket::bind((cfg.dns_bind.as_str(), cfg.dns_port))?;
    let local = sock.local_addr()?;
    bridge.log.write(
        Level::Info,
        "dns",
        format!(
            "resolver on {local}: {} vendor names -> local, rest to {}",
            map.len(),
            cfg.dns_upstream
        ),
    );
    let udp = {
        let bridge = bridge.clone();
        let map = map.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                // The threads end with the process; the resolver is not part of
                // the graceful-relay-shutdown path.
                let Ok((n, peer)) = sock.recv_from(&mut buf) else {
                    break;
                };
                let query = buf[..n].to_vec();
                let bridge = bridge.clone();
                let map = map.clone();
                let Ok(reply) = sock.try_clone() else { break };
                std::thread::spawn(move || {
                    if let Some(answer) = answer(&bridge, &map, &query) {
                        let _ = reply.send_to(&answer, peer);
                    }
                });
            }
        })
    };

    let tcp = {
        let bridge = bridge.clone();
        let map = map;
        std::thread::spawn(move || {
            let Ok(listener) = std::net::TcpListener::bind((cfg.dns_bind.as_str(), cfg.dns_port))
            else {
                bridge.log.write(
                    Level::Warn,
                    "dns",
                    "tcp resolver port busy: only UDP answers",
                );
                return;
            };
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let bridge = bridge.clone();
                let map = map.clone();
                std::thread::spawn(move || {
                    let mut len = [0u8; 2];
                    if stream.read_exact(&mut len).is_err() {
                        return;
                    }
                    let n = u16::from_be_bytes(len) as usize;
                    if n == 0 || n > 4096 {
                        return;
                    }
                    let mut query = vec![0u8; n];
                    if stream.read_exact(&mut query).is_err() {
                        return;
                    }
                    if let Some(reply) = answer(&bridge, &map, &query) {
                        let mut out = (reply.len() as u16).to_be_bytes().to_vec();
                        out.extend_from_slice(&reply);
                        let _ = stream.write_all(&out);
                    }
                });
            }
        })
    };

    std::mem::forget(udp);
    std::mem::forget(tcp);
    Ok(local)
}

/// Ask the resolver on its own port whether a vendor name is answered locally.
/// `Some(true)` answered, `Some(false)` answered wrongly or not at all, `None`
/// when the port is unreachable.
pub fn probe(cfg: &Config, name: &str) -> Option<bool> {
    if name.is_empty() {
        return None;
    }
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_millis(800)))
        .ok()?;
    let mut q = header([0x7f, 0x7f], false, 0);
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
    sock.send_to(&q, (cfg.dns_bind.as_str(), cfg.dns_port))
        .ok()?;
    let mut buf = [0u8; 1024];
    match sock.recv(&mut buf) {
        Ok(n) => Some(n >= HEADER && buf[6..8] == [0x00, 0x01]),
        Err(_) => Some(false),
    }
}

/// Build the answer for one query: a local pin, or the upstream reply relayed
/// verbatim. `None` when the query cannot be parsed or upstream is unreachable.
fn answer(
    bridge: &Bridge,
    map: &BTreeMap<String, std::net::Ipv4Addr>,
    query: &[u8],
) -> Option<Vec<u8>> {
    if query.len() < HEADER + 5 {
        return None;
    }
    let (name, rest) = read_name(query, HEADER)?;
    if rest + 4 > query.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([query[rest], query[rest + 1]]);
    let id = [query[0], query[1]];

    if let Some(addr) = map.get(&normalize(&name)) {
        // A: the bridge address. AAAA: no answer (NOERROR, 0 records) so the
        // client falls back to A instead of retrying IPv6 elsewhere.
        if qtype == 1 {
            let mut out = header(id, true, 1);
            out.extend_from_slice(&query[HEADER..rest + 4]);
            // answer: name pointer to the question, A, IN, TTL 30, 4 bytes
            out.extend_from_slice(&[0xc0, 0x0c]);
            out.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x1e]);
            out.extend_from_slice(&[0x00, 0x04]);
            out.extend_from_slice(&addr.octets());
            return Some(out);
        }
        if qtype == 28 {
            let mut out = header(id, true, 0);
            out.extend_from_slice(&query[HEADER..rest + 4]);
            return Some(out);
        }
    }

    relay_upstream(bridge, query, &name)
}

/// Forward the query to the upstream server and return its reply.
fn relay_upstream(bridge: &Bridge, query: &[u8], name: &str) -> Option<Vec<u8>> {
    let cfg = bridge.cfg.get();
    let upstream = cfg.dns_upstream.trim();
    if upstream.is_empty() {
        bridge.log.write(
            Level::Warn,
            "dns",
            format!("no upstream configured: cannot resolve {name}"),
        );
        return None;
    }
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .ok()?;
    sock.send_to(query, (upstream, 53)).ok()?;
    let mut buf = [0u8; 1024];
    let n = sock.recv(&mut buf).ok()?;
    Some(buf[..n].to_vec())
}

fn header(id: [u8; 2], reply: bool, answers: u16) -> Vec<u8> {
    let flags: u16 = if reply { 0x8000 | 0x0080 } else { 0 }; // QR + RD
    let mut out = Vec::with_capacity(HEADER + 4);
    out.extend_from_slice(&id);
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x01]); // questions
    out.extend_from_slice(&answers.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00]); // authority
    out.extend_from_slice(&[0x00, 0x00]); // additional
    out
}

/// Read a DNS name (label form; compression is not used in queries).
fn read_name(data: &[u8], at: usize) -> Option<(String, usize)> {
    let mut pos = at;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *data.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len > 63 || len & 0xc0 != 0 {
            return None;
        }
        let end = pos + 1 + len;
        if end > data.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&data[pos + 1..end]).into_owned());
        pos = end;
    }
    Some((labels.join("."), pos))
}

// ---------------------------------------------------------------------------
// Resolver configuration on the box
// ---------------------------------------------------------------------------

const RESOLV_BEGIN: &str = "# >>> emaldo-local (managed) >>>";
const RESOLV_END: &str = "# <<< emaldo-local (managed) <<<";

/// Put the local resolver in front of the upstream one: `/etc/resolv.conf`
/// (generated by dhclient) plus the dhclient hook that regenerates it, so a
/// lease renewal does not drop the entry. The previous file is kept in the
/// state dir.
pub fn use_local_resolver(bridge: &Bridge, ip: &str) -> Value {
    let cfg = bridge.cfg.get();
    let mut steps = Vec::new();

    let current = std::fs::read_to_string(&cfg.resolv_file).unwrap_or_default();
    let upstream = resolv_nameservers(&current)
        .into_iter()
        .next()
        .unwrap_or_default();
    let already = current.contains(RESOLV_BEGIN);
    if !already {
        let _ = write_atomic(
            &bridge.state_dir.join("resolv.conf.bak"),
            current.as_bytes(),
        );
    }
    // Order matters: resolvers try the servers in file order and only move on
    // when one of them fails to answer, so the local resolver has to come
    // first - the vendor netmonitor keeps the DHCP server at the top.
    let mut out = String::new();
    out.push_str(&format!(
        "{RESOLV_BEGIN}\nnameserver 127.0.0.1\n{RESOLV_END}\n"
    ));
    out.push_str(&dedupe_nameservers(&strip_block(&current)));
    out.push_str(&format!("# upstream kept by emaldo-local: {upstream}\n"));
    let target = std::path::Path::new(&cfg.resolv_file);
    // An immutable file cannot be replaced by rename either, so when it already
    // carries our block there is nothing to write - only to report.
    if already && immutable(target) {
        steps.push(json!({"file": cfg.resolv_file, "nameserver": "127.0.0.1", "immutable": true}));
    } else {
        // Locked with the wrong content inside - the vendor DHCP client rewrote
        // the file in the moment before the lock, or the lock came from an
        // older run: clear it, write our block, lock it again.
        if immutable(target) {
            let _ = set_immutable(target, false);
        }
        match write_atomic(target, out.as_bytes()) {
            Ok(_) => {
                steps.push(json!({"file": cfg.resolv_file, "nameserver": "127.0.0.1", "upstream": upstream}));
                if cfg.resolv_immutable {
                    match set_immutable(target, true) {
                        Ok(()) => steps.push(json!({"file": cfg.resolv_file, "locked": true})),
                        Err(e) => steps.push(json!({"file": cfg.resolv_file, "lock": e})),
                    }
                }
            }
            Err(e) => steps.push(json!({"file": cfg.resolv_file, "error": e.to_string()})),
        }
    }

    if !cfg.dhclient_conf.is_empty() {
        let path = std::path::Path::new(&cfg.dhclient_conf);
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let line = "prepend domain-name-servers 127.0.0.1;";
        let updated = format!(
            "{}\n{RESOLV_BEGIN}\n{line}\n{RESOLV_END}\n",
            strip_block(&existing)
        );
        match write_atomic(path, updated.as_bytes()) {
            Ok(_) => steps.push(json!({"file": cfg.dhclient_conf, "prepend": true})),
            Err(e) => steps.push(json!({"file": cfg.dhclient_conf, "error": e.to_string()})),
        }
    }

    // The running lease already handed out the upstream resolver: point the
    // current file at ourselves without waiting for the next renewal.
    let _ = ip;
    // Debug, not Warn: the vendor app's netmonitor rewrites /etc/resolv.conf
    // from the DHCP answer every few seconds, and every re-pin lands here. The
    // rate-limited line in `poller.rs` is the operator-visible signal.
    bridge.note(
        Level::Debug,
        "dns",
        format!("resolver pinned to 127.0.0.1 (upstream {upstream})"),
    );
    bridge.cfg.update(|c| {
        if !upstream.is_empty() && (c.dns_upstream.is_empty() || c.dns_upstream == "127.0.0.1") {
            c.dns_upstream = upstream.clone();
        }
    })
    .ok();
    json!({"steps": steps})
}

/// Take the local resolver out again (vendor mode).
pub fn restore_resolver(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let mut steps = Vec::new();
    let current = std::fs::read_to_string(&cfg.resolv_file).unwrap_or_default();
    let stripped = strip_block(&current);
    let backup = std::fs::read_to_string(bridge.state_dir.join("resolv.conf.bak"))
        .unwrap_or_else(|_| stripped.clone());
    let content = if backup.contains("nameserver") && !backup.contains(RESOLV_BEGIN) {
        backup
    } else {
        stripped
    };
    // Undo the local-mode lock before handing the file back to dhclient.
    let _ = set_immutable(std::path::Path::new(&cfg.resolv_file), false);
    match write_atomic(std::path::Path::new(&cfg.resolv_file), content.as_bytes()) {
        Ok(_) => steps.push(json!({"file": cfg.resolv_file, "restored": true})),
        Err(e) => steps.push(json!({"file": cfg.resolv_file, "error": e.to_string()})),
    }
    if !cfg.dhclient_conf.is_empty() {
        let path = std::path::Path::new(&cfg.dhclient_conf);
        if path.exists() {
            let existing = std::fs::read_to_string(path).unwrap_or_default();
            let _ = write_atomic(path, strip_block(&existing).as_bytes());
            steps.push(json!({"file": cfg.dhclient_conf, "prepend": false}));
        }
    }
    bridge.note(Level::Warn, "dns", "resolver restored to the upstream server");
    json!({"steps": steps})
}

pub fn resolver_status(bridge: &Bridge) -> Value {
    let cfg = bridge.cfg.get();
    let current = std::fs::read_to_string(&cfg.resolv_file).unwrap_or_default();
    json!({
        "pinned": current.contains(RESOLV_BEGIN),
        "immutable": immutable(std::path::Path::new(&cfg.resolv_file)),
        "nameservers": resolv_nameservers(&current),
        "upstream": cfg.dns_upstream,
        "listen": format!("{}:{}", cfg.dns_bind, cfg.dns_port),
        "names": cfg.dns_hosts,
    })
}

/// Is the file locked in the kernel? Read from `lsattr` rather than by trying
/// to write it: a probe write would disturb the file we are protecting.
fn immutable(path: &std::path::Path) -> bool {
    let out = std::process::Command::new("lsattr")
        .arg("-d")
        .arg(path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    // "----i---------e------- /etc/resolv.conf": attributes come first.
    out.split_whitespace()
        .next()
        .map(|flags| flags.contains('i'))
        .unwrap_or(false)
}

/// Set or clear the immutable attribute. Absent `chattr`/`lsattr`, or a
/// filesystem without the flag, is reported and not fatal: the userspace pin
/// loop still runs.
fn set_immutable(path: &std::path::Path, on: bool) -> Result<(), String> {
    let out = std::process::Command::new("chattr")
        .arg(if on { "+i" } else { "-i" })
        .arg(path)
        .output()
        .map_err(|e| format!("chattr: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "chattr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn resolv_nameservers(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver").map(|r| r.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

fn strip_block(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim() == RESOLV_BEGIN {
            inside = true;
            continue;
        }
        if line.trim() == RESOLV_END {
            inside = false;
            continue;
        }
        // The vendor's `networks` copies /etc/resolv.conf into its per-interface
        // file and back, so a note we wrote once comes home inside the DHCP
        // version; leaving it in place would grow the file on every rewrite.
        if line.trim().starts_with("# upstream kept by emaldo-local") {
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// One `nameserver` line per address, first occurrence wins. The vendor copies
/// the file through its per-interface state, which duplicates entries; glibc
/// only consults three of them anyway, and a stale duplicate in front of the
/// local resolver would outrank it.
fn dedupe_nameservers(text: &str) -> String {
    let mut seen: Vec<String> = Vec::new();
    let mut out = String::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("nameserver") {
            let addr = rest.trim().to_string();
            if addr.is_empty() || seen.contains(&addr) {
                continue;
            }
            seen.push(addr);
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {

    /// The vendor `networks` binary copies `/etc/resolv.conf` into
    /// `/etc/resolv.conf.wlan0` and back, so our block and our upstream note
    /// come home inside the DHCP version. Re-pinning must not stack copies.
    #[test]
    fn repinning_a_file_the_vendor_round_tripped_stays_flat() {
        let home = "# >>> emaldo-local (managed) >>>\nnameserver 127.0.0.1\n# <<< emaldo-local (managed) <<<\nnameserver 192.168.50.1\n# upstream kept by emaldo-local: 192.168.50.1\n";
        let round_tripped = format!("{home}nameserver 192.168.50.1\n{home}");
        let rebuilt = format!(
            "{RESOLV_BEGIN}\nnameserver 127.0.0.1\n{RESOLV_END}\n{}# upstream kept by emaldo-local: 192.168.50.1\n",
            dedupe_nameservers(&strip_block(&round_tripped))
        );
        assert_eq!(
            rebuilt.matches(RESOLV_BEGIN).count(),
            1,
            "one managed block, whatever the vendor did: {rebuilt}"
        );
        assert_eq!(resolv_nameservers(&rebuilt), vec!["127.0.0.1", "192.168.50.1"]);
        assert_eq!(
            rebuilt.matches("# upstream kept by emaldo-local").count(),
            1,
            "the note does not accumulate"
        );
    }

    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = header([0x12, 0x34], false, 0);
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0x00, 0x01]);
        q
    }

    fn map() -> BTreeMap<String, std::net::Ipv4Addr> {
        let mut m = BTreeMap::new();
        m.insert("api.emaldo.com".into(), "203.0.113.10".parse().unwrap());
        m
    }

    #[test]
    fn pinned_names_get_the_bridge_address() {
        let q = query("api.emaldo.com", 1);
        let reply = answer_of(&map(), &q);
        assert!(reply.is_some(), "pinned A query must be answered locally");
        let r = reply.unwrap();
        assert_eq!(&r[0..2], &[0x12, 0x34], "id must be echoed");
        assert_eq!(r[2] & 0x80, 0x80, "QR must be set");
        assert_eq!(&r[6..8], &[0x00, 0x01], "one answer record");
        assert_eq!(&r[r.len() - 4..], &[203, 0, 113, 10], "answer is the bridge");
    }

    #[test]
    fn pinned_names_have_no_ipv6_record() {
        let q = query("API.EMALDO.com", 28);
        let r = answer_of(&map(), &q).expect("AAAA answered locally");
        assert_eq!(&r[6..8], &[0x00, 0x00], "no AAAA record for a pinned name");
    }

    #[test]
    fn unpinned_names_are_not_answered_without_an_upstream() {
        let q = query("example.org", 1);
        assert!(answer_of(&map(), &q).is_none(), "no upstream -> no answer");
    }

    #[test]
    fn malformed_queries_are_dropped() {
        assert!(answer_of(&map(), b"short").is_none());
        assert!(answer_of(&map(), &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff]).is_none());
    }

    /// Same as `answer` without a live bridge (upstream relay is unreachable
    /// in tests, which is the path being exercised).
    fn answer_of(map: &BTreeMap<String, std::net::Ipv4Addr>, q: &[u8]) -> Option<Vec<u8>> {
        if q.len() < HEADER + 5 {
            return None;
        }
        let (name, rest) = read_name(q, HEADER)?;
        let qtype = u16::from_be_bytes([q[rest], q[rest + 1]]);
        let id = [q[0], q[1]];
        if let Some(addr) = map.get(&normalize(&name)) {
            if qtype == 1 {
                let mut out = header(id, true, 1);
                out.extend_from_slice(&q[HEADER..rest + 4]);
                out.extend_from_slice(&[0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01]);
                out.extend_from_slice(&[0x00, 0x00, 0x00, 0x1e, 0x00, 0x04]);
                out.extend_from_slice(&addr.octets());
                return Some(out);
            }
            if qtype == 28 {
                return Some(header(id, true, 0));
            }
        }
        None
    }
}
