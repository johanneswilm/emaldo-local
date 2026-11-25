// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Own-model PV forecast from Open-Meteo hourly irradiance (keyless).
//
//   GET https://api.open-meteo.com/v1/forecast?latitude=..&longitude=..
//       &hourly=shortwave_radiation,direct_normal_irradiance,
//               terrestrial_radiation,cloud_cover,temperature_2m
//       &forecast_days=3&timezone=UTC
//
// Model: sun position (compact NOAA approximation) -> plane-of-array
// irradiance per configured string (tilt + azimuth) -> AC power
//   P = kWp * (G_poa / 1000) * PR * temp_derate
// summed over strings, resampled to 96 x 15-min kW (noon-UTC day).
//
// This is deliberately simple and deterministic; the optimizer's
// actual-vs-forecast scale corrects residual bias over time.

use crate::config::{SolarConfig, SolarString};
use crate::timectl;
use serde::{Deserialize, Serialize};

pub const HOST: &str = "api.open-meteo.com";
pub const PATH: &str = "/v1/forecast";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Hourly {
    pub time: Vec<i64>,
    pub ghi: Vec<f64>,          // shortwave_radiation W/m²
    pub dni: Vec<f64>,          // direct_normal_irradiance W/m²
    pub clear_sky: Vec<f64>,    // terrestrial_radiation W/m²
    pub cloud: Vec<f64>,        // %
    pub temp_c: Vec<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Forecast {
    pub latitude: f64,
    pub longitude: f64,
    pub kwp: f64,
    pub fetched_at: i64,
    /// Hourly AC power estimates (W), aligned with `hours`.
    pub hours: Vec<i64>,
    pub power_w: Vec<f64>,
    pub peak_w: f64,
    pub day_kwh: f64,
}

pub fn build_url(cfg: &SolarConfig) -> String {
    format!(
        "https://{HOST}{PATH}?latitude={:.5}&longitude={:.5}\
         &hourly=shortwave_radiation,direct_normal_irradiance,terrestrial_radiation,cloud_cover,temperature_2m\
         &forecast_days=3&timezone=UTC",
        cfg.latitude, cfg.longitude
    )
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    hourly: RawHourly,
}

#[derive(Deserialize, Default)]
struct RawHourly {
    #[serde(default)]
    time: Vec<String>,
    #[serde(default)]
    shortwave_radiation: Vec<Option<f64>>,
    #[serde(default)]
    direct_normal_irradiance: Vec<Option<f64>>,
    #[serde(default)]
    terrestrial_radiation: Vec<Option<f64>>,
    #[serde(default)]
    cloud_cover: Vec<Option<f64>>,
    #[serde(default)]
    temperature_2m: Vec<Option<f64>>,
}

fn f(v: &[Option<f64>], i: usize) -> f64 {
    v.get(i).copied().flatten().unwrap_or(0.0)
}

pub fn parse(body: &str) -> Result<Hourly, String> {
    let raw: Raw = serde_json::from_str(body).map_err(|e| format!("json: {e}"))?;
    let h = raw.hourly;
    let mut out = Hourly::default();
    for (i, t) in h.time.iter().enumerate() {
        // time like "2026-11-25T00:00" (UTC, no Z)
        let ts = parse_hour(t).ok_or_else(|| format!("bad time {t:?}"))?;
        out.time.push(ts);
        out.ghi.push(f(&h.shortwave_radiation, i));
        out.dni.push(f(&h.direct_normal_irradiance, i));
        out.clear_sky.push(f(&h.terrestrial_radiation, i));
        out.cloud.push(f(&h.cloud_cover, i));
        out.temp_c.push(f(&h.temperature_2m, i));
    }
    if out.time.is_empty() {
        return Err("no hourly data".into());
    }
    Ok(out)
}

fn parse_hour(s: &str) -> Option<i64> {
    // "YYYY-MM-DDTHH:MM"
    let b = s.as_bytes();
    if b.len() < 13 {
        return None;
    }
    let n = |a: usize, l: usize| s.get(a..a + l)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 2)?, n(8, 2)?);
    let (h, mi) = (n(11, 2)?, if b.len() >= 16 { n(14, 2)? } else { 0 });
    Some(timectl::ymd_to_unix_days(y, mo, d) * 86400 + h * 3600 + mi * 60)
}

/// Solar zenith (deg) and azimuth (deg, 0=N, 90=E) for a UTC instant and
/// position. NOAA compact algorithm; accurate to ~0.5° for our use.
pub fn sun_position(ts: i64, lat: f64, lon: f64) -> (f64, f64) {
    let jd = (ts as f64) / 86400.0 + 2440587.5;
    let jc = (jd - 2451545.0) / 36525.0;
    let mut l0 = 280.46646 + jc * (36000.76983 + jc * 0.0003032);
    l0 = l0.rem_euclid(360.0);
    let m = 357.52911 + jc * (35999.05029 - 0.0001537 * jc);
    let e = 0.016708634 - jc * (0.000042037 + 0.0000001267 * jc);
    let mrad = m.to_radians();
    let c = mrad.sin() * (1.914602 - jc * (0.004817 + 0.000014 * jc))
        + (2.0 * mrad).sin() * (0.019993 - 0.000101 * jc)
        + (3.0 * mrad).sin() * 0.000289;
    let true_long = l0 + c;
    let omega = 125.04 - 1934.136 * jc;
    let app_long = true_long - 0.00569 - 0.00478 * omega.to_radians().sin();
    let mean_obliq = 23.0 + (26.0 + (21.448 - jc * (46.815 + jc * (0.00059 - jc * 0.001813))) / 60.0) / 60.0;
    let obliq = mean_obliq + 0.00256 * omega.to_radians().cos();
    let decl = (obliq.to_radians().sin() * app_long.to_radians().sin()).asin();

    // Equation of time (minutes).
    let y = (obliq / 2.0).to_radians().tan().powi(2);
    let l0r = l0.to_radians();
    let eot = 4.0
        * (y * (2.0 * l0r).sin() - 2.0 * e * mrad.sin()
            + 4.0 * e * y * mrad.sin() * (2.0 * l0r).cos()
            - 0.5 * y * y * (4.0 * l0r).sin()
            - 1.25 * e * e * (2.0 * mrad).sin())
        .to_degrees();

    // UTC minutes-of-day, then true solar time.
    let utc_min = ((ts.rem_euclid(86400)) as f64) / 60.0;
    let true_solar = (utc_min + eot + 4.0 * lon).rem_euclid(1440.0);
    let hour_angle = (true_solar / 4.0 - 180.0).to_radians();

    let latr = lat.to_radians();
    let cos_zen = (latr.sin() * decl.sin() + latr.cos() * decl.cos() * hour_angle.cos()).clamp(-1.0, 1.0);
    let zenith = cos_zen.acos().to_degrees();

    // Azimuth from north, clockwise.
    let az = (hour_angle.sin())
        .atan2(hour_angle.cos() * latr.sin() - decl.tan() * latr.cos())
        .to_degrees()
        + 180.0;
    (zenith, az.rem_euclid(360.0))
}

/// Plane-of-array irradiance (W/m²) for a surface at `tilt`/`azimuth`.
/// Splits DNI across the tilted plane and adds isotropic diffuse + a little
/// ground reflection.
pub fn poa_irradiance(ghi: f64, dni: f64, zenith: f64, sun_az: f64, tilt: f64, az: f64) -> f64 {
    let (zr, sar, tr, ar) = (
        zenith.to_radians(),
        sun_az.to_radians(),
        tilt.to_radians(),
        az.to_radians(),
    );
    let cos_inc = zr.cos() * tr.cos() + zr.sin() * tr.sin() * (sar - ar).cos();
    let cos_inc = cos_inc.max(0.0);
    let dhi = (ghi - dni * zr.cos()).max(0.0); // diffuse horizontal
    let direct = dni * cos_inc;
    let diffuse = dhi * (1.0 + tr.cos()) / 2.0;
    let ground = 0.2 * ghi * (1.0 - tr.cos()) / 2.0; // albedo 0.2
    direct + diffuse + ground
}

const BIFACIAL_GAIN: f64 = 0.06; // modest rear-side bonus for bifacial on a 16° tilt

/// Temperature derate: silicon loses ~0.4 %/°C above 25 °C cell.
fn temp_derate(cell_c: f64) -> f64 {
    (1.0 - 0.004 * (cell_c - 25.0)).clamp(0.7, 1.05)
}

fn string_power_w(s: &SolarString, h: &Hourly, i: usize, lat: f64, lon: f64, pr: f64) -> f64 {
    let (zen, az) = sun_position(h.time[i], lat, lon);
    if zen >= 90.0 {
        return 0.0;
    }
    let poa = poa_irradiance(h.ghi[i], h.dni[i], zen, az, s.tilt_deg, s.azimuth_deg);
    if poa <= 0.0 {
        return 0.0;
    }
    // Cell temp ~ air + irradiance heating (NOCT-like).
    let cell = h.temp_c[i] + (poa / 800.0) * 30.0;
    let gain = if s.bifacial { 1.0 + BIFACIAL_GAIN } else { 1.0 };
    s.kwp() * 1000.0 * (poa / 1000.0) * pr * temp_derate(cell) * gain
}

/// Compute AC power per hour for a whole array.
pub fn compute(cfg: &SolarConfig, h: &Hourly) -> Forecast {
    let kwp: f64 = cfg.strings.iter().map(|s| s.kwp()).sum();
    let mut power = Vec::with_capacity(h.time.len());
    for i in 0..h.time.len() {
        let w: f64 = cfg
            .strings
            .iter()
            .map(|s| string_power_w(s, h, i, cfg.latitude, cfg.longitude, cfg.performance_ratio))
            .sum();
        power.push(w.max(0.0));
    }
    let peak = power.iter().cloned().fold(0.0, f64::max);
    // Energy for today's UTC-day portion present in the data.
    let today = timectl::date_string(timectl::now_unix(), 0);
    let day_kwh: f64 = h
        .time
        .iter()
        .zip(&power)
        .filter(|(t, _)| timectl::date_string(**t, 0) == today)
        .map(|(_, p)| p / 1000.0)
        .sum();
    Forecast {
        latitude: cfg.latitude,
        longitude: cfg.longitude,
        kwp,
        fetched_at: timectl::now_unix(),
        hours: h.time.clone(),
        power_w: power,
        peak_w: peak,
        day_kwh,
    }
}

/// Persist / load the latest forecast next to the price cache.
fn cache_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("solar-forecast.json")
}

pub fn load_cached(dir: &std::path::Path) -> Option<Forecast> {
    let txt = std::fs::read_to_string(cache_path(dir)).ok()?;
    serde_json::from_str(&txt).ok()
}

pub fn store_cached(dir: &std::path::Path, f: &Forecast) {
    if let Ok(txt) = serde_json::to_string_pretty(f) {
        let _ = std::fs::write(cache_path(dir), txt);
    }
}

/// Fetch+compute, caching the result. Read-only outbound HTTPS.
pub fn refresh(dir: &std::path::Path, cfg: &SolarConfig) -> Result<Forecast, String> {
    let rest = build_url(cfg);
    let rest = rest.split_once("://").map(|(_, r)| r).unwrap_or(&rest);
    let (hp, pathq) = rest.split_once('/').ok_or_else(|| "bad url".to_string())?;
    let host = hp.split(':').next().unwrap_or(hp);
    let body = crate::prices::https_get(host, 443, &format!("/{pathq}"))?;
    let hourly = parse(&body)?;
    let f = compute(cfg, &hourly);
    store_cached(dir, &f);
    Ok(f)
}

pub fn start(
    dir: std::path::PathBuf,
    cfg: SolarConfig,
    refresh_secs: u64,
    telem: std::sync::Arc<crate::telemetry::Store>,
) {
    std::thread::Builder::new()
        .name("solar".into())
        .spawn(move || {
            let mut delay = 0u64;
            loop {
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(delay.min(300)));
                }
                match refresh(&dir, &cfg) {
                    Ok(f) => {
                        publish(&telem, &f);
                        telem.add_event(
                            "info",
                            "solar",
                            format!(
                                "{:.2} kWp, today {:.1} kWh, peak {:.0} W",
                                f.kwp, f.day_kwh, f.peak_w
                            ),
                        );
                        delay = refresh_secs.max(1800);
                    }
                    Err(e) => {
                        if let Some(f) = load_cached(&dir) {
                            publish(&telem, &f);
                        }
                        telem.add_event("warn", "solar", format!("forecast: {e} (retry 600s)"));
                        delay = 600;
                    }
                }
            }
        })
        .expect("spawn solar refresher");
}

fn publish(telem: &crate::telemetry::Store, f: &Forecast) {
    let now = timectl::now_unix();
    let next: Vec<crate::telemetry::SolarSlot> = f
        .hours
        .iter()
        .zip(&f.power_w)
        .filter(|(t, _)| **t + 3600 > now && **t <= now + 86400)
        .take(24)
        .map(|(t, p)| crate::telemetry::SolarSlot {
            start: *t as u64,
            local_hhmm: timectl::local_hhmm(*t, 3600),
            power_w: *p,
        })
        .collect();
    let current_w = next.first().map(|s| s.power_w).unwrap_or(0.0);
    telem.update(|st| {
        st.solar = crate::telemetry::SolarState {
            enabled: true,
            latitude: f.latitude,
            longitude: f.longitude,
            kwp: f.kwp,
            updated_at: f.fetched_at as u64,
            current_w,
            peak_w: f.peak_w,
            today_kwh: f.day_kwh,
            next_hours: next,
        };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sun_se_midsummer_noon() {
        // Copenhagen-ish, 2026-06-21 11:00Z: sun high (~55°) and roughly south.
        let ts = 1782043200; // 2026-06-21T12:00:00Z
        let (zen, az) = sun_position(ts, 55.7, 13.2);
        assert!(zen < 60.0, "zenith {zen}");
        assert!(az > 140.0 && az < 220.0, "azimuth {az}");
    }

    #[test]
    fn sun_night_has_zero() {
        let ts = 1795564800 + 3600; // 2026-11-25T01:00Z
        let (zen, _) = sun_position(ts, 55.7, 13.2);
        assert!(zen > 90.0);
    }

    #[test]
    fn poa_east_vs_west_morning() {
        // Morning sun in the SE: east string should beat west.
        let zenith = 70.0;
        let sun_az = 130.0; // SE
        let east = poa_irradiance(300.0, 200.0, zenith, sun_az, 16.0, 90.0);
        let west = poa_irradiance(300.0, 200.0, zenith, sun_az, 16.0, 270.0);
        assert!(east > west, "east {east} west {west}");
    }

    #[test]
    fn parse_open_meteo_shape() {
        let body = r#"{"latitude":55.7,"longitude":13.2,"hourly":{
          "time":["2026-11-25T00:00","2026-11-25T01:00"],
          "shortwave_radiation":[0.0,0.0],
          "direct_normal_irradiance":[0.0,0.0],
          "terrestrial_radiation":[0.0,0.0],
          "cloud_cover":[30,72],
          "temperature_2m":[5.1,4.8]}}"#;
        let h = parse(body).unwrap();
        assert_eq!(h.time.len(), 2);
        assert_eq!(h.temp_c[0], 5.1);
    }

    #[test]
    fn compute_default_array_midday() {
        let cfg = SolarConfig::default();
        // mid June noon-ish, clear sky
        let ts = 1782043200;
        let h = Hourly {
            time: vec![ts, ts + 3600],
            ghi: vec![800.0, 300.0],
            dni: vec![700.0, 200.0],
            clear_sky: vec![900.0, 400.0],
            cloud: vec![0.0, 20.0],
            temp_c: vec![18.0, 17.0],
        };
        let f = compute(&cfg, &h);
        assert!((f.kwp - 4.35).abs() < 1e-9);
        assert!(f.peak_w > 2000.0 && f.peak_w < 5000.0, "peak {}", f.peak_w);
        assert!(f.power_w[1] < f.power_w[0]);
    }
}