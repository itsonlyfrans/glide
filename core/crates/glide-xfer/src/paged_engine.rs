use crate::engine::validate_ranges;
use crate::filesystem;
use crate::receiver::Session;
use crate::*;
use glide_proto::{codec, wire::*};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// A complete published tree and its disk-spooled manifest. Keep its lease alive
/// while clipboard URLs refer to the paths, just as with `Received`.
pub struct ReceivedPaged {
    pub paths: Vec<PathBuf>,
    pub manifest: PagedManifest,
    _lease: File,
}

/// A confirmation prompt retains the complete bounded spool for review/approval.
/// Retry on fresh streams with `manifest.approve()` after human approval.
pub enum PagedReceive {
    Received(ReceivedPaged),
    NeedsApproval(PagedManifest),
}

impl FileEngine {
    /// Opt-in paged protocol. Manifest pages finish before resume or chunk traffic.
    pub async fn send_paged<C, W>(
        &self,
        plan: PagedSendPlan,
        control: &mut C,
        lanes: Vec<W>,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<()>
    where
        C: AsyncRead + AsyncWrite + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let _permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        self.check_lanes(lanes.len())?;
        let manifest = plan.manifest;
        let summary = manifest.summary();
        if summary.chunk_size as usize != self.config.chunk_size
            || summary.bytes > self.config.max_transfer_bytes
        {
            return Err(Error::Invalid("paged source configuration"));
        }
        let id = manifest.first_page().transfer_id.clone();
        let source_manifest = manifest.clone();
        let source_config = self.config.clone();
        let preflight_cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            for id in 0..source_manifest.summary().items {
                preflight_cancel.check()?;
                let entry = source_manifest.entry(id)?;
                filesystem::validate_relative(
                    &entry.relative_path,
                    source_config.max_depth,
                    source_config.max_path_bytes,
                )?;
                if entry.size > source_config.max_file_bytes {
                    return Err(Error::Limit("file size"));
                }
                source_config.chunks(entry.size)?;
            }
            Ok::<_, Error>(())
        })
        .await
        .map_err(|_| Error::Worker)??;
        for index in 0..summary.pages {
            cancel.check()?;
            let spool = manifest.clone();
            let page = tokio::task::spawn_blocking(move || spool.read_page(index))
                .await
                .map_err(|_| Error::Worker)??;
            write_message(
                control,
                &TransferMessage::FileManifest(page),
                self.config.operation_timeout,
                cancel,
            )
            .await?;
        }
        let staging = self.staging.clone();
        let reserve = self.config.disk_reserve_bytes;
        let resume_limit = manifest
            .limits()
            .max_spool_bytes
            .checked_sub(manifest.spool_bytes())
            .ok_or(Error::Limit("resume spool size"))?
            .min(self.config.max_staging_bytes);
        let _guard = self.staging_lock.lock().await;
        let resume_config = self.config.clone();
        let mut resumes = tokio::task::spawn_blocking(move || {
            ResumeWriter::new(&staging, resume_limit, reserve, resume_config)
        })
        .await
        .map_err(|_| Error::Worker)??;
        drop(_guard);
        let mut missing = 0u64;
        for file_id in 0..summary.items {
            cancel.check()?;
            let spool = manifest.clone();
            let entry = tokio::task::spawn_blocking(move || spool.entry(file_id))
                .await
                .map_err(|_| Error::Worker)??;
            let ranges = if entry.is_dir {
                ResumeRanges::new()
            } else {
                let resume = crate::stream::read_resume_message(
                    control,
                    self.config.operation_timeout,
                    cancel,
                )
                .await?;
                if resume.transfer_id != id || resume.file_id != file_id {
                    return Err(Error::Invalid("resume session/file"));
                }
                missing = missing
                    .checked_add(validate_ranges(
                        &resume.missing_chunks,
                        entry.size,
                        &self.config,
                    )?)
                    .ok_or(Error::Limit("resume byte count"))?;
                resume.missing_chunks
            };
            let resume = FileResume {
                transfer_id: id.clone(),
                file_id,
                missing_chunks: ranges,
            };
            let _guard = self.staging_lock.lock().await;
            resumes = tokio::task::spawn_blocking(move || {
                resumes.push(resume)?;
                Ok::<_, Error>(resumes)
            })
            .await
            .map_err(|_| Error::Worker)??;
            drop(_guard);
        }
        progress.reset(
            summary
                .bytes
                .checked_sub(missing)
                .ok_or(Error::Invalid("resume total"))?,
        );
        let resumes = Arc::new(
            tokio::task::spawn_blocking(move || resumes.finish())
                .await
                .map_err(|_| Error::Worker)??,
        );
        let limiter = Arc::new(Mutex::new(Instant::now()));
        let stop = Cancel::new();
        let mut workers = JoinSet::new();
        let lane_count = lanes.len() as u32;
        for (index, lane) in lanes.into_iter().enumerate() {
            workers.spawn(send_paged_lane(
                manifest.clone(),
                resumes.clone(),
                lane,
                index as u32,
                lane_count,
                self.config.clone(),
                cancel.clone(),
                stop.clone(),
                progress.clone(),
                limiter.clone(),
            ));
        }
        let result = async {
            while let Some(result) = workers.join_next().await {
                result.map_err(|_| Error::Worker)??;
            }
            write_message(
                control,
                &TransferMessage::FileComplete(FileComplete {
                    transfer_id: id.clone(),
                }),
                self.config.operation_timeout,
                cancel,
            )
            .await?;
            match read_message(control, self.config.operation_timeout, cancel).await? {
                TransferMessage::FileComplete(done) if done.transfer_id == id => Ok(()),
                _ => Err(Error::Invalid("expected completion acknowledgement")),
            }
        }
        .await;
        if result.is_err() {
            stop.cancel();
            workers.abort_all();
            while workers.join_next().await.is_some() {}
        }
        result
    }

    /// Negotiated chunk geometry, paged approval and coalesced resume are opt-in.
    #[allow(clippy::too_many_arguments)]
    pub async fn receive_paged<C, R>(
        &self,
        peer_id: &str,
        control: &mut C,
        lanes: Vec<R>,
        mut paging: PagingConfig,
        consent: Consent,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<PagedReceive>
    where
        C: AsyncRead + AsyncWrite + Unpin,
        R: AsyncRead + Unpin + Send + 'static,
    {
        let _permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        self.check_lanes(lanes.len())?;
        if !crate::engine::valid_session_name(peer_id) {
            return Err(Error::Invalid("authenticated peer id"));
        }
        paging.max_spool_bytes = paging.max_spool_bytes.min(self.config.max_staging_bytes);
        paging.validate(&self.config)?;
        let first = crate::stream::read_manifest_page(
            control,
            paging
                .page_bytes
                .min(paging.max_manifest_bytes.min(usize::MAX as u64) as usize),
            paging.page_entries.min(paging.max_entries),
            &self.config,
            self.config.operation_timeout,
            cancel,
        )
        .await?;
        let bounds = ChunkSizeBounds {
            min: paging.min_chunk_size,
            max: paging.max_chunk_size,
        };
        bounds.accepts(first.chunk_size as usize)?;
        let mut config = self.config.clone();
        config.chunk_size = first.chunk_size as usize;
        config.validate()?;
        let staging = self.staging.clone();
        let spool_config = config.clone();
        let spool_limits = paging.clone();
        let _guard = self.staging_lock.lock().await;
        let mut spool = tokio::task::spawn_blocking(move || {
            PageSpool::new(staging, spool_config, spool_limits)
        })
        .await
        .map_err(|_| Error::Worker)??;
        drop(_guard);
        let mut page = first;
        loop {
            let final_page = page.final_page;
            let task_cancel = cancel.clone();
            let _guard = self.staging_lock.lock().await;
            let (next, result) = tokio::task::spawn_blocking(move || {
                let result = spool
                    .push(&page, None, &task_cancel)
                    .map_err(Error::storage);
                (spool, result)
            })
            .await
            .map_err(|_| Error::Worker)?;
            spool = next;
            drop(_guard);
            result?;
            if final_page {
                break;
            }
            if spool.remaining_entries() == 0 || spool.remaining_manifest_bytes() == 0 {
                return Err(Error::Limit("manifest aggregate size"));
            }
            let mut remaining_config = config.clone();
            remaining_config.max_transfer_bytes = spool.remaining_transfer_bytes();
            page = crate::stream::read_manifest_page(
                control,
                paging
                    .page_bytes
                    .min(spool.remaining_manifest_bytes().min(usize::MAX as u64) as usize),
                paging.page_entries.min(spool.remaining_entries()),
                &remaining_config,
                config.operation_timeout,
                cancel,
            )
            .await?;
        }
        let _guard = self.staging_lock.lock().await;
        let manifest = tokio::task::spawn_blocking(move || spool.finish())
            .await
            .map_err(|_| Error::Worker)??;
        drop(_guard);
        match consent {
            Consent::Automatic if manifest.summary().bytes > config.max_auto_bytes => {
                return Ok(PagedReceive::NeedsApproval(manifest))
            }
            Consent::Approved { .. } if consent != manifest.approve() => {
                return Err(Error::Invalid("approval manifest mismatch"))
            }
            _ => {}
        }
        let _guard = self.staging_lock.lock().await;
        let staging = self.staging.clone();
        let peer = peer_id.to_owned();
        let opening_cancel = cancel.clone();
        let pages = manifest.clone();
        let first = manifest.first_page().clone();
        let session = tokio::task::spawn_blocking(move || {
            Session::open_with_options(
                staging,
                peer,
                first,
                config,
                &opening_cancel,
                Some(pages),
                Some(bounds),
                true,
            )
            .map_err(Error::storage)
        })
        .await
        .map_err(|_| Error::Worker)??;
        drop(_guard);
        let id = manifest.first_page().transfer_id.clone();
        let (mut session, result) = self
            .receive_session(session, control, lanes, cancel, progress)
            .await?;
        match result {
            Ok(()) => {
                let received =
                    tokio::task::spawn_blocking(move || session.publish().map_err(Error::storage))
                        .await
                        .map_err(|_| Error::Worker)??;
                write_message(
                    control,
                    &TransferMessage::FileComplete(FileComplete { transfer_id: id }),
                    self.config.operation_timeout,
                    cancel,
                )
                .await?;
                Ok(PagedReceive::Received(ReceivedPaged {
                    paths: received.paths,
                    manifest,
                    _lease: received._lease,
                }))
            }
            Err(error) => {
                if matches!(error, Error::Io(_)) {
                    session.preserve();
                } else {
                    tokio::task::spawn_blocking(move || session.discard().map_err(Error::storage))
                        .await
                        .map_err(|_| Error::Worker)??;
                }
                Err(error)
            }
        }
    }
}

const RESUME_BYTES: usize = MAX_RESUME_RANGES * 10 + MAX_TRANSFER_ID_BYTES + 64;

struct ResumeWriter {
    index: File,
    data: File,
    lease: File,
    reservation: File,
    count: u32,
    max_bytes: u64,
    reserve: u64,
    config: Config,
    root: tempfile::TempDir,
}

struct ResumeSpool {
    _lease: File,
    count: u32,
    root: tempfile::TempDir,
}

impl ResumeWriter {
    fn new(parent: &std::path::Path, max_bytes: u64, reserve: u64, config: Config) -> Result<Self> {
        crate::receiver::check_spool_quota(parent, None, 8, &config)?;
        let root = tempfile::Builder::new()
            .prefix("glide-resume-")
            .tempdir_in(parent)?;
        let index = filesystem::open_file(&root.path().join("index"), true, true)?;
        let data = filesystem::open_file(&root.path().join("ranges"), true, true)?;
        let lease = filesystem::lease(&root.path().join("lease"))?;
        let mut reservation = filesystem::open_file(&root.path().join("reservation"), true, true)?;
        reservation.write_all(&8u64.to_le_bytes())?;
        reservation.sync_all()?;
        Ok(Self {
            index,
            data,
            lease,
            reservation,
            root,
            count: 0,
            max_bytes,
            reserve,
            config,
        })
    }
    fn push(&mut self, resume: FileResume) -> Result<()> {
        if resume.file_id != self.count || self.count >= MAX_TRANSFER_ITEMS {
            return Err(Error::Invalid("resume spool sequence"));
        }
        // The wire collection is already bounded; preflight the disk reservation
        // conservatively before encoding even the small resume frame.
        let upper = (MAX_TRANSFER_ID_BYTES + 24 + resume.missing_chunks.len() * 10) as u64;
        let used = self.data.stream_position()? + u64::from(self.count) * 8;
        if used
            .checked_add(upper + 12)
            .is_none_or(|bytes| bytes > self.max_bytes)
        {
            return Err(Error::Limit("resume spool size"));
        }
        if filesystem::disk_available(self.root.path())?
            < upper.saturating_add(12).saturating_add(self.reserve)
        {
            return Err(Error::DiskSpace);
        }
        let bytes = codec::encode_transfer(&TransferMessage::FileResume(resume))?;
        if bytes.len() > RESUME_BYTES {
            return Err(Error::Limit("resume frame"));
        }
        let required = used + bytes.len() as u64 + 12 + 8;
        crate::receiver::check_spool_quota(
            self.root
                .path()
                .parent()
                .ok_or(Error::Invalid("spool parent"))?,
            Some(self.root.path()),
            required,
            &self.config,
        )?;
        self.reservation.seek(SeekFrom::Start(0))?;
        self.reservation.write_all(&required.to_le_bytes())?;
        self.reservation.flush()?;
        self.lease.set_modified(std::time::SystemTime::now())?;
        self.index
            .write_all(&self.data.stream_position()?.to_le_bytes())?;
        self.data.write_all(&(bytes.len() as u32).to_le_bytes())?;
        self.data.write_all(&bytes)?;
        self.count += 1;
        Ok(())
    }
    fn finish(mut self) -> Result<ResumeSpool> {
        self.index.flush()?;
        self.data.flush()?;
        drop(self.index);
        drop(self.data);
        drop(self.reservation);
        Ok(ResumeSpool {
            root: self.root,
            count: self.count,
            _lease: self.lease,
        })
    }
}

impl ResumeSpool {
    fn get(&self, id: u32) -> Result<ResumeRanges> {
        if id >= self.count {
            return Err(Error::Invalid("resume file id"));
        }
        let mut index = filesystem::open_file(&self.root.path().join("index"), false, false)?;
        index.seek(SeekFrom::Start(u64::from(id) * 8))?;
        let mut offset = [0; 8];
        index.read_exact(&mut offset)?;
        let mut data = filesystem::open_file(&self.root.path().join("ranges"), false, false)?;
        data.seek(SeekFrom::Start(u64::from_le_bytes(offset)))?;
        let mut length = [0; 4];
        data.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > RESUME_BYTES {
            return Err(Error::Limit("resume frame"));
        }
        let mut bytes = vec![0; length];
        data.read_exact(&mut bytes)?;
        let TransferMessage::FileResume(resume) = codec::decode_transfer(&bytes)? else {
            return Err(Error::Invalid("resume spool"));
        };
        if resume.file_id != id {
            return Err(Error::Invalid("resume spool id"));
        }
        Ok(resume.missing_chunks)
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_paged_lane<W: AsyncWrite + Unpin>(
    manifest: PagedManifest,
    resumes: Arc<ResumeSpool>,
    mut lane: W,
    index: u32,
    lane_count: u32,
    config: Config,
    cancel: Cancel,
    stop: Cancel,
    progress: Progress,
    limiter: Arc<Mutex<Instant>>,
) -> Result<()> {
    let mut encoder = ChunkEncoder::new(&config)?;
    for file_id in 0..manifest.summary().items {
        cancel.check()?;
        stop.check()?;
        let spool = manifest.clone();
        let resume = resumes.clone();
        let job = tokio::task::spawn_blocking(move || {
            let file = spool.entry(file_id)?;
            if file.is_dir {
                return Ok(None);
            }
            let ranges = resume.get(file_id)?;
            if ranges.is_empty() {
                return Ok(None);
            }
            let source = filesystem::open_file(&spool.source(file_id)?, false, false)?;
            if source.metadata()?.len() != file.size {
                return Err(Error::Invalid("source size changed"));
            }
            Ok(Some((file, ranges, source)))
        })
        .await
        .map_err(|_| Error::Worker)??;
        let Some((file, ranges, mut input)) = job else {
            continue;
        };
        validate_ranges(&ranges, file.size, &config)?;
        for range in ranges.iter() {
            let end = range.first_chunk + range.count;
            let mut chunk_index = range.first_chunk
                + (index + lane_count - range.first_chunk % lane_count) % lane_count;
            while chunk_index < end {
                cancel.check()?;
                stop.check()?;
                let offset = u64::from(chunk_index) * config.chunk_size as u64;
                let length = (file.size - offset).min(config.chunk_size as u64) as usize;
                let name = file.relative_path.clone();
                let (next_input, next_encoder, prepared) = tokio::task::spawn_blocking(move || {
                    input.seek(SeekFrom::Start(offset))?;
                    input.read_exact(encoder.buffer_mut(length)?)?;
                    let prepared = encoder.prepare(&name)?;
                    Ok::<_, Error>((input, encoder, prepared))
                })
                .await
                .map_err(|_| Error::Worker)??;
                input = next_input;
                encoder = next_encoder;
                if let Some(rate) = config.rate_limit_bps {
                    let when = {
                        let mut next = limiter.lock().await;
                        *next = (*next).max(Instant::now())
                            + Duration::from_secs_f64(length as f64 / rate as f64);
                        *next
                    };
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(Error::Cancelled),
                        _ = stop.cancelled() => return Err(Error::Cancelled),
                        _ = tokio::time::sleep_until(when.into()) => {}
                    }
                }
                write_file_chunk(
                    &mut lane,
                    &encoder.view(
                        &manifest.first_page().transfer_id,
                        file_id,
                        chunk_index,
                        prepared,
                    )?,
                    config.operation_timeout,
                    &cancel,
                )
                .await?;
                progress.add(length as u64);
                chunk_index += lane_count;
            }
        }
    }
    write_message(
        &mut lane,
        &TransferMessage::FileComplete(FileComplete {
            transfer_id: manifest.first_page().transfer_id.clone(),
        }),
        config.operation_timeout,
        &cancel,
    )
    .await
}
