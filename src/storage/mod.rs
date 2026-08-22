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
use std::sync::atomic::{AtomicU64, Ordering};

/// Magic bytes identifying a Nexus index file ("NXI\0").
const MAGIC: [u8; 4] = [0x4E, 0x58, 0x49, 0x00];
const HEADER_LEN: u64 = 20;

static INDEX_SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Serializes `index` and writes it to `path`, creating parent directories
/// as needed. Writes to a temporary file first and renames it into place so
/// a crash mid-write cannot corrupt a previously-good index.
pub fn save(index: &Index, path: &Path) -> Result<()> {
    info!("saving index to {}", path.display());
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| NexusError::io(parent, e))?;

    let payload = bincode::serialize(index).map_err(NexusError::Serialize)?;
    let checksum = crc32fast::hash(&payload);
    debug!("payload checksum: {:#010x}", checksum);

    let file_name = path.file_name().ok_or_else(|| {
        NexusError::Other(format!("index path has no file name: {}", path.display()))
    })?;
    let mut attempts = 0u8;
    let (tmp_path, mut file) = loop {
        let suffix = INDEX_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id(),
            suffix
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 32 => {
                attempts += 1;
            }
            Err(error) => return Err(NexusError::io(candidate, error)),
        }
    };
    if let Ok(metadata) = std::fs::metadata(path) {
        if let Err(error) = file.set_permissions(metadata.permissions()) {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(NexusError::io(&tmp_path, error));
        }
    }
    let write_result = (|| {
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
        file.sync_all().map_err(|e| NexusError::io(&tmp_path, e))
    })();
    if let Err(error) = write_result {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    drop(file);
    if let Err(error) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(NexusError::io(path, error));
    }
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
    let payload_len = u64::from_le_bytes(len_bytes);

    let file_len = file.metadata().map_err(|e| NexusError::io(path, e))?.len();
    if file_len < HEADER_LEN {
        return Err(NexusError::CorruptIndex(
            "file shorter than index header".to_string(),
        ));
    }
    let remaining_len = file_len - HEADER_LEN;
    if payload_len != remaining_len || payload_len > usize::MAX as u64 {
        return Err(NexusError::CorruptIndex(format!(
            "declared payload length {payload_len} does not match available payload bytes {remaining_len}",
        )));
    }
    let mut payload = vec![0u8; payload_len as usize];
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

    fn unique_test_dir(label: &str) -> PathBuf {
        let suffix = INDEX_SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("nexus-storage-{label}-{}-{suffix}", std::process::id()))
    }

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
                acl: crate::entity::Acl::public(),
            },
            content: "rust search engine".to_string(),
        };
        index.index_document(doc);

        let dir = unique_test_dir("round-trip");
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
        let dir = unique_test_dir("corrupt");
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

    #[test]
    fn rejects_impossible_declared_payload_length_before_allocating() {
        let mut index = Index::new();
        index.vocabulary.get_or_insert("test");
        let dir = unique_test_dir("declared-length");
        let path = dir.join("index.nxi");
        save(&index, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        assert!(matches!(load(&path), Err(NexusError::CorruptIndex(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_saves_leave_one_complete_index_without_temp_files() {
        let mut first = Index::new();
        first.vocabulary.get_or_insert("first");
        let mut second = Index::new();
        second.vocabulary.get_or_insert("second");
        let dir = unique_test_dir("concurrent-save");
        let path = dir.join("index.nxi");

        std::thread::scope(|scope| {
            let first_path = path.clone();
            scope.spawn(move || save(&first, &first_path).unwrap());
            let second_path = path.clone();
            scope.spawn(move || save(&second, &second_path).unwrap());
        });

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.vocabulary.len(), 1);
        let terms: Vec<_> = loaded.vocabulary.iter().map(|(term, _)| term).collect();
        assert!(terms == vec!["first"] || terms == vec!["second"]);
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("index.nxi")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
