//! Stateful page validation. Approval/resume must bind the digest of every
//! canonical encoded page in order, including IDs, chunk size and final marker.
//! Do not accept chunks or expose files until `finish` succeeds and consent is bound.
use crate::{codec::CodecError, wire::*};

pub fn validate_manifest_page(page: &FileManifest) -> Result<(), CodecError> {
    for id in [&page.transfer_id, &page.clip_id] {
        if id.is_empty() || id.len() > MAX_TRANSFER_ID_BYTES || id.chars().any(char::is_control) {
            return Err(CodecError::InvalidValue("manifest id"));
        }
    }
    if !(1..=MAX_FILE_CHUNK_BYTES as u32).contains(&page.chunk_size)
        || page.page >= MAX_TRANSFER_ITEMS
        || page.files.is_empty()
    {
        return Err(CodecError::InvalidValue("manifest geometry"));
    }
    let first = page.files[0].file_id;
    if page.page == 0 && first != 0 {
        return Err(CodecError::InvalidValue("manifest first file id"));
    }
    let mut total = 0u64;
    for (index, file) in page.files.iter().enumerate() {
        if first.checked_add(index as u32) != Some(file.file_id)
            || file.file_id >= MAX_TRANSFER_ITEMS
            || file.relative_path.is_empty()
            || file.relative_path.len() > MAX_MANIFEST_PATH_BYTES
            || file.relative_path.split('/').count() > MAX_MANIFEST_DEPTH
            || !matches!(
                (file.is_dir, file.size, file.blake3_hash),
                (true, 0, None) | (false, _, Some(_))
            )
        {
            return Err(CodecError::InvalidValue("manifest entry"));
        }
        total = total
            .checked_add(file.size)
            .filter(|total| *total <= MAX_TRANSFER_BYTES)
            .ok_or(CodecError::InvalidValue("manifest byte quota"))?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestSummary {
    pub items: u32,
    pub bytes: u64,
    pub chunk_size: u32,
    pub pages: u32,
}

/// Bounded metadata only; validated pages can be spooled and hashed by the caller.
#[derive(Default)]
pub struct ManifestValidator {
    identity: Option<(String, String, u32)>,
    pages: u32,
    items: u32,
    bytes: u64,
    complete: bool,
}

impl ManifestValidator {
    pub fn push(&mut self, page: &FileManifest) -> Result<(), CodecError> {
        validate_manifest_page(page)?;
        if self.complete
            || page.page != self.pages
            || page.files[0].file_id != self.items
            || self
                .identity
                .as_ref()
                .is_some_and(|(transfer, clip, chunk)| {
                    transfer != &page.transfer_id
                        || clip != &page.clip_id
                        || *chunk != page.chunk_size
                })
        {
            return Err(CodecError::InvalidValue("manifest page sequence/identity"));
        }
        let items = self
            .items
            .checked_add(page.files.len() as u32)
            .filter(|items| *items <= MAX_TRANSFER_ITEMS)
            .ok_or(CodecError::InvalidValue("manifest item quota"))?;
        let bytes = page
            .files
            .iter()
            .try_fold(self.bytes, |sum, file| sum.checked_add(file.size))
            .filter(|bytes| *bytes <= MAX_TRANSFER_BYTES)
            .ok_or(CodecError::InvalidValue("manifest byte quota"))?;
        if self.identity.is_none() {
            self.identity = Some((
                page.transfer_id.clone(),
                page.clip_id.clone(),
                page.chunk_size,
            ));
        }
        self.pages += 1;
        self.items = items;
        self.bytes = bytes;
        self.complete = page.final_page;
        Ok(())
    }

    pub fn finish(&self) -> Result<ManifestSummary, CodecError> {
        if !self.complete {
            return Err(CodecError::InvalidValue("incomplete manifest"));
        }
        let chunk_size = self
            .identity
            .as_ref()
            .ok_or(CodecError::InvalidValue("missing manifest"))?
            .2;
        Ok(ManifestSummary {
            items: self.items,
            bytes: self.bytes,
            chunk_size,
            pages: self.pages,
        })
    }
}
