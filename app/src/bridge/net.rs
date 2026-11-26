// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Johannes Wilm
//
// Minimal HTTP/1.1 over TLS: one thread per connection, keep-alive, bounded
// bodies. The module client (Go/net/http) and the HA client (python-requests)
// both reuse connections, so keep-alive is not optional. No framework: the
// battery has 236 MiB of RAM and this daemon shares it with the vendor app.

use std::io::{BufRead, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use rustls::{pki_types::ServerName, ServerConfig, StreamOwned};

pub struct Req {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: SocketAddr,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn form(&self) -> Vec<(String, String)> {
        emaldo_proto::codec::form_decode(&self.body)
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(body: impl Into<Vec<u8>>) -> Resp {
        Resp {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        }
    }
    pub fn bytes(content_type: &str, body: impl Into<Vec<u8>>) -> Resp {
        Resp {
            status: 200,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
        }
    }
    pub fn text(status: u16, msg: &str) -> Resp {
        Resp {
            status,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: msg.as_bytes().to_vec(),
        }
    }
    /// Empty 204: what a client expects from a connectivity probe when the
    /// network is open (no captive portal).
    pub fn no_content() -> Resp {
        Resp {
            status: 204,
            headers: vec![],
            body: vec![],
        }
    }
    pub fn status(mut self, code: u16) -> Resp {
        self.status = code;
        self
    }
    pub fn header(mut self, k: &str, v: &str) -> Resp {
        self.headers.push((k.into(), v.into()));
        self
    }
}

pub trait Handler: Send + Sync + 'static {
    fn handle(&self, req: &Req) -> Resp;
}

const MAX_BODY: usize = 2 << 20;
const READ_TIMEOUT_SECS: u64 = 30;

/// Serve `bind:port` with TLS; returns the bound port (0 → ephemeral, used by
/// the tests).
pub fn serve_tls(
    bind: &str,
    port: u16,
    cfg: Arc<ServerConfig>,
    handler: Arc<dyn Handler>,
    log: Arc<crate::bridge::log::Log>,
) -> std::io::Result<u16> {
    let listener = TcpListener::bind((bind, port))?;
    let bound = listener.local_addr()?.port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let cfg = cfg.clone();
                    let handler = handler.clone();
                    let log = log.clone();
                    thread::spawn(move || {
                        if let Err(e) = serve_conn(s, cfg, handler) {
                            log.write(crate::bridge::log::Level::Debug, "http", format!("conn: {e}"));
                        }
                    });
                }
                Err(e) => log.write(crate::bridge::log::Level::Warn, "http", format!("accept: {e}")),
            }
        }
    });
    Ok(bound)
}

type Tls = StreamOwned<rustls::ServerConnection, TcpStream>;

fn serve_conn(tcp: TcpStream, cfg: Arc<ServerConfig>, handler: Arc<dyn Handler>) -> std::io::Result<()> {
    let peer = tcp.peer_addr()?;
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(READ_TIMEOUT_SECS)))?;
    tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
    let conn = rustls::ServerConnection::new(Arc::new((*cfg).clone()))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let mut tls: Tls = StreamOwned::new(conn, tcp);
    // Force the handshake so a non-TLS probe fails fast instead of looking like
    // an empty request.
    let _ = tls.write(&[]);
    // One reader for the whole connection: splitting header parsing from body
    // reading would let the buffered reader swallow body bytes.
    let mut rd = std::io::BufReader::new(tls);

    loop {
        let Some(req) = read_request(&mut rd, peer)? else {
            return Ok(());
        };
        let keep = req
            .header("connection")
            .map(|v| !v.eq_ignore_ascii_case("close"))
            .unwrap_or(true);
        let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.handle(&req)));
        let resp = match resp {
            Ok(r) => r,
            Err(_) => Resp::text(500, "internal error"),
        };
        let mut io = rd.get_mut();
        write_response(&mut io, &resp)?;
        if !keep {
            return Ok(());
        }
    }
}

fn read_request(rd: &mut std::io::BufReader<Tls>, peer: SocketAddr) -> std::io::Result<Option<Req>> {
    let mut lines: Vec<String> = Vec::new();
    loop {
        let mut line = String::new();
        let n = rd.read_line(&mut line)?;
        if n == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
        if lines.len() > 100 {
            return Ok(None);
        }
    }
    if lines.is_empty() {
        return Ok(None);
    }
    let mut it = lines[0].split_whitespace();
    let method = it.next().unwrap_or("GET").to_string();
    let target = it.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    for l in &lines[1..] {
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (
            p.to_string(),
            emaldo_proto::codec::form_decode(q.as_bytes()),
        ),
        None => (target.clone(), Vec::new()),
    };

    let clen: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if clen > MAX_BODY {
        return Ok(None);
    }
    let mut body = vec![0u8; clen];
    if clen > 0 {
        rd.read_exact(&mut body)?;
    }
    Ok(Some(Req {
        method,
        path,
        query,
        headers,
        body,
        peer,
    }))
}

fn write_response(tls: &mut Tls, resp: &Resp) -> std::io::Result<()> {
    debug_assert!(!resp.body.is_empty() || resp.status == 204);
    let status_text = match resp.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    // A 204 carries no body and must not claim a length: the connectivity
    // probes the vendor app sends are answered that way.
    let mut head = if resp.status == 204 {
        format!("HTTP/1.1 {} {}\r\n", resp.status, status_text)
    } else {
        format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
            resp.status,
            status_text,
            resp.body.len()
        )
    };
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: keep-alive\r\n\r\n");
    tls.write_all(head.as_bytes())?;
    tls.write_all(&resp.body)?;
    tls.flush()?;
    Ok(())
}

/// Resolve a host for the optional vendor leg.
///
/// The on-device binary is statically linked, and a statically linked glibc
/// `getaddrinfo` needs the shared libraries of the glibc it was linked against
/// at runtime — which the battery does not have. So an in-process lookup that
/// fails falls back to the system resolver (`getent`), which is always correct
/// for the running system.
fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    if let Ok(mut addrs) = std::net::ToSocketAddrs::to_socket_addrs(&(host, port)) {
        if let Some(a) = addrs.next() {
            return Ok(a);
        }
    }
    let out = std::process::Command::new("getent")
        .args(["ahostsv4", host])
        .output()
        .map_err(|e| {
            format!("dns {host}: in-process lookup failed and no system resolver is available: {e}")
        })?;
    if !out.status.success() {
        return Err(format!("dns {host}: the system resolver has no answer"));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let ip = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .ok_or(format!("dns {host}: unparsible resolver output"))?;
    Ok(SocketAddr::new(ip, port))
}

/// Plain-HTTPS POST client (used by the optional vendor leg). TLS verification
/// uses the platform roots compiled into rustls' webpki provider, which is what
/// a vendor call needs.
pub fn https_post(url: &str, body: &[u8], content_type: &str) -> Result<Vec<u8>, String> {
    let (host, port, path) = split_url(url)?;
    let ip_hack = host.parse::<std::net::IpAddr>().ok();
    if ip_hack.is_some() {
        return Err(format!("vendor URL {url} has an IP host; TLS name verification impossible"));
    }
    let addr = resolve(&host, port)?;
    let name: ServerName<'_> =
        host.as_str()
            .try_into()
            .map_err(|e: rustls::pki_types::InvalidDnsNameError| e.to_string())?;
    let mut roots = rustls::RootCertStore::empty();
    // Load the platform trust store (hashed PEM links under /etc/ssl/certs on
    // Debian); a vendor call must verify against real roots.
    let mut loaded = 0usize;
    for dir in ["/etc/ssl/certs", "/etc/pki/tls/certs"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("pem") {
                    continue;
                }
                if let Ok(pem) = std::fs::read(&p) {
                    for c in rustls_pemfile::certs(&mut pem.as_slice()).flatten() {
                        if roots.add(c).is_ok() {
                            loaded += 1;
                        }
                    }
                }
            }
        }
    }
    if loaded == 0 {
        return Err("no TLS root certificates found in /etc/ssl/certs".into());
    }
    rustls_rustcrypto::provider().install_default().ok();
    let cfg = rustls::ClientConfig::builder_with_protocol_versions(&[
        &rustls::version::TLS12,
        &rustls::version::TLS13,
    ])
    .with_root_certificates(roots)
    .with_no_client_auth();
    let conn = rustls::ClientConnection::new(Arc::new(cfg), name.to_owned())
        .map_err(|e: rustls::Error| e.to_string())?;
    let tcp = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(15))
        .map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    tcp.set_write_timeout(Some(std::time::Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    let mut tls: StreamOwned<rustls::ClientConnection, TcpStream> = StreamOwned::new(conn, tcp);

    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n\r\n",
        body.len()
    );
    tls.write_all(req.as_bytes()).map_err(|e: std::io::Error| e.to_string())?;
    tls.write_all(body).map_err(|e: std::io::Error| e.to_string())?;
    tls.flush().map_err(|e: std::io::Error| e.to_string())?;

    let mut all = Vec::new();
    tls.read_to_end(&mut all)
        .map_err(|e: std::io::Error| e.to_string())?;
    let split = find_headers_end(&all).ok_or("malformed HTTP response")?;
    Ok(all[split..].to_vec())
}

fn find_headers_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("only https:// supported: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "bad port")?),
        None => (authority.to_string(), 443),
    };
    Ok((host, port, path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct H;
    impl Handler for H {
        fn handle(&self, req: &Req) -> Resp {
            match req.path.as_str() {
                "/echo" => Resp::json(req.body.clone()),
                "/gone" => Resp::no_content(),
                _ => Resp::text(404, "no"),
            }
        }
    }

    fn test_certs() -> crate::bridge::tls::Certs {
        let dir = std::env::temp_dir().join(format!("el-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = crate::bridge::config::Config::default();
        crate::bridge::tls::ensure(
            &cfg,
            &dir,
            &["localhost".to_string()],
            &["127.0.0.1".parse().unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn serves_http_over_tls_with_keepalive() {
        rustls_rustcrypto::provider().install_default().ok();
        let certs = test_certs();
        let sc = crate::bridge::tls::server_config(&certs).unwrap();
        let log = Arc::new(crate::bridge::log::Log::new(
            "/tmp",
            "httptest",
            crate::bridge::log::Level::Error,
            64 * 1024,
            2,
        ));
        let port = serve_tls("127.0.0.1", 0, sc, Arc::new(H), log).unwrap();

        // Trust only the generated CA, TLS 1.2 like the module.
        let mut store = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut certs.ca_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        {
            store.add(c).unwrap();
        }
        rustls_rustcrypto::provider().install_default().ok();
    let cfg = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
            .with_root_certificates(store)
            .with_no_client_auth();
        let conn = rustls::ClientConnection::new(
            Arc::new(cfg),
            "localhost".try_into().unwrap(),
        )
        .unwrap();
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut tls: StreamOwned<rustls::ClientConnection, TcpStream> =
            StreamOwned::new(conn, tcp);

        for i in 0..2 {
            let body = format!("gm=2&iv=abcd&json=x{i}");
            tls.write_all(format!(
                "POST /echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ).as_bytes())
                .unwrap();
            tls.flush().unwrap();
            let mut resp = Vec::new();
            loop {
                let head = find_headers_end(&resp);
                let need = head.and_then(|h| {
                    let text = String::from_utf8_lossy(&resp[..h]).into_owned();
                    let len = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().parse::<usize>().ok()))
                        .flatten()?;
                    Some(h + len)
                });
                if let Some(n) = need {
                    if resp.len() >= n {
                        break;
                    }
                }
                let mut b = [0u8; 512];
                let n = tls.read(&mut b).unwrap();
                if n == 0 {
                    break;
                }
                resp.extend_from_slice(&b[..n]);
            }
            let s = String::from_utf8_lossy(&resp).into_owned();
            assert!(s.contains("200 OK"), "{s}");
            assert!(s.contains(&format!("x{i}")), "{s}");
        }

        // A probe answer: 204, no Content-Length, no body, connection kept.
        tls.write_all(b"GET /gone HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        tls.flush().unwrap();
        let mut probe = Vec::new();
        while !probe.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut b = [0u8; 128];
            let n = tls.read(&mut b).unwrap();
            assert!(n > 0, "probe response truncated");
            probe.extend_from_slice(&b[..n]);
        }
        let head = String::from_utf8_lossy(&probe).into_owned();
        assert!(head.contains("HTTP/1.1 204"), "{head}");
        assert!(
            !head.to_ascii_lowercase().contains("content-length"),
            "204 must not announce a body: {head}"
        );
    }
}
