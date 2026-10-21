//! Device pairing. The signaling server relays the SDP offer and answer,
//! and each SDP names the DTLS certificate fingerprint its side will use,
//! so a server that swapped the SDPs could sit in the middle of the
//! encrypted connection. Pairing closes that hole without trusting the
//! server:
//!
//! - The host keeps one DTLS certificate on disk, and the browser keeps one
//!   in IndexedDB, so each side's fingerprint is stable across sessions.
//! - The first time a device connects, both sides derive a code from the
//!   two fingerprints *as they each saw them*. A relay that swapped either
//!   one makes the codes differ. The user compares them and approves on the
//!   host, which saves the viewer's fingerprint.
//! - Later sessions from a saved fingerprint connect without asking.
//!   Anything else gets no media and no input: the host never applies its
//!   answer.
//!
//! The DTLS handshake itself checks that each peer's certificate matches the
//! fingerprint in its SDP, so a matching fingerprint means the private key
//! really is on the paired device.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use webrtc::peer_connection::certificate::RTCCertificate;

use crate::signaling::BoxError;

/// Where the host keeps its certificate and the paired devices:
/// `$REMOTEBRIDGE_HOME`, or `~/.remotebridge`.
pub fn state_dir() -> Result<PathBuf, BoxError> {
    if let Ok(dir) = std::env::var("REMOTEBRIDGE_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
    Ok(Path::new(&home).join(".remotebridge"))
}

/// Loads the host's DTLS certificate, creating it on first run. The file
/// holds the private key, so it is created readable by the owner only.
pub fn load_or_create_certificate(dir: &Path) -> Result<RTCCertificate, BoxError> {
    let path = dir.join("host-cert.pem");
    if path.exists() {
        return Ok(RTCCertificate::from_pem(&fs::read_to_string(&path)?)?);
    }
    fs::create_dir_all(dir)?;
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let cert = RTCCertificate::from_key_pair(key_pair)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?
        .write_all(cert.serialize_pem().as_bytes())?;
    println!("created host certificate at {}", path.display());
    Ok(cert)
}

/// The certificate's SHA-256 fingerprint in SDP form ("ab:cd:...").
pub fn certificate_fingerprint(cert: &RTCCertificate) -> Result<String, BoxError> {
    cert.get_fingerprints()
        .into_iter()
        .find(|f| f.algorithm == "sha-256")
        .map(|f| f.value.to_ascii_lowercase())
        .ok_or_else(|| "certificate has no sha-256 fingerprint".into())
}

/// The SHA-256 DTLS fingerprint an SDP announces, lowercased. Every
/// `a=fingerprint` line must agree, as webrtc-rs itself requires; only
/// sha-256 is accepted, which is what browsers send.
pub fn fingerprint_from_sdp(sdp: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for line in sdp.lines() {
        let Some(rest) = line.trim().strip_prefix("a=fingerprint:") else {
            continue;
        };
        let (algorithm, value) = rest.split_once(' ')?;
        if !algorithm.eq_ignore_ascii_case("sha-256") {
            return None;
        }
        let value = value.trim().to_ascii_lowercase();
        match &found {
            Some(existing) if *existing != value => return None,
            _ => found = Some(value),
        }
    }
    found
}

/// Crockford base32: no I, L, O or U, so the code can't be misread.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The code both sides show on first pairing, as "XXXX-XXXX-XXXX": 60 bits
/// of SHA-256 over both fingerprints. It is long on purpose. A relay that
/// swaps fingerprints gets to pick its own keys, and against a 6-digit code
/// it could try about a million until the two sides' codes matched; 60 bits
/// puts that out of reach. client/pairing.mjs computes the same thing.
pub fn pairing_code(host_fp: &str, viewer_fp: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"remotebridge pairing v1\n");
    hasher.update(host_fp.to_ascii_lowercase().as_bytes());
    hasher.update(b"\n");
    hasher.update(viewer_fp.to_ascii_lowercase().as_bytes());
    let digest = hasher.finalize();
    let bits = u64::from_be_bytes(digest[..8].try_into().expect("8 bytes")) >> 4;
    let mut code = String::new();
    for i in 0..12 {
        if i > 0 && i % 4 == 0 {
            code.push('-');
        }
        let index = (bits >> (55 - 5 * i)) & 31;
        code.push(CODE_ALPHABET[index as usize] as char);
    }
    code
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct PairedDevice {
    pub fingerprint: String,
    /// Unix seconds.
    pub paired_at: u64,
}

/// The approved viewers, saved as JSON next to the certificate.
pub struct PairedDevices {
    path: PathBuf,
    devices: Vec<PairedDevice>,
}

impl PairedDevices {
    pub fn load(dir: &Path) -> Result<Self, BoxError> {
        let path = dir.join("paired.json");
        let devices = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, devices })
    }

    pub fn contains(&self, fingerprint: &str) -> bool {
        self.devices.iter().any(|d| d.fingerprint == fingerprint)
    }

    pub fn add(&mut self, fingerprint: &str) -> Result<(), BoxError> {
        if self.contains(fingerprint) {
            return Ok(());
        }
        let paired_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.devices.push(PairedDevice {
            fingerprint: fingerprint.to_owned(),
            paired_at,
        });
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.path, serde_json::to_string_pretty(&self.devices)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_FP: &str = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";
    const VIEWER_FP: &str = "01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef";

    // The same vector is in tests/client/pairing.test.mjs, so the host and
    // the page are checked to derive identical codes.
    #[test]
    fn pairing_code_matches_shared_vector() {
        assert_eq!(pairing_code(HOST_FP, VIEWER_FP), "3ES8-F2PT-1MEY");
    }

    #[test]
    fn pairing_code_depends_on_order_and_both_keys() {
        let code = pairing_code(HOST_FP, VIEWER_FP);
        assert_ne!(code, pairing_code(VIEWER_FP, HOST_FP));
        assert_ne!(code, pairing_code(HOST_FP, HOST_FP));
        assert_eq!(code, pairing_code(&HOST_FP.to_lowercase(), VIEWER_FP));
    }

    #[test]
    fn fingerprint_from_sdp_reads_and_checks_lines() {
        let sdp = format!(
            "v=0\r\na=fingerprint:sha-256 {HOST_FP}\r\nm=video 9\r\na=fingerprint:sha-256 {HOST_FP}\r\n"
        );
        assert_eq!(fingerprint_from_sdp(&sdp), Some(HOST_FP.to_lowercase()));

        let conflicting =
            format!("a=fingerprint:sha-256 {HOST_FP}\r\na=fingerprint:sha-256 {VIEWER_FP}\r\n");
        assert_eq!(fingerprint_from_sdp(&conflicting), None);
        assert_eq!(fingerprint_from_sdp("a=fingerprint:sha-1 AB:CD\r\n"), None);
        assert_eq!(fingerprint_from_sdp("v=0\r\n"), None);
    }

    #[test]
    fn certificate_and_paired_devices_persist() {
        let dir = std::env::temp_dir().join(format!("rb-pairing-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let first = load_or_create_certificate(&dir).unwrap();
        let second = load_or_create_certificate(&dir).unwrap();
        assert_eq!(
            certificate_fingerprint(&first).unwrap(),
            certificate_fingerprint(&second).unwrap()
        );

        let mut devices = PairedDevices::load(&dir).unwrap();
        assert!(!devices.contains("ab:cd"));
        devices.add("ab:cd").unwrap();
        devices.add("ab:cd").unwrap();
        let reloaded = PairedDevices::load(&dir).unwrap();
        assert!(reloaded.contains("ab:cd"));
        assert_eq!(reloaded.devices.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }
}
