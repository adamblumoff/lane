use sha2::{Digest, Sha256};

use super::output::BytePreview;

const BYTE_PREVIEW_LIMIT: usize = 4096;

pub(super) fn byte_preview(bytes: &[u8]) -> BytePreview {
    BytePreview {
        len: bytes.len(),
        sha256: sha256_hex(bytes),
        utf8: utf8_preview(bytes),
        truncated: bytes.len() > BYTE_PREVIEW_LIMIT,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn utf8_preview(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    if bytes.len() <= BYTE_PREVIEW_LIMIT {
        return Some(text.to_owned());
    }

    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next > BYTE_PREVIEW_LIMIT {
            break;
        }
        end = next;
    }
    Some(text[..end].to_owned())
}
