//! EIP-191 `personal_sign` implemented directly against k256 + sha3,
//! plus helpers to derive the Ethereum address from a private key
//! and to acquire a JWT from LibertAI's `/auth/login` endpoint.
//!
//! The `alloy` crate would give us this for free but its current MSRV
//! is newer than what we want to require; k256 + sha3 is ~40 lines.

use anyhow::{anyhow, Context, Result};
use k256::ecdsa::{signature::hazmat::PrehashSigner, RecoveryId, Signature, SigningKey};
use sha3::{Digest, Keccak256};

/// Parse a hex-encoded secp256k1 private key (with or without `0x`).
pub fn signing_key_from_hex(hex_str: &str) -> Result<SigningKey> {
    let stripped = hex_str.trim().trim_start_matches("0x");
    let bytes: zeroize::Zeroizing<Vec<u8>> =
        zeroize::Zeroizing::new(hex::decode(stripped).context("private key must be hex")?);
    if bytes.len() != 32 {
        return Err(anyhow!("private key must decode to 32 bytes"));
    }
    SigningKey::from_slice(&bytes).context("invalid secp256k1 private key")
}

/// Ethereum 0x-prefixed address derived from the signer's public key.
pub fn address_from_signing_key(sk: &SigningKey) -> String {
    let vk = sk.verifying_key();
    let encoded = vk.to_encoded_point(false); // uncompressed: 0x04 || X || Y
    let pub_xy = &encoded.as_bytes()[1..];
    let hash = Keccak256::digest(pub_xy);
    format!("0x{}", hex::encode(&hash[12..]))
}

/// Sign `message` with EIP-191 `personal_sign` and return a 65-byte
/// `0x`-prefixed hex string (`r || s || v` with `v ∈ {27, 28}`).
pub fn personal_sign(sk: &SigningKey, message: &str) -> Result<String> {
    let prefixed = format!("\x19Ethereum Signed Message:\n{}{}", message.len(), message);
    let digest = Keccak256::digest(prefixed.as_bytes());

    let (sig, rec_id): (Signature, RecoveryId) = sk
        .sign_prehash(&digest)
        .context("ecdsa sign failed")?;

    let mut out = [0u8; 65];
    let bytes = sig.to_bytes();
    out[..64].copy_from_slice(&bytes);
    out[64] = 27 + rec_id.to_byte();
    Ok(format!("0x{}", hex::encode(out)))
}
