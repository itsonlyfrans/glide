use crate::filesystem;
use crate::receiver::Session;
use crate::{
    read_message, write_file_chunk, write_message, ChunkEncoder, ChunkMessage, ChunkReader,
};
use glide_proto::{codec::CodecError, ipc, wire::*};
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch, Mutex, Semaphore};
use tokio::task::JoinSet;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("staging file I/O failed: {0}")]
    Storage(std::io::Error),
    #[error("invalid transfer: {0}")]
    Invalid(&'static str),
    #[error("transfer limit exceeded: {0}")]
    Limit(&'static str),
    #[error("file integrity check failed")]
    Integrity,
    #[error("transfer cancelled")]
    Cancelled,
    #[error("peer operation timed out")]
    Timeout,
    #[error("concurrent transfer limit reached")]
    Busy,
    #[error("transfer needs explicit confirmation ({bytes} bytes)")]
    ConfirmationRequired {
        bytes: u64,
        manifest: Box<FileManifest>,
    },
    #[error("insufficient disk space")]
    DiskSpace,
    #[error("wire decode/encode failed: {0}")]
    Codec(#[from] CodecError),
    #[error("transfer worker failed")]
    Worker,
}

#[cfg(feature = "native-transfer")]
fn finish_native<T>(handle: &glide_net::TransferHandle, result: &Result<T>) {
    if matches!(
        result,
        Err(Error::Codec(_) | Error::Invalid(_) | Error::Integrity)
    ) {
        handle.close_on_codec_failure();
    }
    if result.is_err() {
        handle.cancel();
    }
}

impl Error {
    pub(crate) fn storage(self) -> Self {
        match self {
            Self::Io(error) => Self::Storage(error),
            other => other,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub chunk_size: usize,
    pub parallel_streams: usize,
    pub max_concurrent_transfers: usize,
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_path_bytes: usize,
    pub max_file_bytes: u64,
    pub max_transfer_bytes: u64,
    pub max_chunks_per_file: u32,
    pub max_staging_sessions: usize,
    pub max_staging_bytes: u64,
    pub max_auto_bytes: u64,
    pub disk_reserve_bytes: u64,
    pub operation_timeout: Duration,
    pub stale_after: Duration,
    pub purge_interval: Duration,
    pub rate_limit_bps: Option<u64>,
}

/// Opt-in receiver geometry. The sender's manifest selects the size within these bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSizeBounds {
    pub min: usize,
    pub max: usize,
}

impl ChunkSizeBounds {
    pub fn validate(self) -> Result<()> {
        if self.min == 0 || self.min > self.max || self.max > MAX_FILE_CHUNK_BYTES {
            return Err(Error::Invalid("chunk size bounds"));
        }
        Ok(())
    }

    pub(crate) fn accepts(self, size: usize) -> Result<()> {
        self.validate()?;
        if !(self.min..=self.max).contains(&size) {
            return Err(Error::Limit("manifest chunk size"));
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            chunk_size: MAX_FILE_CHUNK_BYTES,
            parallel_streams: 4,
            max_concurrent_transfers: 4,
            max_entries: MAX_MANIFEST_FILES,
            max_depth: 32,
            max_path_bytes: 1024,
            max_file_bytes: 64 << 30,
            max_transfer_bytes: 64 << 30,
            max_chunks_per_file: 1_048_576,
            max_staging_sessions: 32,
            max_staging_bytes: 64 << 30,
            max_auto_bytes: 2048 << 20,
            disk_reserve_bytes: 64 << 20,
            operation_timeout: Duration::from_secs(30),
            stale_after: Duration::from_secs(24 * 3600),
            purge_interval: Duration::from_secs(3600),
            rate_limit_bps: None,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if self.chunk_size == 0
            || self.chunk_size > MAX_FILE_CHUNK_BYTES
            || !(1..=4).contains(&self.parallel_streams)
            || !(1..=32).contains(&self.max_concurrent_transfers)
            || self.max_entries == 0
            || self.max_entries > MAX_MANIFEST_FILES
            || self.max_depth == 0
            || self.max_depth > 64
            || self.max_path_bytes == 0
            || self.max_path_bytes > 4096
            || self.max_chunks_per_file == 0
            || self.max_chunks_per_file > 1_048_576
            || self.max_staging_sessions == 0
            || self.max_staging_sessions > 64
            || self.max_transfer_bytes > self.max_staging_bytes
            || self.operation_timeout.is_zero()
            || self.operation_timeout > Duration::from_secs(300)
            || self.purge_interval < Duration::from_millis(100)
            || self.stale_after.is_zero()
            || self.rate_limit_bps == Some(0)
        {
            return Err(Error::Invalid("engine configuration"));
        }
        if self.rate_limit_bps.is_some_and(|rate| {
            self.chunk_size as f64 * self.parallel_streams as f64 / rate as f64
                >= self.operation_timeout.as_secs_f64()
        }) {
            return Err(Error::Invalid(
                "rate limit cannot satisfy lane timeout; reduce chunk size",
            ));
        }
        Ok(())
    }

    /// Conservative bound for live Rust payload/metadata storage, independent of file size.
    /// Excludes OS/Tokio stacks and native zstd context memory; see README.
    pub fn payload_memory_ceiling(&self) -> usize {
        // Codec frames, bounded decode collections, raw/packed buffers, queue and worker slots.
        4 * MAX_FILE_CONTROL_FRAME_BYTES
            + (4 * self.parallel_streams + 2) * MAX_FILE_CHUNK_FRAME_BYTES
            + self.max_entries * (4 * self.max_path_bytes + 4096 + 512)
    }

    pub(crate) fn chunks(&self, size: u64) -> Result<u32> {
        let count = size.div_ceil(self.chunk_size as u64);
        if count > u64::from(self.max_chunks_per_file) {
            return Err(Error::Limit("chunk count"));
        }
        Ok(count as u32)
    }
}

#[derive(Clone, Debug)]
pub struct Cancel(watch::Sender<bool>);

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}
impl Cancel {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn check(&self) -> Result<()> {
        if *self.0.borrow() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    pub async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        if *receiver.borrow_and_update() {
            return;
        }
        let _ = receiver.changed().await;
    }
    pub(crate) async fn run<T, F: Future<Output = Result<T>>>(
        &self,
        duration: Duration,
        future: F,
    ) -> Result<T> {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(duration, future) => result.map_err(|_| Error::Timeout)?,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Progress(Arc<ProgressInner>);
#[derive(Debug)]
struct ProgressInner {
    done: AtomicU64,
    baseline: AtomicU64,
    started: Instant,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressSnapshot {
    pub bytes_done: u64,
    pub rate_bps: u64,
}
impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}
impl Progress {
    pub fn new() -> Self {
        Self(Arc::new(ProgressInner {
            done: AtomicU64::new(0),
            baseline: AtomicU64::new(0),
            started: Instant::now(),
        }))
    }
    pub(crate) fn add(&self, bytes: u64) {
        self.0.done.fetch_add(bytes, Ordering::Relaxed);
    }
    pub(crate) fn reset(&self, bytes: u64) {
        self.0.done.store(bytes, Ordering::Relaxed);
        self.0.baseline.store(bytes, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> ProgressSnapshot {
        let done = self.0.done.load(Ordering::Relaxed);
        let transferred = done.saturating_sub(self.0.baseline.load(Ordering::Relaxed));
        let rate = transferred as f64 / self.0.started.elapsed().as_secs_f64().max(0.001);
        ProgressSnapshot {
            bytes_done: done,
            rate_bps: rate as u64,
        }
    }
    /// Poll at most 10 Hz, then supply this directly to Core::update_transfer.
    pub fn transfer_snapshot(
        &self,
        manifest: &FileManifest,
        direction: ipc::TransferDirection,
        peer_id: &str,
        state: ipc::TransferState,
        error: Option<String>,
    ) -> ipc::Transfer {
        let snapshot = self.snapshot();
        let total = manifest
            .files
            .iter()
            .fold(0u64, |sum, file| sum.saturating_add(file.size));
        ipc::Transfer {
            id: manifest.transfer_id.clone(),
            direction,
            peer_id: peer_id.to_owned(),
            name: manifest
                .files
                .first()
                .map(|file| file.relative_path.split('/').next().unwrap_or_default())
                .unwrap_or_default()
                .to_owned(),
            items: manifest.files.len() as u32,
            bytes_total: total,
            bytes_done: snapshot.bytes_done.min(total),
            rate_bps: snapshot.rate_bps,
            state,
            error,
        }
    }

    /// Aggregate progress for the complete paged tree, without loading every page.
    pub fn transfer_snapshot_paged(
        &self,
        manifest: &crate::PagedManifest,
        direction: ipc::TransferDirection,
        peer_id: &str,
        state: ipc::TransferState,
        error: Option<String>,
    ) -> ipc::Transfer {
        let mut snapshot =
            self.transfer_snapshot(manifest.first_page(), direction, peer_id, state, error);
        let summary = manifest.summary();
        snapshot.items = summary.items;
        snapshot.bytes_total = summary.bytes;
        snapshot.bytes_done = self.snapshot().bytes_done.min(summary.bytes);
        snapshot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Consent {
    Automatic,
    Approved { manifest_digest: [u8; 32] },
}

impl Consent {
    /// Bind human approval to the exact names, sizes, file hashes, clip and transfer ID.
    pub fn approve(manifest: &FileManifest) -> Result<Self> {
        Self::approve_pages(std::iter::once(manifest))
    }

    /// Approval binds the complete ordered v2 manifest, including negotiated geometry.
    /// Use the opt-in paged engine API to transfer more than one page.
    pub fn approve_pages<'a>(pages: impl IntoIterator<Item = &'a FileManifest>) -> Result<Self> {
        let mut validator = glide_proto::manifest::ManifestValidator::default();
        let mut digest = blake3::Hasher::new();
        for page in pages {
            validator.push(page)?;
            let bytes =
                glide_proto::codec::encode_transfer(&TransferMessage::FileManifest(page.clone()))?;
            digest.update(&bytes);
        }
        validator.finish()?;
        Ok(Self::Approved {
            manifest_digest: *digest.finalize().as_bytes(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct SendPlan {
    pub manifest: FileManifest,
    pub(crate) paths: Vec<PathBuf>,
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        return Err(Error::Invalid("transfer/clipboard id"));
    }
    Ok(())
}

pub(crate) fn validate_manifest(manifest: &FileManifest, config: &Config) -> Result<u64> {
    if manifest.page != 0 || !manifest.final_page {
        return Err(Error::Invalid("paged manifests unsupported"));
    }
    if manifest.chunk_size as usize != config.chunk_size {
        return Err(Error::Invalid(
            "manifest chunk size differs from configuration",
        ));
    }
    validate_id(&manifest.transfer_id)?;
    validate_id(&manifest.clip_id)?;
    if manifest.files.is_empty() || manifest.files.len() > config.max_entries {
        return Err(Error::Limit("entry count"));
    }
    let mut names: HashMap<String, bool> = HashMap::new();
    let mut total = 0u64;
    for (index, entry) in manifest.files.iter().enumerate() {
        if entry.file_id as usize != index {
            return Err(Error::Invalid("noncontiguous file ids"));
        }
        filesystem::validate_relative(
            &entry.relative_path,
            config.max_depth,
            config.max_path_bytes,
        )?;
        let key = entry.relative_path.to_lowercase();
        if let Some((parent, _)) = key.rsplit_once('/') {
            if names.get(parent) != Some(&true) {
                return Err(Error::Invalid("missing directory parent"));
            }
        }
        if names.insert(key, entry.is_dir).is_some() {
            return Err(Error::Invalid("case/path collision"));
        }
        if entry.is_dir {
            if entry.size != 0 || entry.blake3_hash.is_some() {
                return Err(Error::Invalid("directory metadata"));
            }
        } else {
            if entry.blake3_hash.is_none() {
                return Err(Error::Invalid("missing file digest"));
            }
            if entry.size > config.max_file_bytes {
                return Err(Error::Limit("file size"));
            }
            config.chunks(entry.size)?;
            total = total
                .checked_add(entry.size)
                .ok_or(Error::Limit("total size overflow"))?;
            if total > config.max_transfer_bytes {
                return Err(Error::Limit("transfer size"));
            }
        }
    }
    Ok(total)
}

/// Iterative DFS retains at most max_depth directory iterators, never a whole listing.
pub async fn build_manifest(
    roots: Vec<PathBuf>,
    transfer_id: String,
    clip_id: String,
    config: Config,
    cancel: Cancel,
) -> Result<SendPlan> {
    config.validate()?;
    if roots.is_empty() || roots.len() > config.max_entries {
        return Err(Error::Limit("root count"));
    }
    tokio::task::spawn_blocking(move || {
        let mut files = ManifestFiles::new();
        let mut paths = Vec::new();
        let mut stack: Vec<(fs::ReadDir, String)> = Vec::new();
        let mut roots = roots.into_iter();
        let mut buffer = vec![0; 64 * 1024];
        let mut total = 0u64;
        loop {
            cancel.check()?;
            let next = if let Some((iter, parent)) = stack.last_mut() {
                match iter.next() {
                    Some(entry) => {
                        let entry = entry?;
                        Some((entry.path(), parent.clone()))
                    }
                    None => {
                        stack.pop();
                        continue;
                    }
                }
            } else {
                roots.next().map(|path| (path, String::new()))
            };
            let Some((path, parent)) = next else {
                break;
            };
            if path.as_os_str().as_encoded_bytes().len() > 4096 {
                return Err(Error::Limit("absolute source path"));
            }
            if files.len() == config.max_entries {
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
            config.chunks(size)?;
            total = total
                .checked_add(size)
                .ok_or(Error::Limit("total size overflow"))?;
            if total > config.max_transfer_bytes {
                return Err(Error::Limit("transfer size"));
            }
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
            files
                .push(FileManifestEntry {
                    file_id: files.len() as u32,
                    relative_path: relative.clone(),
                    size,
                    is_dir: metadata.is_dir(),
                    blake3_hash: digest,
                })
                .map_err(|_| Error::Limit("manifest count"))?;
            paths.push(path.clone());
            if metadata.is_dir() {
                stack.push((fs::read_dir(path)?, relative));
            }
        }
        let manifest = FileManifest {
            transfer_id,
            clip_id,
            chunk_size: config.chunk_size as u32,
            page: 0,
            final_page: true,
            files,
        };
        validate_manifest(&manifest, &config)?;
        Ok(SendPlan { manifest, paths })
    })
    .await
    .map_err(|_| Error::Worker)?
}

#[derive(Clone)]
pub struct FileEngine {
    pub(crate) config: Config,
    pub(crate) staging: PathBuf,
    pub(crate) slots: Arc<Semaphore>,
    pub(crate) staging_lock: Arc<Mutex<()>>,
    chunk_bounds: Option<ChunkSizeBounds>,
    coalesce_resume: bool,
}

impl FileEngine {
    /// Native authenticated streams, including preamble/manifest binding and
    /// mandatory peer closure on malformed codecs/protocol input.
    #[cfg(feature = "native-transfer")]
    pub async fn send_native(
        &self,
        plan: SendPlan,
        streams: glide_net::TransferStreams,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<()> {
        let handle = streams.handle;
        if handle.is_closed() || plan.manifest.transfer_id != streams.transfer_id {
            handle.cancel();
            return Err(Error::Invalid("transfer preamble/manifest mismatch"));
        }
        let mut control = streams.control;
        let result = self
            .send(plan, &mut control, streams.chunks, cancel, progress)
            .await;
        finish_native(&handle, &result);
        result
    }

    #[cfg(feature = "native-transfer")]
    pub async fn receive_native(
        &self,
        streams: glide_net::TransferStreams,
        consent: Consent,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<Received> {
        let handle = streams.handle;
        if handle.is_closed() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "transfer session closed",
            )));
        }
        let mut control = streams.control;
        let result = self
            .receive_bound(
                &streams.peer_id,
                &mut control,
                streams.chunks,
                consent,
                cancel,
                progress,
                Some(&streams.transfer_id),
            )
            .await;
        finish_native(&handle, &result);
        result
    }
    /// data_dir must already exist and be owned/protected by the daemon.
    pub async fn new(data_dir: impl AsRef<Path>, config: Config) -> Result<Self> {
        config.validate()?;
        filesystem::check_chain(data_dir.as_ref())?;
        let staging = fs::canonicalize(data_dir.as_ref())?.join("glide-xfer");
        if !staging.try_exists()? {
            filesystem::private_dir(&staging)?;
        }
        filesystem::check_chain(&staging)?;
        let engine = Self {
            slots: Arc::new(Semaphore::new(config.max_concurrent_transfers)),
            config,
            staging,
            staging_lock: Arc::new(Mutex::new(())),
            chunk_bounds: None,
            coalesce_resume: false,
        };
        engine.purge_stale().await?;
        Ok(engine)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Existing engines keep their exact-size contract until explicitly opted in.
    pub fn with_chunk_size_bounds(mut self, bounds: ChunkSizeBounds) -> Result<Self> {
        bounds.validate()?;
        self.chunk_bounds = Some(bounds);
        Ok(self)
    }

    /// Preserve verified chunks when missing ranges exceed the wire's 4096-range cap.
    /// Only the overflow tail is coalesced and its covered chunks are retransmitted.
    pub fn with_resume_coalescing(mut self) -> Self {
        self.coalesce_resume = true;
        self
    }

    /// Caller owns this future; run on daemon start and stop it on shutdown.
    pub async fn run_purge(&self, cancel: &Cancel) -> Result<()> {
        let mut timer = tokio::time::interval(self.config.purge_interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = timer.tick() => { self.purge_stale().await?; }
            }
        }
    }

    pub async fn purge_stale(&self) -> Result<usize> {
        let _guard = self.staging_lock.lock().await;
        let staging = self.staging.clone();
        let stale_after = self.config.stale_after;
        tokio::task::spawn_blocking(move || purge_dir(&staging, stale_after))
            .await
            .map_err(|_| Error::Worker)?
    }

    pub async fn send<C, W>(
        &self,
        plan: SendPlan,
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
        let total = validate_manifest(&plan.manifest, &self.config)?;
        if plan.paths.len() != plan.manifest.files.len() {
            return Err(Error::Invalid("source plan"));
        }
        let id = plan.manifest.transfer_id.clone();
        write_message(
            control,
            &TransferMessage::FileManifest(plan.manifest.clone()),
            self.config.operation_timeout,
            cancel,
        )
        .await?;
        let mut ranges = Vec::with_capacity(plan.manifest.files.len());
        let mut missing_bytes = 0;
        for file in plan.manifest.files.iter() {
            if file.is_dir {
                ranges.push(ResumeRanges::new());
                continue;
            }
            let message = read_message(control, self.config.operation_timeout, cancel).await?;
            let TransferMessage::FileResume(resume) = message else {
                return Err(Error::Invalid("expected resume"));
            };
            if resume.transfer_id != id || resume.file_id != file.file_id {
                return Err(Error::Invalid("resume session/file"));
            }
            missing_bytes += validate_ranges(&resume.missing_chunks, file.size, &self.config)?;
            ranges.push(resume.missing_chunks);
        }
        progress.reset(total - missing_bytes);
        let plan = Arc::new(plan);
        let ranges = Arc::new(ranges);
        let limiter = Arc::new(Mutex::new(Instant::now()));
        let stop = Cancel::new();
        let mut workers = JoinSet::new();
        let lane_count = lanes.len() as u32;
        for (index, lane) in lanes.into_iter().enumerate() {
            workers.spawn(send_lane(
                plan.clone(),
                ranges.clone(),
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
                TransferMessage::FileComplete(complete) if complete.transfer_id == id => Ok(()),
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

    pub async fn receive<C, R>(
        &self,
        peer_id: &str,
        control: &mut C,
        lanes: Vec<R>,
        consent: Consent,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<Received>
    where
        C: AsyncRead + AsyncWrite + Unpin,
        R: AsyncRead + Unpin + Send + 'static,
    {
        self.receive_bound(peer_id, control, lanes, consent, cancel, progress, None)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn receive_bound<C, R>(
        &self,
        peer_id: &str,
        control: &mut C,
        lanes: Vec<R>,
        consent: Consent,
        cancel: &Cancel,
        progress: &Progress,
        expected_id: Option<&str>,
    ) -> Result<Received>
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
        if peer_id.len() != 64
            || !peer_id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::Invalid("authenticated peer id"));
        }
        let manifest = match read_message(control, self.config.operation_timeout, cancel).await? {
            TransferMessage::FileManifest(manifest) => manifest,
            _ => return Err(Error::Invalid("expected manifest")),
        };
        if expected_id.is_some_and(|id| id != manifest.transfer_id) {
            return Err(Error::Invalid("transfer preamble/manifest mismatch"));
        }
        let mut session_config = self.config.clone();
        if let Some(bounds) = self.chunk_bounds {
            bounds.accepts(manifest.chunk_size as usize)?;
            session_config.chunk_size = manifest.chunk_size as usize;
        }
        let total = validate_manifest(&manifest, &session_config)?;
        match consent {
            Consent::Automatic if total > self.config.max_auto_bytes => {
                return Err(Error::ConfirmationRequired {
                    bytes: total,
                    manifest: Box::new(manifest),
                });
            }
            Consent::Approved { .. } if consent != Consent::approve(&manifest)? => {
                return Err(Error::Invalid("approval manifest mismatch"));
            }
            _ => {}
        }
        let _guard = self.staging_lock.lock().await;
        let staging = self.staging.clone();
        let config = session_config;
        let bounds = self.chunk_bounds;
        let coalesce = self.coalesce_resume;
        let peer_id = peer_id.to_owned();
        let opening_cancel = cancel.clone();
        let session = tokio::task::spawn_blocking(move || {
            Session::open_with_options(
                staging,
                peer_id,
                manifest,
                config,
                &opening_cancel,
                None,
                bounds,
                coalesce,
            )
            .map_err(Error::storage)
        })
        .await
        .map_err(|_| Error::Worker)??;
        drop(_guard);
        let id = session.manifest.transfer_id.clone();
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
                Ok(received)
            }
            Err(error) => {
                // Only transport I/O interruption preserves resumable state. Protocol failures,
                // timeout, corruption and cancellation purge all private partial contents.
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

    pub(crate) async fn receive_session<C, R>(
        &self,
        session: Session,
        control: &mut C,
        lanes: Vec<R>,
        cancel: &Cancel,
        progress: &Progress,
    ) -> Result<(Session, Result<()>)>
    where
        C: AsyncRead + AsyncWrite + Unpin,
        R: AsyncRead + Unpin + Send + 'static,
    {
        progress.reset(0);
        let id = session.manifest.transfer_id.clone();
        let entry_count = session.entry_count();
        let chunk_size = session.chunk_size();
        let mut session = Some(session);
        // Reverify persisted chunks on the blocking pool before reporting them as present.
        let handshake = async {
            for index in 0..entry_count {
                cancel.check()?;
                let task_cancel = cancel.clone();
                let task_progress = progress.clone();
                let mut current = session.take().ok_or(Error::Worker)?;
                let (next, resume) = tokio::task::spawn_blocking(move || {
                    let result = (|| {
                        if current.entry(index)?.is_dir {
                            return Ok(None);
                        }
                        current
                            .resume(index, &task_cancel, &task_progress)
                            .map(Some)
                    })()
                    .map_err(Error::storage);
                    (current, result)
                })
                .await
                .map_err(|_| Error::Worker)?;
                session = Some(next);
                if let Some(resume) = resume? {
                    write_message(
                        control,
                        &TransferMessage::FileResume(resume),
                        self.config.operation_timeout,
                        cancel,
                    )
                    .await?;
                }
            }
            Ok::<_, Error>(())
        }
        .await;
        if let Err(error) = handshake {
            return Ok((session.ok_or(Error::Worker)?, Err(error)));
        }
        progress.reset(progress.snapshot().bytes_done);
        let (sender, mut receiver) = mpsc::channel(1);
        let mut workers = JoinSet::new();
        for mut lane in lanes {
            let sender = sender.clone();
            let cancel = cancel.clone();
            let id = id.clone();
            let timeout = self.config.operation_timeout;
            workers.spawn(async move {
                let mut frame = ChunkReader::new(chunk_size)?;
                let (returned, mut reclaimed) = mpsc::channel(1);
                loop {
                    let message = frame.read(&mut lane, timeout, &cancel).await?;
                    match message {
                        ChunkMessage::Chunk(chunk) if chunk.transfer_id == id => {
                            cancel
                                .run(timeout, async {
                                    sender
                                        .send((frame, returned.clone()))
                                        .await
                                        .map_err(|_| Error::Worker)
                                })
                                .await?;
                            frame = cancel
                                .run(timeout, async {
                                    reclaimed.recv().await.ok_or(Error::Worker)
                                })
                                .await?;
                        }
                        ChunkMessage::Complete(done) if done.transfer_id == id => return Ok(()),
                        ChunkMessage::Cancel(done) if done.transfer_id == id => {
                            return Err(Error::Cancelled)
                        }
                        _ => return Err(Error::Invalid("unexpected chunk-lane message")),
                    }
                }
            });
        }
        drop(sender);
        let control_message =
            crate::stream::read_control_message(control, self.config.operation_timeout, cancel);
        tokio::pin!(control_message);
        let mut complete = false;
        let result = async {
            while !workers.is_empty() || !receiver.is_closed() || !receiver.is_empty() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    message = &mut control_message, if !complete => {
                        match message? {
                            TransferMessage::FileComplete(done) if done.transfer_id == id => complete = true,
                            TransferMessage::FileCancel(done) if done.transfer_id == id => return Err(Error::Cancelled),
                            _ => return Err(Error::Invalid("expected transfer completion")),
                        }
                    }
                    result = workers.join_next(), if !workers.is_empty() => {
                        if let Some(result) = result { result.map_err(|_| Error::Worker)??; }
                    }
                    chunk = receiver.recv(), if !receiver.is_closed() || !receiver.is_empty() => {
                        if let Some((frame, returned)) = chunk {
                            let task_cancel = cancel.clone();
                            let task_progress = progress.clone();
                            let mut current = session.take().ok_or(Error::Worker)?;
                            let (next, frame, result) = tokio::task::spawn_blocking(move || {
                                let result = (|| {
                                    current.apply_view(frame.chunk()?, &task_cancel, &task_progress)
                                })().map_err(Error::storage);
                                (current, frame, result)
                            }).await.map_err(|_| Error::Worker)?;
                            session = Some(next);
                            result?;
                            returned.send(frame).await.map_err(|_| Error::Worker)?;
                        }
                    }
                }
            }
            let message = if complete { TransferMessage::FileComplete(FileComplete { transfer_id: id.clone() }) }
                else { cancel.run(self.config.operation_timeout, control_message).await? };
            match message {
                TransferMessage::FileComplete(done) if done.transfer_id == id => {
                    let task_cancel = cancel.clone();
                    let mut current = session.take().ok_or(Error::Worker)?;
                    let (next, result) = tokio::task::spawn_blocking(move || {
                        let result = current.verify_all(&task_cancel).map_err(Error::storage);
                        (current, result)
                    })
                    .await
                    .map_err(|_| Error::Worker)?;
                    session = Some(next);
                    result
                }
                TransferMessage::FileCancel(done) if done.transfer_id == id => {
                    Err(Error::Cancelled)
                }
                _ => Err(Error::Invalid("expected transfer completion")),
            }
        }
        .await;
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        Ok((session.ok_or(Error::Worker)?, result))
    }

    pub(crate) fn check_lanes(&self, count: usize) -> Result<()> {
        if count == 0 || count > self.config.parallel_streams {
            return Err(Error::Limit("parallel streams"));
        }
        Ok(())
    }
}

/// Removes session and spool folders nobody is using and that were last touched `stale_after` ago.
/// A live transfer or a clipboard publication holds its folder's exclusive lease, so those are never removed.
pub(crate) fn purge_dir(staging: &std::path::Path, stale_after: Duration) -> Result<usize> {
    filesystem::check_chain(staging)?;
    let mut removed = 0;
    for entry in fs::read_dir(staging)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|name| valid_session_name(name) || valid_spool_name(name))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if filesystem::link(&metadata) || !metadata.is_dir() {
            return Err(Error::Invalid("staging directory type"));
        }
        let timestamp = entry.path().join("lease");
        let modified = match fs::symlink_metadata(&timestamp) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => metadata.modified()?,
            Err(error) => return Err(error.into()),
        };
        if SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            < stale_after
        {
            continue;
        }
        // A live transfer or clipboard publication holds this exclusive lease.
        let lease = match filesystem::lease(&timestamp) {
            Ok(lease) => lease,
            Err(Error::Busy) => continue,
            Err(Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        drop(lease);
        filesystem::check_chain(&entry.path())?;
        fs::remove_dir_all(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}

pub(crate) fn valid_session_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

pub(crate) fn valid_spool_name(name: &str) -> bool {
    ["glide-manifest-", "glide-resume-"].iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            (6..=32).contains(&suffix.len())
                && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
    })
}

/// Keep this lease alive while the OS clipboard references paths; purge skips live leases.
pub struct Received {
    pub paths: Vec<PathBuf>,
    pub manifest: FileManifest,
    pub(crate) _lease: fs::File,
}

pub(crate) fn validate_ranges(ranges: &ResumeRanges, size: u64, config: &Config) -> Result<u64> {
    let count = config.chunks(size)?;
    let mut end = 0;
    let mut bytes = 0;
    for range in ranges.iter() {
        let last = range
            .first_chunk
            .checked_add(range.count)
            .ok_or(Error::Invalid("resume overflow"))?;
        if range.count == 0 || range.first_chunk < end || last > count {
            return Err(Error::Invalid("resume range"));
        }
        bytes += (u64::from(last) * config.chunk_size as u64).min(size)
            - u64::from(range.first_chunk) * config.chunk_size as u64;
        end = last;
    }
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
async fn send_lane<W: AsyncWrite + Unpin>(
    plan: Arc<SendPlan>,
    ranges: Arc<Vec<ResumeRanges>>,
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
    for (file, path) in plan.manifest.files.iter().zip(&plan.paths) {
        if file.is_dir {
            continue;
        }
        let path = path.clone();
        let size = file.size;
        let mut input = tokio::task::spawn_blocking(move || {
            let input = filesystem::open_file(&path, false, false)?;
            if input.metadata()?.len() != size {
                return Err(Error::Invalid("source size changed"));
            }
            Ok(input)
        })
        .await
        .map_err(|_| Error::Worker)??;
        for range in ranges[file.file_id as usize].iter() {
            let end = range.first_chunk + range.count;
            let mut chunk_index = range.first_chunk
                + (index + lane_count - range.first_chunk % lane_count) % lane_count;
            while chunk_index < end {
                cancel.check()?;
                stop.check()?;
                let offset = u64::from(chunk_index) * config.chunk_size as u64;
                let length = (file.size - offset).min(config.chunk_size as u64) as usize;
                let file_id = file.file_id;
                let encoding_plan = plan.clone();
                let (source, next, chunk) = tokio::task::spawn_blocking(move || {
                    input.seek(std::io::SeekFrom::Start(offset))?;
                    input.read_exact(encoder.buffer_mut(length)?)?;
                    let chunk = encoder
                        .prepare(&encoding_plan.manifest.files[file_id as usize].relative_path)?;
                    Ok::<_, Error>((input, encoder, chunk))
                })
                .await
                .map_err(|_| Error::Worker)??;
                encoder = next;
                input = source;
                if let Some(rate) = config.rate_limit_bps {
                    let when = {
                        let mut next = limiter.lock().await;
                        let when = (*next).max(Instant::now());
                        *next = when + Duration::from_secs_f64(length as f64 / rate as f64);
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
                    &encoder.view(&plan.manifest.transfer_id, file_id, chunk_index, chunk)?,
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
            transfer_id: plan.manifest.transfer_id.clone(),
        }),
        config.operation_timeout,
        &cancel,
    )
    .await
}
