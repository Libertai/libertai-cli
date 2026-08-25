//! Plugin content signing + verification — the `.libertai-plugin/` "verified
//! publisher" differentiator.
//!
//! A plugin author signs a deterministic digest of the plugin's files with
//! their wallet key (EIP-191 `personal_sign`, the same scheme as `libertai
//! login`) and commits `.libertai-plugin/signature.json`. On install/audit we
//! recompute the digest, recover the signer address, and report whether the
//! plugin is unsigned, signed by an unknown key, signed by a trusted
//! publisher, or tampered.
//!
//! Honest scope: a valid signature proves the content is intact and was signed
//! by the key controlling `address`. It proves *identity* only when that
//! address is in the user's trusted-publishers allowlist — signatures are
//! integrity + provenance, not a naming authority.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use k256::ecdsa::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::wallet::{address_from_signing_key, personal_sign, recover_address};

/// Path of the signature sidecar, relative to the plugin root.
pub const SIGNATURE_REL: &str = ".libertai-plugin/signature.json";

/// The signature sidecar written into a signed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureFile {
    /// Signing scheme identifier.
    pub algorithm: String,
    /// `0x` address of the signer.
    pub address: String,
    /// The content digest that was signed (`sha256:<hex>`).
    pub digest: String,
    /// `0x` EIP-191 signature over `digest`.
    pub signature: String,
}

/// Verification outcome for a plugin's content signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    /// No `signature.json` present.
    Unsigned,
    /// Valid signature whose signer is in the trusted-publishers allowlist.
    VerifiedPublisher(String),
    /// Valid signature, but the signer is not a known/allowlisted publisher.
    Signed(String),
    /// A signature is present but does not verify (tampered content or bad sig).
    Invalid(String),
}

impl SignatureStatus {
    /// One-line human label for the audit/install display.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            SignatureStatus::Unsigned => "unsigned".to_string(),
            SignatureStatus::VerifiedPublisher(addr) => format!("verified publisher ({addr})"),
            SignatureStatus::Signed(addr) => format!("signed, unverified publisher ({addr})"),
            SignatureStatus::Invalid(reason) => format!("INVALID signature — {reason}"),
        }
    }

    /// Whether the signature is present and cryptographically valid (verified
    /// or merely signed) — i.e. not unsigned and not tampered.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        matches!(
            self,
            SignatureStatus::VerifiedPublisher(_) | SignatureStatus::Signed(_)
        )
    }
}

/// Deterministic content digest of a plugin directory: `sha256` over the
/// sorted `"<relpath>\0<sha256(file)>\n"` lines for every file under `root`,
/// excluding the signature sidecar itself and any `.git` directory. Returns
/// `sha256:<hex>`.
pub fn plugin_digest(root: &Path) -> Result<String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_file_hashes(root, root, &mut entries)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for (rel, file_hash) in &entries {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(file_hash.as_bytes());
        hasher.update([b'\n']);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn collect_file_hashes(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_file_hashes(root, &path, out)?;
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel == SIGNATURE_REL {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        out.push((rel, hex::encode(Sha256::digest(&bytes))));
    }
    Ok(())
}

fn signature_path(root: &Path) -> PathBuf {
    root.join(".libertai-plugin").join("signature.json")
}

/// Verify a plugin's signature against its recomputed content digest, then
/// classify the signer against the `trusted` publisher allowlist.
#[must_use]
pub fn verify_plugin_signature(root: &Path, trusted: &[String]) -> SignatureStatus {
    let Ok(raw) = std::fs::read_to_string(signature_path(root)) else {
        return SignatureStatus::Unsigned;
    };
    let Ok(sig) = serde_json::from_str::<SignatureFile>(&raw) else {
        return SignatureStatus::Invalid("signature.json is malformed".to_string());
    };
    let digest = match plugin_digest(root) {
        Ok(d) => d,
        Err(e) => return SignatureStatus::Invalid(format!("cannot hash plugin: {e}")),
    };
    if digest != sig.digest {
        return SignatureStatus::Invalid(
            "content digest does not match the signed digest (tampered?)".to_string(),
        );
    }
    let recovered = match recover_address(&sig.digest, &sig.signature) {
        Ok(addr) => addr,
        Err(e) => return SignatureStatus::Invalid(format!("bad signature: {e}")),
    };
    if !recovered.eq_ignore_ascii_case(&sig.address) {
        return SignatureStatus::Invalid(
            "signature was not produced by the declared address".to_string(),
        );
    }
    if trusted.iter().any(|t| t.eq_ignore_ascii_case(&recovered)) {
        SignatureStatus::VerifiedPublisher(recovered)
    } else {
        SignatureStatus::Signed(recovered)
    }
}

/// Sign a plugin directory in place: compute its digest, sign it with `sk`,
/// and write `.libertai-plugin/signature.json`. Returns the written record.
pub fn sign_plugin(root: &Path, sk: &SigningKey) -> Result<SignatureFile> {
    let digest = plugin_digest(root)?;
    let file = SignatureFile {
        algorithm: "eip191-secp256k1".to_string(),
        address: address_from_signing_key(sk),
        signature: personal_sign(sk, &digest)?,
        digest,
    };
    let dir = root.join(".libertai-plugin");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let json = serde_json::to_string_pretty(&file).context("serializing signature.json")?;
    std::fs::write(dir.join("signature.json"), json).context("writing signature.json")?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::wallet::signing_key_from_hex;

    fn test_key() -> SigningKey {
        signing_key_from_hex("0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
            .unwrap()
    }

    fn write_plugin(root: &Path) {
        std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
        std::fs::write(
            root.join(".claude-plugin").join("plugin.json"),
            r#"{"name":"p"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("skills").join("bar")).unwrap();
        std::fs::write(root.join("skills").join("bar").join("SKILL.md"), "body").unwrap();
    }

    #[test]
    fn unsigned_when_no_signature_file() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path());
        assert_eq!(
            verify_plugin_signature(dir.path(), &[]),
            SignatureStatus::Unsigned
        );
    }

    #[test]
    fn sign_then_verify_roundtrips_and_respects_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path());
        let sk = test_key();
        let signed = sign_plugin(dir.path(), &sk).unwrap();

        // Not in allowlist → Signed(addr).
        match verify_plugin_signature(dir.path(), &[]) {
            SignatureStatus::Signed(addr) => assert!(addr.eq_ignore_ascii_case(&signed.address)),
            other => panic!("expected Signed, got {other:?}"),
        }
        // In allowlist → VerifiedPublisher(addr).
        let trusted = vec![signed.address.clone()];
        assert!(matches!(
            verify_plugin_signature(dir.path(), &trusted),
            SignatureStatus::VerifiedPublisher(_)
        ));
    }

    #[test]
    fn tampering_after_signing_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path());
        sign_plugin(dir.path(), &test_key()).unwrap();
        // Mutate a file after signing.
        std::fs::write(
            dir.path().join("skills").join("bar").join("SKILL.md"),
            "EVIL",
        )
        .unwrap();
        assert!(matches!(
            verify_plugin_signature(dir.path(), &[]),
            SignatureStatus::Invalid(_)
        ));
    }

    #[test]
    fn digest_is_stable_and_excludes_signature_file() {
        let dir = tempfile::tempdir().unwrap();
        write_plugin(dir.path());
        let before = plugin_digest(dir.path()).unwrap();
        sign_plugin(dir.path(), &test_key()).unwrap();
        // Adding the signature file must not change the digest.
        let after = plugin_digest(dir.path()).unwrap();
        assert_eq!(before, after);
    }
}
