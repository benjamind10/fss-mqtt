//! Private keys from PEM files, including passphrase-protected ones, which
//! rustls can't read by itself:
//!
//! - `ENCRYPTED PRIVATE KEY`: PKCS#8 with PBES2 (PBKDF2 with HMAC-SHA1/256/384/512,
//!   then AES-128/192/256-CBC or 3DES-CBC), what `openssl genpkey -aes256` and
//!   `openssl pkcs8 -topk8` write.
//! - The older OpenSSL form: `RSA/EC PRIVATE KEY` with `Proc-Type: 4,ENCRYPTED` and
//!   `DEK-Info: AES-256-CBC,<iv>` (also AES-128/192, DES-EDE3, DES), keyed with
//!   OpenSSL's MD5-based EVP_BytesToKey.
//!
//! Unencrypted PKCS#8, PKCS#1 (RSA) and SEC1 (EC) keys pass straight through.

use std::num::NonZeroU32;

use aes::cipher::{BlockCipherDecrypt, KeyInit};
use md5::{Digest, Md5};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer};

#[derive(Debug, PartialEq)]
pub enum KeyError {
    /// The key is encrypted and no passphrase was given.
    NeedsPassphrase,
    /// The passphrase didn't decrypt it.
    WrongPassphrase,
    Other(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::NeedsPassphrase => f.write_str("the key is encrypted and needs a passphrase"),
            KeyError::WrongPassphrase => f.write_str("wrong passphrase for the key"),
            KeyError::Other(e) => f.write_str(e),
        }
    }
}

fn other(e: impl Into<String>) -> KeyError {
    KeyError::Other(e.into())
}

/// The first private key in `pem`, decrypted with `passphrase` if it needs one.
pub fn load(pem: &[u8], passphrase: Option<&str>) -> Result<PrivateKeyDer<'static>, KeyError> {
    let blocks = blocks(pem);
    let Some(b) = blocks.iter().find(|b| b.label.ends_with("PRIVATE KEY")) else {
        return Err(other("no private key found (expected a PEM \"PRIVATE KEY\" block)"));
    };
    let der = crate::base64::decode(&b.body).ok_or_else(|| other("the key's base64 is damaged"))?;
    let need = || passphrase.ok_or(KeyError::NeedsPassphrase);
    match b.label.as_str() {
        "PRIVATE KEY" => Ok(PrivatePkcs8KeyDer::from(der).into()),
        "ENCRYPTED PRIVATE KEY" => {
            let plain = pkcs8_decrypt(&der, need()?)?;
            Ok(PrivatePkcs8KeyDer::from(plain).into())
        }
        "RSA PRIVATE KEY" | "EC PRIVATE KEY" => {
            let der = match b.header("DEK-Info") {
                Some(info) => legacy_decrypt(&der, info, need()?)?,
                None => der,
            };
            Ok(if b.label.starts_with("RSA") {
                PrivatePkcs1KeyDer::from(der).into()
            } else {
                PrivateSec1KeyDer::from(der).into()
            })
        }
        other_label => Err(other(format!("unsupported key type \"{other_label}\""))),
    }
}

// ── PEM ────────────────────────────────────────────────────────────────────

struct Block {
    label: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Block {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn blocks(pem: &[u8]) -> Vec<Block> {
    let text = String::from_utf8_lossy(pem);
    let mut out = Vec::new();
    let mut cur: Option<Block> = None;
    for line in text.lines().map(str::trim) {
        if let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) {
            cur = Some(Block {
                label: label.to_string(),
                headers: Vec::new(),
                body: String::new(),
            });
        } else if line.starts_with("-----END ") {
            out.extend(cur.take());
        } else if let Some(b) = &mut cur {
            match line.split_once(':') {
                Some((k, v)) if b.body.is_empty() && !k.contains(' ') => b.headers.push((k.into(), v.trim().into())),
                _ => b.body.push_str(line),
            }
        }
    }
    out
}

// ── DER (just enough for EncryptedPrivateKeyInfo) ─────────────────────────

struct Der<'a>(&'a [u8]);

impl<'a> Der<'a> {
    /// The next element: (tag, contents).
    fn next(&mut self) -> Result<(u8, &'a [u8]), KeyError> {
        let bad = || other("the encrypted key's structure is damaged");
        let b = self.0;
        let tag = *b.first().ok_or_else(bad)?;
        let first = *b.get(1).ok_or_else(bad)? as usize;
        let (len, head) = if first < 0x80 {
            (first, 2)
        } else {
            let n = first & 0x7f;
            if n == 0 || n > 4 {
                return Err(bad());
            }
            let len = b
                .get(2..2 + n)
                .ok_or_else(bad)?
                .iter()
                .fold(0usize, |a, &x| a << 8 | x as usize);
            (len, 2 + n)
        };
        let body = b.get(head..head.checked_add(len).ok_or_else(bad)?).ok_or_else(bad)?;
        self.0 = &b[head + len..];
        Ok((tag, body))
    }

    fn expect(&mut self, tag: u8) -> Result<&'a [u8], KeyError> {
        match self.next()? {
            (t, body) if t == tag => Ok(body),
            _ => Err(other("the encrypted key's structure is damaged")),
        }
    }
}

const SEQ: u8 = 0x30;
const OID: u8 = 0x06;
const OCTETS: u8 = 0x04;
const INT: u8 = 0x02;

// Object identifiers, DER-encoded contents.
const PBES2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0d];
const PBKDF2: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x05, 0x0c];
const SCRYPT: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0xda, 0x47, 0x04, 0x0b];
const HMAC_SHA1: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x07];
const HMAC_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x09];
const HMAC_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0a];
const HMAC_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x0b];
const AES128_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x02];
const AES192_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x16];
const AES256_CBC: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x01, 0x2a];
const DES_EDE3_CBC: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x03, 0x07];

/// Iteration counts beyond this are refused (OpenSSL's default is 2048).
const MAX_ITERATIONS: u32 = 10_000_000;

#[derive(Clone, Copy)]
enum Cipher {
    Aes128,
    Aes192,
    Aes256,
    DesEde3,
    Des,
}

impl Cipher {
    fn key_len(self) -> usize {
        match self {
            Cipher::Aes128 => 16,
            Cipher::Aes192 | Cipher::DesEde3 => 24,
            Cipher::Aes256 => 32,
            Cipher::Des => 8,
        }
    }
    fn block_len(self) -> usize {
        match self {
            Cipher::Des | Cipher::DesEde3 => 8,
            _ => 16,
        }
    }
}

fn pkcs8_decrypt(der: &[u8], pass: &str) -> Result<Vec<u8>, KeyError> {
    let mut top = Der(Der(der).expect(SEQ)?);
    let mut alg = Der(top.expect(SEQ)?);
    let data = top.expect(OCTETS)?;
    if alg.expect(OID)? != PBES2 {
        return Err(other(
            "the key uses an old PKCS#5 v1 encryption scheme; re-encrypt it with `openssl pkcs8 -topk8 -v2 aes256`",
        ));
    }
    let mut params = Der(alg.expect(SEQ)?);
    let mut kdf = Der(params.expect(SEQ)?);
    let mut enc = Der(params.expect(SEQ)?);

    let kdf_oid = kdf.expect(OID)?;
    if kdf_oid == SCRYPT {
        return Err(other(
            "the key is encrypted with scrypt, which isn't supported; use PBKDF2 (openssl's default)",
        ));
    }
    if kdf_oid != PBKDF2 {
        return Err(other("the key uses an unsupported key derivation"));
    }
    let mut kp = Der(kdf.expect(SEQ)?);
    let salt = kp.expect(OCTETS)?;
    let iterations = uint(kp.expect(INT)?)?;
    let mut prf = &ring::pbkdf2::PBKDF2_HMAC_SHA1;
    // Optional keyLength (INTEGER), then optional prf (SEQUENCE).
    while let Ok((tag, body)) = kp.next() {
        if tag == SEQ {
            prf = match Der(body).expect(OID)? {
                HMAC_SHA1 => &ring::pbkdf2::PBKDF2_HMAC_SHA1,
                HMAC_SHA256 => &ring::pbkdf2::PBKDF2_HMAC_SHA256,
                HMAC_SHA384 => &ring::pbkdf2::PBKDF2_HMAC_SHA384,
                HMAC_SHA512 => &ring::pbkdf2::PBKDF2_HMAC_SHA512,
                _ => return Err(other("the key's PBKDF2 uses an unsupported hash")),
            };
        }
    }
    let cipher = match enc.expect(OID)? {
        AES128_CBC => Cipher::Aes128,
        AES192_CBC => Cipher::Aes192,
        AES256_CBC => Cipher::Aes256,
        DES_EDE3_CBC => Cipher::DesEde3,
        _ => return Err(other("the key uses an unsupported cipher")),
    };
    let iv = enc.expect(OCTETS)?;
    let iterations = NonZeroU32::new(iterations)
        .filter(|n| n.get() <= MAX_ITERATIONS)
        .ok_or_else(|| other("the key's PBKDF2 iteration count is out of range"))?;
    let mut key = vec![0u8; cipher.key_len()];
    ring::pbkdf2::derive(*prf, iterations, salt, pass.as_bytes(), &mut key);
    let plain = cbc_decrypt(cipher, &key, iv, data)?;
    // A wrong passphrase almost always breaks the padding; if not, the result
    // still has to look like a PrivateKeyInfo.
    if Der(&plain).expect(SEQ).is_err() {
        return Err(KeyError::WrongPassphrase);
    }
    Ok(plain)
}

fn uint(b: &[u8]) -> Result<u32, KeyError> {
    if b.is_empty() || b.len() > 5 || (b.len() == 5 && b[0] != 0) {
        return Err(other("the key's PBKDF2 iteration count is out of range"));
    }
    Ok(b.iter().fold(0u64, |a, &x| a << 8 | x as u64).min(u32::MAX as u64) as u32)
}

/// `DEK-Info: AES-256-CBC,<hex iv>`; the key is OpenSSL's EVP_BytesToKey with
/// MD5, one round, salted with the first 8 bytes of the IV.
fn legacy_decrypt(der: &[u8], info: &str, pass: &str) -> Result<Vec<u8>, KeyError> {
    let (name, iv_hex) = info.split_once(',').ok_or_else(|| other("bad DEK-Info header"))?;
    let cipher = match name.trim().to_ascii_uppercase().as_str() {
        "AES-128-CBC" => Cipher::Aes128,
        "AES-192-CBC" => Cipher::Aes192,
        "AES-256-CBC" => Cipher::Aes256,
        "DES-EDE3-CBC" => Cipher::DesEde3,
        "DES-CBC" => Cipher::Des,
        n => return Err(other(format!("the key uses an unsupported cipher ({n})"))),
    };
    let iv = hex(iv_hex.trim())
        .filter(|iv| iv.len() == cipher.block_len())
        .ok_or_else(|| other("bad DEK-Info IV"))?;
    let salt = &iv[..8];
    let mut key = Vec::with_capacity(cipher.key_len() + 16);
    let mut prev: Vec<u8> = Vec::new();
    while key.len() < cipher.key_len() {
        let mut h = Md5::new();
        h.update(&prev);
        h.update(pass.as_bytes());
        h.update(salt);
        prev = h.finalize().to_vec();
        key.extend_from_slice(&prev);
    }
    key.truncate(cipher.key_len());
    let plain = cbc_decrypt(cipher, &key, &iv, der)?;
    if Der(&plain).expect(SEQ).is_err() {
        return Err(KeyError::WrongPassphrase);
    }
    Ok(plain)
}

fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// CBC decryption with PKCS#7 padding. A bad pad is reported as a wrong passphrase.
fn cbc_decrypt(cipher: Cipher, key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, KeyError> {
    fn run<C: BlockCipherDecrypt + KeyInit>(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, KeyError> {
        let n = C::block_size();
        let c = C::new_from_slice(key).map_err(|_| other("bad key length"))?;
        if iv.len() != n || data.is_empty() || !data.len().is_multiple_of(n) {
            return Err(other("the encrypted key has a bad length"));
        }
        let mut out = data.to_vec();
        let mut prev = iv.to_vec();
        for chunk in out.chunks_mut(n) {
            let saved = chunk.to_vec();
            let block = chunk.try_into().map_err(|_| other("bad block"))?;
            c.decrypt_block(block);
            chunk.iter_mut().zip(&prev).for_each(|(b, p)| *b ^= p);
            prev = saved;
        }
        let pad = *out.last().unwrap_or(&0) as usize;
        if pad == 0 || pad > n || !out[out.len() - pad..].iter().all(|&b| b as usize == pad) {
            return Err(KeyError::WrongPassphrase);
        }
        out.truncate(out.len() - pad);
        Ok(out)
    }
    match cipher {
        Cipher::Aes128 => run::<aes::Aes128>(key, iv, data),
        Cipher::Aes192 => run::<aes::Aes192>(key, iv, data),
        Cipher::Aes256 => run::<aes::Aes256>(key, iv, data),
        Cipher::DesEde3 => run::<des::TdesEde3>(key, iv, data),
        Cipher::Des => run::<des::Des>(key, iv, data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/keys/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn der(k: &PrivateKeyDer) -> Vec<u8> {
        k.secret_der().to_vec()
    }

    #[test]
    fn decrypts_every_supported_form() {
        let rsa8 = fixture("rsa-pkcs8.der");
        let rsa1 = fixture("rsa-pkcs1.der");
        let ec8 = fixture("ec-pkcs8.der");
        let ec1 = fixture("ec-sec1.der");
        for (file, want) in [
            ("rsa-pkcs8-aes256.key", &rsa8),
            ("rsa-pkcs8-aes128-sha1.key", &rsa8),
            ("rsa-pkcs8-aes192-sha512.key", &rsa8),
            ("rsa-pkcs8-des3.key", &rsa8),
            ("ec-pkcs8-aes256.key", &ec8),
            ("rsa-legacy-aes256.key", &rsa1),
            ("rsa-legacy-des3.key", &rsa1),
            ("ec-legacy-aes128.key", &ec1),
        ] {
            let pem = fixture(file);
            let k = load(&pem, Some("s3cret")).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(&der(&k), want, "{file}");
            assert_eq!(load(&pem, None), Err(KeyError::NeedsPassphrase), "{file}");
            assert_eq!(load(&pem, Some("wrong")), Err(KeyError::WrongPassphrase), "{file}");
        }
        // The decrypted key types are what rustls expects.
        assert!(matches!(
            load(&fixture("rsa-legacy-aes256.key"), Some("s3cret")),
            Ok(PrivateKeyDer::Pkcs1(_))
        ));
        assert!(matches!(
            load(&fixture("ec-legacy-aes128.key"), Some("s3cret")),
            Ok(PrivateKeyDer::Sec1(_))
        ));
        assert!(matches!(
            load(&fixture("ec-pkcs8-aes256.key"), Some("s3cret")),
            Ok(PrivateKeyDer::Pkcs8(_))
        ));
    }

    #[test]
    fn plain_keys_pass_through() {
        for (file, want) in [
            ("rsa.key", fixture("rsa-pkcs8.der")),
            ("ec.key", fixture("ec-pkcs8.der")),
        ] {
            let pem = fixture(file);
            assert_eq!(der(&load(&pem, None).unwrap()), want, "{file}");
            // A passphrase given for an unencrypted key is simply not needed.
            assert_eq!(der(&load(&pem, Some("s3cret")).unwrap()), want, "{file}");
        }
    }

    #[test]
    fn unsupported_forms_say_why() {
        let msg = |f: &str| load(&fixture(f), Some("s3cret")).unwrap_err().to_string();
        assert!(msg("rsa-pkcs8-scrypt.key").contains("scrypt"));
        assert!(msg("rsa-pkcs8-v1.key").contains("PKCS#5 v1"));
        assert!(
            load(b"no key here", None)
                .unwrap_err()
                .to_string()
                .contains("no private key")
        );
        let cert = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        assert!(load(cert, None).unwrap_err().to_string().contains("no private key"));
    }

    #[test]
    fn never_panics_on_damaged_keys() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for file in [
            "rsa-pkcs8-aes256.key",
            "rsa-legacy-des3.key",
            "ec-legacy-aes128.key",
            "rsa-pkcs8-des3.key",
        ] {
            let good = fixture(file);
            for _ in 0..300 {
                let mut f = good.clone();
                for _ in 0..(rnd() % 6 + 1) {
                    let i = (rnd() as usize) % f.len();
                    f[i] = rnd() as u8;
                }
                let _ = load(&f, Some("s3cret"));
                let cut = (rnd() as usize) % f.len();
                let _ = load(&f[..cut], Some("s3cret"));
            }
        }
    }
}
