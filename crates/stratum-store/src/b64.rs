//! Base64, RFC 4648.
//!
//! Three places needed this before it existed here — SSH key
//! fingerprints, CloudFront policy signatures and SMTP `AUTH PLAIN` —
//! and each had grown its own copy with its own padding rules. One
//! implementation with one set of tests is both smaller and the only way
//! the edge cases (a 1-byte tail, a 2-byte tail, rejected input) get
//! exercised by everything that depends on them.
//!
//! Alphabet substitutions (CloudFront's `-~_`, URL-safe `-_`) are the
//! caller's business: they are a `replace` over the standard output, not
//! a second encoder.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding.
pub fn encode(data: &[u8]) -> String {
    let mut s = encode_nopad(data);
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    s
}

/// Standard base64 with the padding omitted — what OpenSSH prints in a
/// `SHA256:` fingerprint.
pub fn encode_nopad(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

/// Decode standard base64. Padding is optional; anything outside the
/// alphabet is `None` rather than silently skipped, because this parses
/// credentials and a lenient decoder makes two spellings of one key.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0;
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 4648 §10 vectors, which is where every tail length lives.
    #[test]
    fn rfc4648_vectors_round_trip() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), encoded, "encoding {raw:?}");
            assert_eq!(
                encode_nopad(raw.as_bytes()),
                encoded.trim_end_matches('='),
                "unpadded {raw:?}"
            );
            assert_eq!(decode(encoded).unwrap(), raw.as_bytes(), "decoding {raw:?}");
            // Padding is optional on the way back in.
            assert_eq!(
                decode(encoded.trim_end_matches('=')).unwrap(),
                raw.as_bytes()
            );
        }
    }

    /// Both non-alphabet characters and the whole high byte range, so a
    /// key with a stray space or newline in it is refused rather than
    /// quietly decoding to something shorter.
    #[test]
    fn anything_outside_the_alphabet_is_refused() {
        for bad in ["Zg=!", "Zm 8=", "Zm9v\n", "-_", "Zm9vYmFy…"] {
            assert!(decode(bad).is_none(), "{bad:?} decoded");
        }
    }

    /// Every byte value survives the round trip, which the seven short
    /// vectors above do not reach.
    #[test]
    fn every_byte_value_round_trips() {
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
    }
}
