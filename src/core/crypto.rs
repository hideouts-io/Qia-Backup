//! Cryptography for backup-set objects.
//!
//! Encrypted objects use a small binary envelope:
//! `magic | HMAC-SHA256 | master IV | encrypted(data IV + session key) | ciphertext`.
//! Master keys are stored in `encryptedkeyset.dat`, protected by PBKDF2-HMAC-SHA256
//! (200,000 rounds) and AES-256-CBC + PKCS#7, with an HMAC over the envelope.

use aes::Aes256;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::core::error::{BackupError, Result};

type HmacSha256 = Hmac<Sha256>;
type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;

pub const PBKDF2_ROUNDS: u32 = 200_000;
// These magic values are part of the on-disk format and must remain stable.
pub const KEYSET_MAGIC: &[u8] = b"ARQ_ENCRYPTED_MASTER_KEYS";
pub const OBJECT_MAGIC: &[u8] = b"ARQO";
const OBJECT_HEADER_LEN: usize = 4 + 32 + 16;
const SESSION_OBJECT_LIMIT: u32 = 256;

#[derive(Clone)]
pub struct MasterKeys {
    pub encryption_key: [u8; 32],
    pub hmac_key: [u8; 32],
    pub identifier_salt: [u8; 32],
}

impl std::fmt::Debug for MasterKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKeys([redacted])")
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

fn derive_wrap_keys(password: &str, salt: &[u8; 16]) -> ([u8; 32], [u8; 32]) {
    let mut derived = [0u8; 64];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, PBKDF2_ROUNDS, &mut derived);
    let mut enc = [0u8; 32];
    let mut mac = [0u8; 32];
    enc.copy_from_slice(&derived[..32]);
    mac.copy_from_slice(&derived[32..]);
    (enc, mac)
}

fn aes_encrypt(key: &[u8; 32], iv: &[u8; 16], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256CbcEnc::new_from_slices(key, iv)
        .map_err(|e| BackupError::Crypto(format!("AES key/IV: {e}")))?;
    Ok(cipher.encrypt_padded_vec_mut::<Pkcs7>(plaintext))
}

fn aes_decrypt(key: &[u8; 32], iv: &[u8; 16], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256CbcDec::new_from_slices(key, iv)
        .map_err(|e| BackupError::Crypto(format!("AES key/IV: {e}")))?;
    cipher
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| BackupError::Crypto("invalid encrypted data padding".into()))
}

pub fn new_master_keys() -> MasterKeys {
    MasterKeys {
        encryption_key: random_bytes(),
        hmac_key: random_bytes(),
        identifier_salt: random_bytes(),
    }
}

/// Encrypt the master keys into `encryptedkeyset.dat` bytes.
pub fn encrypt_keyset(keys: &MasterKeys, password: &str) -> Result<Vec<u8>> {
    let salt: [u8; 16] = random_bytes();
    let iv: [u8; 16] = random_bytes();
    let (wrap_key, wrap_mac_key) = derive_wrap_keys(password, &salt);

    let mut plaintext = Vec::with_capacity(4 + 8 + 32 + 8 + 32 + 8 + 32);
    plaintext.extend_from_slice(&3u32.to_be_bytes());
    for field in [
        &keys.encryption_key[..],
        &keys.hmac_key[..],
        &keys.identifier_salt[..],
    ] {
        plaintext.extend_from_slice(&(field.len() as u64).to_be_bytes());
        plaintext.extend_from_slice(field);
    }
    let ciphertext = aes_encrypt(&wrap_key, &iv, &plaintext)?;
    let tag = hmac_sha256(&wrap_mac_key, &[&salt, &iv, &ciphertext]);

    let mut out = Vec::with_capacity(KEYSET_MAGIC.len() + 16 + 32 + 16 + ciphertext.len());
    out.extend_from_slice(KEYSET_MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&tag);
    out.extend_from_slice(&iv);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

pub fn decrypt_keyset(data: &[u8], password: &str) -> Result<MasterKeys> {
    let min = KEYSET_MAGIC.len() + 16 + 32 + 16 + 16;
    if data.len() < min {
        return Err(BackupError::Crypto("encrypted keyset is too short".into()));
    }
    if !data.starts_with(KEYSET_MAGIC) {
        return Err(BackupError::Crypto(
            "not an encrypted backup keyset (bad magic)".into(),
        ));
    }
    let mut pos = KEYSET_MAGIC.len();
    let salt: [u8; 16] = data[pos..pos + 16].try_into().unwrap();
    pos += 16;
    let tag = &data[pos..pos + 32];
    pos += 32;
    let iv: [u8; 16] = data[pos..pos + 16].try_into().unwrap();
    pos += 16;
    let ciphertext = &data[pos..];

    let (wrap_key, wrap_mac_key) = derive_wrap_keys(password, &salt);
    // Constant-time verification.
    let mut mac = HmacSha256::new_from_slice(&wrap_mac_key)
        .map_err(|e| BackupError::Crypto(format!("HMAC: {e}")))?;
    mac.update(&salt);
    mac.update(&iv);
    mac.update(ciphertext);
    mac.verify_slice(tag)
        .map_err(|_| BackupError::Crypto("wrong password (keyset HMAC mismatch)".into()))?;

    let plaintext = aes_decrypt(&wrap_key, &iv, ciphertext)?;
    let mut r = KeysetReader::new(&plaintext);
    let version = r.u32()?;
    if version != 3 {
        return Err(BackupError::Crypto(format!(
            "unsupported keyset version {version}"
        )));
    }
    let encryption_key = r.fixed::<32>()?;
    let hmac_key = r.fixed::<32>()?;
    let identifier_salt = r.fixed::<32>()?;
    r.finish()?;
    Ok(MasterKeys {
        encryption_key,
        hmac_key,
        identifier_salt,
    })
}

struct KeysetReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> KeysetReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return Err(BackupError::Crypto("truncated keyset".into()));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let len = self.u64()?;
        if len != N as u64 {
            return Err(BackupError::Crypto(format!(
                "keyset field length {len}, expected {N}"
            )));
        }
        Ok(self.take(N)?.try_into().unwrap())
    }
    fn finish(self) -> Result<()> {
        if self.pos != self.data.len() {
            return Err(BackupError::Crypto("trailing keyset bytes".into()));
        }
        Ok(())
    }
}

/// Encrypts objects with a reused session key (up to 256 objects
/// per session key). Each object still receives a fresh data IV.
pub struct ObjectEncryptor {
    keys: MasterKeys,
    session_key: [u8; 32],
    remaining: u32,
}

impl ObjectEncryptor {
    pub fn new(keys: MasterKeys) -> Self {
        Self {
            keys,
            session_key: random_bytes(),
            remaining: SESSION_OBJECT_LIMIT,
        }
    }

    pub fn keys(&self) -> &MasterKeys {
        &self.keys
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        if self.remaining == 0 {
            self.session_key = random_bytes();
            self.remaining = SESSION_OBJECT_LIMIT;
        }
        self.remaining -= 1;

        let master_iv: [u8; 16] = random_bytes();
        let data_iv: [u8; 16] = random_bytes();
        let mut iv_and_key = Vec::with_capacity(48);
        iv_and_key.extend_from_slice(&data_iv);
        iv_and_key.extend_from_slice(&self.session_key);
        let encrypted_iv_key = aes_encrypt(&self.keys.encryption_key, &master_iv, &iv_and_key)?;
        if encrypted_iv_key.len() != 64 {
            return Err(BackupError::Crypto(
                "unexpected IV/key ciphertext length".into(),
            ));
        }
        let ciphertext = aes_encrypt(&self.session_key, &data_iv, plaintext)?;
        let tag = hmac_sha256(
            &self.keys.hmac_key,
            &[&master_iv, &encrypted_iv_key, &ciphertext],
        );

        let mut out =
            Vec::with_capacity(OBJECT_HEADER_LEN + encrypted_iv_key.len() + ciphertext.len());
        out.extend_from_slice(OBJECT_MAGIC);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&master_iv);
        out.extend_from_slice(&encrypted_iv_key);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }
}

pub fn is_encrypted_object(data: &[u8]) -> bool {
    data.starts_with(OBJECT_MAGIC)
}

pub fn decrypt_object(keys: &MasterKeys, data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < OBJECT_HEADER_LEN + 64 + 16 {
        return Err(BackupError::Crypto("encrypted object is too short".into()));
    }
    if !data.starts_with(OBJECT_MAGIC) {
        return Err(BackupError::Crypto("not an encrypted backup object".into()));
    }
    let tag = &data[4..4 + 32];
    let master_iv: [u8; 16] = data[36..52].try_into().unwrap();
    let encrypted_iv_key = &data[52..52 + 64];
    let ciphertext = &data[52 + 64..];

    let mut mac = HmacSha256::new_from_slice(&keys.hmac_key)
        .map_err(|e| BackupError::Crypto(format!("HMAC: {e}")))?;
    mac.update(&master_iv);
    mac.update(encrypted_iv_key);
    mac.update(ciphertext);
    mac.verify_slice(tag)
        .map_err(|_| BackupError::Crypto("encrypted object HMAC mismatch".into()))?;

    let iv_and_key = aes_decrypt(&keys.encryption_key, &master_iv, encrypted_iv_key)?;
    if iv_and_key.len() != 48 {
        return Err(BackupError::Crypto(
            "invalid encrypted IV/session key".into(),
        ));
    }
    let data_iv: [u8; 16] = iv_and_key[..16].try_into().unwrap();
    let session_key: [u8; 32] = iv_and_key[16..].try_into().unwrap();
    aes_decrypt(&session_key, &data_iv, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyset_roundtrip() {
        let keys = new_master_keys();
        let blob = encrypt_keyset(&keys, "correct horse battery staple").unwrap();
        let decoded = decrypt_keyset(&blob, "correct horse battery staple").unwrap();
        assert_eq!(decoded.encryption_key, keys.encryption_key);
        assert_eq!(decoded.hmac_key, keys.hmac_key);
        assert_eq!(decoded.identifier_salt, keys.identifier_salt);

        let wrong = decrypt_keyset(&blob, "wrong");
        assert!(wrong.is_err());
    }

    #[test]
    fn object_roundtrip() {
        let keys = new_master_keys();
        let mut enc = ObjectEncryptor::new(keys.clone());
        let ciphertext = enc.encrypt(b"hello qia").unwrap();
        assert!(is_encrypted_object(&ciphertext));
        assert_eq!(decrypt_object(&keys, &ciphertext).unwrap(), b"hello qia");
        let mut tampered = ciphertext.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(decrypt_object(&keys, &tampered).is_err());
    }

    #[test]
    fn session_key_rotates() {
        let mut enc = ObjectEncryptor::new(new_master_keys());
        for _ in 0..(SESSION_OBJECT_LIMIT + 2) {
            assert!(enc.encrypt(b"x").is_ok());
        }
    }
}
