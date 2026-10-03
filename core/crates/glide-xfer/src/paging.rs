//! Bounded, disk-backed manifest paging for large file trees.
//!
//! A `PageSpool` validates each page before writing it, binds the canonical ordered page bytes
//! into the same digest used by `Consent::approve_pages`, and keeps only fixed-size indexes in
//! memory. Page and source lookups read one bounded page or one bounded path from disk.

use crate::{filesystem, Cancel, ChunkSizeBounds, Config, Consent, Error, Result};
use glide_proto::{
    codec,
    manifest::{ManifestSummary, ManifestValidator},
    wire::{
        FileManifest, FileManifestEntry, ManifestFiles, TransferMessage, MAX_FILE_CHUNK_BYTES,
        MAX_FILE_CONTROL_FRAME_BYTES, MAX_MANIFEST_DEPTH, MAX_MANIFEST_FILES,
        MAX_MANIFEST_PATH_BYTES, MAX_TRANSFER_BYTES, MAX_TRANSFER_ITEMS,
    },
};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tempfile::{Builder, TempDir};

const PAGE_INDEX_RECORD_BYTES: u64 = 24;
const PATH_INDEX_RECORD_BYTES: u64 = 24;
const SOURCE_INDEX_RECORD_BYTES: u64 = 16;
const ENTRY_INDEX_RECORD_BYTES: u64 = 16;
const ENTRY_FIXED_BYTES: u64 = 46;
const RESERVATION_MARKER_BYTES: u64 = 8;
const MAX_SOURCE_PATH_BYTES: usize = 4096;
const MAX_PATH_PROBES: u64 = 4096;
const MAX_PATH_KEY_BYTES: usize = MAX_MANIFEST_PATH_BYTES * 4;
const HARD_MAX_MANIFEST_BYTES: u64 = 4 << 30;
const HARD_MAX_SPOOL_BYTES: u64 = 8 << 30;
const HARD_MAX_ROOTS: usize = MAX_MANIFEST_FILES;

#[derive(Clone, Debug)]
pub struct PagingConfig {
    /// Aggregate entries, across every page. `Config::max_entries` remains the legacy
    /// single-page limit and is deliberately not repurposed.
    pub max_entries: usize,
    /// Aggregate canonical encoded page bytes.
    pub max_manifest_bytes: u64,
    pub page_entries: usize,
    pub page_bytes: usize,
    pub max_roots: usize,
    pub min_chunk_size: usize,
    pub max_chunk_size: usize,
    /// Peak temporary spool footprint, including the disk path-collision index.
    pub max_spool_bytes: u64,
}

impl Default for PagingConfig {
    fn default() -> Self {
        Self {
            max_entries: MAX_TRANSFER_ITEMS as usize,
            max_manifest_bytes: 1 << 30,
            page_entries: MAX_MANIFEST_FILES,
            page_bytes: MAX_FILE_CONTROL_FRAME_BYTES,
            max_roots: HARD_MAX_ROOTS,
            min_chunk_size: 1,
            max_chunk_size: MAX_FILE_CHUNK_BYTES,
            max_spool_bytes: 4 << 30,
        }
    }
}

impl PagingConfig {
    pub fn validate(&self, config: &Config) -> Result<()> {
        config.validate()?;
        if self.max_entries == 0
            || self.max_entries > MAX_TRANSFER_ITEMS as usize
            || self.max_manifest_bytes == 0
            || self.max_manifest_bytes > HARD_MAX_MANIFEST_BYTES
            || !(1..=MAX_MANIFEST_FILES).contains(&self.page_entries)
            || self.page_bytes == 0
            || self.page_bytes > MAX_FILE_CONTROL_FRAME_BYTES
            || self.max_roots == 0
            || self.max_roots > HARD_MAX_ROOTS
            || self.max_roots > self.max_entries
            || self.max_spool_bytes == 0
            || self.max_spool_bytes > HARD_MAX_SPOOL_BYTES
            || config.max_depth > MAX_MANIFEST_DEPTH
            || config.max_path_bytes > MAX_MANIFEST_PATH_BYTES
        {
            return Err(Error::Invalid("paging configuration"));
        }
        ChunkSizeBounds {
            min: self.min_chunk_size,
            max: self.max_chunk_size,
        }
        .validate()?;
        Ok(())
    }

    /// Conservative live Rust payload/metadata ceiling, independent of tree size.
    /// Disk indexes, native zstd allocations and caller-owned transport buffers are excluded.
    pub fn payload_memory_ceiling(&self, config: &Config) -> usize {
        let mut bounded = config.clone();
        bounded.max_entries = self.page_entries;
        bounded
            .payload_memory_ceiling()
            .saturating_add(4 * self.page_entries * config.max_path_bytes)
    }

    fn check_chunk_size(&self, size: usize) -> Result<()> {
        ChunkSizeBounds {
            min: self.min_chunk_size,
            max: self.max_chunk_size,
        }
        .accepts(size)
    }
}

#[derive(Clone)]
pub struct PagedManifest {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    summary: ManifestSummary,
    digest: [u8; 32],
    first_page: FileManifest,
    page_bytes: usize,
    pages_bytes: u64,
    index_bytes: u64,
    entry_bytes: u64,
    source_bytes: u64,
    has_sources: bool,
    journal_bytes: u64,
    max_path_bytes: usize,
    max_depth: usize,
    paging: PagingConfig,
    pages: Mutex<File>,
    page_index: Mutex<File>,
    entries: Mutex<File>,
    entries_index: Mutex<File>,
    sources: Option<Mutex<File>>,
    source_index: Option<Mutex<File>>,
    _lease: File,
    _dir: TempDir,
}

impl PagedManifest {
    pub fn first_page(&self) -> &FileManifest {
        &self.inner.first_page
    }

    pub fn summary(&self) -> ManifestSummary {
        self.inner.summary
    }

    pub fn digest(&self) -> [u8; 32] {
        self.inner.digest
    }

    /// Read exactly one validated page. Allocation is bounded by `PagingConfig::page_bytes`.
    pub fn read_page(&self, page: u32) -> Result<FileManifest> {
        if page >= self.inner.summary.pages {
            return Err(Error::Invalid("manifest page index"));
        }
        let mut pages = self.inner.pages.lock().map_err(|_| Error::Worker)?;
        let mut index = self.inner.page_index.lock().map_err(|_| Error::Worker)?;
        read_page_from_files(&mut pages, &mut index, self.inner.page_bytes, page)
    }

    /// Read one manifest entry by its contiguous transfer-wide ID.
    pub fn entry(&self, file_id: u32) -> Result<FileManifestEntry> {
        if file_id >= self.inner.summary.items {
            return Err(Error::Invalid("manifest entry id"));
        }
        let mut entries = self.inner.entries.lock().map_err(|_| Error::Worker)?;
        let mut index = self.inner.entries_index.lock().map_err(|_| Error::Worker)?;
        read_entry_from_files(
            &mut entries,
            &mut index,
            file_id,
            self.inner.max_path_bytes,
            self.inner.max_depth,
        )
    }

    /// Return a source path when this spool was built by a sender.
    pub fn source(&self, file_id: u32) -> Result<PathBuf> {
        if !self.inner.has_sources || file_id >= self.inner.summary.items {
            return Err(Error::Invalid("manifest source path unavailable"));
        }
        let mut index = self
            .inner
            .source_index
            .as_ref()
            .ok_or(Error::Invalid("source index unavailable"))?
            .lock()
            .map_err(|_| Error::Worker)?;
        index.seek(SeekFrom::Start(
            u64::from(file_id)
                .checked_mul(SOURCE_INDEX_RECORD_BYTES)
                .ok_or(Error::Limit("source index offset"))?,
        ))?;
        let mut record = [0; SOURCE_INDEX_RECORD_BYTES as usize];
        index.read_exact(&mut record)?;
        let offset = u64::from_le_bytes(
            record[..8]
                .try_into()
                .map_err(|_| Error::Invalid("source index"))?,
        );
        let len = u32::from_le_bytes(
            record[8..12]
                .try_into()
                .map_err(|_| Error::Invalid("source index"))?,
        ) as usize;
        if len == 0 || len > MAX_SOURCE_PATH_BYTES {
            return Err(Error::Invalid("source path length"));
        }
        let mut data = vec![0; len];
        let mut sources = self
            .inner
            .sources
            .as_ref()
            .ok_or(Error::Invalid("source spool unavailable"))?
            .lock()
            .map_err(|_| Error::Worker)?;
        sources.seek(SeekFrom::Start(offset))?;
        sources.read_exact(&mut data)?;
        decode_source_path(&data)
    }

    /// Disk used by the finished manifest spool, excluding the temporary uniqueness index.
    pub fn spool_bytes(&self) -> u64 {
        self.inner
            .pages_bytes
            .saturating_add(self.inner.index_bytes)
            .saturating_add(self.inner.entry_bytes)
            .saturating_add(self.inner.source_bytes)
    }

    pub fn journal_bytes(&self) -> u64 {
        self.inner.journal_bytes
    }

    pub fn limits(&self) -> &PagingConfig {
        &self.inner.paging
    }

    /// Parent directory containing the private disk spool. The transfer engine uses this to
    /// exclude this already-reserved spool when accounting its own staging quota.
    pub(crate) fn spool_root(&self) -> &Path {
        self.inner._dir.path()
    }

    /// Approval is bound to the canonical encoded page digest in page order.
    pub fn approve(&self) -> Consent {
        Consent::Approved {
            manifest_digest: self.inner.digest,
        }
    }
}

/// Incrementally validates and spools a manifest. Pass source paths only for sender manifests;
/// receive-side spools pass `None` for every page.
pub struct PageSpool {
    config: Config,
    paging: PagingConfig,
    validator: ManifestValidator,
    digest: blake3::Hasher,
    pages: File,
    page_index: File,
    entries: File,
    entries_index: File,
    path_index: Option<File>,
    path_keys: Option<File>,
    reservation: File,
    path_slots: u64,
    path_index_bytes: u64,
    working_spool_bytes: u64,
    sources: Option<File>,
    source_index: Option<File>,
    source_mode: Option<bool>,
    encoded_bytes: u64,
    transfer_bytes: u64,
    journal_bytes: u64,
    page_count: u32,
    item_count: u32,
    root_count: usize,
    pages_bytes: u64,
    source_data_bytes: u64,
    path_key_bytes: u64,
    entry_data_bytes: u64,
    failed: bool,
    _lease: File,
    dir: TempDir,
}

impl PageSpool {
    pub fn new(spool_dir: impl AsRef<Path>, config: Config, paging: PagingConfig) -> Result<Self> {
        paging.validate(&config)?;
        filesystem::check_chain(spool_dir.as_ref())?;
        let dir = Builder::new()
            .prefix("glide-manifest-")
            .tempdir_in(spool_dir.as_ref())
            .map_err(Error::Storage)?;
        filesystem::check_chain(dir.path())?;
        let lease = filesystem::lease(&dir.path().join("lease"))?;
        lease.set_modified(std::time::SystemTime::now())?;
        let mut reservation = create_spool_file(dir.path(), "reservation")?;
        let pages = create_spool_file(dir.path(), "pages.bin")?;
        let page_index = create_spool_file(dir.path(), "pages.idx")?;
        let entries = create_spool_file(dir.path(), "entries.bin")?;
        let entries_index = create_spool_file(dir.path(), "entries.idx")?;
        let path_index = create_spool_file(dir.path(), "paths.idx")?;
        let path_keys = create_spool_file(dir.path(), "paths.bin")?;
        let path_slots = paging
            .max_entries
            .checked_mul(2)
            .and_then(|slots| slots.checked_next_power_of_two())
            .ok_or(Error::Limit("path index size"))? as u64;
        let path_index_bytes = path_slots
            .checked_mul(PATH_INDEX_RECORD_BYTES)
            .ok_or(Error::Limit("path index size"))?;
        let initial_reservation = spool_reservation(path_index_bytes)?;
        if initial_reservation > paging.max_spool_bytes {
            return Err(Error::Limit("manifest spool size"));
        }
        crate::receiver::check_spool_quota(
            spool_dir.as_ref(),
            Some(dir.path()),
            initial_reservation,
            &config,
        )?;
        write_reservation(&mut reservation, initial_reservation)?;
        check_disk_available(
            spool_dir.as_ref(),
            path_index_bytes,
            config.disk_reserve_bytes,
        )?;
        path_index
            .set_len(path_index_bytes)
            .map_err(Error::Storage)?;
        Ok(Self {
            _lease: lease,
            dir,
            config,
            paging,
            validator: ManifestValidator::default(),
            digest: blake3::Hasher::new(),
            pages,
            page_index,
            entries,
            entries_index,
            path_index: Some(path_index),
            path_keys: Some(path_keys),
            reservation,
            path_slots,
            path_index_bytes,
            working_spool_bytes: path_index_bytes,
            sources: None,
            source_index: None,
            source_mode: None,
            encoded_bytes: 0,
            transfer_bytes: 0,
            journal_bytes: 0,
            page_count: 0,
            item_count: 0,
            root_count: 0,
            pages_bytes: 0,
            source_data_bytes: 0,
            path_key_bytes: 0,
            entry_data_bytes: 0,
            failed: false,
        })
    }

    pub(crate) fn remaining_entries(&self) -> usize {
        self.paging
            .max_entries
            .saturating_sub(self.item_count as usize)
    }

    pub(crate) fn remaining_manifest_bytes(&self) -> u64 {
        self.paging
            .max_manifest_bytes
            .saturating_sub(self.encoded_bytes)
    }

    pub(crate) fn remaining_transfer_bytes(&self) -> u64 {
        self.config
            .max_transfer_bytes
            .min(MAX_TRANSFER_BYTES)
            .saturating_sub(self.transfer_bytes)
    }

    pub fn push(
        &mut self,
        page: &FileManifest,
        source_paths: Option<&[PathBuf]>,
        cancel: &Cancel,
    ) -> Result<()> {
        if self.failed {
            return Err(Error::Invalid("manifest spool is unusable"));
        }
        let result = self.push_inner(page, source_paths, cancel);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(
        &mut self,
        page: &FileManifest,
        source_paths: Option<&[PathBuf]>,
        cancel: &Cancel,
    ) -> Result<()> {
        cancel.check()?;
        glide_proto::manifest::validate_manifest_page(page)?;
        self.paging.check_chunk_size(page.chunk_size as usize)?;
        let estimated_len = estimated_manifest_page_bytes(page)? as u64;
        if page.files.len() > self.paging.page_entries
            || estimated_len > self.paging.page_bytes as u64
            || self
                .item_count
                .checked_add(page.files.len() as u32)
                .is_none_or(|count| count > self.paging.max_entries as u32)
        {
            return Err(Error::Limit("manifest page size"));
        }
        self.encoded_bytes
            .checked_add(estimated_len)
            .filter(|bytes| *bytes <= self.paging.max_manifest_bytes)
            .ok_or(Error::Limit("manifest bytes"))?;
        if source_paths.is_some_and(|paths| paths.len() != page.files.len()) {
            return Err(Error::Invalid("manifest source count"));
        }
        let source_mode = source_paths.is_some();
        if self
            .source_mode
            .is_some_and(|current| current != source_mode)
        {
            return Err(Error::Invalid("mixed manifest source paths"));
        }
        if let Some(paths) = source_paths {
            self.ensure_source_files()?;
            for (entry, path) in page.files.iter().zip(paths) {
                validate_source_path(path, entry)?;
            }
        }

        // The key map is limited to one configured page; the transfer-wide index stays on disk.
        let mut page_paths = HashMap::<String, bool>::with_capacity(page.files.len());
        let mut new_roots = 0usize;
        let mut page_transfer_bytes = 0u64;
        let mut page_journal_bytes = 0u64;
        let mut page_source_bytes = 0u64;
        let mut page_path_key_bytes = 0u64;
        let mut page_entry_bytes = 0u64;
        for entry in page.files.iter() {
            cancel.check()?;
            filesystem::validate_relative(
                &entry.relative_path,
                self.config.max_depth,
                self.config.max_path_bytes,
            )?;
            let key = entry.relative_path.to_lowercase();
            if page_paths.contains_key(&key) || self.lookup_path(&key)?.is_some() {
                return Err(Error::Invalid("case/path collision"));
            }
            if let Some((parent, _)) = key.rsplit_once('/') {
                let parent_is_dir = match page_paths.get(parent).copied() {
                    Some(is_dir) => Some(is_dir),
                    None => self.lookup_path(parent)?.map(|entry| entry.is_dir),
                };
                if parent_is_dir != Some(true) {
                    return Err(Error::Invalid("missing directory parent"));
                }
            } else {
                new_roots += 1;
            }
            let key_len = key.len();
            if key_len > MAX_PATH_KEY_BYTES {
                return Err(Error::Limit("manifest path key"));
            }
            page_path_key_bytes = page_path_key_bytes
                .checked_add(key_len as u64)
                .ok_or(Error::Limit("manifest path key"))?;
            page_entry_bytes = page_entry_bytes
                .checked_add(ENTRY_FIXED_BYTES + entry.relative_path.len() as u64)
                .ok_or(Error::Limit("manifest entry spool"))?;
            page_paths.insert(key, entry.is_dir);

            if entry.is_dir {
                if entry.size != 0 || entry.blake3_hash.is_some() {
                    return Err(Error::Invalid("directory metadata"));
                }
            } else {
                if entry.blake3_hash.is_none() {
                    return Err(Error::Invalid("missing file digest"));
                }
                if entry.size > self.config.max_file_bytes {
                    return Err(Error::Limit("file size"));
                }
                let chunks = entry.size.div_ceil(u64::from(page.chunk_size));
                if chunks > u64::from(self.config.max_chunks_per_file) {
                    return Err(Error::Limit("chunk count"));
                }
                page_transfer_bytes = page_transfer_bytes
                    .checked_add(entry.size)
                    .ok_or(Error::Limit("transfer size"))?;
                page_journal_bytes = page_journal_bytes
                    .checked_add(chunks.checked_mul(33).ok_or(Error::Limit("journal size"))?)
                    .ok_or(Error::Limit("journal size"))?;
            }
        }
        let roots = self
            .root_count
            .checked_add(new_roots)
            .ok_or(Error::Limit("manifest roots"))?;
        if roots > self.paging.max_roots {
            return Err(Error::Limit("manifest roots"));
        }
        let aggregate_bytes = self
            .transfer_bytes
            .checked_add(page_transfer_bytes)
            .filter(|bytes| {
                *bytes <= self.config.max_transfer_bytes && *bytes <= MAX_TRANSFER_BYTES
            })
            .ok_or(Error::Limit("transfer size"))?;
        let aggregate_journal = self
            .journal_bytes
            .checked_add(page_journal_bytes)
            .ok_or(Error::Limit("journal size"))?;

        let next_items = self
            .item_count
            .checked_add(page.files.len() as u32)
            .filter(|items| *items <= self.paging.max_entries as u32)
            .ok_or(Error::Limit("entry count"))?;
        let next_pages = self
            .pages_bytes
            .checked_add(estimated_len)
            .ok_or(Error::Limit("manifest spool size"))?;
        let next_page_count = self
            .page_count
            .checked_add(1)
            .ok_or(Error::Limit("manifest page count"))?;
        let next_index = u64::from(next_page_count)
            .checked_mul(PAGE_INDEX_RECORD_BYTES)
            .ok_or(Error::Limit("manifest spool size"))?;
        if let Some(paths) = source_paths {
            for path in paths {
                let len = source_path_storage_len(path)?;
                if len == 0 || len > MAX_SOURCE_PATH_BYTES {
                    return Err(Error::Limit("source path bytes"));
                }
                page_source_bytes = page_source_bytes
                    .checked_add(len as u64)
                    .ok_or(Error::Limit("source path bytes"))?;
            }
        }
        let next_source_index = if source_mode {
            u64::from(next_items)
                .checked_mul(SOURCE_INDEX_RECORD_BYTES)
                .ok_or(Error::Limit("manifest spool size"))?
        } else {
            0
        };
        let next_source_data = self
            .source_data_bytes
            .checked_add(page_source_bytes)
            .ok_or(Error::Limit("manifest spool size"))?;
        let next_path_keys = self
            .path_key_bytes
            .checked_add(page_path_key_bytes)
            .ok_or(Error::Limit("manifest spool size"))?;
        let next_entry_data = self
            .entry_data_bytes
            .checked_add(page_entry_bytes)
            .ok_or(Error::Limit("manifest entry spool"))?;
        let next_entry_index = u64::from(next_items)
            .checked_mul(ENTRY_INDEX_RECORD_BYTES)
            .ok_or(Error::Limit("manifest entry index"))?;
        let working_bytes = self
            .path_index_bytes
            .checked_add(next_pages)
            .and_then(|bytes| bytes.checked_add(next_index))
            .and_then(|bytes| bytes.checked_add(next_source_index))
            .and_then(|bytes| bytes.checked_add(next_source_data))
            .and_then(|bytes| bytes.checked_add(next_path_keys))
            .and_then(|bytes| bytes.checked_add(next_entry_index))
            .and_then(|bytes| bytes.checked_add(next_entry_data))
            .filter(|bytes| *bytes <= self.paging.max_spool_bytes)
            .ok_or(Error::Limit("manifest spool size"))?;
        let reservation_bytes = spool_reservation(working_bytes)?;
        if reservation_bytes > self.paging.max_spool_bytes {
            return Err(Error::Limit("manifest spool size"));
        }
        crate::receiver::check_spool_quota(
            self.dir
                .path()
                .parent()
                .ok_or(Error::Invalid("manifest spool parent"))?,
            Some(self.dir.path()),
            reservation_bytes,
            &self.config,
        )?;
        write_reservation(&mut self.reservation, reservation_bytes)?;
        self._lease.set_modified(std::time::SystemTime::now())?;
        check_disk_available(
            self.dir.path(),
            working_bytes.saturating_sub(self.working_spool_bytes),
            self.config.disk_reserve_bytes,
        )?;
        let encoded = codec::encode_transfer(&TransferMessage::FileManifest(page.clone()))?;
        let encoded_len = encoded.len() as u64;
        if encoded.len() > self.paging.page_bytes || encoded_len != estimated_len {
            return Err(Error::Limit("manifest page bytes"));
        }
        let next_manifest_bytes = self.encoded_bytes + encoded_len;

        self.validator.push(page)?;

        let page_offset = self.pages.seek(SeekFrom::End(0))?;
        self.pages.write_all(&encoded).map_err(Error::Storage)?;
        write_page_index(
            &mut self.page_index,
            page_offset,
            encoded_len as u32,
            page.files[0].file_id,
            page.files.len() as u32,
        )?;
        if let Some(paths) = source_paths {
            self.write_source_paths(paths)?;
        }
        for entry in page.files.iter() {
            write_entry_record(&mut self.entries, &mut self.entries_index, entry)?;
            self.insert_path(
                &entry.relative_path.to_lowercase(),
                entry.file_id,
                entry.is_dir,
            )?;
        }
        if self.source_mode.is_none() {
            self.source_mode = Some(source_mode);
        }
        self.digest.update(&encoded);
        self.encoded_bytes = next_manifest_bytes;
        self.transfer_bytes = aggregate_bytes;
        self.journal_bytes = aggregate_journal;
        self.page_count += 1;
        self.item_count = next_items;
        self.root_count = roots;
        self.pages_bytes = next_pages;
        self.source_data_bytes = next_source_data;
        self.path_key_bytes = next_path_keys;
        self.entry_data_bytes = next_entry_data;
        self.working_spool_bytes = working_bytes;
        Ok(())
    }

    pub fn finish(mut self) -> Result<PagedManifest> {
        if self.failed {
            return Err(Error::Invalid("manifest spool is unusable"));
        }
        let summary = self.validator.finish()?;
        if self.page_count == 0 || self.item_count == 0 {
            return Err(Error::Invalid("empty manifest"));
        }
        self.pages.sync_all().map_err(Error::Storage)?;
        self.page_index.sync_all().map_err(Error::Storage)?;
        self.entries.sync_all().map_err(Error::Storage)?;
        self.entries_index.sync_all().map_err(Error::Storage)?;
        if let Some(sources) = &self.sources {
            sources.sync_all().map_err(Error::Storage)?;
        }
        if let Some(index) = &self.source_index {
            index.sync_all().map_err(Error::Storage)?;
        }
        self.path_index.take();
        fs::remove_file(self.dir.path().join("paths.idx")).map_err(Error::Storage)?;
        self.path_keys.take();
        fs::remove_file(self.dir.path().join("paths.bin")).map_err(Error::Storage)?;
        let source_index_bytes = if self.source_mode == Some(true) {
            u64::from(self.item_count)
                .checked_mul(SOURCE_INDEX_RECORD_BYTES)
                .ok_or(Error::Limit("source index size"))?
        } else {
            0
        };
        let source_bytes = source_index_bytes
            .checked_add(self.source_data_bytes)
            .ok_or(Error::Limit("source spool size"))?;
        let index_bytes = u64::from(self.page_count)
            .checked_mul(PAGE_INDEX_RECORD_BYTES)
            .and_then(|bytes| {
                u64::from(self.item_count)
                    .checked_mul(ENTRY_INDEX_RECORD_BYTES)
                    .and_then(|entry_index| bytes.checked_add(entry_index))
            })
            .ok_or(Error::Limit("page index size"))?;
        let finished_bytes = self
            .pages_bytes
            .checked_add(index_bytes)
            .and_then(|bytes| bytes.checked_add(self.entry_data_bytes))
            .and_then(|bytes| bytes.checked_add(source_bytes))
            .ok_or(Error::Limit("manifest spool size"))?;
        let finished_reservation = spool_reservation(finished_bytes)?;
        if finished_reservation > self.paging.max_spool_bytes {
            return Err(Error::Limit("manifest spool size"));
        }
        write_reservation(&mut self.reservation, finished_reservation)?;
        self.reservation.sync_all().map_err(Error::Storage)?;
        let first_page = read_page_from_files(
            &mut self.pages,
            &mut self.page_index,
            self.paging.page_bytes,
            0,
        )?;
        let digest = *self.digest.finalize().as_bytes();
        Ok(PagedManifest {
            inner: Arc::new(StoreInner {
                summary,
                digest,
                first_page,
                page_bytes: self.paging.page_bytes,
                pages_bytes: self.pages_bytes,
                index_bytes,
                entry_bytes: self.entry_data_bytes,
                source_bytes,
                has_sources: self.source_mode == Some(true),
                journal_bytes: self.journal_bytes,
                max_path_bytes: self.config.max_path_bytes,
                max_depth: self.config.max_depth,
                paging: self.paging.clone(),
                pages: Mutex::new(self.pages),
                page_index: Mutex::new(self.page_index),
                entries: Mutex::new(self.entries),
                entries_index: Mutex::new(self.entries_index),
                sources: self.sources.map(Mutex::new),
                source_index: self.source_index.map(Mutex::new),
                _lease: self._lease,
                _dir: self.dir,
            }),
        })
    }

    fn ensure_source_files(&mut self) -> Result<()> {
        if self.sources.is_none() {
            self.sources = Some(create_spool_file(self.dir.path(), "sources.bin")?);
            self.source_index = Some(create_spool_file(self.dir.path(), "sources.idx")?);
        }
        Ok(())
    }

    fn write_source_paths(&mut self, paths: &[PathBuf]) -> Result<()> {
        let data = self
            .sources
            .as_mut()
            .ok_or(Error::Invalid("source spool"))?;
        let index = self
            .source_index
            .as_mut()
            .ok_or(Error::Invalid("source index"))?;
        for path in paths {
            let encoded = encode_source_path(path)?;
            let offset = data.seek(SeekFrom::End(0))?;
            data.write_all(&encoded).map_err(Error::Storage)?;
            let mut record = [0; SOURCE_INDEX_RECORD_BYTES as usize];
            record[..8].copy_from_slice(&offset.to_le_bytes());
            record[8..12].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
            index.write_all(&record).map_err(Error::Storage)?;
        }
        Ok(())
    }

    fn lookup_path(&mut self, key: &str) -> Result<Option<PathInfo>> {
        // ponytail: expected O(1) disk probing, capped at 4096 to reject crafted clusters;
        // use a disk B-tree if legitimate workloads ever hit this ceiling.
        let hash = blake3::hash(key.as_bytes());
        let fingerprint = &hash.as_bytes()[..8];
        let mut slot = u64::from_le_bytes(
            hash.as_bytes()[..8]
                .try_into()
                .map_err(|_| Error::Invalid("path hash"))?,
        ) & (self.path_slots - 1);
        let index = self
            .path_index
            .as_mut()
            .ok_or(Error::Invalid("path index"))?;
        let keys = self.path_keys.as_mut().ok_or(Error::Invalid("path keys"))?;
        for _ in 0..self.path_slots.min(MAX_PATH_PROBES) {
            index.seek(SeekFrom::Start(slot * PATH_INDEX_RECORD_BYTES))?;
            let mut record = [0; PATH_INDEX_RECORD_BYTES as usize];
            index.read_exact(&mut record)?;
            let raw_id = u32::from_le_bytes(
                record[..4]
                    .try_into()
                    .map_err(|_| Error::Invalid("path index"))?,
            );
            if raw_id == 0 {
                return Ok(None);
            }
            if &record[16..24] == fingerprint {
                let key_len = u32::from_le_bytes(
                    record[4..8]
                        .try_into()
                        .map_err(|_| Error::Invalid("path index"))?,
                ) as usize;
                let key_offset = u64::from_le_bytes(
                    record[8..16]
                        .try_into()
                        .map_err(|_| Error::Invalid("path index"))?,
                );
                if key_len == 0 || key_len > MAX_PATH_KEY_BYTES {
                    return Err(Error::Invalid("path index key length"));
                }
                let mut stored_key = vec![0; key_len];
                keys.seek(SeekFrom::Start(key_offset))?;
                keys.read_exact(&mut stored_key)?;
                if stored_key == key.as_bytes() {
                    return Ok(Some(PathInfo {
                        is_dir: raw_id & 0x8000_0000 != 0,
                    }));
                }
            }
            slot = (slot + 1) & (self.path_slots - 1);
        }
        Err(Error::Limit("manifest path index"))
    }

    fn insert_path(&mut self, path: &str, file_id: u32, is_dir: bool) -> Result<()> {
        let key = path.as_bytes();
        if key.is_empty() || key.len() > MAX_PATH_KEY_BYTES {
            return Err(Error::Limit("manifest path key"));
        }
        let hash = blake3::hash(path.as_bytes());
        let mut slot = u64::from_le_bytes(
            hash.as_bytes()[..8]
                .try_into()
                .map_err(|_| Error::Invalid("path hash"))?,
        ) & (self.path_slots - 1);
        let index = self
            .path_index
            .as_mut()
            .ok_or(Error::Invalid("path index"))?;
        let keys = self.path_keys.as_mut().ok_or(Error::Invalid("path keys"))?;
        let key_offset = keys.seek(SeekFrom::End(0))?;
        keys.write_all(key).map_err(Error::Storage)?;
        for _ in 0..self.path_slots.min(MAX_PATH_PROBES) {
            index.seek(SeekFrom::Start(slot * PATH_INDEX_RECORD_BYTES))?;
            let mut record = [0; PATH_INDEX_RECORD_BYTES as usize];
            index.read_exact(&mut record)?;
            let raw_id = u32::from_le_bytes(
                record[..4]
                    .try_into()
                    .map_err(|_| Error::Invalid("path index"))?,
            );
            if raw_id == 0 {
                let id = file_id
                    .checked_add(1)
                    .filter(|id| *id < 0x8000_0000)
                    .ok_or(Error::Limit("manifest file id"))?;
                let id = if is_dir { id | 0x8000_0000 } else { id };
                record[..4].copy_from_slice(&id.to_le_bytes());
                record[4..8].copy_from_slice(&(key.len() as u32).to_le_bytes());
                record[8..16].copy_from_slice(&key_offset.to_le_bytes());
                record[16..24].copy_from_slice(&hash.as_bytes()[..8]);
                index.seek(SeekFrom::Start(slot * PATH_INDEX_RECORD_BYTES))?;
                index.write_all(&record).map_err(Error::Storage)?;
                return Ok(());
            }
            slot = (slot + 1) & (self.path_slots - 1);
        }
        Err(Error::Limit("manifest path index"))
    }
}

#[derive(Clone, Copy)]
struct PathInfo {
    is_dir: bool,
}

#[derive(Clone)]
pub struct PagedSendPlan {
    pub manifest: PagedManifest,
}

/// Stream a tree into bounded pages. Only the configured DFS depth and one page of entries and
/// source paths are retained at a time; each file is hashed in a 64 KiB buffer.
pub async fn build_manifest_paged(
    roots: Vec<PathBuf>,
    transfer_id: String,
    clip_id: String,
    config: Config,
    paging: PagingConfig,
    spool_dir: impl AsRef<Path>,
    cancel: Cancel,
) -> Result<PagedSendPlan> {
    let spool_dir = spool_dir.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || {
        build_manifest_paged_blocking(
            roots,
            transfer_id,
            clip_id,
            config,
            paging,
            spool_dir,
            cancel,
        )
    })
    .await
    .map_err(|_| Error::Worker)?
}

fn build_manifest_paged_blocking(
    roots: Vec<PathBuf>,
    transfer_id: String,
    clip_id: String,
    config: Config,
    paging: PagingConfig,
    spool_dir: PathBuf,
    cancel: Cancel,
) -> Result<PagedSendPlan> {
    config.validate()?;
    paging.validate(&config)?;
    crate::validate_id(&transfer_id)?;
    crate::validate_id(&clip_id)?;
    paging.check_chunk_size(config.chunk_size)?;
    if roots.is_empty() || roots.len() > paging.max_roots {
        return Err(Error::Limit("root count"));
    }
    let mut spool = PageSpool::new(&spool_dir, config.clone(), paging.clone())?;
    let mut files = ManifestFiles::new();
    let mut paths = Vec::with_capacity(paging.page_entries);
    let mut stack: Vec<(fs::ReadDir, String)> = Vec::new();
    let mut roots = roots.into_iter();
    let mut buffer = vec![0; 64 * 1024];
    let mut total = 0u64;
    let mut item_id = 0u32;
    let mut page_no = 0u32;
    loop {
        cancel.check()?;
        let next = if let Some((iter, parent)) = stack.last_mut() {
            match iter.next() {
                Some(entry) => Some((entry?.path(), parent.clone())),
                None => {
                    stack.pop();
                    continue;
                }
            }
        } else {
            roots.next().map(|path| (path, String::new()))
        };
        let Some((path, parent)) = next else { break };
        if path.as_os_str().as_encoded_bytes().len() > MAX_SOURCE_PATH_BYTES {
            return Err(Error::Limit("absolute source path"));
        }
        if item_id as usize >= paging.max_entries {
            return Err(Error::Limit("entry count"));
        }
        filesystem::check_chain(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if filesystem::link(&metadata) || !(metadata.is_file() || metadata.is_dir()) {
            return Err(Error::Invalid("source symlink/special file"));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Invalid("source name is not UTF-8"))?;
        filesystem::validate_name(name)?;
        let relative = if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}/{name}")
        };
        filesystem::validate_relative(&relative, config.max_depth, config.max_path_bytes)?;
        let size = if metadata.is_dir() { 0 } else { metadata.len() };
        if size > config.max_file_bytes {
            return Err(Error::Limit("file size"));
        }
        let chunks = size.div_ceil(config.chunk_size as u64);
        if chunks > u64::from(config.max_chunks_per_file) {
            return Err(Error::Limit("chunk count"));
        }
        total = total
            .checked_add(size)
            .filter(|bytes| *bytes <= config.max_transfer_bytes && *bytes <= MAX_TRANSFER_BYTES)
            .ok_or(Error::Limit("transfer size"))?;
        let digest = if metadata.is_dir() {
            None
        } else {
            let mut file = filesystem::open_file(&path, false, false)?;
            let mut hash = blake3::Hasher::new();
            loop {
                cancel.check()?;
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            if file.metadata()?.len() != size {
                return Err(Error::Invalid("source changed"));
            }
            Some(*hash.finalize().as_bytes())
        };
        if files.len() == paging.page_entries {
            let page = FileManifest {
                transfer_id: transfer_id.clone(),
                clip_id: clip_id.clone(),
                chunk_size: config.chunk_size as u32,
                page: page_no,
                final_page: false,
                files: std::mem::replace(&mut files, ManifestFiles::new()),
            };
            spool.push(&page, Some(&paths), &cancel)?;
            paths.clear();
            page_no += 1;
        }
        files
            .push(FileManifestEntry {
                file_id: item_id,
                relative_path: relative.clone(),
                size,
                is_dir: metadata.is_dir(),
                blake3_hash: digest,
            })
            .map_err(|_| Error::Limit("manifest page count"))?;
        paths.push(path.clone());
        item_id += 1;
        if metadata.is_dir() {
            stack.push((fs::read_dir(path)?, relative));
        }
    }
    if files.is_empty() {
        return Err(Error::Invalid("empty manifest"));
    }
    let page = FileManifest {
        transfer_id,
        clip_id,
        chunk_size: config.chunk_size as u32,
        page: page_no,
        final_page: true,
        files,
    };
    spool.push(&page, Some(&paths), &cancel)?;
    Ok(PagedSendPlan {
        manifest: spool.finish()?,
    })
}

fn estimated_manifest_page_bytes(page: &FileManifest) -> Result<usize> {
    let mut size = 1u64; // TransferMessage::FileManifest discriminant.
    size = size
        .checked_add(varint_len(page.transfer_id.len() as u64) + page.transfer_id.len() as u64)
        .and_then(|sum| {
            sum.checked_add(varint_len(page.clip_id.len() as u64) + page.clip_id.len() as u64)
        })
        .and_then(|sum| sum.checked_add(varint_len(u64::from(page.chunk_size))))
        .and_then(|sum| sum.checked_add(varint_len(u64::from(page.page)) + 1))
        .and_then(|sum| sum.checked_add(varint_len(page.files.len() as u64)))
        .ok_or(Error::Limit("manifest page bytes"))?;
    for entry in page.files.iter() {
        size = size
            .checked_add(varint_len(u64::from(entry.file_id)))
            .and_then(|sum| sum.checked_add(varint_len(entry.relative_path.len() as u64)))
            .and_then(|sum| sum.checked_add(entry.relative_path.len() as u64))
            .and_then(|sum| sum.checked_add(varint_len(entry.size)))
            .and_then(|sum| sum.checked_add(2 + u64::from(entry.blake3_hash.is_some()) * 32))
            .ok_or(Error::Limit("manifest page bytes"))?;
    }
    usize::try_from(size).map_err(|_| Error::Limit("manifest page bytes"))
}

fn varint_len(mut value: u64) -> u64 {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn create_spool_file(dir: &Path, name: &str) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(dir.join(name))
        .map_err(Error::Storage)
}

fn check_disk_available(path: &Path, additional: u64, reserve: u64) -> Result<()> {
    let required = additional
        .checked_add(reserve)
        .ok_or(Error::Limit("manifest spool reservation"))?;
    if filesystem::disk_available(path)? < required {
        return Err(Error::DiskSpace);
    }
    Ok(())
}

fn spool_reservation(logical_bytes: u64) -> Result<u64> {
    logical_bytes
        .checked_add(RESERVATION_MARKER_BYTES)
        .ok_or(Error::Limit("manifest spool reservation"))
}

fn write_reservation(file: &mut File, bytes: u64) -> Result<()> {
    file.seek(SeekFrom::Start(0)).map_err(Error::Storage)?;
    file.write_all(&bytes.to_le_bytes())
        .map_err(Error::Storage)?;
    file.set_len(RESERVATION_MARKER_BYTES)
        .map_err(Error::Storage)?;
    file.sync_all().map_err(Error::Storage)
}

fn write_page_index(
    index: &mut File,
    offset: u64,
    len: u32,
    first_id: u32,
    count: u32,
) -> Result<()> {
    let mut record = [0; PAGE_INDEX_RECORD_BYTES as usize];
    record[..8].copy_from_slice(&offset.to_le_bytes());
    record[8..12].copy_from_slice(&len.to_le_bytes());
    record[12..16].copy_from_slice(&first_id.to_le_bytes());
    record[16..20].copy_from_slice(&count.to_le_bytes());
    index.write_all(&record).map_err(Error::Storage)
}

fn write_entry_record(
    entries: &mut File,
    index: &mut File,
    entry: &FileManifestEntry,
) -> Result<()> {
    let path = entry.relative_path.as_bytes();
    let path_len = u32::try_from(path.len()).map_err(|_| Error::Limit("manifest path length"))?;
    let offset = entries.seek(SeekFrom::End(0))?;
    entries
        .write_all(&path_len.to_le_bytes())
        .and_then(|()| entries.write_all(path))
        .and_then(|()| entries.write_all(&entry.size.to_le_bytes()))
        .and_then(|()| entries.write_all(&[u8::from(entry.is_dir)]))
        .map_err(Error::Storage)?;
    match entry.blake3_hash {
        Some(hash) => {
            entries.write_all(&[1]).map_err(Error::Storage)?;
            entries.write_all(&hash).map_err(Error::Storage)?;
        }
        None => {
            entries.write_all(&[0]).map_err(Error::Storage)?;
            entries.write_all(&[0; 32]).map_err(Error::Storage)?;
        }
    }
    let len = ENTRY_FIXED_BYTES
        .checked_add(path.len() as u64)
        .ok_or(Error::Limit("manifest entry size"))?;
    let mut record = [0; ENTRY_INDEX_RECORD_BYTES as usize];
    record[..8].copy_from_slice(&offset.to_le_bytes());
    record[8..12].copy_from_slice(
        &u32::try_from(len)
            .map_err(|_| Error::Limit("manifest entry size"))?
            .to_le_bytes(),
    );
    index.write_all(&record).map_err(Error::Storage)
}

fn read_entry_from_files(
    entries: &mut File,
    index: &mut File,
    file_id: u32,
    max_path_bytes: usize,
    max_depth: usize,
) -> Result<FileManifestEntry> {
    index.seek(SeekFrom::Start(
        u64::from(file_id)
            .checked_mul(ENTRY_INDEX_RECORD_BYTES)
            .ok_or(Error::Limit("entry index offset"))?,
    ))?;
    let mut index_record = [0; ENTRY_INDEX_RECORD_BYTES as usize];
    index.read_exact(&mut index_record)?;
    let offset = u64::from_le_bytes(
        index_record[..8]
            .try_into()
            .map_err(|_| Error::Invalid("entry index"))?,
    );
    let len = u32::from_le_bytes(
        index_record[8..12]
            .try_into()
            .map_err(|_| Error::Invalid("entry index"))?,
    ) as usize;
    let maximum = max_path_bytes
        .checked_add(ENTRY_FIXED_BYTES as usize)
        .ok_or(Error::Limit("entry record size"))?;
    if len < ENTRY_FIXED_BYTES as usize || len > maximum {
        return Err(Error::Invalid("entry record size"));
    }
    let mut bytes = vec![0; len];
    entries.seek(SeekFrom::Start(offset))?;
    entries.read_exact(&mut bytes)?;
    let path_len = u32::from_le_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| Error::Invalid("entry path length"))?,
    ) as usize;
    if path_len == 0
        || path_len > max_path_bytes
        || path_len.checked_add(ENTRY_FIXED_BYTES as usize) != Some(len)
    {
        return Err(Error::Invalid("entry path length"));
    }
    let path_end = 4 + path_len;
    let relative_path = std::str::from_utf8(&bytes[4..path_end])
        .map_err(|_| Error::Invalid("entry path encoding"))?
        .to_owned();
    filesystem::validate_relative(&relative_path, max_depth, max_path_bytes)?;
    let size = u64::from_le_bytes(
        bytes[path_end..path_end + 8]
            .try_into()
            .map_err(|_| Error::Invalid("entry size"))?,
    );
    let is_dir = match bytes[path_end + 8] {
        0 => false,
        1 => true,
        _ => return Err(Error::Invalid("entry kind")),
    };
    let hash = match bytes[path_end + 9] {
        0 => None,
        1 => Some(
            bytes[path_end + 10..path_end + 42]
                .try_into()
                .map_err(|_| Error::Invalid("entry digest"))?,
        ),
        _ => return Err(Error::Invalid("entry digest")),
    };
    if (is_dir && (size != 0 || hash.is_some())) || (!is_dir && hash.is_none()) {
        return Err(Error::Invalid("entry metadata"));
    }
    Ok(FileManifestEntry {
        file_id,
        relative_path,
        size,
        is_dir,
        blake3_hash: hash,
    })
}

fn read_page_index(index: &mut File, page: u32) -> Result<PageRecord> {
    index.seek(SeekFrom::Start(
        u64::from(page)
            .checked_mul(PAGE_INDEX_RECORD_BYTES)
            .ok_or(Error::Limit("page index offset"))?,
    ))?;
    let mut record = [0; PAGE_INDEX_RECORD_BYTES as usize];
    index.read_exact(&mut record)?;
    Ok(PageRecord {
        offset: u64::from_le_bytes(
            record[..8]
                .try_into()
                .map_err(|_| Error::Invalid("page index"))?,
        ),
        len: u32::from_le_bytes(
            record[8..12]
                .try_into()
                .map_err(|_| Error::Invalid("page index"))?,
        ),
        first_id: u32::from_le_bytes(
            record[12..16]
                .try_into()
                .map_err(|_| Error::Invalid("page index"))?,
        ),
        count: u32::from_le_bytes(
            record[16..20]
                .try_into()
                .map_err(|_| Error::Invalid("page index"))?,
        ),
    })
}

#[derive(Clone, Copy)]
struct PageRecord {
    offset: u64,
    len: u32,
    first_id: u32,
    count: u32,
}

fn read_page_from_files(
    pages: &mut File,
    index: &mut File,
    page_bytes: usize,
    page_no: u32,
) -> Result<FileManifest> {
    let record = read_page_index(index, page_no)?;
    let len = record.len as usize;
    if len == 0 || len > page_bytes {
        return Err(Error::Invalid("spooled manifest page size"));
    }
    let mut bytes = vec![0; len];
    pages.seek(SeekFrom::Start(record.offset))?;
    pages.read_exact(&mut bytes)?;
    let TransferMessage::FileManifest(page) = codec::decode_transfer(&bytes)? else {
        return Err(Error::Invalid("spooled manifest page type"));
    };
    if page.files.len() != record.count as usize
        || page.files.first().map(|file| file.file_id) != Some(record.first_id)
    {
        return Err(Error::Invalid("spooled manifest index"));
    }
    Ok(page)
}

fn validate_source_path(path: &Path, entry: &FileManifestEntry) -> Result<()> {
    let encoded_len = source_path_storage_len(path)?;
    if encoded_len == 0 || encoded_len > MAX_SOURCE_PATH_BYTES {
        return Err(Error::Limit("absolute source path"));
    }
    filesystem::check_chain(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if filesystem::link(&metadata)
        || metadata.is_dir() != entry.is_dir
        || !(metadata.is_file() || metadata.is_dir())
        || (!entry.is_dir && metadata.len() != entry.size)
    {
        return Err(Error::Invalid("source changed"));
    }
    Ok(())
}

fn encode_source_path(path: &Path) -> Result<Vec<u8>> {
    let len = source_path_storage_len(path)?;
    if len == 0 || len > MAX_SOURCE_PATH_BYTES {
        return Err(Error::Limit("absolute source path"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for unit in wide {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        if bytes.len() > MAX_SOURCE_PATH_BYTES {
            return Err(Error::Limit("absolute source path"));
        }
        Ok(bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(path.to_string_lossy().as_bytes().to_vec())
    }
}

fn source_path_storage_len(path: &Path) -> Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes().len())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .count()
            .checked_mul(2)
            .ok_or(Error::Limit("absolute source path"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(path.to_string_lossy().len())
    }
}

fn decode_source_path(bytes: &[u8]) -> Result<PathBuf> {
    if bytes.is_empty() || bytes.len() > MAX_SOURCE_PATH_BYTES {
        return Err(Error::Invalid("source path length"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if !bytes.len().is_multiple_of(2) {
            return Err(Error::Invalid("source path encoding"));
        }
        let wide = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let value =
            std::str::from_utf8(bytes).map_err(|_| Error::Invalid("source path encoding"))?;
        Ok(PathBuf::from(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paging() -> PagingConfig {
        PagingConfig {
            max_entries: 32,
            max_manifest_bytes: 1 << 20,
            page_entries: 4,
            page_bytes: 4096,
            max_roots: 8,
            min_chunk_size: 2,
            max_chunk_size: 1024,
            max_spool_bytes: 1 << 20,
        }
    }

    fn entry(file_id: u32, path: &str, is_dir: bool) -> FileManifestEntry {
        FileManifestEntry {
            file_id,
            relative_path: path.to_owned(),
            size: if is_dir { 0 } else { 2 },
            is_dir,
            blake3_hash: (!is_dir).then_some([file_id as u8; 32]),
        }
    }

    fn page(page: u32, final_page: bool, entries: Vec<FileManifestEntry>) -> FileManifest {
        FileManifest {
            transfer_id: "transfer".to_owned(),
            clip_id: "clip".to_owned(),
            chunk_size: 2,
            page,
            final_page,
            files: ManifestFiles::try_from_vec(entries).expect("bounded test page"),
        }
    }

    fn push(spool: &mut PageSpool, page: &FileManifest) -> Result<()> {
        spool.push(page, None, &Cancel::new())
    }

    // macOS keeps its temp folder behind a system link (/var -> /private/var) and the spool refuses links in its
    // path, so these tests start from the real location (they failed on every Mac otherwise).
    fn real_tempdir() -> tempfile::TempDir {
        let root = std::env::temp_dir().canonicalize().expect("temp dir");
        tempfile::Builder::new().tempdir_in(root).expect("tempdir")
    }

    #[test]
    fn page_order_contiguity_identity_and_digest_are_enforced() {
        let temp = real_tempdir();
        let mut skipped = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        assert!(push(&mut skipped, &page(1, true, vec![entry(0, "a", false)])).is_err());

        let mut replay = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        let first = page(0, false, vec![entry(0, "a", false)]);
        push(&mut replay, &first).expect("first page");
        assert!(push(&mut replay, &first).is_err());

        let mut changed = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        push(&mut changed, &page(0, false, vec![entry(0, "a", false)])).expect("first page");
        let mut other = page(1, true, vec![entry(1, "b", false)]);
        other.clip_id = "other".to_owned();
        assert!(push(&mut changed, &other).is_err());

        let mut good = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        let pages = [
            page(0, false, vec![entry(0, "a", false)]),
            page(1, true, vec![entry(1, "b", false)]),
        ];
        for page in &pages {
            push(&mut good, page).expect("ordered page");
        }
        let stored = good.finish().expect("complete manifest");
        assert_eq!(stored.summary().items, 2);
        assert_eq!(stored.entry(1).expect("entry").relative_path, "b");
        assert_eq!(
            stored.approve(),
            Consent::approve_pages(pages.iter()).expect("approval")
        );
    }

    #[test]
    fn names_require_prior_explicit_directory_and_reject_case_aliases() {
        let temp = real_tempdir();
        let mut spool = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        assert!(push(
            &mut spool,
            &page(0, false, vec![entry(0, "dir/file", false)])
        )
        .is_err());

        let mut spool = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        push(&mut spool, &page(0, false, vec![entry(0, "dir", true)])).expect("directory");
        assert!(push(&mut spool, &page(1, true, vec![entry(1, "DIR", true)])).is_err());

        let mut spool = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        push(&mut spool, &page(0, false, vec![entry(0, "dir", true)])).expect("directory");
        push(
            &mut spool,
            &page(1, true, vec![entry(1, "dir/file", false)]),
        )
        .expect("child");
        let stored = spool.finish().expect("complete manifest");
        assert_eq!(stored.read_page(1).expect("page").files[0].file_id, 1);
        assert!(stored.spool_bytes() > 0);
    }

    #[test]
    fn chunk_range_and_page_byte_limits_are_checked_before_encoding() {
        let temp = real_tempdir();
        let mut spool = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        let mut invalid_chunk = page(0, true, vec![entry(0, "a", false)]);
        invalid_chunk.chunk_size = 1;
        assert!(push(&mut spool, &invalid_chunk).is_err());

        let mut too_large = paging();
        too_large.page_bytes = 32;
        let mut spool = PageSpool::new(temp.path(), Config::default(), too_large).expect("spool");
        let page = page(0, true, vec![entry(0, "a", false)]);
        assert!(estimated_manifest_page_bytes(&page).expect("size") > 32);
        assert!(push(&mut spool, &page).is_err());
    }

    #[test]
    fn encoded_size_preflight_matches_canonical_postcard_bytes() {
        let page = page(0, true, vec![entry(0, "file-name.txt", false)]);
        let bytes = codec::encode_transfer(&TransferMessage::FileManifest(page.clone()))
            .expect("encoded page");
        assert_eq!(
            estimated_manifest_page_bytes(&page).expect("estimate"),
            bytes.len()
        );
    }

    #[test]
    fn source_paths_round_trip_without_a_transfer_wide_path_vector() {
        let temp = real_tempdir();
        let source = temp.path().join("source");
        fs::write(&source, [1, 2]).expect("source file");
        let mut spool = PageSpool::new(temp.path(), Config::default(), paging()).expect("spool");
        let page = page(0, true, vec![entry(0, "source", false)]);
        spool
            .push(&page, Some(std::slice::from_ref(&source)), &Cancel::new())
            .expect("page");
        let stored = spool.finish().expect("complete manifest");
        assert_eq!(stored.source(0).expect("source path"), source);
    }
}
