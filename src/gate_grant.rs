//! Gate grants — the signed authorization an `allow`/`review` approval must
//! carry since P0b.
//!
//! A grant is an ed25519 signature by the scepter-side issuer key over the
//! canonical grant message:
//!
//! ```text
//! malkuth-gate-grant/v1
//! proposal_id:<32 hex>
//! worker_id:<id>
//! action:<restart|drain>
//! pid:<u64>
//! starttime:<u64>
//! issued_at:<unix sec u64>
//! ```
//!
//! (every line `\n`-terminated, including the last). The signature binds the
//! approval to the exact live process identity (`pid` + `/proc` starttime) of
//! the worker it names, so a grant minted for one worker incarnation cannot
//! vouch for a different process, a respawned worker, or another action.
//!
//! # Key material
//!
//! * The verification (public) key is embedded below. This is PUBLIC key
//!   material — it may live in the repo; only the signing seed is secret and
//!   it never enters any tree (class-A bootstrap root on the issuer side).
//! * `MALKUTH_GATE_VERIFY_KEY` (hex) overrides the embedded key, read once at
//!   server construction. A malformed override is refused loudly — never a
//!   silent fallback to the embedded key.
//!
//! # Digest
//!
//! `grant_digest` is the SHA-256 of the canonical message, hex-encoded. The
//! registry stores it for audit trails; verification itself always re-derives
//! the message and checks the signature, never the digest alone.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// The production issuer's ed25519 PUBLIC key (hex). Public material by
/// ruling 2026-11-08: a verification key may enter the repo, the signing seed
/// may not.
const DEFAULT_GATE_VERIFY_KEY_HEX: &str =
    "31513b3d792003474bc584120306d5f1be28f1638c0e4351b837522f333e6548";

/// Why a grant did not verify. Every variant maps to the caller-facing
/// `approval_unattested` — the distinction exists for logs and tests only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// The signature is not 128 hex characters decoding to 64 bytes.
    MalformedSignature,
    /// The configured verify key is not 64 hex characters decoding to 32
    /// bytes.
    MalformedVerifyKey,
    /// Well-formed but signed by another key, or over another message.
    InvalidSignature,
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MalformedSignature => f.write_str("grant signature is not 128 hex chars"),
            Self::MalformedVerifyKey => f.write_str("gate verify key is not 64 hex chars"),
            Self::InvalidSignature => f.write_str("grant signature check failed"),
        }
    }
}

impl std::error::Error for GrantError {}

/// A verified grant as stored in the approval registry: the worker identity
/// the signature vouches for, plus the wire fields kept for audits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    /// Live `pid` of the named worker at issuance.
    pub pid: u64,
    /// Live `/proc` starttime (jiffies-since-boot) of that pid.
    pub starttime: u64,
    /// Issuer clock at signing (unix seconds). Cross-node skew is bounded by
    /// the registry TTL at registration (`approval_epoch_stale`).
    pub issued_at: u64,
    /// SHA-256 of the canonical message, hex (audit copy).
    pub digest: String,
    /// The ed25519 signature itself, 128 hex chars (audit copy).
    pub sig: String,
}

/// The canonical signed message. Field order and separators are frozen: the
/// scepter-side signer, the Rust verifier, and the devtools reference oracle
/// all build this exact byte string. Any change is a new `/v2` prefix, never
/// an edit of `/v1`.
#[must_use]
pub fn grant_message(
    proposal_id: &str,
    worker_id: &str,
    action: &str,
    pid: u64,
    starttime: u64,
    issued_at: u64,
) -> String {
    format!(
        "malkuth-gate-grant/v1\nproposal_id:{proposal_id}\nworker_id:{worker_id}\naction:{action}\npid:{pid}\nstarttime:{starttime}\nissued_at:{issued_at}\n"
    )
}

/// SHA-256 of the canonical message, hex-encoded.
#[must_use]
pub fn grant_digest(message: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(message.as_bytes());
    encode_hex(&hasher.finalize())
}

/// Verify a grant signature against one explicit hex verify key.
///
/// # Errors
///
/// [`GrantError::MalformedVerifyKey`] / [`GrantError::MalformedSignature`]
/// for bad encodings, [`GrantError::InvalidSignature`] when the signature
/// does not check out. Constant-time comparison is ed25519-dalek's concern;
/// this wrapper only rejects malformed encodings before delegating.
pub fn verify_with_key(
    message: &str,
    sig_hex: &str,
    verify_key_hex: &str,
) -> Result<(), GrantError> {
    let key_bytes = decode_hex(verify_key_hex).ok_or(GrantError::MalformedVerifyKey)?;
    let key_arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| GrantError::MalformedVerifyKey)?;
    let sig_bytes = decode_hex(sig_hex).ok_or(GrantError::MalformedSignature)?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| GrantError::MalformedSignature)?;
    let key = VerifyingKey::from_bytes(&key_arr).map_err(|_| GrantError::MalformedVerifyKey)?;
    let sig = Signature::from_bytes(&sig_arr);
    key.verify(message.as_bytes(), &sig)
        .map_err(|_| GrantError::InvalidSignature)
}

/// Resolve the verify key: `MALKUTH_GATE_VERIFY_KEY` when set (and valid
/// hex), the embedded production key when the variable is absent or blank.
///
/// # Errors
///
/// [`GrantError::MalformedVerifyKey`] when the override is set but unusable
/// (bad hex, wrong length, or non-UTF-8 bytes) — a configured key must never
/// silently fall back to the embedded one.
pub fn resolve_verify_key() -> Result<String, GrantError> {
    match std::env::var("MALKUTH_GATE_VERIFY_KEY") {
        Ok(raw) => {
            let trimmed = raw.trim().to_ascii_lowercase();
            if trimmed.is_empty() {
                return Ok(DEFAULT_GATE_VERIFY_KEY_HEX.to_string());
            }
            if decode_hex(&trimmed).map(|b| b.len() == 32) != Some(true) {
                return Err(GrantError::MalformedVerifyKey);
            }
            Ok(trimmed)
        }
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_GATE_VERIFY_KEY_HEX.to_string()),
        // The variable IS configured but carries non-UTF-8 bytes: refusing is
        // the fail-closed direction — accepting it would mean verifying
        // against a key nobody can name.
        Err(std::env::VarError::NotUnicode(_)) => Err(GrantError::MalformedVerifyKey),
    }
}

// ── hex helpers (no hex crate: keep the dependency surface minimal) ──

#[must_use]
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Decode canonical lowercase hex; rejects any non-hex char, odd lengths,
/// and uppercase (grant messages and signatures are minted by our own
/// encoders, which only emit lowercase).
#[must_use]
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    fn hex_val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::Signer;

    /// RFC 8032 §7.1 TEST 1 seed — a published test vector (public data).
    /// Public key: d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a
    const TEST_SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    /// The public half of [`TEST_SEED`] — pinned as this crate's test gate
    /// key (the mcp boundary tests build their server against it).
    pub(crate) const TEST_VERIFY_KEY_HEX: &str =
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    /// Sign `message` with the test key — the runtime twin of the frozen
    /// golden vector, for snapshots a fixed constant can never cover (a live
    /// worker's `/proc` starttime).
    pub(crate) fn sign_test_message(message: &str) -> String {
        let signing = ed25519_dalek::SigningKey::from_bytes(&TEST_SEED);
        encode_hex(&signing.sign(message.as_bytes()).to_bytes())
    }

    /// The frozen golden vector: fixed fields, the exact canonical message,
    /// and the signature the devtools reference oracle (pure-python RFC 8032)
    /// computed for it. Cross-pinned in celestia-devtools
    /// `tests/test_ed25519_ref.py`.
    const GOLDEN: (&str, &str, &str, &str, u64, u64, u64) = (
        "0123456789abcdef0123456789abcdef",
        "w-alpha-1",
        "restart",
        // sig over the message below, by TEST_SEED
        "17d864db178f5ee0d72a12d6d69a5b3a558027e3679d08ac332f93368142f34382084fd0f26a0af523ef467658bb13fa292bd94cdb0ab363a7a123f3b55c520c",
        4242,
        987654,
        1_762_610_000,
    );
    const GOLDEN_DIGEST: &str = "3fc9ff018ff2baba21c744ec2ce4867db493048af4df11bf9ae501af4eef48d5";

    fn golden_message() -> String {
        let (proposal, worker, action, _sig, pid, starttime, issued_at) = GOLDEN;
        grant_message(proposal, worker, action, pid, starttime, issued_at)
    }

    #[test]
    fn canonical_message_is_frozen() {
        assert_eq!(
            golden_message(),
            "malkuth-gate-grant/v1\n\
             proposal_id:0123456789abcdef0123456789abcdef\n\
             worker_id:w-alpha-1\n\
             action:restart\n\
             pid:4242\n\
             starttime:987654\n\
             issued_at:1762610000\n"
        );
    }

    #[test]
    fn golden_vector_verifies_and_pins_the_digest() {
        let (_, _, _, sig, _, _, _) = GOLDEN;
        assert_eq!(grant_digest(&golden_message()), GOLDEN_DIGEST);
        assert_eq!(
            verify_with_key(&golden_message(), sig, TEST_VERIFY_KEY_HEX),
            Ok(())
        );
    }

    #[test]
    fn every_field_is_bound_by_the_signature() {
        let (proposal, worker, action, sig, pid, starttime, issued_at) = GOLDEN;
        let cases: [String; 6] = [
            grant_message(
                &format!("{proposal}:x"),
                worker,
                action,
                pid,
                starttime,
                issued_at,
            ),
            grant_message(
                proposal,
                &format!("{worker}2"),
                action,
                pid,
                starttime,
                issued_at,
            ),
            grant_message(proposal, worker, "drain", pid, starttime, issued_at),
            grant_message(proposal, worker, action, pid + 1, starttime, issued_at),
            grant_message(proposal, worker, action, pid, starttime + 1, issued_at),
            grant_message(proposal, worker, action, pid, starttime, issued_at + 1),
        ];
        for tampered in cases {
            assert_eq!(
                verify_with_key(&tampered, sig, TEST_VERIFY_KEY_HEX),
                Err(GrantError::InvalidSignature),
                "tampered message must not verify: {tampered:?}"
            );
        }
    }

    #[test]
    fn malformed_signature_and_key_encodings_are_refused() {
        let (_, _, _, sig, _, _, _) = GOLDEN;
        assert_eq!(
            verify_with_key(&golden_message(), "zz", TEST_VERIFY_KEY_HEX),
            Err(GrantError::MalformedSignature)
        );
        // 127 hex chars: decodes to 63.5 bytes -> rejected before crypto.
        assert_eq!(
            verify_with_key(&golden_message(), &sig[..127], TEST_VERIFY_KEY_HEX),
            Err(GrantError::MalformedSignature)
        );
        assert_eq!(
            verify_with_key(&golden_message(), sig, "not-hex"),
            Err(GrantError::MalformedVerifyKey)
        );
    }

    #[test]
    fn a_runtime_signed_grant_round_trips() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&TEST_SEED);
        let msg = grant_message(
            "ffffffffffffffffffffffffffffffff",
            "w-beta-2",
            "drain",
            7,
            8,
            9,
        );
        let sig = signing.sign(msg.as_bytes());
        assert_eq!(
            verify_with_key(&msg, &encode_hex(&sig.to_bytes()), TEST_VERIFY_KEY_HEX),
            Ok(())
        );
    }

    #[test]
    fn hex_helpers_reject_bad_input() {
        assert_eq!(decode_hex("0f"), Some(vec![0x0f]));
        assert_eq!(decode_hex("0F"), None, "uppercase is not canonical here");
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("0g"), None);
        assert_eq!(encode_hex(&[0x00, 0xff]), "00ff");
    }

    /// The embedded production key must stay well-formed; a typo in the const
    /// would otherwise only surface at first production verification. Decodes
    /// the constant DIRECTLY (no env read) so it cannot race the ENV_LOCK
    /// test family that mutates `MALKUTH_GATE_VERIFY_KEY`; the
    /// absent/blank-env resolution path is covered under the lock by
    /// `resolve_verify_key_prefers_the_embedded_key_when_unset_or_blank`.
    #[test]
    fn embedded_production_key_decodes_to_32_bytes() {
        assert_eq!(
            decode_hex(DEFAULT_GATE_VERIFY_KEY_HEX).map(|bytes| bytes.len()),
            Some(32),
            "key: {DEFAULT_GATE_VERIFY_KEY_HEX}"
        );
    }

    // ── env override semantics (serialized: env is process-global) ──

    /// `resolve_verify_key` mutates one process-global variable, so every
    /// test touching it must hold this lock and restore the variable before
    /// releasing it.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_gate_key_env(value: Option<&str>, body: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded under ENV_LOCK; the variable is always
        // removed before the lock is released, so no other test can observe
        // the override.
        unsafe {
            match value {
                Some(v) => std::env::set_var("MALKUTH_GATE_VERIFY_KEY", v),
                None => std::env::remove_var("MALKUTH_GATE_VERIFY_KEY"),
            }
        }
        body();
        // SAFETY: same exclusive section — restore the pristine state.
        unsafe {
            std::env::remove_var("MALKUTH_GATE_VERIFY_KEY");
        }
    }

    #[test]
    fn resolve_verify_key_prefers_the_embedded_key_when_unset_or_blank() {
        with_gate_key_env(None, || {
            assert_eq!(
                resolve_verify_key().as_deref(),
                Ok(DEFAULT_GATE_VERIFY_KEY_HEX)
            );
        });
        for blank in ["", "   "] {
            with_gate_key_env(Some(blank), || {
                assert_eq!(
                    resolve_verify_key().as_deref(),
                    Ok(DEFAULT_GATE_VERIFY_KEY_HEX),
                    "blank override {blank:?} must mean the embedded key"
                );
            });
        }
    }

    #[test]
    fn resolve_verify_key_accepts_a_valid_override_case_insensitively() {
        // The TEST key is 64 hex chars; feed its uppercase form.
        let upper = TEST_VERIFY_KEY_HEX.to_ascii_uppercase();
        with_gate_key_env(Some(&format!("  {upper}  ")), || {
            assert_eq!(
                resolve_verify_key().as_deref(),
                Ok(TEST_VERIFY_KEY_HEX),
                "trim + lowercase normalization"
            );
        });
    }

    #[test]
    fn resolve_verify_key_refuses_a_malformed_override() {
        for bad in ["zz", "0".repeat(63).as_str(), "0".repeat(65).as_str()] {
            with_gate_key_env(Some(bad), || {
                assert_eq!(
                    resolve_verify_key(),
                    Err(GrantError::MalformedVerifyKey),
                    "override {bad:?} must be an error, never a silent fallback"
                );
            });
        }
    }

    /// A set-but-non-UTF-8 override is still a CONFIGURED key: the only
    /// fail-closed answer is an error, not the embedded production key.
    #[test]
    fn resolve_verify_key_refuses_a_non_utf8_override() {
        use std::os::unix::ffi::OsStringExt;
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded under ENV_LOCK; removed before release.
        unsafe {
            std::env::set_var(
                "MALKUTH_GATE_VERIFY_KEY",
                std::ffi::OsString::from_vec(vec![0x80, 0xff]),
            );
        }
        assert_eq!(
            resolve_verify_key(),
            Err(GrantError::MalformedVerifyKey),
            "NotUnicode must not fall back to the embedded key"
        );
        // SAFETY: same exclusive section — restore the pristine state.
        unsafe {
            std::env::remove_var("MALKUTH_GATE_VERIFY_KEY");
        }
    }
}
