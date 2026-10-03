use super::*;

#[derive(Clone)]
pub(super) struct Outgoing {
    pub(super) snapshot: Arc<ClipboardSnapshot>,
    pub(super) announcement: wire::ClipAnnounce,
    pub(super) eager: Vec<ClipboardMessage>,
    pub(super) native_required: bool,
}

pub(super) struct PendingNativeSend {
    pub(super) peer: String,
    pub(super) token: glide_net::PeerToken,
    pub(super) clip_id: String,
    pub(super) probe: bool,
    pub(super) manifest: Arc<std::sync::Mutex<Option<glide_proto::wire::FileManifest>>>,
}

pub(super) struct NativeJob {
    pub(super) transfer: glide_proto::ipc::Transfer,
    pub(super) peer_token: glide_net::PeerToken,
    pub(super) probe: bool,
    pub(super) progress: glide_xfer::Progress,
    pub(super) cancel: glide_xfer::Cancel,
    pub(super) task: JoinHandle<Result<NativeCompletion, glide_xfer::Error>>,
    pub(super) last_progress_update: Instant,
    pub(super) manifest: Arc<std::sync::Mutex<Option<glide_proto::wire::FileManifest>>>,
}

pub(super) enum NativeCompletion {
    Sent(glide_proto::wire::FileManifest),
    ProbeClosed(glide_proto::wire::FileManifest),
    Received {
        manifest: glide_proto::wire::FileManifest,
        received: glide_xfer::Received,
        contents: Vec<ClipboardContent>,
    },
    ConsentRequired(glide_proto::wire::FileManifest),
}

pub(super) fn transfer_id(peer: &str, clip_id: &str) -> String {
    format!("{peer}-{clip_id}")
}

pub(super) fn wire_transfer_id(clip_id: &str) -> &str {
    clip_id
}

pub(super) fn payload_name(clip_id: &str, kind: &str) -> String {
    format!("glide-clipboard-{clip_id}-{kind}.bin")
}

pub(super) fn manifest_totals(manifest: &glide_proto::wire::FileManifest) -> (u32, u64) {
    let items = manifest.files.len().min(u32::MAX as usize) as u32;
    let bytes = manifest
        .files
        .iter()
        .filter(|entry| !entry.is_dir)
        .fold(0u64, |sum, entry| sum.saturating_add(entry.size));
    (items, bytes)
}

pub(super) fn announced_totals(announcement: &wire::ClipAnnounce) -> (u32, u64) {
    let items =
        (announcement.files.len() + announcement.formats.len()).min(u32::MAX as usize) as u32;
    let bytes = announcement
        .files
        .iter()
        .filter(|entry| !entry.is_dir)
        .fold(0u64, |sum, entry| sum.saturating_add(entry.size))
        .saturating_add(
            announcement
                .formats
                .iter()
                .fold(0u64, |sum, format| sum.saturating_add(format.size)),
        );
    (items, bytes)
}

pub(super) fn validate_received_for_announcement(
    announcement: &wire::ClipAnnounce,
    received: &glide_xfer::Received,
) -> Result<Vec<ClipboardContent>, &'static str> {
    if received.manifest.clip_id != announcement.clip_id
        || received.manifest.transfer_id != wire_transfer_id(&announcement.clip_id)
    {
        return Err("The received transfer did not match its clipboard announcement.");
    }

    let expected_payloads = announcement
        .formats
        .iter()
        .map(|format| payload_name(&received.manifest.clip_id, &format.kind))
        .collect::<HashSet<_>>();
    let root_entries = received
        .manifest
        .files
        .iter()
        .filter(|entry| !entry.relative_path.contains('/'))
        .collect::<Vec<_>>();
    if root_entries.len() != received.paths.len() {
        return Err("The received transfer had an invalid file list.");
    }

    let mut files = Vec::new();
    let mut contents = Vec::new();
    let mut seen_payloads = HashSet::new();
    for (entry, path) in root_entries.into_iter().zip(&received.paths) {
        if expected_payloads.contains(&entry.relative_path) {
            let Some(announced) = announcement.formats.iter().find(|format| {
                payload_name(&received.manifest.clip_id, &format.kind) == entry.relative_path
            }) else {
                return Err("The received clipboard format was not announced.");
            };
            if entry.is_dir || entry.size != announced.size || !seen_payloads.insert(entry.file_id)
            {
                return Err("The received clipboard format did not match its announcement.");
            }
            let data =
                std::fs::read(path).map_err(|_| "A verified clipboard item could not be read.")?;
            if data.len() as u64 != announced.size {
                return Err("The verified clipboard item changed before publication.");
            }
            let format =
                format(&announced.kind).ok_or("The received clipboard format is unsupported.")?;
            if matches!(format, ClipboardFormat::Text | ClipboardFormat::Html)
                && std::str::from_utf8(&data).is_err()
            {
                return Err("The received clipboard text was invalid.");
            }
            contents.push(ClipboardContent {
                format,
                data: ClipboardData::Bytes(data),
                sensitivity: ClipboardSensitivity::default(),
            });
        } else {
            let name = entry
                .relative_path
                .rsplit('/')
                .next()
                .ok_or("The received file name was invalid.")?;
            if name.is_empty() || name.chars().any(char::is_control) {
                return Err("The received file name was invalid.");
            }
            files.push(glide_platform::FileEntry {
                path: path.clone(),
                name: name.to_owned(),
                size: entry.size,
                is_dir: entry.is_dir,
            });
        }
    }
    if seen_payloads.len() != expected_payloads.len() {
        return Err("The received transfer omitted an announced clipboard format.");
    }

    let announced_files = announcement.files.iter().collect::<Vec<_>>();
    if files.len() != announced_files.len()
        || files
            .iter()
            .zip(announced_files)
            .any(|(actual, announced)| {
                actual.name != announced.name
                    || actual.size != announced.size
                    || actual.is_dir != announced.is_dir
            })
    {
        return Err("The received file list did not match its announcement.");
    }
    if !files.is_empty() {
        contents.push(ClipboardContent::files(glide_platform::FileList {
            entries: files,
            sensitivity: ClipboardSensitivity::default(),
        }));
    }
    Ok(contents)
}

/// Compare the actual xfer manifest with the untrusted clipboard announcement before consent or staging.
pub(super) fn validate_manifest(
    incoming: &Incoming,
    manifest: &glide_proto::wire::FileManifest,
) -> Result<u64, &'static str> {
    validate_manifest_for_announcement(&incoming.peer, &incoming.announcement, manifest)
}

pub(super) fn validate_manifest_for_announcement(
    peer: &str,
    announcement: &wire::ClipAnnounce,
    manifest: &glide_proto::wire::FileManifest,
) -> Result<u64, &'static str> {
    if manifest.clip_id != announcement.clip_id
        || manifest.transfer_id != wire_transfer_id(&manifest.clip_id)
        || transfer_id(peer, &manifest.clip_id).len() > 128
    {
        return Err("The received transfer did not match its clipboard announcement.");
    }

    let expected_payloads = announcement
        .formats
        .iter()
        .map(|format| (payload_name(&manifest.clip_id, &format.kind), format.size))
        .collect::<HashMap<_, _>>();
    let mut payload_names = HashSet::new();
    let mut announced_files = Vec::new();
    for entry in manifest
        .files
        .iter()
        .filter(|entry| !entry.relative_path.contains('/'))
    {
        if let Some(size) = expected_payloads.get(&entry.relative_path) {
            if entry.is_dir || entry.size != *size || !payload_names.insert(&entry.relative_path) {
                return Err("The received clipboard format did not match its announcement.");
            }
        } else {
            announced_files.push((entry.relative_path.as_str(), entry.size, entry.is_dir));
        }
    }
    if payload_names.len() != expected_payloads.len()
        || announced_files.len() != announcement.files.len()
        || announced_files.iter().zip(announcement.files.iter()).any(
            |((name, size, is_dir), announced)| {
                *name != announced.name || *size != announced.size || *is_dir != announced.is_dir
            },
        )
    {
        return Err("The received file list did not match its announcement.");
    }
    let total = manifest
        .files
        .iter()
        .filter(|entry| !entry.is_dir)
        .try_fold(0u64, |sum, entry| sum.checked_add(entry.size))
        .ok_or("The received transfer size overflowed.")?;
    Ok(total)
}
