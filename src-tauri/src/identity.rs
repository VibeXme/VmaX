//! Persistent device identity and the zero-prompt self-connect policy.
//!
//! Every VmaX install owns one Ed25519 key pair and one UUIDv4 `device_id`,
//! stored in the app local data directory. The public half is published to
//! `public.devices`; the secret never leaves the machine.
//!
//! Joining another machine is decided locally by [`decide_join`] so the QUIC
//! handshake can complete without a human in the loop when the peer belongs to
//! the same account or a friend with auto-approve enabled.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const IDENTITY_FILE: &str = "device-identity-ed25519.json";

/// On-disk identity. `secret_key` is the 32-byte Ed25519 seed in base64 and is
/// only ever written to the app-local data directory.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub public_key: String,
    secret_key: String,
    pub created_at_ms: u64,
}

/// What the UI and `public.devices` see.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySummary {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub public_key: String,
    /// First 8 bytes of SHA-256(public_key), hex. Stable, short, non-secret.
    pub fingerprint: String,
    pub can_host: bool,
}

impl DeviceIdentity {
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let identity: Self =
                    serde_json::from_str(&raw).map_err(|_| "Invalid device identity file")?;
                if hex::encode(identity.signing_key()?.verifying_key().to_bytes())
                    != identity.public_key
                {
                    return Err("Device identity key does not match".into());
                }
                return Ok(identity);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }

        let mut seed = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut seed);
        let signing = SigningKey::from_bytes(&seed);
        let identity = Self {
            device_id: new_uuid_v4(),
            device_name: default_device_name(),
            platform: std::env::consts::OS.to_string(),
            public_key: hex::encode(signing.verifying_key().to_bytes()),
            secret_key: b64().encode(seed),
            created_at_ms: now_ms(),
        };
        match crate::connections::create_json(path, &identity) {
            Ok(()) => Ok(identity),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Self::load_or_create(path),
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn signing_key(&self) -> Result<SigningKey, String> {
        let seed = b64()
            .decode(self.secret_key.as_bytes())
            .map_err(|err| format!("device identity secret is not valid base64: {err}"))?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "device identity secret must be 32 bytes".to_string())?;
        Ok(SigningKey::from_bytes(&seed))
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.public_key)
    }

    pub fn summary(&self, can_host: bool) -> IdentitySummary {
        IdentitySummary {
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            platform: self.platform.clone(),
            public_key: self.public_key.clone(),
            fingerprint: self.fingerprint(),
            can_host,
        }
    }

    /// Sign an arbitrary payload (handshake nonce, invite claims, presence
    /// heartbeat). Returns lowercase hex.
    pub fn sign(&self, payload: &[u8]) -> Result<String, String> {
        let key = self.signing_key()?;
        Ok(hex::encode(key.sign(payload).to_bytes()))
    }
}

/// Verify a peer's signature with the public key recorded in `public.devices`.
/// A malformed key or signature is a failed verification, never a panic.
pub fn verify(public_key_hex: &str, payload: &[u8], signature_hex: &str) -> bool {
    let Ok(key_bytes) = hex::decode(public_key_hex.trim()) else {
        return false;
    };
    let Ok(key_bytes): Result<[u8; 32], _> = key_bytes.try_into() else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    let Ok(sig_bytes): Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    key.verify_strict(payload, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

/// Who the incoming request claims to be, already checked against the signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinDecision {
    /// Same account: start streaming with no prompt at all.
    SelfConnect,
    /// A friend allowed unattended access to this machine.
    AutoApprove,
    /// Show the Accept / Always Allow / Decline overlay.
    Prompt,
}

impl JoinDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfConnect => "self_connect",
            Self::AutoApprove => "auto_approve",
            Self::Prompt => "prompt",
        }
    }

    pub fn starts_streaming(self) -> bool {
        matches!(self, Self::SelfConnect | Self::AutoApprove)
    }
}

/// The whole join policy, in one testable place.
///
/// * Same owner always wins (a phone joining your own desktop, a laptop joining
///   your own PC): zero prompts.
/// * An accepted friendship with `auto_approve_join` skips the prompt.
/// * Everything else prompts, including pending or blocked friendships — those
///   are filtered by the caller before we get here, but a prompt is the safe
///   default.
pub fn decide_join(
    local_owner: Option<&str>,
    incoming_owner: Option<&str>,
    friendship_auto_approve: bool,
) -> JoinDecision {
    if let (Some(local), Some(incoming)) = (local_owner, incoming_owner)
        && !local.is_empty()
        && local == incoming
    {
        return JoinDecision::SelfConnect;
    }
    if friendship_auto_approve {
        return JoinDecision::AutoApprove;
    }
    JoinDecision::Prompt
}

pub fn fingerprint_of(public_key_hex: &str) -> String {
    let digest = Sha256::digest(public_key_hex.trim().to_ascii_lowercase().as_bytes());
    hex::encode(&digest[..8])
}

pub fn identity_path(dir: &Path) -> PathBuf {
    dir.join(IDENTITY_FILE)
}

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "VmaX device".to_string())
}

/// RFC 4122 version 4 UUID, generated from the OS CSPRNG.
pub fn new_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
#[test]
fn persistent_device_verifies_only_the_original_request() {
    let dir = std::env::temp_dir().join(format!("vmax-identity-test-{}", new_uuid_v4()));
    let path = identity_path(&dir);
    let identity = DeviceIdentity::load_or_create(&path).unwrap();
    let proof = b"device-a -> device-b for QUIC fingerprint A";
    let signature = identity.sign(proof).unwrap();
    let loaded = DeviceIdentity::load_or_create(&path).unwrap();
    assert_eq!(loaded.device_id, identity.device_id);
    assert!(verify(&loaded.public_key, proof, &signature));
    assert!(!verify(&loaded.public_key, b"substituted QUIC fingerprint", &signature));
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(dir.join("device-identity-ed25519.json.lock")).unwrap();
    std::fs::remove_dir(dir).unwrap();
}
