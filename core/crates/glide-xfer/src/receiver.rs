use crate::engine::{valid_session_name, valid_spool_name, validate_manifest};
use crate::filesystem;
use crate::paging::PagedManifest;
use crate::{Cancel, ChunkSizeBounds, Config, Error, Progress, Received, Result};
use glide_proto::codec::FileChunkView;
use glide_proto::wire::*;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const RECORD_BYTES: u64 = 33;

#[derive(Serialize, Deserialize)]
struct Persisted {
    peer_id: String,
    manifest: FileManifest,
    chunk_size: usize,
    #[serde(default)]
    paged: Option<PagedMetadata>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct PagedMetadata {
    digest: [u8; 32],
    items: u32,
    pages: u32,
    bytes: u64,
    journal_bytes: u64,
    spool_bytes: u64,
}

impl PagedMetadata {
    fn from_manifest(manifest: &PagedManifest) -> Self {
        let summary = manifest.summary();
        Self {
            digest: manifest.digest(),
            items: summary.items,
            pages: summary.pages,
            bytes: summary.bytes,
            journal_bytes: manifest.journal_bytes(),
            spool_bytes: manifest.spool_bytes(),
        }
    }

    fn storage_bytes(&self, config: &Config) -> Result<u64> {
        if self.items == 0
            || self.items > MAX_TRANSFER_ITEMS
            || self.pages == 0
            || self.pages > self.items
            || self.bytes > config.max_transfer_bytes
            || self.journal_bytes
                > u64::from(self.items) * u64::from(config.max_chunks_per_file) * RECORD_BYTES
        {
            return Err(Error::Limit("stored paged manifest"));
        }
        self.bytes
            .checked_add(self.journal_bytes)
            .and_then(|size| size.checked_add(self.spool_bytes))
            .and_then(|size| size.checked_add(MAX_FILE_CONTROL_FRAME_BYTES as u64))
            .ok_or(Error::Limit("staging size overflow"))
    }
}

pub(crate) struct Session {
    pub(crate) manifest: FileManifest,
    pages: Option<PagedManifest>,
    config: Config,
    root: PathBuf,
    lease: Option<File>,
    cache: Option<(usize, File, File)>,
    raw: Vec<u8>,
    decompressor: zstd::bulk::Decompressor<'static>,
    retain: bool,
    coalesce_resume: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cache.take();
        self.lease.take();
        if !self.retain {
            // RAII also handles task/future abandonment. Removal never follows directory links.
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn load_metadata(path: &std::path::Path) -> Result<Persisted> {
    let mut file = filesystem::open_file(path, false, false)?;
    if file.metadata()?.len() > MAX_FILE_CONTROL_FRAME_BYTES as u64 {
        return Err(Error::Limit("staging metadata"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("staging metadata"))
}

fn storage_bytes(manifest: &FileManifest, config: &Config) -> Result<u64> {
    let mut size = validate_manifest(manifest, config)?;
    for entry in manifest.files.iter() {
        size = size
            .checked_add(u64::from(config.chunks(entry.size)?) * RECORD_BYTES)
            .ok_or(Error::Limit("staging size overflow"))?;
    }
    // Reserve metadata and filesystem bookkeeping without trusting peer estimates.
    size.checked_add(MAX_FILE_CONTROL_FRAME_BYTES as u64)
        .ok_or(Error::Limit("staging size"))
}

pub(crate) fn read_spool_reservation(root: &std::path::Path) -> Result<u64> {
    #[cfg(windows)]
    let mut file = {
        use std::os::windows::fs::OpenOptionsExt;
        filesystem::check_chain(root)?;
        // Reservation writers stay open under a live lease. A reader must share
        // write access with that existing handle; final opens still never follow links.
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(0x0020_0000)
            .open(root.join("reservation"))?;
        if !filesystem::regular(&file.metadata()?) {
            return Err(Error::Invalid("spool reservation type"));
        }
        file
    };
    #[cfg(not(windows))]
    let mut file = filesystem::open_file(&root.join("reservation"), false, false)?;
    if file.metadata()?.len() != 8 {
        return Err(Error::Invalid("spool reservation"));
    }
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/// Every copy attempt leaves a staging folder behind (kept a day so interrupted copies can resume), and the engine
/// refuses new copies once `max_staging_sessions` folders exist. Before refusing, clear the unused ones that have been
/// idle for a quarter of an hour; a folder in use by a transfer or the clipboard holds its lease and stays.
fn make_room(staging: &std::path::Path, config: &Config) -> Result<()> {
    let mut count = 0usize;
    for entry in fs::read_dir(staging)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| valid_session_name(name) || valid_spool_name(name))
        {
            count += 1;
        }
    }
    if count >= config.max_staging_sessions {
        crate::engine::purge_dir(staging, Duration::from_secs(15 * 60))?;
    }
    Ok(())
}

/// Includes pending approval and interrupted manifest/resume spools in the same
/// staging quota as private/published jobs. The caller serializes reservations.
pub(crate) fn check_spool_quota(
    parent: &std::path::Path,
    own: Option<&std::path::Path>,
    required: u64,
    config: &Config,
) -> Result<()> {
    make_room(parent, config)?;
    let mut count = 0usize;
    let mut bytes = 0u64;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let path = entry.path();
        if own == Some(path.as_path()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let stored = if valid_session_name(name) {
            let saved = load_metadata(&path.join("manifest.json"))?;
            if saved.chunk_size == 0
                || saved.chunk_size > MAX_FILE_CHUNK_BYTES
                || saved.manifest.chunk_size as usize != saved.chunk_size
            {
                return Err(Error::Invalid("stored chunk configuration"));
            }
            let mut stored_config = config.clone();
            stored_config.chunk_size = saved.chunk_size;
            match saved.paged {
                Some(metadata) => metadata.storage_bytes(&stored_config)?,
                None => storage_bytes(&saved.manifest, &stored_config)?,
            }
        } else if valid_spool_name(name) {
            filesystem::check_chain(&path)?;
            read_spool_reservation(&path)?
        } else {
            continue;
        };
        count += 1;
        bytes = bytes
            .checked_add(stored)
            .ok_or(Error::Limit("staging byte quota"))?;
    }
    if count >= config.max_staging_sessions {
        return Err(Error::Limit("staging session count"));
    }
    if bytes
        .checked_add(required)
        .is_none_or(|bytes| bytes > config.max_staging_bytes)
    {
        return Err(Error::Limit("staging byte quota"));
    }
    Ok(())
}

impl Session {
    #[cfg(test)]
    pub(crate) fn open(
        staging: PathBuf,
        peer_id: String,
        manifest: FileManifest,
        config: Config,
        cancel: &Cancel,
    ) -> Result<Self> {
        Self::open_with_options(
            staging, peer_id, manifest, config, cancel, None, None, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_with_options(
        staging: PathBuf,
        peer_id: String,
        manifest: FileManifest,
        config: Config,
        cancel: &Cancel,
        pages: Option<PagedManifest>,
        chunk_bounds: Option<ChunkSizeBounds>,
        coalesce_resume: bool,
    ) -> Result<Self> {
        cancel.check()?;
        filesystem::check_chain(&staging)?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"glide-staging-v1\0");
        hash.update(peer_id.as_bytes());
        hash.update(b"\0");
        hash.update(manifest.transfer_id.as_bytes());
        let root = staging.join(hash.finalize().to_hex().as_str());
        let exists = root.try_exists()?;
        let paged_metadata = pages.as_ref().map(PagedMetadata::from_manifest);
        let required = match &paged_metadata {
            Some(metadata) => metadata.storage_bytes(&config)?,
            None => storage_bytes(&manifest, &config)?,
        };
        if !exists {
            make_room(&staging, &config)?;
            let mut count = 0;
            let mut stored = 0u64;
            for entry in fs::read_dir(&staging)? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if valid_spool_name(name) {
                    // This manifest's spool is already included in `required`.
                    if pages
                        .as_ref()
                        .is_some_and(|pages| pages.spool_root() == entry.path())
                    {
                        continue;
                    }
                    count += 1;
                    if count >= config.max_staging_sessions {
                        return Err(Error::Limit("staging session count"));
                    }
                    stored = stored
                        .checked_add(read_spool_reservation(&entry.path())?)
                        .ok_or(Error::Limit("staging quota"))?;
                    continue;
                }
                if !valid_session_name(name) {
                    continue;
                }
                count += 1;
                if count >= config.max_staging_sessions {
                    return Err(Error::Limit("staging session count"));
                }
                filesystem::check_chain(&entry.path())?;
                let saved = load_metadata(&entry.path().join("manifest.json"))?;
                // A different configuration cannot silently reinterpret journals on disk.
                if saved.chunk_size != saved.manifest.chunk_size as usize {
                    return Err(Error::Invalid("stored manifest chunk size"));
                }
                if let Some(bounds) = chunk_bounds {
                    bounds.accepts(saved.chunk_size)?;
                } else if saved.chunk_size != config.chunk_size {
                    return Err(Error::Invalid("stored chunk configuration"));
                }
                let mut saved_config = config.clone();
                saved_config.chunk_size = saved.chunk_size;
                let reservation = match saved.paged {
                    Some(metadata) => metadata.storage_bytes(&saved_config)?,
                    None => storage_bytes(&saved.manifest, &saved_config)?,
                };
                stored = stored
                    .checked_add(reservation)
                    .ok_or(Error::Limit("staging quota"))?;
            }
            if stored
                .checked_add(required)
                .is_none_or(|bytes| bytes > config.max_staging_bytes)
            {
                return Err(Error::Limit("staging byte quota"));
            }
        }
        if filesystem::disk_available(&staging)?
            < required.saturating_add(config.disk_reserve_bytes)
        {
            return Err(Error::DiskSpace);
        }
        if !exists {
            filesystem::private_dir(&root)?;
        }
        filesystem::check_chain(&root)?;
        let lease = filesystem::lease(&root.join("lease"))?;
        lease.set_modified(SystemTime::now())?;
        let mut decompressor = zstd::bulk::Decompressor::new()?;
        decompressor.window_log_max(22)?;
        let mut session = Self {
            manifest,
            pages,
            config,
            root,
            lease: Some(lease),
            cache: None,
            raw: Vec::new(),
            decompressor,
            retain: false,
            coalesce_resume,
        };
        if exists {
            if session.root.join("ready").try_exists()? {
                session.retain = true;
                return Err(Error::Invalid("completed transfer replay"));
            }
            let saved = load_metadata(&session.root.join("manifest.json"))?;
            if saved.peer_id != peer_id
                || saved.manifest != session.manifest
                || saved.chunk_size != session.config.chunk_size
                || saved.paged != paged_metadata
            {
                // Preserve the original job; a conflicting manifest may not erase it.
                session.retain = true;
                return Err(Error::Invalid("resume manifest mismatch"));
            }
            filesystem::check_chain(&session.root.join("incoming"))?;
            filesystem::check_chain(&session.root.join("parts"))?;
        } else {
            let saved = Persisted {
                peer_id,
                manifest: session.manifest.clone(),
                chunk_size: session.config.chunk_size,
                paged: paged_metadata,
            };
            let bytes =
                serde_json::to_vec(&saved).map_err(|_| Error::Invalid("metadata encode"))?;
            if bytes.len() > MAX_FILE_CONTROL_FRAME_BYTES {
                return Err(Error::Limit("staging metadata"));
            }
            let mut file = filesystem::open_file(&session.root.join("manifest.json"), true, true)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            filesystem::private_dir(&session.root.join("parts"))?;
            filesystem::private_dir(&session.root.join("incoming"))?;
            for index in 0..session.entry_count() {
                cancel.check()?;
                let entry = session.entry(index)?;
                let path = session.root.join("incoming").join(&entry.relative_path);
                if entry.is_dir {
                    filesystem::private_dir(&path)?;
                } else {
                    // create_new catches aliases on the actual filesystem (including Unicode
                    // normalization on macOS), beyond our conservative case-collision check.
                    filesystem::open_file(&path, true, true)?;
                    filesystem::open_file(&session.part(entry.file_id, "part"), true, true)?;
                    let journal =
                        filesystem::open_file(&session.part(entry.file_id, "chunks"), true, true)?;
                    journal
                        .set_len(u64::from(session.config.chunks(entry.size)?) * RECORD_BYTES)?;
                }
            }
        }
        session.raw.resize(session.config.chunk_size, 0);
        Ok(session)
    }

    fn part(&self, file_id: u32, suffix: &str) -> PathBuf {
        self.root.join("parts").join(format!("{file_id}.{suffix}"))
    }

    pub(crate) fn entry_count(&self) -> usize {
        self.pages
            .as_ref()
            .map_or(self.manifest.files.len(), |pages| {
                pages.summary().items as usize
            })
    }

    pub(crate) fn entry(&self, index: usize) -> Result<FileManifestEntry> {
        if index >= self.entry_count() {
            return Err(Error::Invalid("file id"));
        }
        if let Some(pages) = &self.pages {
            pages.entry(index as u32)
        } else {
            self.manifest
                .files
                .get(index)
                .cloned()
                .ok_or(Error::Invalid("file id"))
        }
    }

    pub(crate) fn chunk_size(&self) -> usize {
        self.config.chunk_size
    }

    fn geometry(&self, index: usize) -> Result<(bool, u64)> {
        if self.pages.is_some() {
            let entry = self.entry(index)?;
            Ok((entry.is_dir, entry.size))
        } else {
            let entry = self
                .manifest
                .files
                .get(index)
                .ok_or(Error::Invalid("file id"))?;
            Ok((entry.is_dir, entry.size))
        }
    }

    fn open_cached(&mut self, index: usize) -> Result<&mut (usize, File, File)> {
        if self
            .cache
            .as_ref()
            .is_none_or(|(current, _, _)| *current != index)
        {
            self.cache.take();
            let file = self.entry(index)?;
            if file.is_dir {
                return Err(Error::Invalid("chunk addressed to directory"));
            }
            let part = filesystem::open_file(&self.part(file.file_id, "part"), true, false)?;
            let journal = filesystem::open_file(&self.part(file.file_id, "chunks"), true, false)?;
            if journal.metadata()?.len() != u64::from(self.config.chunks(file.size)?) * RECORD_BYTES
            {
                return Err(Error::Invalid("journal length"));
            }
            self.cache = Some((index, part, journal));
        }
        self.cache.as_mut().ok_or(Error::Invalid("file cache"))
    }

    /// Rehash each on-disk chunk. A journal bit alone is never evidence of integrity.
    pub(crate) fn resume(
        &mut self,
        index: usize,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<FileResume> {
        let entry = self.entry(index)?;
        let size = entry.size;
        let file_id = entry.file_id;
        let count = self.config.chunks(size)?;
        let chunk_size = self.config.chunk_size;
        let coalesce = self.coalesce_resume;
        let mut ranges = ResumeRanges::new();
        let mut missing: Option<ChunkRange> = None;
        let mut present_bytes = 0u64;
        let mut fragmented = false;
        // Take the reusable buffer out so journal/file borrows remain disjoint.
        let mut raw = std::mem::take(&mut self.raw);
        let result = (|| {
            let (_, part, journal) = self.open_cached(index)?;
            let mut record = [0u8; RECORD_BYTES as usize];
            for chunk in 0..count {
                cancel.check()?;
                let length =
                    (size - u64::from(chunk) * chunk_size as u64).min(chunk_size as u64) as usize;
                journal.seek(SeekFrom::Start(u64::from(chunk) * RECORD_BYTES))?;
                journal.read_exact(&mut record)?;
                let verified = if record[0] == 1 {
                    part.seek(SeekFrom::Start(u64::from(chunk) * chunk_size as u64))?;
                    part.read_exact(&mut raw[..length]).is_ok()
                        && blake3::hash(&raw[..length]).as_bytes() == &record[1..]
                } else {
                    false
                };
                if verified {
                    present_bytes += length as u64;
                    if let Some(range) = missing.take() {
                        if ranges.push(range).is_err() {
                            fragmented = true;
                            if coalesce {
                                let tail = ranges.last_mut().ok_or(Error::Worker)?;
                                tail.count = range.first_chunk + range.count - tail.first_chunk;
                            }
                        }
                    }
                } else {
                    // Invalid, short or changed data is explicitly made unverified.
                    journal.seek(SeekFrom::Start(u64::from(chunk) * RECORD_BYTES))?;
                    journal.write_all(&[0; RECORD_BYTES as usize])?;
                    if let Some(range) = missing.as_mut() {
                        range.count += 1;
                    } else {
                        missing = Some(ChunkRange {
                            first_chunk: chunk,
                            count: 1,
                        });
                    }
                }
            }
            if let Some(range) = missing {
                if ranges.push(range).is_err() {
                    fragmented = true;
                    if coalesce {
                        let tail = ranges.last_mut().ok_or(Error::Worker)?;
                        tail.count = range.first_chunk + range.count - tail.first_chunk;
                    }
                }
            }
            if fragmented && coalesce {
                // The first 4095 gaps remain exact; only the overflowing tail is widened.
                // Clear covered verified records so retransmission is explicit, not a replay.
                let tail = ranges.last().ok_or(Error::Worker)?;
                for chunk in tail.first_chunk..tail.first_chunk + tail.count {
                    cancel.check()?;
                    journal.seek(SeekFrom::Start(u64::from(chunk) * RECORD_BYTES))?;
                    journal.read_exact(&mut record)?;
                    if record[0] == 1 {
                        let length =
                            (size - u64::from(chunk) * chunk_size as u64).min(chunk_size as u64);
                        present_bytes -= length;
                        journal.seek(SeekFrom::Start(u64::from(chunk) * RECORD_BYTES))?;
                        journal.write_all(&[0; RECORD_BYTES as usize])?;
                    }
                }
            } else if fragmented {
                // Wire range cap: restart this file rather than inventing paginated resume.
                ranges = ResumeRanges::new();
                if count > 0 {
                    ranges
                        .push(ChunkRange {
                            first_chunk: 0,
                            count,
                        })
                        .map_err(|_| Error::Limit("resume ranges"))?;
                }
                journal.seek(SeekFrom::Start(0))?;
                let zeros = [0u8; 4096];
                let mut left = u64::from(count) * RECORD_BYTES;
                while left > 0 {
                    cancel.check()?;
                    let length = left.min(zeros.len() as u64) as usize;
                    journal.write_all(&zeros[..length])?;
                    left -= length as u64;
                }
                present_bytes = 0;
            }
            progress.add(present_bytes);
            Ok(FileResume {
                transfer_id: self.manifest.transfer_id.clone(),
                file_id,
                missing_chunks: ranges,
            })
        })();
        self.raw = raw;
        result
    }

    #[cfg(test)]
    pub(crate) fn apply(
        &mut self,
        chunk: FileChunk,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<()> {
        self.apply_view(
            FileChunkView {
                transfer_id: &chunk.transfer_id,
                file_id: chunk.file_id,
                chunk_index: chunk.chunk_index,
                offset: chunk.offset,
                data: &chunk.data,
                uncompressed_size: chunk.uncompressed_size,
                compressed: chunk.compressed,
                blake3_hash: chunk.blake3_hash,
            },
            cancel,
            progress,
        )
    }

    pub(crate) fn apply_view(
        &mut self,
        chunk: FileChunkView<'_>,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<()> {
        cancel.check()?;
        if chunk.transfer_id != self.manifest.transfer_id {
            return Err(Error::Invalid("chunk session"));
        }
        let (is_dir, size) = self.geometry(chunk.file_id as usize)?;
        let offset = u64::from(chunk.chunk_index) * self.config.chunk_size as u64;
        let length = chunk.uncompressed_size as usize;
        if is_dir
            || chunk.chunk_index >= self.config.chunks(size)?
            || chunk.offset != offset
            || length != (size - offset).min(self.config.chunk_size as u64) as usize
            || length == 0
            || chunk.data.is_empty()
            || chunk.data.len() > self.config.chunk_size
        {
            return Err(Error::Invalid("chunk geometry/length"));
        }
        let bytes = if chunk.compressed {
            // Fixed output slice and a 4 MiB decoder window; allocation never uses a peer length.
            let written = self
                .decompressor
                .decompress_to_buffer(chunk.data, &mut self.raw[..length])
                .map_err(|_| Error::Invalid("compressed chunk"))?;
            if written != length {
                return Err(Error::Invalid("decompressed length"));
            }
            &self.raw[..length]
        } else {
            if chunk.data.len() != length {
                return Err(Error::Invalid("raw chunk length"));
            }
            chunk.data
        };
        if *blake3::hash(bytes).as_bytes() != chunk.blake3_hash {
            return Err(Error::Integrity);
        }
        if filesystem::disk_available(&self.root)?
            < (length as u64).saturating_add(self.config.disk_reserve_bytes)
        {
            return Err(Error::DiskSpace);
        }
        // Move raw storage out to keep the data borrow independent of the cached file handles.
        let raw = std::mem::take(&mut self.raw);
        let bytes: &[u8] = if chunk.compressed {
            &raw[..length]
        } else {
            chunk.data
        };
        let result = (|| {
            let (_, part, journal) = self.open_cached(chunk.file_id as usize)?;
            journal.seek(SeekFrom::Start(u64::from(chunk.chunk_index) * RECORD_BYTES))?;
            let mut record = [0; RECORD_BYTES as usize];
            journal.read_exact(&mut record)?;
            if record[0] != 0 {
                return Err(Error::Invalid("duplicate/replayed chunk"));
            }
            part.seek(SeekFrom::Start(offset))?;
            part.write_all(bytes)?;
            record[0] = 1;
            record[1..].copy_from_slice(&chunk.blake3_hash);
            journal.seek(SeekFrom::Start(u64::from(chunk.chunk_index) * RECORD_BYTES))?;
            journal.write_all(&record)?;
            Ok(())
        })();
        self.raw = raw;
        result?;
        if let Some(lease) = &self.lease {
            lease.set_modified(SystemTime::now())?;
        }
        progress.add(length as u64);
        Ok(())
    }

    pub(crate) fn verify_all(&mut self, cancel: &Cancel) -> Result<()> {
        self.cache.take();
        for index in 0..self.entry_count() {
            cancel.check()?;
            let entry = self.entry(index)?;
            if entry.is_dir {
                continue;
            }
            let mut journal =
                filesystem::open_file(&self.part(entry.file_id, "chunks"), false, false)?;
            let count = self.config.chunks(entry.size)?;
            if journal.metadata()?.len() != u64::from(count) * RECORD_BYTES {
                return Err(Error::Integrity);
            }
            let mut record = [0; RECORD_BYTES as usize];
            for _ in 0..count {
                cancel.check()?;
                journal.read_exact(&mut record)?;
                if record[0] != 1 {
                    return Err(Error::Invalid("incomplete file"));
                }
            }
            let mut part = filesystem::open_file(&self.part(entry.file_id, "part"), true, false)?;
            if part.metadata()?.len() != entry.size {
                return Err(Error::Integrity);
            }
            let mut hash = blake3::Hasher::new();
            loop {
                cancel.check()?;
                let count = part.read(&mut self.raw)?;
                if count == 0 {
                    break;
                }
                hash.update(&self.raw[..count]);
            }
            if Some(*hash.finalize().as_bytes()) != entry.blake3_hash {
                return Err(Error::Integrity);
            }
            part.sync_all()?;
            drop(part);
            let destination = self.root.join("incoming").join(&entry.relative_path);
            filesystem::check_chain(&destination)?;
            // Only the private placeholder is replaced; no caller sees incoming paths.
            fs::rename(self.part(entry.file_id, "part"), destination)?;
        }
        Ok(())
    }

    pub(crate) fn preserve(&mut self) {
        self.retain = true;
    }

    pub(crate) fn discard(mut self) -> Result<()> {
        self.cache.take();
        self.lease.take();
        filesystem::check_chain(&self.root)?;
        fs::remove_dir_all(&self.root)?;
        self.retain = true;
        Ok(())
    }

    pub(crate) fn publish(mut self) -> Result<Received> {
        self.cache.take();
        filesystem::check_chain(&self.root.join("incoming"))?;
        if self.root.join("ready").try_exists()? {
            return Err(Error::Invalid("publication collision"));
        }
        fs::rename(self.root.join("incoming"), self.root.join("ready"))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        let mut paths = Vec::new();
        for index in 0..self.entry_count() {
            let entry = self.entry(index)?;
            if !entry.relative_path.contains('/') {
                paths.push(self.root.join("ready").join(&entry.relative_path));
            }
        }
        self.retain = true;
        let lease = self
            .lease
            .take()
            .ok_or(Error::Invalid("missing publication lease"))?;
        Ok(Received {
            paths,
            manifest: self.manifest.clone(),
            _lease: lease,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChunkEncoder, FileEngine};
    use std::time::Duration;

    const PEER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn temporary() -> tempfile::TempDir {
        tempfile::tempdir_in(std::env::var_os("CARGO_TARGET_DIR").expect("target dir"))
            .expect("temp dir")
    }
    fn manifest(bytes: &[u8], chunk_size: usize) -> FileManifest {
        FileManifest {
            transfer_id: "session".into(),
            clip_id: "clip".into(),
            chunk_size: chunk_size as u32,
            page: 0,
            final_page: true,
            files: ManifestFiles::try_from_vec(vec![FileManifestEntry {
                file_id: 0,
                relative_path: "payload.bin".into(),
                size: bytes.len() as u64,
                is_dir: false,
                blake3_hash: Some(*blake3::hash(bytes).as_bytes()),
            }])
            .expect("manifest"),
        }
    }
    fn job(bytes: &[u8], config: Config) -> (tempfile::TempDir, Session) {
        let dir = temporary();
        let staging = fs::canonicalize(dir.path()).expect("canonical");
        let session = Session::open(
            staging,
            PEER.into(),
            manifest(bytes, config.chunk_size),
            config,
            &Cancel::new(),
        )
        .expect("open");
        (dir, session)
    }
    fn encode(bytes: &[u8], index: u32, config: &Config) -> FileChunk {
        let mut encoder = ChunkEncoder::new(config).expect("encoder");
        encoder
            .buffer_mut(bytes.len())
            .expect("buffer")
            .copy_from_slice(bytes);
        encoder
            .encode("session", 0, index, "payload.bin")
            .expect("encode")
    }

    #[test]
    fn duplicate_replay_and_incomplete_completion_fail_closed() {
        let config = Config {
            chunk_size: 1,
            ..Config::default()
        };
        let (_dir, mut session) = job(&[1, 2], config.clone());
        let cancel = Cancel::new();
        let progress = Progress::new();
        session
            .apply(encode(&[1], 0, &config), &cancel, &progress)
            .expect("first chunk");
        assert!(matches!(
            session.apply(encode(&[1], 0, &config), &cancel, &progress),
            Err(Error::Invalid("duplicate/replayed chunk"))
        ));
        assert!(matches!(
            session.verify_all(&cancel),
            Err(Error::Invalid("incomplete file"))
        ));
        let root = session.root.clone();
        drop(session);
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn corrupt_staging_io_is_not_mistaken_for_a_transport_disconnect() {
        let dir = temporary();
        let config = Config::default();
        let engine = FileEngine::new(dir.path(), config.clone())
            .await
            .expect("engine");
        let mut session = Session::open(
            engine.staging.clone(),
            PEER.into(),
            manifest(&[1], config.chunk_size),
            config.clone(),
            &Cancel::new(),
        )
        .expect("job");
        let root = session.root.clone();
        session.preserve();
        drop(session);
        fs::remove_file(root.join("parts/0.part")).expect("remove staged file");
        let (mut left, mut right) = tokio::io::duplex(64 * 1024);
        let (_send_lane, receive_lane) = tokio::io::duplex(64 * 1024);
        let cancel = Cancel::new();
        let progress = Progress::new();
        let message = TransferMessage::FileManifest(manifest(&[1], config.chunk_size));
        let (send, receive) = tokio::join!(
            crate::write_message(&mut left, &message, Duration::from_secs(1), &cancel),
            engine.receive(
                PEER,
                &mut right,
                vec![receive_lane],
                crate::Consent::Automatic,
                &cancel,
                &progress
            )
        );
        send.expect("manifest");
        assert!(matches!(receive, Err(Error::Storage(_))));
        assert!(!root.exists(), "unusable staging must be purged");
    }

    #[test]
    fn fragmentation_falls_back_to_full_file_with_bounded_ranges() {
        let config = Config {
            chunk_size: 1,
            ..Config::default()
        };
        let bytes = vec![1; 8194];
        let (_dir, mut session) = job(&bytes, config.clone());
        let progress = Progress::new();
        let cancel = Cancel::new();
        for index in (0..8194).step_by(2) {
            session
                .apply(encode(&[1], index, &config), &cancel, &progress)
                .expect("chunk");
        }
        let resumed = Progress::new();
        let ranges = session.resume(0, &cancel, &resumed).expect("resume");
        assert_eq!(
            &*ranges.missing_chunks,
            &[ChunkRange {
                first_chunk: 0,
                count: 8194
            }]
        );
        assert_eq!(resumed.snapshot().bytes_done, 0);
        session
            .apply(encode(&[1], 0, &config), &cancel, &progress)
            .expect("restart accepts cleared chunk");
    }

    #[test]
    fn opted_in_fragmented_resume_keeps_prefix_and_binds_sender_geometry() {
        let config = Config {
            chunk_size: 1,
            ..Config::default()
        };
        let bytes = vec![1; 10_000];
        let (dir, mut session) = job(&bytes, config.clone());
        let progress = Progress::new();
        let cancel = Cancel::new();
        for index in (0..10_000).step_by(2) {
            session
                .apply(encode(&[1], index, &config), &cancel, &progress)
                .expect("verified chunk");
        }
        let root = session.root.clone();
        session.preserve();
        drop(session);
        let mut changed = manifest(&bytes, 2);
        changed.transfer_id = "session".into();
        assert!(matches!(
            Session::open_with_options(
                fs::canonicalize(dir.path()).expect("canonical"),
                PEER.into(),
                changed,
                Config {
                    chunk_size: 2,
                    ..config.clone()
                },
                &cancel,
                None,
                Some(ChunkSizeBounds { min: 1, max: 2 }),
                true
            ),
            Err(Error::Invalid("resume manifest mismatch"))
        ));
        assert!(
            root.exists(),
            "a geometry substitution cannot erase the old job"
        );
        let mut session = Session::open_with_options(
            fs::canonicalize(dir.path()).expect("canonical"),
            PEER.into(),
            manifest(&bytes, 1),
            config.clone(),
            &cancel,
            None,
            Some(ChunkSizeBounds { min: 1, max: 2 }),
            true,
        )
        .expect("same geometry reconnect");
        let resumed = Progress::new();
        let ranges = session
            .resume(0, &cancel, &resumed)
            .expect("coalesced resume");
        assert_eq!(ranges.missing_chunks.len(), MAX_RESUME_RANGES);
        assert_eq!(
            ranges.missing_chunks[0],
            ChunkRange {
                first_chunk: 1,
                count: 1
            }
        );
        assert_eq!(
            ranges.missing_chunks[MAX_RESUME_RANGES - 1],
            ChunkRange {
                first_chunk: 8191,
                count: 1809
            }
        );
        assert_eq!(
            resumed.snapshot().bytes_done,
            4096,
            "verified prefix remains present"
        );
        assert!(matches!(
            session.apply(encode(&[1], 0, &config), &cancel, &progress),
            Err(Error::Invalid("duplicate/replayed chunk"))
        ));
        session
            .apply(encode(&[1], 8192, &config), &cancel, &progress)
            .expect("covered verified tail explicitly retransmitted");
    }

    #[test]
    fn changed_manifest_preserves_original_staging_and_completed_id_cannot_replay() {
        let config = Config {
            chunk_size: 1,
            ..Config::default()
        };
        let bytes = [1];
        let (dir, mut session) = job(&bytes, config.clone());
        let root = session.root.clone();
        session.preserve();
        drop(session);
        let mut changed = manifest(&bytes, config.chunk_size);
        changed.files[0].relative_path = "renamed.bin".into();
        assert!(matches!(
            Session::open(
                fs::canonicalize(dir.path()).expect("canonical"),
                PEER.into(),
                changed,
                config.clone(),
                &Cancel::new()
            ),
            Err(Error::Invalid("resume manifest mismatch"))
        ));
        assert!(root.exists());
        let mut session = Session::open(
            fs::canonicalize(dir.path()).expect("canonical"),
            PEER.into(),
            manifest(&bytes, config.chunk_size),
            config.clone(),
            &Cancel::new(),
        )
        .expect("resume");
        session
            .apply(encode(&bytes, 0, &config), &Cancel::new(), &Progress::new())
            .expect("chunk");
        session.verify_all(&Cancel::new()).expect("verify");
        let received = session.publish().expect("publish");
        assert_eq!(fs::read(&received.paths[0]).expect("published"), bytes);
        drop(received);
        assert!(matches!(
            Session::open(
                fs::canonicalize(dir.path()).expect("canonical"),
                PEER.into(),
                manifest(&bytes, config.chunk_size),
                config,
                &Cancel::new()
            ),
            Err(Error::Invalid("completed transfer replay"))
        ));
        assert!(root.join("ready/payload.bin").exists());
    }

    // Bug: after about 32 copy attempts in a day every new copy failed with "too many earlier copies kept", because
    // finished or abandoned staging folders were only removed after 24 hours.
    #[tokio::test]
    async fn a_full_staging_folder_makes_room_by_clearing_idle_old_sessions() {
        let data = temporary();
        let config = Config {
            max_staging_sessions: 1,
            ..Config::default()
        };
        let engine = FileEngine::new(data.path(), config.clone())
            .await
            .expect("engine");
        let mut first = Session::open(
            engine.staging.clone(),
            PEER.into(),
            manifest(&[], config.chunk_size),
            config.clone(),
            &Cancel::new(),
        )
        .expect("first");
        let first_root = first.root.clone();
        first
            .lease
            .as_ref()
            .expect("lease")
            .set_modified(SystemTime::UNIX_EPOCH)
            .expect("age");
        first.preserve();
        drop(first);
        let mut second = manifest(&[], config.chunk_size);
        second.transfer_id = "other".into();
        assert!(Session::open(
            engine.staging.clone(),
            PEER.into(),
            second,
            config,
            &Cancel::new()
        )
        .is_ok());
        assert!(!first_root.exists());
    }

    #[tokio::test]
    async fn staging_quota_live_leases_startup_and_scheduled_purge() {
        let data = temporary();
        let config = Config {
            stale_after: Duration::from_millis(1),
            purge_interval: Duration::from_millis(100),
            max_staging_sessions: 1,
            ..Config::default()
        };
        let engine = FileEngine::new(data.path(), config.clone())
            .await
            .expect("engine");
        let mut session = Session::open(
            engine.staging.clone(),
            PEER.into(),
            manifest(&[], config.chunk_size),
            config.clone(),
            &Cancel::new(),
        )
        .expect("job");
        let root = session.root.clone();
        session
            .lease
            .as_ref()
            .expect("lease")
            .set_modified(SystemTime::UNIX_EPOCH)
            .expect("age");
        assert_eq!(
            engine.purge_stale().await.expect("purge"),
            0,
            "live lease protects job"
        );
        let mut second = manifest(&[], config.chunk_size);
        second.transfer_id = "other".into();
        assert!(matches!(
            Session::open(
                engine.staging.clone(),
                PEER.into(),
                second,
                config.clone(),
                &Cancel::new()
            ),
            Err(Error::Limit("staging session count"))
        ));
        session.preserve();
        drop(session);
        let restarted = FileEngine::new(data.path(), config.clone())
            .await
            .expect("restart purges");
        assert!(!root.exists());
        let mut session = Session::open(
            restarted.staging.clone(),
            PEER.into(),
            manifest(&[], config.chunk_size),
            config,
            &Cancel::new(),
        )
        .expect("job");
        session
            .lease
            .as_ref()
            .expect("lease")
            .set_modified(SystemTime::UNIX_EPOCH)
            .expect("age");
        session.preserve();
        drop(session);
        let cancel = Cancel::new();
        let stop = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        };
        let (result, _) = tokio::join!(restarted.run_purge(&cancel), stop);
        result.expect("scheduled purge");
        assert!(!root.exists());
    }

    #[test]
    fn chunk_geometry_zero_directory_unknown_and_decoder_window_limits() {
        let config = Config {
            chunk_size: 4096,
            ..Config::default()
        };
        let bytes = vec![0; 4096];
        let (_dir, mut session) = job(&bytes, config.clone());
        let mut chunk = encode(&bytes, 0, &config);
        chunk.uncompressed_size = u32::MAX;
        assert!(matches!(
            session.apply(chunk, &Cancel::new(), &Progress::new()),
            Err(Error::Invalid(_))
        ));
        let mut chunk = encode(&bytes, 0, &config);
        chunk.transfer_id = "other".into();
        assert!(matches!(
            session.apply(chunk, &Cancel::new(), &Progress::new()),
            Err(Error::Invalid(_))
        ));
        // A stream frame requesting a window larger than 4 MiB must fail, even if the
        // actual decompressed payload is tiny. This uses zstd's standard frame header.
        let huge_window = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xf8, 0x09, 0x00, 0x00, 0x00];
        let mut chunk = encode(&bytes, 0, &config);
        chunk.data = ChunkBytes::try_from_vec(huge_window).expect("bounded");
        chunk.compressed = true;
        assert!(matches!(
            session.apply(chunk, &Cancel::new(), &Progress::new()),
            Err(Error::Invalid(_))
        ));
    }
}
