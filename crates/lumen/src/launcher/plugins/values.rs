//! Value generator: guid, md5/sha1/sha256/sha384/sha512 <text>, base64 / base64d <text>,
//! url / urld <text>.

use windows::core::{w, PCWSTR};
use windows::Win32::Security::Cryptography::*;

use crate::launcher::item::{Action, Item, Kind};

pub fn query(q: &str, out: &mut Vec<Item>) {
    let q = q.trim_start();
    let (cmd, arg) = q.split_once(' ').map(|(c, a)| (c, a)).unwrap_or((q, ""));
    let cmd = cmd.to_ascii_lowercase();
    let mut push = |title: String, what: &str| {
        out.push(Item::new(Kind::Values, title.clone(), format!("{what}  ·  Enter to copy"), Action::Copy(title)).key(format!("values:{what}")).score(4000));
    };
    match cmd.as_str() {
        "guid" | "uuid" => {
            let g = windows::core::GUID::new().map(|g| format!("{g:?}").to_lowercase()).unwrap_or_default();
            push(g.clone(), "GUID");
            push(g.to_uppercase(), "GUID (upper case)");
        }
        "md5" | "sha1" | "sha256" | "sha384" | "sha512" if !arg.is_empty() => {
            if let Some(h) = hash(&cmd, arg.as_bytes()) {
                push(h, &format!("{} of \"{arg}\"", cmd.to_uppercase()));
            }
        }
        "base64" if !arg.is_empty() => push(base64(arg.as_bytes()), "Base64 encoded"),
        "base64d" if !arg.is_empty() => {
            if let Some(d) = unbase64(arg).and_then(|b| String::from_utf8(b).ok()) {
                push(d, "Base64 decoded");
            }
        }
        "url" if !arg.is_empty() => push(url_encode(arg), "URL encoded"),
        "urld" if !arg.is_empty() => push(url_decode(arg), "URL decoded"),
        _ => {
            for (c, d) in [
                ("guid", "New GUID"),
                ("sha256", "SHA-256 hash of text"),
                ("md5", "MD5 hash of text"),
                ("base64", "Base64 encode text"),
                ("base64d", "Base64 decode text"),
                ("url", "URL encode text"),
            ] {
                if c.starts_with(cmd.as_str()) {
                    out.push(Item::new(Kind::Values, format!("# {c}"), d, Action::Copy(String::new())).key(format!("values:hint:{c}")).score(100));
                }
            }
        }
    }
}

fn hash(algo: &str, data: &[u8]) -> Option<String> {
    let id: PCWSTR = match algo {
        "md5" => w!("MD5"),
        "sha1" => w!("SHA1"),
        "sha256" => w!("SHA256"),
        "sha384" => w!("SHA384"),
        _ => w!("SHA512"),
    };
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        BCryptOpenAlgorithmProvider(&mut alg, id, None, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0)).ok().ok()?;
        let mut len_buf = [0u8; 4];
        let mut got = 0u32;
        let r = BCryptGetProperty(alg, BCRYPT_HASH_LENGTH, Some(&mut len_buf), &mut got, 0);
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut digest = vec![0u8; len];
        let ok = r.is_ok() && BCryptHash(alg, None, data, &mut digest).is_ok();
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        ok.then(|| digest.iter().map(|b| format!("{b:02x}")).collect())
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.trim().bytes().filter(|&c| c != b'=') {
        let v = B64.iter().position(|&x| x == c).or(match c {
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        })? as u32;
        bits = bits << 6 | v;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    Some(out)
}

pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 3 <= b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(unbase64("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
        assert_eq!(url_decode("a%20b%26c"), "a b&c");
        assert_eq!(hash("sha256", b"abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash("md5", b"abc").unwrap(), "900150983cd24fb0d6963f7d28e17f72");
    }
}
