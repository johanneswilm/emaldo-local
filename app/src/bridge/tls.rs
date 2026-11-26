// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// TLS material for the bridge.
//
// The module connects to https://api.emaldo.com (and the DP host) and the HA
// integration talks to the same front, so the bridge must serve TLS for the
// vendor domain names. It cannot obtain a publicly trusted certificate for
// those, so the bridge creates its own CA and leaf certificate covering:
//
//   * every vendor host the module resolves (api.emaldo.com, dp.emaldo.com,
//     e2e2.emaldo.com, …)
//   * the bridge's LAN address as IP SAN, so HA can be pointed straight at the
//     battery's address
//   * any extra names the operator configures (a takeover domain, mDNS name)
//
// The CA (`ca.pem`) is then installed into the battery's trust store; HA needs
// the same CA, or the operator points `cert_file`/`key_file` at an externally
// issued certificate (for example a Let's Encrypt cert for a takeover domain,
// which is what an installation that already has a domain should use).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::bridge::config::Config;

pub struct Certs {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

/// Best-effort primary LAN address: bind a UDP socket towards a public address
/// (no packet is sent) and read back the source address the kernel picked. Only
/// used to add an IP SAN to the generated certificate.
pub fn primary_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("198.51.100.1:80").ok()?;
    match sock.local_addr().ok() {
        Some(a) if !a.ip().is_loopback() => Some(a.ip()),
        _ => None,
    }
}

fn names_sidecar(state_dir: &Path) -> PathBuf {
    state_dir.join("leaf.names.json")
}

/// Sidecar: the SAN set the cached leaf covers, plus the CA's fixed creation
/// timestamp (so re-signing the CA stays byte-identical and the installed trust
/// anchor never has to be re-imported).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Issued {
    names: Vec<String>,
    ca_created: i64,
}

fn wanted_names(dns: &[String], ips: &[IpAddr]) -> Vec<String> {
    let mut v: Vec<String> = dns
        .iter()
        .filter(|d| d.parse::<IpAddr>().is_err())
        .cloned()
        .collect();
    v.extend(ips.iter().map(|i| i.to_string()));
    v.sort();
    v
}

/// Load the configured certificate, or create/reuse a CA + leaf covering
/// `dns` + `ips`. Generated files live in the state dir with mode 0600.
pub fn ensure(
    cfg: &Config,
    state_dir: &Path,
    dns: &[String],
    ips: &[IpAddr],
) -> Result<Certs, String> {
    if let (Some(cert), Some(key)) = (&cfg.cert_file, &cfg.key_file) {
        let cert_pem = std::fs::read_to_string(cert).map_err(|e| format!("cert_file {cert}: {e}"))?;
        let key_pem = std::fs::read_to_string(key).map_err(|e| format!("key_file {key}: {e}"))?;
        let ca_pem = cfg
            .ca_file
            .as_ref()
            .map(|p| std::fs::read_to_string(p).unwrap_or_default())
            .unwrap_or_default();
        return Ok(Certs {
            ca_pem,
            cert_pem,
            key_pem,
        });
    }

    let ca_key_path = state_dir.join("ca.key");
    let ca_pem_path = state_dir.join("ca.pem");
    let leaf_key_path = state_dir.join("leaf.key");
    let leaf_pem_path = state_dir.join("leaf.pem");
    let want = wanted_names(dns, ips);

    let issued: Issued = std::fs::read_to_string(names_sidecar(state_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if let (Ok(cert_pem), Ok(key_pem), Ok(ca_pem)) = (
        std::fs::read_to_string(&leaf_pem_path),
        std::fs::read_to_string(&leaf_key_path),
        std::fs::read_to_string(&ca_pem_path),
    ) {
        if issued.names == want && issued.ca_created != 0 {
            return Ok(Certs {
                ca_pem,
                cert_pem,
                key_pem,
            });
        }
    }

    let ca_created = if issued.ca_created != 0 {
        issued.ca_created
    } else {
        emaldo_proto::codec::unix_s() as i64
    };
    let ca_stamp = time::OffsetDateTime::from_unix_timestamp(ca_created)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());

    let mut ca_params =
        rcgen::CertificateParams::new(vec!["emaldo-local".to_string()]).map_err(|e| e.to_string())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "emaldo-local CA");
    ca_params.not_before = ca_stamp;
    ca_params.not_after = ca_stamp + time::Duration::days(3650);
    let ca_key = match std::fs::read_to_string(&ca_key_path) {
        Ok(pem) => rcgen::KeyPair::from_pem(&pem).map_err(|e| e.to_string())?,
        Err(_) => rcgen::KeyPair::generate().map_err(|e| e.to_string())?,
    };
    let ca = ca_params.self_signed(&ca_key).map_err(|e| e.to_string())?;

    let mut leaf_params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    for d in &want {
        if let Ok(ip) = d.parse::<IpAddr>() {
            leaf_params
                .subject_alt_names
                .push(rcgen::SanType::IpAddress(ip));
        } else {
            let name = rcgen::Ia5String::try_from(d.clone()).map_err(|e| e.to_string())?;
            leaf_params.subject_alt_names.push(rcgen::SanType::DnsName(name));
        }
    }
    // The leaf is re-issued when the name set changes; its validity window is
    // anchored to the CA creation time so re-issuing stays deterministic.
    leaf_params.not_before = ca_stamp;
    leaf_params.not_after = ca_stamp + time::Duration::days(3650);
    let leaf_key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca, &ca_key)
        .map_err(|e| e.to_string())?;

    let certs = Certs {
        ca_pem: ca.pem(),
        cert_pem: leaf.pem(),
        key_pem: leaf_key.serialize_pem(),
    };
    for (p, c) in [
        (ca_pem_path.clone(), certs.ca_pem.clone()),
        (ca_key_path.clone(), ca_key.serialize_pem()),
        (leaf_pem_path.clone(), certs.cert_pem.clone()),
        (leaf_key_path.clone(), certs.key_pem.clone()),
    ] {
        crate::bridge::config::write_atomic(&p, c.as_bytes()).map_err(|e| e.to_string())?;
    }
    crate::bridge::config::write_atomic(
        &names_sidecar(state_dir),
        serde_json::to_vec(&Issued {
            names: want,
            ca_created,
        })
        .unwrap()
        .as_slice(),
    )
    .map_err(|e| e.to_string())?;
    Ok(certs)
}

/// rustls server config, used by both the vendor-API front and the UI front.
pub fn server_config(certs: &Certs) -> Result<Arc<rustls::ServerConfig>, String> {
    // Pure-Rust crypto provider: the static ARM build has no ring/C dependency.
    rustls_rustcrypto::provider().install_default().ok();
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut certs.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut certs.key_pem.as_bytes())
        .map_err(|e| e.to_string())?
        .ok_or("no private key in leaf.key")?;
    let cfg = rustls::ServerConfig::builder_with_protocol_versions(&[
        &rustls::version::TLS12,
        &rustls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .map_err(|e| e.to_string())?;
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_a_usable_leaf_and_reuses_it() {
        rustls_rustcrypto::provider().install_default().ok();
        let dir = std::env::temp_dir().join(format!("el-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config::default();
        let dns = vec!["api.emaldo.com".to_string(), "dp.emaldo.com".to_string()];
        let ips: Vec<IpAddr> = vec!["203.0.113.10".parse().unwrap()];
        let a = ensure(&cfg, &dir, &dns, &ips).unwrap();
        assert!(a.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(a.ca_pem.contains("BEGIN CERTIFICATE"));
        let b = ensure(&cfg, &dir, &dns, &ips).unwrap();
        assert_eq!(a.cert_pem, b.cert_pem, "unchanged SAN set reuses the leaf");
        let c = ensure(&cfg, &dir, &[dns[0].clone()], &ips).unwrap();
        assert_ne!(a.cert_pem, c.cert_pem, "changed SAN set re-issues");
        // A server config that builds means the leaf chain and key parse; the
        // end-to-end TLS handshake is exercised in `net`'s test.
        let _sc = server_config(&c).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
