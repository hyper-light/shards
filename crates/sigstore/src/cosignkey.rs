//! cosign's private keys as cosign v3.1.3 reads and makes them (pkg/cosign/keys.go
//! LoadPrivateKey; go-securesystemslib v0.11.0 encrypted/encrypted.go): a PEM block,
//! `ENCRYPTED SIGSTORE PRIVATE KEY` (or the older `ENCRYPTED COSIGN PRIVATE KEY`),
//! holding JSON of the key's PKCS #8 sealed in NaCl's secretbox under a key scrypt made
//! of the password, read as Go's encoding/json reads it into encrypted's `data`; each
//! refusal in their words.

use crate::godec::{Dec, GoSlice};
use crate::tlog::gojson::{self, JValue};

/// The block type cosign writes.
pub const SIGSTORE_PEM: &str = "ENCRYPTED SIGSTORE PRIVATE KEY";
/// The block type cosign wrote before, which it still reads.
pub const COSIGN_PEM: &str = "ENCRYPTED COSIGN PRIVATE KEY";

/// The scrypt parameters encrypted takes (N, r, p): its Legacy, Standard and OWASP sets
/// alone, refusing any other, which a tampered file could make cost any memory
/// (encrypted.go:39-61, CheckParams 125-139). It writes Standard.
const PARAMS: [(i64, i64, i64); 3] = [(1 << 15, 8, 1), (1 << 16, 8, 1), (1 << 17, 8, 1)];
const STANDARD: (i64, i64, i64) = PARAMS[1];
const SALT: usize = 32;
const KEY: usize = 32;
const NONCE: usize = 24;

/// encrypted's `data`, as decoded.
#[derive(Default)]
struct Data {
    kdf_name: String,
    n: i64,
    r: i64,
    p: i64,
    salt: GoSlice<u8>,
    cipher_name: String,
    nonce: GoSlice<u8>,
    ciphertext: GoSlice<u8>,
}

/// json.Unmarshal of `bytes` into encrypted's `data`.
fn unmarshal(bytes: &[u8]) -> Result<Data, String> {
    let v = gojson::unmarshal(bytes)?;
    let mut d = Dec::new();
    let mut data = Data::default();
    d.object(
        &v,
        "data",
        "encrypted.data",
        &[("kdf", "kdf"), ("cipher", "cipher"), ("ciphertext", "ciphertext")],
        |d, i, x| match i {
            0 => kdf(d, x, &mut data),
            1 => d.object(
                x,
                "secretBoxCipher",
                "encrypted.secretBoxCipher",
                &[("name", "name"), ("nonce", "nonce")],
                |d, i, x| match i {
                    0 => d.string(x, &mut data.cipher_name, "string"),
                    _ => d.bytes(x, &mut data.nonce),
                },
            ),
            _ => d.bytes(x, &mut data.ciphertext),
        },
    );
    d.done(data)
}

fn kdf(d: &mut Dec, x: &JValue, data: &mut Data) {
    d.object(
        x,
        "scryptKDF",
        "encrypted.scryptKDF",
        &[("name", "name"), ("params", "params"), ("salt", "salt")],
        |d, i, x| match i {
            0 => d.string(x, &mut data.kdf_name, "string"),
            1 => d.object(
                x,
                "scryptParams",
                "encrypted.scryptParams",
                &[("N", "N"), ("r", "r"), ("p", "p")],
                |d, i, x| match i {
                    0 => d.int64(x, &mut data.n, "int"),
                    1 => d.int64(x, &mut data.r, "int"),
                    _ => d.int64(x, &mut data.p, "int"),
                },
            ),
            _ => d.bytes(x, &mut data.salt),
        },
    );
}

/// scrypt of `password` and `salt` with one of [`PARAMS`], 32 bytes (AWS-LC's, RFC 7914),
/// given exactly the memory those parameters take.
fn scrypt(password: &[u8], salt: &[u8], (n, r, p): (i64, i64, i64)) -> Result<[u8; KEY], String> {
    let (n, r, p) = (
        u64::try_from(n).map_err(|e| e.to_string())?,
        u64::try_from(r).map_err(|e| e.to_string())?,
        u64::try_from(p).map_err(|e| e.to_string())?,
    );
    // B, T and V: p, 1 and N blocks of 2r 64-byte words (scrypt.c).
    let memory = n
        .checked_add(p)
        .and_then(|b| b.checked_add(1))
        .and_then(|b| b.checked_mul(2 * r * 64))
        .and_then(|m| usize::try_from(m).ok())
        .ok_or("scrypt: parameters too large")?;
    let mut key = [0u8; KEY];
    // SAFETY: EVP_PBE_scrypt reads the password's and salt's bytes and writes `key`'s 32.
    let ok = unsafe {
        aws_lc_sys::EVP_PBE_scrypt(
            password.as_ptr().cast(),
            password.len(),
            salt.as_ptr(),
            salt.len(),
            n,
            r,
            p,
            memory,
            key.as_mut_ptr(),
            KEY,
        )
    };
    if ok != 1 {
        return Err("scrypt: failed".into());
    }
    Ok(key)
}

/// LoadPrivateKey's first half: the PKCS #8 of the key `pem` holds, decrypted with
/// `password`, or why not, as cosign says it.
pub fn decrypt(pem: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    let (block, _) = crate::tlog::pem::decode(pem).ok_or("invalid pem block")?;
    if block.kind != SIGSTORE_PEM.as_bytes() && block.kind != COSIGN_PEM.as_bytes() {
        return Err(format!(
            "unsupported pem type: {}",
            String::from_utf8_lossy(&block.kind)
        ));
    }
    open(&block.bytes, password).map_err(|e| format!("decrypt: {e}"))
}

/// LoadPrivateKey: the signer of the key `pem` holds, decrypted with `password`, or why
/// not, as cosign says it: a PKCS #8 x509.ParsePKCS8PrivateKey does not read is refused
/// in encoding/asn1's words, and a key of a type cosign has no default signer for (a P-224
/// key, an RSA key of other than 2048, 3072 or 4096 bits) as unsupported.
pub fn load(pem: &[u8], password: &[u8]) -> Result<crate::sign::Signer, String> {
    let mut der = decrypt(pem, password)?;
    let signer = pkcs8(&der)
        .map_err(|e| format!("parsing private key: {e}"))
        .and_then(|()| {
            crate::sign::Signer::from_pkcs8(&der).map_err(|_| "unsupported public key type".to_string())
        });
    der.fill(0);
    signer
}

/// asn1.Unmarshal into x509's `pkcs8`: its version, its AlgorithmIdentifier (an OID and
/// optional parameters) and its key's octets.
fn pkcs8(der: &[u8]) -> Result<(), String> {
    use crate::asn1::{Fields, Kind, Params, Value, unmarshal};
    let fail = |e: crate::asn1::Asn1Error| e.0;
    let (v, _) = unmarshal(der, Kind::Struct, &Params::default().named("pkcs8")).map_err(fail)?;
    let Value::Struct { inner, .. } = v else {
        return Err("asn1: structure error: not a SEQUENCE".into());
    };
    let mut f = Fields::new(inner);
    f.next(Kind::Int64, &Params::default().named("int"))
        .map_err(fail)?;
    if let Some(Value::Struct { inner, .. }) = f
        .next(Kind::Struct, &Params::default().named("AlgorithmIdentifier"))
        .map_err(fail)?
    {
        let mut a = Fields::new(inner);
        a.next(Kind::Oid, &Params::default().named("ObjectIdentifier"))
            .map_err(fail)?;
        a.next(Kind::Raw, &Params::default().optional().named("RawValue"))
            .map_err(fail)?;
    }
    f.next(Kind::Bytes, &Params::default()).map_err(fail)?;
    Ok(())
}

/// encrypted.Decrypt.
fn open(json: &[u8], password: &[u8]) -> Result<Vec<u8>, String> {
    let data = unmarshal(json)?;
    if data.kdf_name != "scrypt" {
        return Err(format!(
            "encrypted: unknown kdf name {}",
            shards_dockerfile::go::quote(data.kdf_name.as_bytes())
        ));
    }
    if data.cipher_name != "nacl/secretbox" {
        return Err(format!(
            "encrypted: unknown cipher name {}",
            shards_dockerfile::go::quote(data.cipher_name.as_bytes())
        ));
    }
    let params = (data.n, data.r, data.p);
    if !PARAMS.contains(&params) {
        return Err("unsupported scrypt parameters".into());
    }
    let key = scrypt(password, data.salt.items(), params)?;
    let nonce: [u8; NONCE] = data
        .nonce
        .items()
        .try_into()
        .map_err(|_| "encrypted: incorrect nonce size")?;
    crate::secretbox::open(data.ciphertext.items(), &nonce, &key)
        .ok_or_else(|| "encrypted: decryption failed".to_string())
}

/// What cosign's generate-key-pair writes of the PKCS #8 `pkcs8` (encrypted.Encrypt under
/// Standard parameters, then the PEM block): `salt` and `nonce`, 32 and 24 random bytes,
/// are the caller's.
pub fn encrypt(
    pkcs8: &[u8],
    password: &[u8],
    salt: &[u8; SALT],
    nonce: &[u8; NONCE],
) -> Result<Vec<u8>, String> {
    let key = scrypt(password, salt, STANDARD)?;
    let sealed = crate::secretbox::seal(pkcs8, nonce, &key);
    let b64 = crate::tlog::gocodec::std_encode;
    // json.Marshal of data: its fields in order, []byte as standard base64.
    let json = format!(
        r#"{{"kdf":{{"name":"scrypt","params":{{"N":{},"r":{},"p":{}}},"salt":"{}"}},"cipher":{{"name":"nacl/secretbox","nonce":"{}"}},"ciphertext":"{}"}}"#,
        STANDARD.0,
        STANDARD.1,
        STANDARD.2,
        b64(salt),
        b64(nonce),
        b64(&sealed)
    );
    Ok(crate::tlog::pem::encode(SIGSTORE_PEM, json.as_bytes()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../testdata/cosign/oracle.json")).unwrap()
    }

    fn hex(s: &str) -> Vec<u8> {
        crate::tlog::gocodec::hex_decode(s.as_bytes()).unwrap()
    }

    /// Each key cosign v3.1.3's LoadPrivateKey was given (scripts/cosign/generate): the
    /// PKCS #8 it decrypted and the hint it names the key by, or its error, word for word.
    #[test]
    fn keys_are_read_as_cosign_reads_them() {
        let o = oracle();
        let keys = o["keys"].as_array().unwrap();
        assert!(keys.len() >= 20);
        for k in keys {
            let name = k["name"].as_str().unwrap();
            let pem = k["pem"].as_str().unwrap().as_bytes();
            let password = k["password"].as_str().unwrap().as_bytes();
            match k["error"].as_str() {
                None => {
                    let der = decrypt(pem, password).unwrap_or_else(|e| panic!("{name}: {e}"));
                    assert_eq!(der, hex(k["pkcs8"].as_str().unwrap()), "{name}");
                    let signer = load(pem, password).unwrap_or_else(|e| panic!("{name}: {e}"));
                    assert_eq!(signer.hint(), k["hint"].as_str().unwrap(), "{name}");
                }
                Some(want) => {
                    let got = load(pem, password).map(|_| ()).unwrap_err();
                    assert_eq!(got, want, "{name}");
                }
            }
        }
    }

    /// What `encrypt` writes, cosign's generate-key-pair's format, reads back.
    #[test]
    fn written_keys_read_back() {
        let o = oracle();
        let k = o["keys"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == "ecdsa p256 standard")
            .unwrap();
        let der = hex(k["pkcs8"].as_str().unwrap());
        let pem = encrypt(&der, b"secret", &[7u8; 32], &[9u8; 24]).unwrap();
        let text = String::from_utf8(pem.clone()).unwrap();
        assert!(
            text.starts_with("-----BEGIN ENCRYPTED SIGSTORE PRIVATE KEY-----\n"),
            "{text}"
        );
        assert!(text.lines().all(|l| l.len() <= 64), "{text}");
        assert_eq!(decrypt(&pem, b"secret").unwrap(), der);
        assert_eq!(load(&pem, b"secret").unwrap().hint(), k["hint"].as_str().unwrap());
        assert_eq!(
            load(&pem, b"other").unwrap_err(),
            "decrypt: encrypted: decryption failed"
        );
    }
}
