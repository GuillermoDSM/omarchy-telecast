//! HAP TLV8 and authenticated record framing. These primitives do not perform pairing.
use anyhow::{Result, ensure};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use std::collections::BTreeMap;

/// Values longer than 255 bytes are fragmented as required by TLV8.
pub fn encode_tlv(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(tag, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[tag, 0]);
        } else {
            for chunk in value.chunks(255) {
                out.extend_from_slice(&[tag, chunk.len() as u8]);
                out.extend_from_slice(chunk);
            }
        }
    }
    out
}

/// Reject malformed lengths and discontiguous repeated tags, avoiding ambiguous messages.
pub fn decode_tlv(data: &[u8]) -> Result<BTreeMap<u8, Vec<u8>>> {
    let mut out: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    let mut offset = 0;
    let mut previous = None;
    while offset < data.len() {
        ensure!(data.len() - offset >= 2, "truncated TLV header");
        let tag = data[offset];
        let len = data[offset + 1] as usize;
        offset += 2;
        ensure!(len <= data.len() - offset, "truncated TLV value");
        ensure!(
            !out.contains_key(&tag) || previous == Some((tag, 255)),
            "ambiguous repeated TLV tag"
        );
        out.entry(tag)
            .or_default()
            .extend_from_slice(&data[offset..offset + len]);
        offset += len;
        previous = Some((tag, len));
    }
    Ok(out)
}

/// Independent read and write counters must be maintained for the two channel directions.
pub struct RecordCipher {
    cipher: ChaCha20Poly1305,
    counter: u64,
}
impl RecordCipher {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new((&key).into()),
            counter: 0,
        }
    }
    fn nonce(&mut self) -> Result<[u8; 12]> {
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("HAP nonce exhausted"))?;
        Ok(nonce)
    }
    /// HAP records carry at most 1024 plaintext bytes.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        ensure!(plaintext.len() <= 1024, "HAP record too large");
        let length = (plaintext.len() as u16).to_le_bytes();
        let nonce = self.nonce()?;
        let encrypted = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: &length,
                },
            )
            .map_err(|_| anyhow::anyhow!("HAP encryption failed"))?;
        let mut result = length.to_vec();
        result.extend_from_slice(&encrypted);
        Ok(result)
    }
    /// A failed authentication consumes the nonce: close the connection on error.
    pub fn open(&mut self, record: &[u8]) -> Result<Vec<u8>> {
        ensure!(record.len() >= 18, "short HAP record");
        let len = u16::from_le_bytes([record[0], record[1]]) as usize;
        ensure!(
            len <= 1024 && record.len() == len + 18,
            "invalid HAP record length"
        );
        let nonce = self.nonce()?;
        self.cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &record[2..],
                    aad: &record[..2],
                },
            )
            .map_err(|_| anyhow::anyhow!("HAP authentication failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragments_roundtrip() {
        let value = vec![7; 600];
        let data = encode_tlv(&[(3, &value)]);
        assert_eq!(decode_tlv(&data).unwrap()[&3], value);
        assert!(decode_tlv(&[1, 2, 3]).is_err());
    }
    #[test]
    fn authenticated_records() {
        let key = [9; 32];
        let mut a = RecordCipher::new(key);
        let mut b = RecordCipher::new(key);
        let record = a.seal(b"hello").unwrap();
        assert_eq!(b.open(&record).unwrap(), b"hello");
        let mut record = a.seal(b"world").unwrap();
        record[3] ^= 1;
        assert!(b.open(&record).is_err());
    }
}
