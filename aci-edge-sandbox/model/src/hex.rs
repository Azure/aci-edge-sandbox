// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Lowercase hexadecimal encoding of SHA-256 digests.

/// Returns `bytes` as lowercase hexadecimal digits.
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

/// Decodes a SHA-256 digest from exactly 64 lowercase hexadecimal digits.
pub fn decode_sha256(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

/// Serde helper that writes a SHA-256 digest as 64 lowercase hexadecimal digits.
///
/// Use it as `#[serde(with = "aci_edge_sandboxes_model::hex::sha256")]`.
pub mod sha256 {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes `digest` as hexadecimal digits.
    pub fn serialize<S: Serializer>(digest: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::encode(digest))
    }

    /// Deserializes 64 lowercase hexadecimal digits.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(deserializer)?;
        super::decode_sha256(&text).ok_or_else(|| {
            serde::de::Error::custom("a SHA-256 digest must be 64 lowercase hexadecimal digits")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_round_trip_and_reject_other_spellings() {
        let digest: [u8; 32] = std::array::from_fn(|index| index as u8 * 7);
        let text = encode(&digest);
        assert_eq!(text.len(), 64);
        assert_eq!(decode_sha256(&text), Some(digest));
        assert_eq!(decode_sha256(&text.to_uppercase()), None);
        assert_eq!(decode_sha256(&text[1..]), None);
        assert_eq!(decode_sha256(&format!("{}g", &text[1..])), None);
    }
}
