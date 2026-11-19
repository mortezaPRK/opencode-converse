//! Decoder-only reader for Moonshine's flat `tokenizer.bin`.
//!
//! Entries are length-prefixed byte strings (one byte below 128, otherwise
//! `second * 128 + first - 128` with the next byte as `second`; a zero first byte is an
//! empty entry). The entry index is the token id. Word boundaries are U+2581.
use anyhow::{ensure, Result};
use std::path::Path;

pub struct Tokenizer {
    tokens: Vec<Vec<u8>>,
}

impl Tokenizer {
    pub fn from_file(path: &Path, vocab_size: usize) -> Result<Self> {
        Self::parse(&std::fs::read(path)?, vocab_size)
    }

    pub fn parse(data: &[u8], vocab_size: usize) -> Result<Self> {
        ensure!(
            !data.starts_with(&[0xFF, 0xFF, b'B', b'T', b'O', b'K']),
            "Headered Moonshine tokenizers are not supported"
        );
        let mut tokens = Vec::with_capacity(vocab_size);
        let mut at = 0;
        while at < data.len() {
            let first = data[at] as usize;
            at += 1;
            let len = match first {
                0 => 0,
                1..=127 => first,
                _ => {
                    ensure!(at < data.len(), "Truncated Moonshine tokenizer");
                    let second = data[at] as usize;
                    at += 1;
                    second * 128 + first - 128
                }
            };
            ensure!(
                len <= data.len() - at,
                "Truncated Moonshine tokenizer entry"
            );
            tokens.push(data[at..at + len].to_vec());
            at += len;
        }
        ensure!(
            tokens.len() == vocab_size,
            "Moonshine tokenizer has {} entries, expected {vocab_size}",
            tokens.len()
        );
        Ok(Self { tokens })
    }

    /// Concatenate pieces, expanding `<0xNN>` byte-fallback pieces, dropping other
    /// `<special>` pieces, and mapping U+2581 to a space.
    pub fn decode(&self, ids: &[i64]) -> String {
        let mut bytes = Vec::new();
        for piece in ids
            .iter()
            .filter_map(|id| self.tokens.get(usize::try_from(*id).ok()?))
        {
            if piece.len() == 6 && piece.starts_with(b"<0x") && piece.ends_with(b">") {
                if let Some(byte) = std::str::from_utf8(&piece[3..5])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    bytes.push(byte);
                    continue;
                }
            }
            if piece.len() > 2 && piece[0] == b'<' && piece[piece.len() - 1] == b'>' {
                continue;
            }
            bytes.extend_from_slice(piece);
        }
        String::from_utf8_lossy(&bytes)
            .replace('\u{2581}', " ")
            .trim()
            .to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(entries: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for entry in entries {
            if entry.is_empty() {
                out.push(0);
            } else if entry.len() < 128 {
                out.push(entry.len() as u8);
            } else {
                out.push(128 + (entry.len() % 128) as u8);
                out.push((entry.len() / 128) as u8);
            }
            out.extend_from_slice(entry);
        }
        out
    }

    #[test]
    fn decodes_pieces_bytes_and_specials() {
        let long = vec![b'a'; 300];
        let data = encode(&[
            b"<s>",
            "\u{2581}open".as_bytes(),
            b"<0x41>",
            "\u{2581}src".as_bytes(),
            b"",
            &long,
        ]);
        let tok = Tokenizer::parse(&data, 6).unwrap();
        assert_eq!(tok.decode(&[0, 1, 2, 3, 4]), "openA src");
        assert_eq!(tok.tokens[5].len(), 300);
        assert_eq!(tok.decode(&[99, -1]), "");
        assert!(Tokenizer::parse(&data, 7).is_err());
        assert!(Tokenizer::parse(&data[..data.len() - 1], 6).is_err());
    }
}
