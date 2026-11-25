// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Nordpool day-ahead price feed (keyless Data Portal API) plus an on-disk
// two-day cache.
//
//   GET https://dataportal-api.nordpoolgroup.com/api/DayAheadPrices
//         ?date=YYYY-MM-DD&market=DayAhead&deliveryArea=SE4&currency=EUR
//
// Response: {"deliveryDateCET":"...","version":N,"multiAreaEntries":[
//   {"deliveryStart":"2026-11-24T23:00:00Z","deliveryEnd":"...",
//    "entryPerArea":{"SE4":225.63}}, ...]}
//
// 96 quarter-hour entries per local delivery day; price in EUR/MWh.
// Prices are immutable once a day is published, so the cache only needs a
// refetch when a calendar day rolls over or tomorrow's `version` changes.

use crate::timectl;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const API_HOST: &str = "dataportal-api.nordpoolgroup.com";
pub const API_PORT: u16 = 443;
pub const PATH: &str = "/api/DayAheadPrices";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Slot {
    /// Slot start, UNIX seconds (UTC).
    pub start: i64,
    /// Spot price EUR/MWh (integer source value kept as f64).
    pub eur_mwh: f64,
}

impl Slot {
    pub fn eur_kwh(&self) -> f64 {
        self.eur_mwh / 1000.0
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Prices {
    pub delivery_area: String,
    pub currency: String,
    /// Local delivery date the cache was built for (`YYYY-MM-DD`).
    pub today: String,
    pub tomorrow: Option<String>,
    pub version_today: Option<i64>,
    pub version_tomorrow: Option<i64>,
    pub fetched_at: i64,
    /// 96 slots for today then 96 for tomorrow (if known), sorted by start.
    pub slots: Vec<Slot>,
}

impl Prices {
    /// Mean EUR/MWh for the slots of a given local date.
    pub fn day_mean(&self, local_date: &str, offset: i64) -> Option<f64> {
        let v: Vec<f64> = self
            .slots
            .iter()
            .filter(|s| timectl::date_string(s.start, offset) == local_date)
            .map(|s| s.eur_mwh)
            .collect();
        if v.is_empty() {
            None
        } else {
            Some(v.iter().sum::<f64>() / v.len() as f64)
        }
    }
}

/// Raw API shape (the fields we use).
#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    version: Option<i64>,
    #[serde(rename = "multiAreaEntries", default)]
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "deliveryStart")]
    delivery_start: String,
    #[serde(rename = "entryPerArea", default)]
    per_area: std::collections::BTreeMap<String, Option<f64>>,
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` to UNIX seconds (UTC only; Nordpool always
/// emits Z here).
pub fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |a: usize, n: usize| -> Option<i64> { s.get(a..a + n)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    let (h, mi, se) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    Some(
        timectl::ymd_to_unix_days(y, mo, d) * 86400 + h * 3600 + mi * 60 + se,
    )
}

/// Parse a day's JSON into slots for `area`.
pub fn parse_day(body: &str, area: &str) -> Result<(Option<i64>, Vec<Slot>), String> {
    let t = body.trim();
    // Some Nordpool responses for unpublished/future days are a bare
    // integer (e.g. "2") rather than an object; treat as "no data".
    if !t.starts_with('{') {
        return Ok((None, Vec::new()));
    }
    let raw: Raw = serde_json::from_str(t).map_err(|e| format!("json: {e}"))?;
    let mut slots = Vec::with_capacity(raw.entries.len());
    for e in raw.entries {
        let start = parse_utc(&e.delivery_start)
            .ok_or_else(|| format!("bad deliveryStart {:?}", e.delivery_start))?;
        if let Some(Some(price)) = e.per_area.get(area) {
            slots.push(Slot {
                start,
                eur_mwh: *price,
            });
        }
    }
    slots.sort_by_key(|s| s.start);
    Ok((raw.version, slots))
}

pub fn build_url(area: &str, date: &str, currency: &str) -> String {
    format!(
        "https://{API_HOST}{PATH}?date={date}&market=DayAhead&deliveryArea={area}&currency={currency}"
    )
}

fn cache_path(dir: &Path, area: &str, date: &str) -> PathBuf {
    dir.join(format!("prices-{area}-{date}.json"))
}

/// Load a cached day file (top-level shape of `Prices` is fine to reuse; we
/// only need the slots/version), returning slots regardless of area label.
fn load_cached(path: &Path) -> Option<Prices> {
    let txt = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Prices>(&txt).ok()
}

fn store_cached(path: &Path, p: &Prices) {
    if let Ok(txt) = serde_json::to_string_pretty(p) {
        let _ = std::fs::write(path, txt);
    }
}

/// Fetch one local delivery day. Tries the cache first; on a miss fetches the
/// API. `fetcher` is injected so the bench can run without network.
pub fn day(
    dir: &Path,
    area: &str,
    currency: &str,
    date: &str,
    mut fetcher: impl FnMut(&str) -> Result<String, String>,
) -> Result<(Option<i64>, Vec<Slot>), String> {
    let path = cache_path(dir, area, date);

    let body = match fetcher(&build_url(area, date, currency)) {
        Ok(b) if !b.trim().is_empty() => b,
        // Empty body (HTTP 204) = day not published yet; or a network error.
        _ => {
            if let Some(cached) = load_cached(&path) {
                return Ok((cached.version_today, cached.slots));
            }
            return Err(format!("no data for {date} @ {area}"));
        }
    };

    let (version, slots) = parse_day(&body, area)?;
    if slots.is_empty() {
        return Err(format!("no slots for {area} on {date}"));
    }
    let p = Prices {
        delivery_area: area.into(),
        currency: currency.into(),
        today: date.into(),
        version_today: version,
        fetched_at: timectl::now_unix(),
        slots: slots.clone(),
        ..Default::default()
    };
    store_cached(&path, &p);
    Ok((version, slots))
}

/// Refresh today (+ tomorrow when published) into one `Prices` object.
/// `offset_secs` is the local UTC offset used to key delivery days.
pub fn refresh(
    dir: &Path,
    area: &str,
    currency: &str,
    offset_secs: i64,
    mut fetcher: impl FnMut(&str) -> Result<String, String>,
) -> Result<Prices, String> {
    let now = timectl::now_unix();
    let today = timectl::date_string(now, offset_secs);
    let tomorrow = timectl::date_string(now + 86400, offset_secs);

    let mut out = Prices {
        delivery_area: area.into(),
        currency: currency.into(),
        today: today.clone(),
        fetched_at: now,
        ..Default::default()
    };

    let (tv, mut slots) = day(dir, area, currency, &today, &mut fetcher)?;
    out.version_today = tv;

    // Tomorrow is optional: before ~13:00 the API 400s / returns no slots.
    if let Ok((dv, tslots)) = day(dir, area, currency, &tomorrow, &mut fetcher) {
        if !tslots.is_empty() {
            out.tomorrow = Some(tomorrow.clone());
            out.version_tomorrow = dv;
            slots.extend(tslots);
        }
    }
    slots.sort_by_key(|s| s.start);
    slots.dedup_by_key(|s| s.start);
    out.slots = slots;
    // Persist the combined view for fast reload.
    store_cached(&dir.join(format!("prices-{area}-latest.json")), &out);
    Ok(out)
}

/// Minimal HTTPS GET using the crate's rustls client config.
pub fn https_get(host: &str, port: u16, path: &str) -> Result<String, String> {
    use rustls::StreamOwned;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let ip = crate::dns::resolve(host).map_err(|e| format!("resolve {host}: {e}"))?;
    let addr = SocketAddr::new(ip, port);
    let sock = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(10))
        .map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(15)));
    let cfg = crate::tls::client_config(false);
    let name = crate::tls::server_name(host)?;
    let conn = rustls::ClientConnection::new(cfg, name).map_err(|e| format!("tls: {e}"))?;
    let mut tls = StreamOwned::new(conn, sock);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: emaldo-driver\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut resp = Vec::new();
    tls.read_to_end(&mut resp)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&resp);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed http response".to_string())?;
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    let body = if chunked { dechunk(body) } else { body.to_string() };
    let body = body.as_str();
    let status = head.lines().next().unwrap_or("");
    if status.contains(" 204") || body.trim().is_empty() {
        // Nordpool answers 204 (empty body) for a day not yet published.
        return Ok(String::new());
    }
    if !status.contains(" 200") {
        return Err(format!("http: {status}"));
    }
    Ok(body.to_string())
}

/// Decode a chunked HTTP body if it uses `<hexlen>\r\n<data>\r\n` framing;
/// pass through otherwise.
fn dechunk(body: &str) -> String {
    let mut rest = body;
    let mut out = String::new();
    let mut first = true;
    loop {
        let Some(nl) = rest.find("\r\n") else {
            if first {
                return body.to_string();
            }
            break;
        };
        let size_s = rest[..nl].split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_s, 16) else {
            // Not chunked: return original.
            return body.to_string();
        };
        first = false;
        if size == 0 {
            break;
        }
        let start = nl + 2;
        let end = (start + size).min(rest.len());
        out.push_str(&rest[start..end]);
        rest = rest.get(end + 2..).unwrap_or("");
    }
    out
}

/// Convenience wrapper used by the background refresher / CLI.
pub fn refresh_live(
    dir: &Path,
    area: &str,
    currency: &str,
    offset_secs: i64,
) -> Result<Prices, String> {
    refresh(dir, area, currency, offset_secs, |url| {
        let rest = url
            .split_once("://")
            .map(|(_, r)| r)
            .ok_or_else(|| "bad url".to_string())?;
        let (hostport, pathq) = rest.split_once('/').ok_or_else(|| "bad url".to_string())?;
        let host = hostport.split(':').next().unwrap_or(hostport);
        https_get(host, API_PORT, &format!("/{pathq}"))
    })
}

/// Background refresher: fetch today+tomorrow and publish a compact view into
/// the telemetry store. Read-only (outbound HTTPS GET). Refreshes every
/// `refresh_secs`; a failed fetch keeps the last good data.
pub fn start(
    dir: std::path::PathBuf,
    area: String,
    currency: String,
    offset: i64,
    refresh_secs: u64,
    telem: std::sync::Arc<crate::telemetry::Store>,
) {
    std::thread::Builder::new()
        .name("prices".into())
        .spawn(move || {
            let mut last_ok: Option<Prices> = None;
            let mut delay = 0u64;
            loop {
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(delay.min(300)));
                }
                match refresh_live(&dir, &area, &currency, offset) {
                    Ok(p) => {
                        publish(&telem, &p, offset);
                        telem.add_event("info", "prices", format!(
                            "{} {}: {} slots, today mean {:?} EUR/MWh",
                            p.delivery_area, p.currency, p.slots.len(),
                            p.day_mean(&p.today, offset).map(|v| (v * 100.0).round() / 100.0)));
                        last_ok = Some(p);
                        delay = refresh_secs.max(300);
                    }
                    Err(e) => {
                        if let Some(p) = &last_ok {
                            publish(&telem, p, offset);
                        }
                        telem.add_event("warn", "prices", format!("refresh: {e} (retrying in 120s)"));
                        delay = 120;
                    }
                }
            }
        })
        .expect("spawn prices refresher");
}

fn publish(telem: &crate::telemetry::Store, p: &Prices, offset: i64) {
    let now = timectl::now_unix();
    let next: Vec<crate::telemetry::PriceSlot> = p
        .slots
        .iter()
        .filter(|s| s.start + 900 > now && s.start <= now + 48 * 3600)
        .take(192)
        .map(|s| crate::telemetry::PriceSlot {
            start: s.start as u64,
            local_hhmm: timectl::local_hhmm(s.start, offset),
            eur_kwh: s.eur_kwh(),
        })
        .collect();
    telem.update(|st| {
        st.prices = crate::telemetry::PricesState {
            enabled: true,
            area: p.delivery_area.clone(),
            currency: p.currency.clone(),
            today: p.today.clone(),
            tomorrow: p.tomorrow.clone(),
            updated_at: p.fetched_at as u64,
            today_mean_eur_mwh: p.day_mean(&p.today, offset),
            tomorrow_mean_eur_mwh: p
                .tomorrow
                .as_deref()
                .and_then(|d| p.day_mean(d, offset)),
            slots: p.slots.len(),
            next_slots: next,
        };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"deliveryDateCET":"2026-11-25","version":3,"deliveryAreas":["SE4"],
      "multiAreaEntries":[
        {"deliveryStart":"2026-11-24T23:00:00Z","deliveryEnd":"2026-11-24T23:15:00Z","entryPerArea":{"SE4":225.63}},
        {"deliveryStart":"2026-11-24T23:15:00Z","deliveryEnd":"2026-11-24T23:30:00Z","entryPerArea":{"SE4":205.10}},
        {"deliveryStart":"2026-11-24T23:30:00Z","deliveryEnd":"2026-11-24T23:45:00Z","entryPerArea":{"NO1":1.0,"SE4":204.04}}
      ]}"#;

    #[test]
    fn parse_and_filter_area() {
        let (v, slots) = parse_day(SAMPLE, "SE4").unwrap();
        assert_eq!(v, Some(3));
        assert_eq!(slots.len(), 3);
        assert_eq!(slots[0].eur_mwh, 225.63);
        assert!((slots[0].eur_kwh() - 0.22563).abs() < 1e-9);
        // sorted, contiguous 15-min starts
        assert_eq!(slots[1].start - slots[0].start, 900);
    }

    #[test]
    fn dechunk_decodes() {
        let raw = "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(dechunk(raw), "hello world");
        assert_eq!(dechunk("plain body"), "plain body");
    }

    #[test]
    fn parse_utc_known() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-11-24T23:00:00Z"), Some(1795561200));
        assert_eq!(parse_utc("bogus"), None);
    }

    #[test]
    fn url_shape() {
        let u = build_url("SE4", "2026-11-26", "EUR");
        assert!(u.contains("deliveryArea=SE4"));
        assert!(u.contains("date=2026-11-26"));
        assert!(u.starts_with("https://dataportal-api.nordpoolgroup.com/api/DayAheadPrices?"));
    }

    #[test]
    fn refresh_uses_fetcher_and_handles_missing_tomorrow() {
        let dir = std::env::temp_dir().join(format!("np-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = refresh(&dir, "SE4", "EUR", 3600, |url| {
            if url.contains("date=2026-11-26") {
                Err("http: 400".into())
            } else {
                Ok(SAMPLE.to_string())
            }
        })
        .unwrap();
        // today derived from the real clock; we just assert structural sanity
        assert_eq!(p.delivery_area, "SE4");
        assert_eq!(p.slots.len(), 3);
        assert!(p.tomorrow.is_none());
        assert!(dir.join("prices-SE4-latest.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn day_mean_buckets_by_local_date() {
        let p = Prices {
            slots: vec![
                Slot { start: 1795561200, eur_mwh: 100.0 },
                Slot { start: 1795562100, eur_mwh: 200.0 },
                Slot { start: 1795561200 + 86400, eur_mwh: 0.0 },
            ],
            ..Default::default()
        };
        let d = timectl::date_string(1795561200, 3600);
        assert!((p.day_mean(&d, 3600).unwrap() - 150.0).abs() < 1e-9);
    }
}
