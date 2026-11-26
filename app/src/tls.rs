// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// TLS setup (rustls + ring). Server side: load PEM cert/key from config,
// or generate and persist a self-signed pair on first start. Client side
// (mqtts): webpki roots, or an explicit insecure mode for LAN brokers.

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

/// Install the ring crypto provider once at process start.
pub fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn load_pem_cert(path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut rd = BufReader::new(f);
    let certs: Vec<_> = rustls_pemfile::certs(&mut rd)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("parse {path}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("no certificates in {path}"));
    }
    Ok(certs)
}

fn load_pem_key(path: &str) -> Result<PrivateKeyDer<'static>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut rd = BufReader::new(f);
    rustls_pemfile::private_key(&mut rd)
        .map_err(|e| format!("parse {path}: {e}"))?
        .ok_or_else(|| format!("no private key in {path}"))
}

/// Server TLS config. If cert/key paths are empty, a self-signed pair is
/// generated once and persisted under `state_dir` (web-cert.pem/web-key.pem)
/// so the fingerprint is stable across restarts.
pub fn server_config(
    cert_path: &str,
    key_path: &str,
    state_dir: &Path,
    names: Vec<String>,
) -> Result<Arc<ServerConfig>, String> {
    let (certs, key) = if !cert_path.is_empty() && !key_path.is_empty() {
        (load_pem_cert(cert_path)?, load_pem_key(key_path)?)
    } else {
        let cert_p = state_dir.join("web-cert.pem");
        let key_p = state_dir.join("web-key.pem");
        if cert_p.exists() && key_p.exists() {
            (
                load_pem_cert(cert_p.to_str().unwrap())?,
                load_pem_key(key_p.to_str().unwrap())?,
            )
        } else {
            let names = if names.is_empty() {
                vec!["emaldo-driver.local".to_string()]
            } else {
                names
            };
            let rcgen::CertifiedKey { cert, key_pair } =
                rcgen::generate_simple_self_signed(names).map_err(|e| e.to_string())?;
            std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
            std::fs::write(&cert_p, cert.pem()).map_err(|e| e.to_string())?;
            std::fs::write(&key_p, key_pair.serialize_pem()).map_err(|e| e.to_string())?;
            let _ = std::fs::set_permissions(
                &key_p,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
            (
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(key_pair.serialize_der().into()),
            )
        }
    };
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("tls server config: {e}"))?;
    Ok(Arc::new(cfg))
}

/// Accept-any-certificate verifier for mqtts to LAN brokers
/// (config: mqtt.tls_insecure = true).
#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Client TLS config for mqtts:// brokers.
pub fn client_config(insecure: bool) -> Arc<ClientConfig> {
    if insecure {
        let cfg = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        Arc::new(cfg)
    } else {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Arc::new(cfg)
    }
}

pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|e| format!("bad server name {host}: {e}"))
}
