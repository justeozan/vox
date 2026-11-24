//! Evey licence client.
//!
//! Vox is open source and fully usable without a licence. An Evey key can be entered in
//! Settings; it is exchanged for an Ed25519-signed token issued by
//! `https://evey.run/api/license` for this app and this device (valid 14 days). What a
//! valid licence unlocks is decided later — nothing is gated on it today.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const APP: &str = "vox";
const ENDPOINT: &str = "https://evey.run/api/license";
const PUBLIC_KEY_B64: &str = "XGHjkcgCJpOG4qOxuqp1OZ758ZYjf7/LJ1xciviWcCk=";
/// How often a running app re-validates its key.
pub const REFRESH_EVERY: Duration = Duration::from_secs(12 * 3600);

#[derive(Debug, Deserialize)]
pub struct Claims {
    pub v: u32,
    pub sub: String,
    pub app: String,
    pub device: String,
    pub iat: u64,
    pub exp: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    Malformed,
    BadSignature,
    Version,
    WrongApp,
    WrongDevice,
    Expired,
}

/// Verify `payload.signature` for this app and device at time `now` (unix seconds).
pub fn verify(
    licence: &str,
    key: &VerifyingKey,
    app: &str,
    device: &str,
    now: u64,
) -> Result<Claims, VerifyError> {
    let (payload, sig) = licence.split_once('.').ok_or(VerifyError::Malformed)?;
    let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| VerifyError::Malformed)?;
    let sig = Signature::from_slice(&sig).map_err(|_| VerifyError::Malformed)?;
    key.verify(payload.as_bytes(), &sig).map_err(|_| VerifyError::BadSignature)?;
    let json = URL_SAFE_NO_PAD.decode(payload).map_err(|_| VerifyError::Malformed)?;
    let claims: Claims = serde_json::from_slice(&json).map_err(|_| VerifyError::Malformed)?;
    if claims.v != 1 {
        return Err(VerifyError::Version);
    }
    if claims.app != app {
        return Err(VerifyError::WrongApp);
    }
    if claims.device != device {
        return Err(VerifyError::WrongDevice);
    }
    if now >= claims.exp {
        return Err(VerifyError::Expired);
    }
    Ok(claims)
}

fn embedded_key() -> Option<VerifyingKey> {
    let raw = STANDARD.decode(PUBLIC_KEY_B64).ok()?;
    VerifyingKey::from_bytes(&raw.try_into().ok()?).ok()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Default, Serialize, Deserialize, Clone)]
struct Stored {
    key: Option<String>,
    device: Option<String>,
    licence: Option<String>,
}

fn store_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/app.vox.bar/licence.json")
}

fn load() -> Stored {
    std::fs::read(store_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(s: &Stored) {
    let path = store_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(s) {
        let _ = std::fs::write(&path, bytes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

/// Random id generated once per install; no hardware fingerprinting.
fn new_device_id() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn device_id(s: &mut Stored) -> String {
    if let Some(d) = &s.device {
        return d.clone();
    }
    let d = new_device_id();
    s.device = Some(d.clone());
    save(s);
    d
}

pub fn valid_key(key: &str) -> bool {
    Regex::new(r"^sub_[A-Za-z0-9]+\.[A-Za-z0-9_-]{32}$").map(|r| r.is_match(key)).unwrap_or(false)
}

/// The stored key, if any (used to build subscriber download links).
pub fn stored_key() -> Option<String> {
    load().key
}

/// Set when the server last said the subscription has ended.
static INACTIVE: Mutex<bool> = Mutex::new(false);

/// Current state for the front-end: `none` (no key), `valid`, `invalid` or `inactive`.
pub fn status() -> Value {
    let s = load();
    let valid = match (&s.licence, &s.device, embedded_key()) {
        (Some(l), Some(d), Some(k)) => verify(l, &k, APP, d, now()).is_ok(),
        _ => false,
    };
    let inactive = *INACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let state = if valid {
        "valid"
    } else if inactive {
        "inactive"
    } else if s.key.is_none() {
        "none"
    } else {
        "invalid"
    };
    json!({ "state": state, "hasKey": s.key.is_some() })
}

/// True when a valid licence is held (for features unlocked later).
pub fn is_valid() -> bool {
    status()["state"] == "valid"
}

/// Forget the stored key and licence (the device id is kept).
pub fn clear() {
    let mut s = load();
    s.key = None;
    s.licence = None;
    save(&s);
    *INACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = false;
}

enum Outcome {
    Granted(String),
    Denied(bool), // true = inactive (403), false = unknown key
    Unreachable,
}

fn request(key: &str, device: &str) -> Outcome {
    let res = ureq::post(ENDPOINT)
        .set("User-Agent", "vox-licence")
        .timeout(Duration::from_secs(10))
        .send_json(json!({ "key": key, "app": APP, "device": device }));
    match res {
        Ok(r) => match r.into_json::<Value>() {
            Ok(v) => match v.get("licence").and_then(|l| l.as_str()) {
                Some(l) => Outcome::Granted(l.to_string()),
                None => Outcome::Unreachable,
            },
            Err(_) => Outcome::Unreachable,
        },
        Err(ureq::Error::Status(403, _)) => Outcome::Denied(true),
        Err(ureq::Error::Status(400 | 404, _)) => Outcome::Denied(false),
        Err(_) => Outcome::Unreachable, // network, 429, 503: keep what we have
    }
}

/// Ask the server about the stored key and update the stored licence. Blocking.
pub fn refresh() {
    let mut s = load();
    let Some(key) = s.key.clone() else { return };
    let device = device_id(&mut s);
    match request(&key, &device) {
        Outcome::Granted(l) => {
            s.licence = Some(l);
            *INACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = false;
            save(&s);
        }
        Outcome::Denied(inactive) => {
            s.licence = None;
            *INACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = inactive;
            save(&s);
        }
        Outcome::Unreachable => {}
    }
}

/// Store a new key and activate it right away. Blocking.
pub fn activate(key: &str) -> Result<Value, String> {
    let key = key.trim();
    if !valid_key(key) {
        return Err("That does not look like an Evey key.".into());
    }
    let mut s = load();
    s.key = Some(key.to_string());
    s.licence = None;
    save(&s);
    *INACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = false;
    refresh();
    let st = status();
    if st["state"] == "valid" {
        Ok(st)
    } else if st["state"] == "inactive" {
        Err("This subscription has ended.".into())
    } else {
        Err("Could not activate this key. Check it and your connection.".into())
    }
}

/// Subscriber download link when a key is stored, else the given fallback.
pub fn download_url(fallback: &str) -> String {
    match stored_key() {
        Some(k) if valid_key(&k) => format!("https://evey.run/download/{APP}?lk={k}"),
        _ => fallback.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const NOW: u64 = 1_800_000_000;

    fn keypair() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn issue(sk: &SigningKey, app: &str, device: &str, exp: u64) -> String {
        let claims = json!({"v":1,"sub":"sub_x","app":app,"device":device,"iat":NOW,"exp":exp});
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let sig = sk.sign(payload.as_bytes());
        format!("{payload}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    #[test]
    fn valid_licence_verifies() {
        let sk = keypair();
        let l = issue(&sk, "vox", "dev12345", NOW + 100);
        let c = verify(&l, &sk.verifying_key(), "vox", "dev12345", NOW).unwrap();
        assert_eq!(c.sub, "sub_x");
    }

    #[test]
    fn bad_signature_is_rejected() {
        let sk = keypair();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let l = issue(&other, "vox", "dev12345", NOW + 100);
        assert_eq!(
            verify(&l, &sk.verifying_key(), "vox", "dev12345", NOW).unwrap_err(),
            VerifyError::BadSignature
        );
    }

    #[test]
    fn wrong_app_or_device_is_rejected() {
        let sk = keypair();
        let l = issue(&sk, "orb", "dev12345", NOW + 100);
        assert_eq!(
            verify(&l, &sk.verifying_key(), "vox", "dev12345", NOW).unwrap_err(),
            VerifyError::WrongApp
        );
        let l = issue(&sk, "vox", "other000", NOW + 100);
        assert_eq!(
            verify(&l, &sk.verifying_key(), "vox", "dev12345", NOW).unwrap_err(),
            VerifyError::WrongDevice
        );
    }

    #[test]
    fn expired_licence_is_rejected() {
        let sk = keypair();
        let l = issue(&sk, "vox", "dev12345", NOW);
        assert_eq!(
            verify(&l, &sk.verifying_key(), "vox", "dev12345", NOW).unwrap_err(),
            VerifyError::Expired
        );
    }

    #[test]
    fn garbage_is_malformed() {
        let sk = keypair();
        assert_eq!(
            verify("nope", &sk.verifying_key(), "vox", "d", NOW).unwrap_err(),
            VerifyError::Malformed
        );
    }

    #[test]
    fn embedded_key_decodes_and_key_format_is_checked() {
        assert!(embedded_key().is_some());
        assert!(valid_key(&format!("sub_abc123.{}", "a".repeat(32))));
        assert!(!valid_key("sub_abc.short"));
    }
}
