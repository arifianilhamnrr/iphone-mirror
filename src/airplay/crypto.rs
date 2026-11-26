//! Cryptographic primitives for AirPlay / RAOP authentication, FairPlay key
//! derivation, and media stream decryption.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{bail, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha512};
use x25519_dalek::{EphemeralSecret, PublicKey};

/// Streaming AES-128-CTR cipher matching OpenSSL `EVP_aes_128_ctr` behavior
/// (big-endian 128-bit counter incremented after each 16-byte keystream block,
/// maintaining byte offset across calls).
pub struct Aes128Ctr {
    cipher: Aes128,
    counter: [u8; 16],
    keystream: [u8; 16],
    offset: usize,
}

impl Aes128Ctr {
    pub fn new(key: [u8; 16], iv: [u8; 16]) -> Self {
        Self {
            cipher: Aes128::new_from_slice(&key).expect("16-byte AES key"),
            counter: iv,
            keystream: [0u8; 16],
            offset: 16,
        }
    }

    pub fn apply_keystream(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            if self.offset == 16 {
                let mut block = self.counter;
                self.cipher.encrypt_block((&mut block).into());
                self.keystream = block;
                let next = u128::from_be_bytes(self.counter).wrapping_add(1);
                self.counter = next.to_be_bytes();
                self.offset = 0;
            }
            *byte ^= self.keystream[self.offset];
            self.offset += 1;
        }
    }
}

/// Decrypts `buf` in place using AES-128-CBC (zero padding; trailing `<16` bytes are
/// left untouched per RAOP audio packet specification).
pub fn aes_cbc_decrypt_in_place(key: &[u8; 16], iv: &[u8; 16], buf: &mut [u8]) {
    let cipher = Aes128::new_from_slice(key).expect("16-byte AES key");
    let mut prev = *iv;
    for chunk in buf.chunks_exact_mut(16) {
        let mut ciphertext = [0u8; 16];
        ciphertext.copy_from_slice(chunk);
        cipher.decrypt_block(chunk.into());
        for (b, p) in chunk.iter_mut().zip(prev.iter()) {
            *b ^= *p;
        }
        prev = ciphertext;
    }
}

/// Persistent Ed25519 identity used to identify this AirPlay receiver.
pub struct ServerIdentity {
    pub signing_key: SigningKey,
}

impl ServerIdentity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn public_key_hex(&self) -> String {
        self.public_key_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Per-connection AirPlay `POST /pair-verify` X25519 + Ed25519 state.
#[derive(Default)]
pub struct PairVerifyState {
    ecdh_secret: Option<[u8; 32]>,
    client_ed25519: Option<[u8; 32]>,
    client_x25519: Option<[u8; 32]>,
    server_x25519_pub: Option<[u8; 32]>,
    ctr: Option<Aes128Ctr>,
}

impl PairVerifyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage 1 (`req[0] == 1`): 4-byte header + 32-byte X25519 pk + 32-byte Ed25519 pk.
    /// Returns 96-byte response: `server_x25519_pk (32) || aes_ctr(ed25519_sig (64))`.
    pub fn step1(&mut self, req: &[u8], signing_key: &SigningKey) -> Result<[u8; 96]> {
        if req.len() != 68 || req[0] != 1 {
            bail!("invalid pair-verify step1 packet (len={})", req.len());
        }
        let mut client_x25519 = [0u8; 32];
        let mut client_ed25519 = [0u8; 32];
        client_x25519.copy_from_slice(&req[4..36]);
        client_ed25519.copy_from_slice(&req[36..68]);

        let secret = EphemeralSecret::random_from_rng(OsRng);
        let server_pub = PublicKey::from(&secret).to_bytes();
        let shared = secret
            .diffie_hellman(&PublicKey::from(client_x25519))
            .to_bytes();

        let mut msg = [0u8; 64];
        msg[..32].copy_from_slice(&server_pub);
        msg[32..].copy_from_slice(&client_x25519);
        let sig = signing_key.sign(&msg).to_bytes();

        let mut key_hash = Sha512::new();
        key_hash.update(b"Pair-Verify-AES-Key");
        key_hash.update(shared);
        let key_digest = key_hash.finalize();

        let mut iv_hash = Sha512::new();
        iv_hash.update(b"Pair-Verify-AES-IV");
        iv_hash.update(shared);
        let iv_digest = iv_hash.finalize();

        let mut aes_key = [0u8; 16];
        let mut aes_iv = [0u8; 16];
        aes_key.copy_from_slice(&key_digest[..16]);
        aes_iv.copy_from_slice(&iv_digest[..16]);

        let mut ctr = Aes128Ctr::new(aes_key, aes_iv);
        let mut encrypted_sig = sig;
        ctr.apply_keystream(&mut encrypted_sig);

        self.ecdh_secret = Some(shared);
        self.client_ed25519 = Some(client_ed25519);
        self.client_x25519 = Some(client_x25519);
        self.server_x25519_pub = Some(server_pub);
        self.ctr = Some(ctr);

        let mut out = [0u8; 96];
        out[..32].copy_from_slice(&server_pub);
        out[32..].copy_from_slice(&encrypted_sig);
        Ok(out)
    }

    /// Stage 2 (`req[0] == 0`): 4-byte header + 64-byte encrypted client Ed25519 signature.
    pub fn step2(&mut self, req: &[u8]) -> Result<()> {
        if req.len() != 68 || req[0] != 0 {
            bail!("invalid pair-verify step2 packet (len={})", req.len());
        }
        let ctr = self
            .ctr
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("pair-verify step2 before step1"))?;
        let client_ed = self
            .client_ed25519
            .ok_or_else(|| anyhow::anyhow!("missing client ed25519 key"))?;
        let client_x = self
            .client_x25519
            .ok_or_else(|| anyhow::anyhow!("missing client x25519 key"))?;
        let server_x = self
            .server_x25519_pub
            .ok_or_else(|| anyhow::anyhow!("missing server x25519 key"))?;

        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&req[4..68]);
        ctr.apply_keystream(&mut sig_bytes);

        let verifying_key = VerifyingKey::from_bytes(&client_ed)?;
        let signature = Signature::from_bytes(&sig_bytes);
        let mut msg = [0u8; 64];
        msg[..32].copy_from_slice(&client_x);
        msg[32..].copy_from_slice(&server_x);
        verifying_key.verify(&msg, &signature)?;
        Ok(())
    }

    pub fn ecdh_secret(&self) -> Option<[u8; 32]> {
        self.ecdh_secret
    }
}

/// Combines FairPlay-decrypted AES key with optional `pair-verify` ECDH secret via SHA-512.
pub fn derive_session_aes_key(fairplay_key: [u8; 16], ecdh_secret: Option<[u8; 32]>) -> [u8; 16] {
    match ecdh_secret {
        Some(secret) => {
            let mut hasher = Sha512::new();
            hasher.update(fairplay_key);
            hasher.update(secret);
            let digest = hasher.finalize();
            let mut out = [0u8; 16];
            out.copy_from_slice(&digest[..16]);
            out
        }
        None => fairplay_key,
    }
}

/// Derives the AES-128-CTR `(key, iv)` for video mirror stream type 110.
pub fn derive_video_key_iv(aes_key: &[u8; 16], stream_connection_id: u64) -> ([u8; 16], [u8; 16]) {
    let key_label = format!("AirPlayStreamKey{stream_connection_id}");
    let iv_label = format!("AirPlayStreamIV{stream_connection_id}");

    let mut kh = Sha512::new();
    kh.update(key_label.as_bytes());
    kh.update(aes_key);
    let kd = kh.finalize();

    let mut ih = Sha512::new();
    ih.update(iv_label.as_bytes());
    ih.update(aes_key);
    let id = ih.finalize();

    let mut key = [0u8; 16];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&kd[..16]);
    iv.copy_from_slice(&id[..16]);
    (key, iv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_ctr_roundtrip_across_chunks() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let plain = b"AirPlay-Mirror-H264-Test-Payload-Across-Block-Boundary-123456";
        let mut buf = plain.to_vec();

        let mut enc = Aes128Ctr::new(key, iv);
        enc.apply_keystream(&mut buf[..13]);
        enc.apply_keystream(&mut buf[13..]);
        assert_ne!(&buf[..], &plain[..]);

        let mut dec = Aes128Ctr::new(key, iv);
        dec.apply_keystream(&mut buf[..20]);
        dec.apply_keystream(&mut buf[20..]);
        assert_eq!(&buf[..], &plain[..]);
    }

    #[test]
    fn video_key_iv_derivation_is_deterministic() {
        let aes_key = [0xABu8; 16];
        let (k1, iv1) = derive_video_key_iv(&aes_key, 123456789);
        let (k2, iv2) = derive_video_key_iv(&aes_key, 123456789);
        let (k3, _) = derive_video_key_iv(&aes_key, 987654321);
        assert_eq!(k1, k2);
        assert_eq!(iv1, iv2);
        assert_ne!(k1, k3);
        assert_ne!(k1, iv1);
    }

    #[test]
    fn session_aes_key_uses_sha512() {
        let fp_key = [0x12u8; 16];
        let secret = [0x34u8; 32];
        let derived = derive_session_aes_key(fp_key, Some(secret));
        let mut expected = Sha512::new();
        expected.update(fp_key);
        expected.update(secret);
        let digest = expected.finalize();
        assert_eq!(derived, digest[..16]);
        assert_eq!(derive_session_aes_key(fp_key, None), fp_key);
    }

    #[test]
    fn aes_cbc_leaves_short_tail_unencrypted() {
        let key = [0x42u8; 16];
        let iv = [0x01u8; 16];
        let mut data = [0xAAu8; 21];
        let tail = data[16..].to_vec();
        aes_cbc_decrypt_in_place(&key, &iv, &mut data);
        assert_eq!(&data[16..], &tail[..]);
    }
}
