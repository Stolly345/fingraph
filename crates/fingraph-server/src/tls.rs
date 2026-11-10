//! The server's own certificate, made on first start. Apps don't check it against a public authority;
//! they check its fingerprint, which they learn when pairing (from the QR code, or confirmed by the
//! pairing code), so no certificate warnings and no domain name are needed.
use std::path::{Path, PathBuf};

pub struct Identity {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// SHA-256 of the certificate, lowercase hex.
    pub fingerprint: String,
}

pub fn load_or_create(dir: &Path, address: Option<&str>) -> Result<Identity, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    if !cert.exists() || !key.exists() {
        let mut names = vec!["localhost".to_string(), "fingraph.local".to_string()];
        if let Some(host) = address.map(host_of).filter(|h| !h.is_empty()) {
            names.push(host);
        }
        let ck = rcgen::generate_simple_self_signed(names).map_err(|e| e.to_string())?;
        std::fs::write(&key, ck.key_pair.serialize_pem()).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::write(&cert, ck.cert.pem()).map_err(|e| e.to_string())?;
    }
    let pem = std::fs::read_to_string(&cert).map_err(|e| e.to_string())?;
    let fingerprint = fingerprint_of_pem(&pem).ok_or("the server's certificate couldn't be read")?;
    Ok(Identity { cert, key, fingerprint })
}

/// `host:port` or `https://host:port/` → `host`
pub fn host_of(address: &str) -> String {
    let a = address.trim().trim_start_matches("https://").trim_start_matches("http://");
    let a = a.split('/').next().unwrap_or("");
    if let Some(rest) = a.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("").to_string();
    }
    a.rsplit_once(':').map(|(h, _)| h).unwrap_or(a).to_string()
}

fn fingerprint_of_pem(pem: &str) -> Option<String> {
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = base64_decode(&b64)?;
    Some(crate::state::sha256_hex(&der))
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The first 8 bytes as `AB12 CD34 EF56 7890`, for a person to compare on two screens.
pub fn short_fingerprint(fp: &str) -> String {
    fp.chars().take(16).collect::<Vec<_>>().chunks(4).map(|c| c.iter().collect::<String>().to_uppercase()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_a_certificate_once_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create(dir.path(), Some("192.168.1.20:8743")).unwrap();
        let b = load_or_create(dir.path(), None).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint.len(), 64);
        assert_eq!(short_fingerprint(&a.fingerprint).len(), 19);
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("192.168.1.20:8743"), "192.168.1.20");
        assert_eq!(host_of("https://server.lan:8743/"), "server.lan");
        assert_eq!(host_of("[fd7a::1]:8743"), "fd7a::1");
        assert_eq!(host_of("nas"), "nas");
    }

    #[test]
    fn base64() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
    }
}
