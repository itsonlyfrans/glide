use super::*;
use glide_net::{
    NativeConfig, NativeError, NativePeerManager, NetFuture, PairingHost, PairingSession,
};

pub(super) fn native_error(failure: NativeError) -> IpcError {
    match failure {
        NativeError::Security(_) => error(
            ErrorCode::PermissionDenied,
            "Secure identity storage is unavailable or damaged. Unlock the OS keystore and retry.",
        ),
        NativeError::Io(ref failure) if failure.kind() == std::io::ErrorKind::AddrInUse => error(
            ErrorCode::Unreachable,
            "The listening port is already in use. Choose another port or close the other Glide instance.",
        ),
        NativeError::Io(_) => error(
            ErrorCode::Unreachable,
            "Could not open secure network storage or bind the listening port.",
        ),
        NativeError::Internal(_) => error(
            ErrorCode::Internal,
            "Secure networking could not be initialized.",
        ),
    }
}

impl Core {
    /// Native pinned networking with either the current OS backend or explicit mock platform.
    /// Configured peer metadata is never a source of trust: only the native pin store is restored.
    pub async fn native(
        data_dir: &Path,
        port: Option<u16>,
        mock_platform: bool,
        discovery: Option<bool>,
    ) -> Result<Self, IpcError> {
        Self::native_with_transfer_rate(data_dir, port, mock_platform, discovery, None).await
    }

    /// Native construction with an optional aggregate file sender byte-rate cap.
    pub async fn native_with_transfer_rate(
        data_dir: &Path,
        port: Option<u16>,
        mock_platform: bool,
        discovery: Option<bool>,
        rate_limit_bps: Option<u64>,
    ) -> Result<Self, IpcError> {
        if rate_limit_bps.is_some_and(|rate| rate <= (2 * 1024 * 1024) / 30) {
            return Err(error(
                ErrorCode::InvalidParams,
                "The transfer rate cap is too low for the two file lanes and their timeout.",
            ));
        }
        let config = Self::load_config(data_dir, port, discovery).map_err(|_| {
            error(
                ErrorCode::InvalidParams,
                "Could not load validated daemon configuration.",
            )
        })?;
        std::fs::create_dir_all(data_dir).map_err(|_| {
            error(
                ErrorCode::PermissionDenied,
                "Could not create the daemon data directory.",
            )
        })?;
        let repair_dir = data_dir.to_owned();
        let repair = tokio::task::spawn_blocking(move || {
            NativePeerManager::repair_unfinished_pairing(&repair_dir)
        })
        .await
        .map_err(|_| error(ErrorCode::Internal, "Pairing recovery worker failed."))?
        .map_err(native_error)?;
        let discarded = !repair.removed_files.is_empty() || !repair.removed_peer_ids.is_empty();
        let result = async {
            let os = if cfg!(target_os = "macos") {
                Os::Macos
            } else {
                Os::Windows
            };
            let mock = if mock_platform {
                let mock = MockPlatform::new(os, data_dir.to_owned());
                mock.input
                    .set_permissions(Permissions {
                        accessibility: PermissionStatus::Granted,
                        input_monitoring: PermissionStatus::Granted,
                        injection: PermissionStatus::Granted,
                    })
                    .map_err(|_| error(ErrorCode::Internal, "Could not initialize mock input."))?;
                Some(mock)
            } else {
                None
            };
            let platform: Arc<dyn Platform> = match &mock {
                Some(mock) => Arc::new(mock.clone()),
                None => native_platform()?,
            };
            let monitors = platform
                .input_backend()
                .monitors()
                .map(|native| {
                    crate::arrangement::arrange(&native, &config.settings.display.arrangement)
                })
                .map_err(|_| {
                    error(
                        ErrorCode::PermissionDenied,
                        "Could not read local displays.",
                    )
                })?;
            let native = Arc::new(
                NativePeerManager::new(NativeConfig {
                    data_dir: data_dir.to_owned(),
                    bind_addr: ([0, 0, 0, 0], config.settings.network.port).into(),
                    name: config.settings.device_name.clone(),
                    os: platform.os(),
                    monitors,
                    discovery: config.settings.network.discovery,
                })
                .await
                .map_err(native_error)?,
            );
            let mut core =
                Self::finish_native(data_dir, config, platform, mock, native, rate_limit_bps)
                    .await?;
            if discarded {
                core.events.push(Event::Notification(Notification {
                    level: "warning".into(),
                    title: "Unfinished pairing discarded".into(),
                    body: "Unfinished trust records were removed. Pair affected devices again."
                        .into(),
                    action: None,
                }));
            }
            core.pump_peer_events().await;
            Ok(core)
        }
        .await;
        result.map_err(|mut failure: IpcError| {
            if discarded { failure.message.push_str(" Unfinished pairing records were discarded; affected devices must be paired again."); }
            failure
        })
    }

    async fn finish_native(
        data_dir: &Path,
        mut config: Config,
        platform: Arc<dyn Platform>,
        mock: Option<MockPlatform>,
        native: Arc<NativePeerManager>,
        rate_limit_bps: Option<u64>,
    ) -> Result<Self, IpcError> {
        let old_id = std::mem::replace(
            &mut config.mock_device_id,
            native.identity().device_id().to_owned(),
        );
        if old_id != config.mock_device_id {
            for device in &mut config.layout.devices {
                if device.device_id == old_id {
                    device.device_id.clone_from(&config.mock_device_id);
                }
            }
            config.layout_version = (config.layout_version.0, config.mock_device_id.clone());
        }
        let mut peers = native.paired_peers().map_err(native_error)?;
        for peer in &mut peers {
            if let Some(saved) = config.peers.iter().find(|p| p.device_id == peer.device_id) {
                peer.clipboard_enabled = saved.clipboard_enabled;
            }
        }
        config.layout.devices.retain(|device| {
            device.device_id == config.mock_device_id
                || peers.iter().any(|p| p.device_id == device.device_id)
        });
        config.peers = peers;
        let link = Arc::new(native.link());
        let mut core = Self::initialize(
            data_dir,
            config,
            platform,
            mock,
            Some(native.identity().fingerprint().to_owned()),
        )
        .await
        .map_err(|_| {
            error(
                ErrorCode::PermissionDenied,
                "Could not initialize local capture, desktop layout or configuration storage.",
            )
        })?;
        core.set_peer_interfaces(Box::new(NativeHandle(native.clone())), link)
            .await?;
        core.state.self_info.listen_port = native.local_addr().map_err(native_error)?.port();
        core.state.settings.network.port = core.state.self_info.listen_port;
        core.native_manager = Some(native.clone());
        // PeerUpdated carries the authenticated snapshot; still drain the bounded
        // accept queue so repeated reconnects cannot exhaust its 32 slots.
        let accepted_link = native.link();
        core.accept_job = Some(tokio::spawn(async move {
            while accepted_link.accept().await.is_ok() {}
        }));
        // The transfer store refuses any symlink in its path. Resolve the data folder's own location once (macOS keeps
        // /var and /tmp behind a system link); links inside the folder are still rejected by the store.
        let staging_root = if cfg!(unix) {
            data_dir
                .canonicalize()
                .unwrap_or_else(|_| data_dir.to_owned())
        } else {
            data_dir.to_owned()
        };
        let engine = glide_xfer::FileEngine::new(
            &staging_root,
            glide_xfer::Config {
                chunk_size: 1024 * 1024,
                parallel_streams: 2,
                max_concurrent_transfers: 2,
                rate_limit_bps,
                // Core inspects the complete manifest before current-policy approval.
                max_auto_bytes: 0,
                ..glide_xfer::Config::default()
            },
        )
        .await
        .map_err(|_| {
            error(
                ErrorCode::PermissionDenied,
                "Could not initialize protected transfer staging or purge stale data.",
            )
        })?;
        core.initialize_native_clipboard(native.link(), engine.clone());
        core.persist(&core.state)?;
        let cancel = core.purge_cancel.clone();
        core.purge_job = Some(tokio::spawn(async move { engine.run_purge(&cancel).await }));
        Ok(core)
    }

    /// Isolated keystore only for tests; crypto, trust persistence and network remain native.
    #[cfg(test)]
    pub(super) async fn native_with_test_keystore(
        data_dir: &Path,
        port: u16,
        os: Os,
        store: &glide_net::TestKeyStore,
    ) -> Result<Self, IpcError> {
        let config = Self::load_config(data_dir, (port != 0).then_some(port), Some(false))
            .map_err(|_| {
                error(
                    ErrorCode::InvalidParams,
                    "Could not load test configuration.",
                )
            })?;
        let mock = MockPlatform::new(os, data_dir.to_owned());
        mock.input
            .set_permissions(Permissions {
                accessibility: PermissionStatus::Granted,
                input_monitoring: PermissionStatus::Granted,
                injection: PermissionStatus::Granted,
            })
            .map_err(|_| error(ErrorCode::Internal, "Could not initialize test input."))?;
        let native = Arc::new(
            NativePeerManager::with_test_keystore(
                NativeConfig {
                    data_dir: data_dir.to_owned(),
                    bind_addr: ([127, 0, 0, 1], port).into(),
                    name: config.settings.device_name.clone(),
                    os,
                    monitors: mock
                        .input_backend()
                        .monitors()
                        .map_err(|_| error(ErrorCode::Internal, "Could not read displays."))?,
                    discovery: false,
                },
                store,
            )
            .await
            .map_err(native_error)?,
        );
        Self::finish_native(
            data_dir,
            config,
            Arc::new(mock.clone()),
            Some(mock),
            native,
            None,
        )
        .await
    }
}

fn native_platform() -> Result<Arc<dyn Platform>, IpcError> {
    #[cfg(windows)]
    let platform =
        glide_platform_win::WindowsPlatform::new().map(|p| Arc::new(p) as Arc<dyn Platform>);
    #[cfg(target_os = "macos")]
    let platform = glide_platform_mac::MacPlatform::new().map(|p| Arc::new(p) as Arc<dyn Platform>);
    #[cfg(not(any(windows, target_os = "macos")))]
    let platform: Result<Arc<dyn Platform>, glide_platform::BackendError> =
        Err(glide_platform::BackendError::Unsupported);
    platform.map_err(|_| error(ErrorCode::PermissionDenied, "Native input and clipboard are unavailable. Check OS permissions and the interactive desktop."))
}

// The manager owns the endpoint; this shared handle also permits discovery configuration.
struct NativeHandle(Arc<NativePeerManager>);
impl PeerManager for NativeHandle {
    fn pair_host(&self, now_ms: u64) -> NetFuture<'_, Result<PairingHost, IpcError>> {
        self.0.pair_host(now_ms)
    }
    fn pair_join<'a>(
        &'a self,
        target: PairTarget,
        code: &'a str,
        now_ms: u64,
    ) -> NetFuture<'a, Result<PairingSession, IpcError>> {
        self.0.pair_join(target, code, now_ms)
    }
    fn confirm_pairing(
        &self,
        accepted: bool,
        now_ms: u64,
    ) -> NetFuture<'_, Result<Option<Peer>, IpcError>> {
        self.0.confirm_pairing(accepted, now_ms)
    }
    fn cancel_pair_host(&self) -> NetFuture<'_, Result<(), IpcError>> {
        self.0.cancel_pair_host()
    }
    fn discover(&self) -> NetFuture<'_, Result<Vec<DiscoveredPeer>, IpcError>> {
        self.0.discover()
    }
    fn add_manual<'a>(
        &'a self,
        address: &'a str,
    ) -> NetFuture<'a, Result<DiscoveredPeer, IpcError>> {
        self.0.add_manual(address)
    }
    fn unpair<'a>(&'a self, id: &'a str) -> NetFuture<'a, Result<(), IpcError>> {
        self.0.unpair(id)
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<PeerManagerEvent> {
        self.0.events()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_rate_is_rejected_before_keystore_or_network_work() {
        let dir = tempfile::tempdir().expect("test directory");
        for rate in [0, 1, (2 * 1024 * 1024) / 30] {
            let failure =
                Core::native_with_transfer_rate(dir.path(), None, true, Some(false), Some(rate))
                    .await
                    .err()
                    .expect("invalid rate rejected");
            assert_eq!(failure.code, ErrorCode::InvalidParams);
            assert!(!dir.path().join("identity.namespace").exists());
        }
    }

    #[test]
    fn occupied_port_and_unavailable_keystore_have_actionable_errors() {
        let port = native_error(NativeError::Io(std::io::Error::from(
            std::io::ErrorKind::AddrInUse,
        )));
        assert_eq!(port.code, ErrorCode::Unreachable);
        assert!(port.message.contains("already in use"));
        let identity = native_error(NativeError::Security("private diagnostic".into()));
        assert_eq!(identity.code, ErrorCode::PermissionDenied);
        assert!(identity.message.contains("OS keystore"));
        assert!(!identity.message.contains("private diagnostic"));
    }
}
