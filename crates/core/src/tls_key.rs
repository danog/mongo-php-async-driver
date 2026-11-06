//! The private key of tlsCertificateKeyFile, as OpenSSL's PEM reader finds it
//! (SSL_CTX_use_PrivateKey_file with tlsCertificateKeyFilePassword as the passphrase):
//! PKCS#8 ("PRIVATE KEY"), encrypted PKCS#8 ("ENCRYPTED PRIVATE KEY", PBES2), PKCS#1
//! ("RSA PRIVATE KEY") and SEC1 ("EC PRIVATE KEY"), the last two also with OpenSSL's legacy
//! encryption headers (Proc-Type: 4,ENCRYPTED; DEK-Info: AES-128/192/256-CBC or DES-EDE3-CBC).

use rustls::pki_types::{PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer};

/// The first private key of `pem` (None: no key block), decrypted with `password` if needed.
pub fn private_key(pem: &[u8], password: Option<&str>) -> Result<Option<PrivateKeyDer<'static>>, &'static str> {
    let text = std::str::from_utf8(pem).map_err(|_| "not PEM")?;
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN ") {
        let after = &rest[start + 11..];
        let Some(label_end) = after.find("-----") else { break };
        let label = &after[..label_end];
        let body_start = &after[label_end + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(end) = body_start.find(&end_marker) else { break };
        let body = &body_start[..end];
        rest = &body_start[end + end_marker.len()..];
        match label {
            "PRIVATE KEY" => return Ok(Some(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(b64(body)?)))),
            "ENCRYPTED PRIVATE KEY" => {
                let pw = password.ok_or("encrypted key without password")?;
                let der = b64(body)?;
                let info = pkcs8::EncryptedPrivateKeyInfo::try_from(der.as_slice()).map_err(|_| "bad encrypted key")?;
                let doc = info.decrypt(pw.as_bytes()).map_err(|_| "bad password")?;
                return Ok(Some(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(doc.as_bytes().to_vec()))));
            }
            "RSA PRIVATE KEY" | "EC PRIVATE KEY" => {
                let der = legacy(body, password)?;
                return Ok(Some(if label == "RSA PRIVATE KEY" {
                    PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(der))
                } else {
                    PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(der))
                }));
            }
            _ => {}
        }
    }
    Ok(None)
}

fn b64(body: &str) -> Result<Vec<u8>, &'static str> {
    let s: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    crate::auth::scram::b64_decode(s.as_bytes(), s.len() / 4 * 3 + 3).ok_or("bad base64")
}

/// A PKCS#1/SEC1 body, decrypted when it carries OpenSSL's legacy encryption headers.
fn legacy(body: &str, password: Option<&str>) -> Result<Vec<u8>, &'static str> {
    let mut dek = None;
    let mut encrypted = false;
    let mut data = String::new();
    for line in body.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("Proc-Type:") {
            encrypted = v.trim() == "4,ENCRYPTED";
        } else if let Some(v) = l.strip_prefix("DEK-Info:") {
            dek = Some(v.trim().to_owned());
        } else {
            data.push_str(l);
        }
    }
    let der = b64(&data)?;
    if !encrypted {
        return Ok(der);
    }
    let pw = password.ok_or("encrypted key without password")?;
    let dek = dek.ok_or("no DEK-Info")?;
    let (alg, iv_hex) = dek.split_once(',').ok_or("bad DEK-Info")?;
    let iv = hex(iv_hex.trim())?;
    let (key_len, block) = match alg {
        "AES-128-CBC" => (16, 16),
        "AES-192-CBC" => (24, 16),
        "AES-256-CBC" => (32, 16),
        "DES-EDE3-CBC" => (24, 8),
        _ => return Err("unsupported DEK-Info cipher"),
    };
    if iv.len() != block || der.is_empty() || der.len() % block != 0 {
        return Err("bad encrypted key");
    }
    let key = evp_bytes_to_key(pw.as_bytes(), &iv[..8], key_len);
    let mut buf = der;
    use cbc::cipher::{BlockDecryptMut, KeyIvInit};
    let pt_len = match key_len {
        16 if block == 16 => cbc::Decryptor::<aes::Aes128>::new_from_slices(&key, &iv).map_err(|_| "key")?.decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buf).map(|p| p.len()),
        24 if block == 16 => cbc::Decryptor::<aes::Aes192>::new_from_slices(&key, &iv).map_err(|_| "key")?.decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buf).map(|p| p.len()),
        32 => cbc::Decryptor::<aes::Aes256>::new_from_slices(&key, &iv).map_err(|_| "key")?.decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buf).map(|p| p.len()),
        _ => cbc::Decryptor::<des::TdesEde3>::new_from_slices(&key, &iv).map_err(|_| "key")?.decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buf).map(|p| p.len()),
    }
    .map_err(|_| "bad password")?;
    buf.truncate(pt_len);
    Ok(buf)
}

/// EVP_BytesToKey(cipher, MD5, salt, password, count 1): D_i = MD5(D_{i-1} || password || salt).
fn evp_bytes_to_key(password: &[u8], salt: &[u8], len: usize) -> Vec<u8> {
    use md5::Digest;
    let mut out = Vec::with_capacity(len + 16);
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < len {
        let mut h = md5::Md5::new();
        h.update(&prev);
        h.update(password);
        h.update(salt);
        prev = h.finalize().to_vec();
        out.extend_from_slice(&prev);
    }
    out.truncate(len);
    out
}

fn hex(s: &str) -> Result<Vec<u8>, &'static str> {
    if s.len() % 2 != 0 {
        return Err("bad hex");
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "bad hex")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn evp_bytes_to_key_matches_openssl() {
        // openssl enc -aes-128-cbc -k password -S 0102030405060708 -P -md md5
        // key=E7B0971E52CA5CC8D0539FB3412F6316 iv=F7BA2E6EE293D9F3457B99436B51CE02 (the IV is the
        // next 16 bytes of the same derivation)
        let k = super::evp_bytes_to_key(b"password", &[1, 2, 3, 4, 5, 6, 7, 8], 32);
        assert_eq!(k, super::hex("E7B0971E52CA5CC8D0539FB3412F6316F7BA2E6EE293D9F3457B99436B51CE02").unwrap());
        assert_eq!(super::hex("0a0B").unwrap(), vec![10, 11]);
    }
}
