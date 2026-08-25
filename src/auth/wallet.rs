//! EIP-191 `personal_sign` implemented directly against k256 + sha3,
//! plus helpers to derive the Ethereum address from a private key
//! and to acquire a JWT from LibertAI's `/auth/login` endpoint.
//!
//! The `alloy` crate would give us this for free but its current MSRV
//! is newer than what we want to require; k256 + sha3 is ~40 lines.

use anyhow::{anyhow, Context, Result};
use k256::ecdsa::{
    signature::hazmat::PrehashSigner, RecoveryId, Signature, SigningKey, VerifyingKey,
};
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

    let (sig, rec_id): (Signature, RecoveryId) =
        sk.sign_prehash(&digest).context("ecdsa sign failed")?;

    let mut out = [0u8; 65];
    let bytes = sig.to_bytes();
    out[..64].copy_from_slice(&bytes);
    out[64] = 27 + rec_id.to_byte();
    Ok(format!("0x{}", hex::encode(out)))
}

/// Recover the `0x` address that produced an EIP-191 `personal_sign`
/// signature over `message` — the inverse of [`personal_sign`], used to
/// verify a signature without holding the private key. Accepts a 65-byte
/// `r||s||v` hex signature with `v ∈ {0,1,27,28}`.
pub fn recover_address(message: &str, signature_hex: &str) -> Result<String> {
    let prefixed = format!("\x19Ethereum Signed Message:\n{}{}", message.len(), message);
    let digest = Keccak256::digest(prefixed.as_bytes());

    let raw = hex::decode(signature_hex.trim().trim_start_matches("0x"))
        .context("signature must be hex")?;
    if raw.len() != 65 {
        return Err(anyhow!("signature must be 65 bytes (r||s||v)"));
    }
    let rec_byte = if raw[64] >= 27 { raw[64] - 27 } else { raw[64] };
    let rec_id = RecoveryId::from_byte(rec_byte)
        .ok_or_else(|| anyhow!("invalid recovery id {}", raw[64]))?;
    let sig = Signature::from_slice(&raw[..64]).context("invalid signature bytes")?;

    let vk = VerifyingKey::recover_from_prehash(&digest, &sig, rec_id)
        .context("could not recover signer key from signature")?;
    let encoded = vk.to_encoded_point(false);
    let hash = Keccak256::digest(&encoded.as_bytes()[1..]);
    Ok(format!("0x{}", hex::encode(&hash[12..])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_then_recover_roundtrips_to_the_signer_address() {
        // A well-known test private key.
        let sk = signing_key_from_hex(
            "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
        )
        .unwrap();
        let addr = address_from_signing_key(&sk);
        let sig = personal_sign(&sk, "sha256:deadbeef").unwrap();
        let recovered = recover_address("sha256:deadbeef", &sig).unwrap();
        assert_eq!(recovered.to_lowercase(), addr.to_lowercase());
    }

    #[test]
    fn recover_differs_when_message_tampered() {
        let sk = signing_key_from_hex(
            "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
        )
        .unwrap();
        let addr = address_from_signing_key(&sk);
        let sig = personal_sign(&sk, "sha256:aaaa").unwrap();
        let recovered = recover_address("sha256:bbbb", &sig).unwrap();
        assert_ne!(recovered.to_lowercase(), addr.to_lowercase());
    }
}
