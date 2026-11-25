// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Vendor-global protocol constants of the Emaldo platform.
//
// These values are identical for every Emaldo installation: they are baked
// into the vendor Android APK and the module binary, and they are already
// published by the community SDK (`emaldo_re/extract_keys.py` extracts them
// from the APK). They are protocol parameters, not per-installation secrets,
// so they live in source. Per-installation identity (device id, e2e
// end_id/end_secret/chat_secret, reg token, certificates) never appears
// here; it is read from the 0600 identity file on the device.

/// Vendor app id: URL path suffix for the user/app (gm1) API and the
/// `APP_ID` (0xB5) option in MSCT frames.
pub const APP_ID: &str = "CXRqKjx2MzSAkdyucR9NDyPiiQR2vQcQ";

/// RC4/AES key for the REST envelopes (gm1/gm2), from the APK.
pub const APP_SECRET: &[u8] = b"FpF4Uqiio9k8p9VUSX36UZxy9wLs7ybT";

/// Product constants of the Power Core 2.0 line (overridable per device in
/// the on-device identity file).
pub const DEFAULT_MODEL: &str = "PC1-BAK15-HS10";
pub const DEFAULT_NAME: &str = "Power Core 2.0";

/// e2e relay ports the module is handed in the login response.
pub const E2E_PORT: u16 = 1050;
pub const E2E_UTIL_PORT: u16 = 1051;

/// Vendor relay host used only as a documented default; the host the module
/// actually dials always comes from the login response.
pub const VENDOR_E2E_HOST: &str = "e2e2.emaldo.com";

/// Vendor domains the module resolves. In local mode these are pinned to the
/// bridge through /etc/hosts (no DNS or router change).
pub const VENDOR_API_HOSTS: &[&str] = &[
    "api.emaldo.com",
    "dp.emaldo.com",
    "e2e2.emaldo.com",
    "files.digi.iot.emaldo.com",
];

/// Vendor cloud origins, used only when the optional vendor leg is enabled.
pub const VENDOR_API_HTTPS: &str = "https://api.emaldo.com";
pub const VENDOR_DP_HTTPS: &str = "https://dp.emaldo.com";

/// App version the bridge reports as current when the client does not say
/// which version it is (the community SDK ships the same constant).
pub const APP_VERSION_DEFAULT: &str = "2.35.3";

/// Timestamp unit the vendor APIs use: unix milliseconds expressed in
/// nanosecond-ish scale (`unix_ms * 1_000_000`).
pub fn gmtime() -> u64 {
    crate::codec::unix_ms() * 1_000_000
}
