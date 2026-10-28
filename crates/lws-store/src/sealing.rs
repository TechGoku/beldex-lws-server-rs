//! At-rest encryption of view keys.
//!
//! A database dump or stolen backup should not hand an attacker every
//! account's view key (and with it the account's full history). Keys are
//! sealed with XChaCha20-Poly1305 under a server key that lives outside the
//! database; the account's address is the associated data, so a sealed key
//! copied onto another row fails to open.
//!
//! Sealed layout: `0x01 | key_id | nonce[24] | ciphertext[32] | tag[16]`.
//! A bare 32-byte value is a plaintext key (development mode, or rows written
//! before a key was configured) and is still readable.

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use lws_core::{Key, ViewKey};

use crate::{Error, Result};

const VERSION: u8 = 1;
const SEALED_LEN: usize = 2 + 24 + 32 + 16;

pub struct Sealer {
    /// Key used for new writes.
    current: Option<(u8, XChaCha20Poly1305)>,
    /// Older keys still accepted for reading, for rotation.
    previous: Vec<(u8, XChaCha20Poly1305)>,
}

impl Sealer {
    /// Stores view keys in plaintext. For development only.
    pub fn plaintext() -> Sealer {
        Sealer { current: None, previous: Vec::new() }
    }

    /// `current` seals new writes; `previous` are only used to open older rows.
    pub fn new(current: (u8, &Key), previous: &[(u8, Key)]) -> Sealer {
        let cipher = |k: &Key| XChaCha20Poly1305::new(k.into());
        Sealer {
            current: Some((current.0, cipher(current.1))),
            previous: previous.iter().map(|(id, k)| (*id, cipher(k))).collect(),
        }
    }

    pub fn is_encrypting(&self) -> bool {
        self.current.is_some()
    }

    pub fn seal(&self, address: &str, key: &ViewKey) -> Result<Vec<u8>> {
        let Some((id, cipher)) = &self.current else { return Ok(key.bytes().to_vec()) };
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, Payload { msg: key.bytes(), aad: address.as_bytes() })
            .map_err(|_| Error::Corrupt("view key sealing failed".into()))?;
        let mut out = Vec::with_capacity(SEALED_LEN);
        out.push(VERSION);
        out.push(*id);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, address: &str, stored: &[u8]) -> Result<ViewKey> {
        if stored.len() == 32 {
            let mut k = [0u8; 32];
            k.copy_from_slice(stored);
            return Ok(ViewKey::new(k));
        }
        if stored.len() != SEALED_LEN || stored[0] != VERSION {
            return Err(Error::Corrupt(format!("sealed view key for {address} has an unknown format")));
        }
        let id = stored[1];
        let cipher = self
            .current
            .iter()
            .chain(self.previous.iter())
            .find(|(k, _)| *k == id)
            .map(|(_, c)| c)
            .ok_or_else(|| Error::Corrupt(format!("view key for {address} is sealed with unknown key id {id}")))?;
        let nonce = XNonce::from_slice(&stored[2..26]);
        let plain = zeroize::Zeroizing::new(
            cipher
                .decrypt(nonce, Payload { msg: &stored[26..], aad: address.as_bytes() })
                .map_err(|_| Error::Corrupt(format!("view key for {address} failed authentication")))?,
        );
        let mut k = [0u8; 32];
        k.copy_from_slice(&plain);
        Ok(ViewKey::new(k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_rotates() {
        let old = Sealer::new((1, &[1; 32]), &[]);
        let key = ViewKey::new([9; 32]);
        let sealed = old.seal("addr", &key).unwrap();
        assert_eq!(sealed.len(), SEALED_LEN);
        assert_ne!(&sealed[26..58], key.bytes());
        assert_eq!(old.open("addr", &sealed).unwrap(), key);

        // Bound to the address.
        assert!(old.open("other", &sealed).is_err());

        // A rotated server still opens rows sealed with the previous key and
        // seals new ones with the new key.
        let rotated = Sealer::new((2, &[2; 32]), &[(1, [1; 32])]);
        assert_eq!(rotated.open("addr", &sealed).unwrap(), key);
        assert_eq!(rotated.seal("addr", &key).unwrap()[1], 2);

        // Plaintext rows remain readable everywhere.
        assert_eq!(rotated.open("addr", key.bytes()).unwrap(), key);
        assert_eq!(Sealer::plaintext().seal("addr", &key).unwrap(), key.bytes().to_vec());
        assert!(Sealer::plaintext().open("addr", &sealed).is_err());
    }
}
