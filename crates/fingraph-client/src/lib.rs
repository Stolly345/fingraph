//! Talks to a FinGraph server. The server makes its own certificate, so instead of a certificate
//! authority this client trusts exactly one certificate: the one whose SHA-256 fingerprint it learned
//! when pairing. A small HTTP/1.1 client over rustls, blocking (the desktop calls it off the UI thread).
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 8743;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The server couldn't be reached (off, wrong address, no network).
    Unreachable(String),
    /// Something answered, but with a different certificate than the paired server's.
    WrongServer,
    /// The server doesn't know this device (never paired, or unpaired).
    NotPaired(String),
    /// Someone else holds the edit lease, or the server's ledger moved on.
    Conflict(String),
    /// Anything else the server said no to, in its own words.
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unreachable(e) => write!(f, "The server can't be reached ({e})."),
            Error::WrongServer => write!(f, "Something answered at that address, but it isn't the server this computer was paired with."),
            Error::NotPaired(e) | Error::Conflict(e) | Error::Refused(e) => f.write_str(e),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// `ABCD-EFGH` → `ABCDEFGH`
pub fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// The proof the server checks when pairing: the code bound to the certificate this client sees.
pub fn pairing_proof(code: &str, fingerprint_hex: &str) -> String {
    sha256_hex(format!("fingraph-pair:{}:{}", normalize_code(code), fingerprint_hex.to_ascii_lowercase()).as_bytes())
}

/// The first 8 bytes as `AB12 CD34 EF56 7890`, as the server prints it.
pub fn short_fingerprint(fp: &str) -> String {
    fp.chars().take(16).collect::<Vec<_>>().chunks(4).map(|c| c.iter().collect::<String>().to_uppercase()).collect::<Vec<_>>().join(" ")
}

/// `server`, `server:8743`, `https://server:8743/` → `server:8743`
pub fn normalize_address(a: &str) -> String {
    let a = a.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
    let a = a.split('/').next().unwrap_or("");
    let has_port = if a.starts_with('[') { a.contains("]:") } else { a.matches(':').count() == 1 };
    if has_port || a.is_empty() { a.to_string() } else { format!("{a}:{DEFAULT_PORT}") }
}

/// Accepts exactly the pinned certificate; with no pin, accepts any and remembers it (first contact,
/// before pairing proves it).
#[derive(Debug)]
struct Pin {
    expected: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let fp = sha256_hex(end_entity.as_ref());
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(fp.clone());
        match &self.expected {
            Some(want) if !want.eq_ignore_ascii_case(&fp) => Err(rustls::Error::General("certificate fingerprint mismatch".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A request body.
pub enum Body<'a> {
    None,
    Json(Value),
    File(&'a Path),
}

/// Where a response body goes.
enum Sink<'a> {
    Memory,
    File(&'a Path),
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn error_text(&self) -> String {
        self.json()["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("The server answered {}.", self.status))
    }
}

/// One HTTPS request. Returns the response and the fingerprint of the certificate the server showed.
#[allow(clippy::too_many_arguments)]
fn request(address: &str, pin: Option<&str>, token: Option<&str>, method: &str, path: &str, body: Body, sink: Sink, timeout: Duration) -> Result<(Response, String)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pin { expected: pin.map(str::to_string), seen: Mutex::new(None), provider: provider.clone() });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Unreachable(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    let address = normalize_address(address);
    let host = address.rsplit_once(':').map(|(h, _)| h.trim_start_matches('[').trim_end_matches(']')).unwrap_or(&address).to_string();
    let sockaddr = address.to_socket_addrs().map_err(|e| Error::Unreachable(e.to_string()))?.next().ok_or_else(|| Error::Unreachable("no such address".into()))?;
    let tcp = TcpStream::connect_timeout(&sockaddr, Duration::from_secs(8)).map_err(|e| Error::Unreachable(e.to_string()))?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    // the name isn't checked (the fingerprint is), but TLS wants one
    let name = ServerName::try_from(host.clone()).unwrap_or_else(|_| ServerName::try_from("fingraph.local".to_string()).expect("a valid name"));
    let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| Error::Unreachable(e.to_string()))?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let tls_err = |e: std::io::Error| {
        if e.to_string().contains("fingerprint mismatch") { Error::WrongServer } else { Error::Unreachable(e.to_string()) }
    };

    let (content_type, len) = match &body {
        Body::None => (None, 0u64),
        Body::Json(v) => (Some("application/json"), serde_json::to_vec(v).map(|b| b.len() as u64).unwrap_or(0)),
        Body::File(p) => (Some("application/octet-stream"), std::fs::metadata(p).map_err(|e| Error::Refused(e.to_string()))?.len()),
    };
    // a big upload waits for the server's go-ahead, so a refusal (someone else is editing) arrives as
    // an answer instead of a connection cut off halfway through the file
    let expect = matches!(body, Body::File(_));
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: fingraph-desktop\r\nContent-Length: {len}\r\n");
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    if let Some(t) = token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if expect {
        head.push_str("Expect: 100-continue\r\n");
    }
    head.push_str("\r\n");
    tls.write_all(head.as_bytes()).map_err(tls_err)?;
    tls.flush().map_err(tls_err)?;
    let seen = || verifier.seen.lock().unwrap_or_else(|p| p.into_inner()).clone().unwrap_or_default();

    let mut r = BufReader::new(tls);
    let mut line = String::new();
    let read_status = |r: &mut BufReader<_>, line: &mut String| -> Result<u16> {
        line.clear();
        r.read_line(line).map_err(tls_err)?;
        line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| Error::Unreachable(format!("not a FinGraph server ({})", line.trim())))
    };
    let read_headers = |r: &mut BufReader<_>, line: &mut String| -> Result<Vec<(String, String)>> {
        let mut headers = Vec::new();
        loop {
            line.clear();
            r.read_line(line).map_err(tls_err)?;
            let l = line.trim_end();
            if l.is_empty() {
                return Ok(headers);
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    };
    let mut status = 0;
    let mut headers = Vec::new();
    if expect {
        status = read_status(&mut r, &mut line)?;
        headers = read_headers(&mut r, &mut line)?;
    }
    if !expect || status == 100 {
        let w = r.get_mut();
        match body {
            Body::None => {}
            Body::Json(v) => w.write_all(&serde_json::to_vec(&v).unwrap_or_default()).map_err(tls_err)?,
            Body::File(p) => {
                let mut f = std::fs::File::open(p).map_err(|e| Error::Refused(e.to_string()))?;
                std::io::copy(&mut f, w).map_err(tls_err)?;
            }
        }
        w.flush().map_err(tls_err)?;
        status = read_status(&mut r, &mut line)?;
        headers = read_headers(&mut r, &mut line)?;
    }
    let seen = seen();
    let get = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
    let chunked = get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    let length: Option<u64> = get("content-length").and_then(|v| v.parse().ok());
    // only successful downloads go to the file; errors are read into memory to show
    let to_file = matches!(sink, Sink::File(_)) && (200..300).contains(&status);
    let mut body = Vec::new();
    let mut out: Box<dyn Write> = match (&sink, to_file) {
        (Sink::File(p), true) => Box::new(std::io::BufWriter::new(std::fs::File::create(p).map_err(|e| Error::Refused(e.to_string()))?)),
        _ => Box::new(&mut body),
    };
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line).map_err(tls_err)?;
            let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            std::io::copy(&mut (&mut r).take(size), &mut out).map_err(tls_err)?;
            line.clear();
            r.read_line(&mut line).map_err(tls_err)?;
        }
    } else if let Some(n) = length {
        let copied = std::io::copy(&mut (&mut r).take(n), &mut out).map_err(tls_err)?;
        if copied != n {
            return Err(Error::Unreachable("the connection was cut off".into()));
        }
    } else {
        std::io::copy(&mut r, &mut out).map_err(tls_err)?;
    }
    out.flush().map_err(|e| Error::Refused(e.to_string()))?;
    drop(out);
    Ok((Response { status, headers, body }, seen))
}

fn check(r: Response) -> Result<Response> {
    match r.status {
        200..=299 => Ok(r),
        401 => Err(Error::NotPaired(r.error_text())),
        409 => Err(Error::Conflict(r.error_text())),
        _ => Err(Error::Refused(r.error_text())),
    }
}

/// What a server says about itself before pairing.
#[derive(Debug, Clone, Deserialize)]
pub struct Info {
    pub product: String,
    pub version: String,
    pub api: u32,
}

/// Looks at a server for the first time: what it is and its certificate's fingerprint, which the person
/// can compare with the one the server printed next to the pairing code.
pub fn probe(address: &str) -> Result<(Info, String)> {
    let (r, fp) = request(address, None, None, "GET", "/api/v1/info", Body::None, Sink::Memory, Duration::from_secs(10))?;
    let r = check(r)?;
    let info: Info = serde_json::from_slice(&r.body).map_err(|_| Error::Unreachable("that isn't a FinGraph server".into()))?;
    if info.product != "fingraph-server" {
        return Err(Error::Unreachable("that isn't a FinGraph server".into()));
    }
    Ok((info, fp))
}

/// A paired server: everything needed to reach it again. Kept in the desktop's settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    pub address: String,
    pub fingerprint: String,
    pub token: String,
    pub device_id: i64,
}

/// Pairs this device with the server at `address` using a code from `fingraph-server pair`. With
/// `fingerprint` (from a QR code) the server must have exactly that certificate; without it, the code
/// itself proves the certificate (it's bound into the proof).
pub fn pair(address: &str, code: &str, fingerprint: Option<&str>, device_name: &str, platform: &str) -> Result<Server> {
    let fp = match fingerprint {
        Some(f) => f.to_ascii_lowercase(),
        None => probe(address)?.1,
    };
    let body = Body::Json(serde_json::json!({ "proof": pairing_proof(code, &fp), "name": device_name, "platform": platform }));
    let (r, _) = request(address, Some(&fp), None, "POST", "/api/v1/pair", body, Sink::Memory, Duration::from_secs(15))?;
    let r = check(r)?;
    let v = r.json();
    let token = v["token"].as_str().ok_or_else(|| Error::Refused("the server didn't send a token".into()))?.to_string();
    Ok(Server { address: normalize_address(address), fingerprint: fp, token, device_id: v["device"]["id"].as_i64().unwrap_or(0) })
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Lease {
    pub device_id: i64,
    pub device: String,
    pub since: String,
    pub renewed: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    pub version: String,
    pub api: u32,
    pub ledger_version: i64,
    pub ledger_changed: Option<String>,
    pub workspaces: usize,
    pub lease: Option<Lease>,
    pub inbox: usize,
    pub needs_attention: Vec<Value>,
    pub last_backup: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub platform: String,
    pub created: String,
    pub last_seen: Option<String>,
}

impl Server {
    fn call(&self, method: &str, path: &str, body: Body, timeout: Duration) -> Result<Response> {
        let (r, _) = request(&self.address, Some(&self.fingerprint), Some(&self.token), method, path, body, Sink::Memory, timeout)?;
        check(r)
    }

    /// The server's state: ledger version, who's editing, inbox, backups.
    pub fn status(&self) -> Result<Status> {
        let r = self.call("GET", "/api/v1/me", Body::None, Duration::from_secs(15))?;
        serde_json::from_value(r.json()["server"].clone()).map_err(|e| Error::Refused(format!("unexpected answer: {e}")))
    }

    /// Takes (or renews) the edit lease. `Err(Conflict)` names the device that has it.
    pub fn take_lease(&self) -> Result<(Lease, i64)> {
        let r = self.call("POST", "/api/v1/lease", Body::None, Duration::from_secs(15))?;
        let v = r.json();
        let lease = serde_json::from_value(v["lease"].clone()).map_err(|e| Error::Refused(e.to_string()))?;
        Ok((lease, v["ledger_version"].as_i64().unwrap_or(0)))
    }

    pub fn release_lease(&self) -> Result<()> {
        self.call("DELETE", "/api/v1/lease", Body::None, Duration::from_secs(15)).map(|_| ())
    }

    /// Fetches the server's ledger as a complete backup into `dest`. Returns its version.
    pub fn download_ledger(&self, dest: &Path) -> Result<i64> {
        let (r, _) = request(&self.address, Some(&self.fingerprint), Some(&self.token), "GET", "/api/v1/ledger", Body::None, Sink::File(dest), Duration::from_secs(600))?;
        let r = check(r)?;
        Ok(r.header("x-ledger-version").and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Sends this device's ledger (a complete backup at `src`), refused if the server's version isn't
    /// `base` any more. Returns the new version.
    pub fn upload_ledger(&self, src: &Path, base: Option<i64>) -> Result<i64> {
        let path = match base {
            Some(b) => format!("/api/v1/ledger?base={b}"),
            None => "/api/v1/ledger".into(),
        };
        let r = self.call("PUT", &path, Body::File(src), Duration::from_secs(600))?;
        Ok(r.json()["ledger_version"].as_i64().unwrap_or(0))
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        let r = self.call("GET", "/api/v1/devices", Body::None, Duration::from_secs(15))?;
        serde_json::from_value(r.json()["devices"].clone()).map_err(|e| Error::Refused(e.to_string()))
    }

    pub fn unpair(&self, device_id: i64) -> Result<()> {
        self.call("DELETE", &format!("/api/v1/devices/{device_id}"), Body::None, Duration::from_secs(15)).map(|_| ())
    }

    /// Sends a statement PDF to the server's inbox.
    pub fn send_statement(&self, pdf: &Path) -> Result<String> {
        let name: String = pdf.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "statement.pdf".into());
        let enc: String = name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
        let r = self.call("POST", &format!("/api/v1/statements?name={enc}"), Body::File(pdf), Duration::from_secs(120))?;
        Ok(r.json()["queued"].as_str().unwrap_or_default().to_string())
    }

    /// One of the screen views (`summary`, `budget`, …) as JSON.
    pub fn view(&self, name: &str, query: &str) -> Result<Value> {
        let q = if query.is_empty() { String::new() } else { format!("?{query}") };
        Ok(self.call("GET", &format!("/api/v1/view/{name}{q}"), Body::None, Duration::from_secs(30))?.json())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(normalize_address("nas"), "nas:8743");
        assert_eq!(normalize_address("https://192.168.1.20:9000/"), "192.168.1.20:9000");
        assert_eq!(normalize_address(" 10.0.0.2 "), "10.0.0.2:8743");
        assert_eq!(normalize_address("[fd7a::1]:8743"), "[fd7a::1]:8743");
    }

    #[test]
    fn proof_matches_the_servers() {
        assert_eq!(pairing_proof("abcd-efgh", "AB"), pairing_proof("ABCDEFGH", "ab"));
        assert_eq!(short_fingerprint(&"ab12".repeat(16)), "AB12 AB12 AB12 AB12");
    }
}
