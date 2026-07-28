//! Persistent binary storage for the index.
//!
//! The on-disk format is: a fixed 4-byte magic number, a 4-byte format
//! version, a 4-byte CRC32 checksum of the payload, an 8-byte payload
//! length, and then the bincode-serialized [`Index`] payload itself.
//! Storing the checksum and length up front lets [`load`] detect
//! truncation or corruption before attempting to deserialize, producing a
//! clear error instead of a confusing panic deep inside bincode.

pub mod content_cache;

use crate::error::{NexusError, Result};
use crate::index::{Index, INDEX_FORMAT_VERSION};
use log::{debug, info, warn};
use std::io::{Read, Write};
use std::path::Path;

/// Magic bytes identifying a Nexus index file ("NXI\0").
const MAGIC: [u8; 4] = [0x4E, 0x58, 0x49, 0x00];

/// Serializes `index` and writes it to `path`, creating parent directories
/// as needed. Writes to a temporary file first and renames it into place so
/// a crash mid-write cannot corrupt a previously-good index.
pub fn save(index: &Index, path: &Path) -> Result<()> {
    info!("saving index to {}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;
    }

    let payload = bincode::serialize(index).map_err(NexusError::Serialize)?;
    let checksum = crc32fast::hash(&payload);
    debug!("payload checksum: {:#010x}", checksum);

    let tmp_path = path.with_extension("nxi.tmp");
    {
        let mut file =
            std::fs::File::create(&tmp_path).map_err(|e| NexusError::io(&tmp_path, e))?;
        file.write_all(&MAGIC)
            .map_err(|e| NexusError::io(&tmp_path, e))?;
        file.write_all(&INDEX_FORMAT_VERSION.to_le_bytes())
            .map_err(|e| NexusError::io(&tmp_path, e))?;
        file.write_all(&checksum.to_le_bytes())
            .map_err(|e| NexusError::io(&tmp_path, e))?;
        file.write_all(&(payload.len() as u64).to_le_bytes())
            .map_err(|e| NexusError::io(&tmp_path, e))?;
        file.write_all(&payload)
            .map_err(|e| NexusError::io(&tmp_path, e))?;
        file.sync_all().map_err(|e| NexusError::io(&tmp_path, e))?;
    }
    std::fs::rename(&tmp_path, path).map_err(|e| NexusError::io(path, e))?;
    info!("index saved to {}", path.display());
    Ok(())
}

/// Loads and validates an index from `path`, returning
/// [`NexusError::CorruptIndex`] if the magic number, version, checksum, or
/// declared length do not match what is actually on disk.
pub fn load(path: &Path) -> Result<Index> {
    info!("loading index from {}", path.display());
    let mut file = std::fs::File::open(path).map_err(|e| NexusError::io(path, e))?;

    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|e| NexusError::io(path, e))?;
    if magic != MAGIC {
        return Err(NexusError::CorruptIndex(
            "invalid magic number: not a Nexus index file".to_string(),
        ));
    }

    let mut version_bytes = [0u8; 4];
    file.read_exact(&mut version_bytes)
        .map_err(|e| NexusError::io(path, e))?;
    let version = u32::from_le_bytes(version_bytes);
    if version != INDEX_FORMAT_VERSION {
        return Err(NexusError::CorruptIndex(format!(
            "index format version {} is not supported (expected {})",
            version, INDEX_FORMAT_VERSION
        )));
    }

    let mut checksum_bytes = [0u8; 4];
    file.read_exact(&mut checksum_bytes)
        .map_err(|e| NexusError::io(path, e))?;
    let expected_checksum = u32::from_le_bytes(checksum_bytes);
    debug!("expected checksum: {:#010x}", expected_checksum);

    let mut len_bytes = [0u8; 8];
    file.read_exact(&mut len_bytes)
        .map_err(|e| NexusError::io(path, e))?;
    let payload_len = u64::from_le_bytes(len_bytes) as usize;

    let mut payload = vec![0u8; payload_len];
    file.read_exact(&mut payload).map_err(|_| {
        NexusError::CorruptIndex("payload shorter than declared length".to_string())
    })?;

    let actual_checksum = crc32fast::hash(&payload);
    debug!("actual checksum: {:#010x}", actual_checksum);
    if actual_checksum != expected_checksum {
        return Err(NexusError::CorruptIndex(format!(
            "checksum mismatch: expected {:#010x}, got {:#010x}",
            expected_checksum, actual_checksum
        )));
    }

    bincode::deserialize(&payload).map_err(NexusError::Deserialize)
}

/// Returns `true` if an index file already exists at `path`.
pub fn exists(path: &Path) -> bool {
    let exists = path.exists();
    if !exists {
        warn!("index file does not exist: {}", path.display());
    }
    exists
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Document, DocumentMetadata};
    use std::path::PathBuf;

    #[test]
    fn round_trips_an_index() {
        let mut index = Index::new();
        let doc = Document {
            metadata: DocumentMetadata {
                path: PathBuf::from("/tmp/example.txt"),
                file_name: "example.txt".to_string(),
                extension: "txt".to_string(),
                size_bytes: 42,
                modified_unix: 0,
                token_count: 0,
            },
            content: "rust search engine".to_string(),
        };
        index.index_document(doc);

        let dir = std::env::temp_dir().join(format!("nexus-test-{}", std::process::id()));
        let path = dir.join("index.nxi");
        save(&index, &path).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.document_count(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_corruption() {
        let mut index = Index::new();
        index.vocabulary.get_or_insert("test");
        let dir = std::env::temp_dir().join(format!("nexus-test-corrupt-{}", std::process::id()));
        let path = dir.join("index.nxi");
        save(&index, &path).unwrap();

        // Flip a byte in the middle of the file to corrupt the payload.
        let mut bytes = std::fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let result = load(&path);
        assert!(matches!(result, Err(NexusError::CorruptIndex(_))));
        std::fs::remove_dir_all(&dir).ok();
    }
}
