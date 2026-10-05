#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "glided", version, about = "Glide software KVM daemon")]
struct Cli {
    #[arg(long)]
    data_dir: PathBuf,
    /// Use mock input, clipboard, displays and networking for UI development.
    #[arg(long, conflicts_with = "mock_platform")]
    mock_backends: bool,
    /// Use mock input, clipboard and displays with real keystore-backed secure networking.
    #[arg(long)]
    mock_platform: bool,
    /// Seed the explicit mock platform for process-level clipboard tests.
    #[cfg(debug_assertions)]
    #[arg(long, requires = "mock_platform")]
    mock_clipboard_text: Option<String>,
    /// Run independently of stdin, with authenticated local control and native Windows tray.
    #[arg(long)]
    headless: bool,
    /// Absolute existing app executable opened on demand from the native tray.
    #[arg(long)]
    ui: Option<PathBuf>,
    /// Disable mDNS advertisement and browsing; manual pairing remains available.
    #[arg(long)]
    no_discovery: bool,
    /// Override and persist the UDP listening port.
    #[arg(long)]
    port: Option<u16>,
    /// Optional aggregate file-send cap in MiB/s; no cap by default.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    transfer_rate_mbps: Option<u32>,
    #[arg(long, default_value = "info")]
    log_level: String,
}

fn main() -> anyhow::Result<()> {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(cores.clamp(2, 4))
        .max_blocking_threads(cores.saturating_sub(1).clamp(1, 4))
        .enable_all();
    // The threads that receive mouse movement must not be starved when the Mac is busy.
    #[cfg(target_os = "macos")]
    builder.on_thread_start(glide_platform_mac::prioritize_current_thread);
    let runtime = builder.build()?;
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    if result
        .as_ref()
        .is_err_and(|e| e.is::<glide_daemon::control::AlreadyRunning>())
    {
        eprintln!("Glide is already running");
        std::process::exit(75);
    }
    result
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Some(ui) = &cli.ui {
        glide_daemon::autostart::validate_ui(ui)?;
    }
    let instance = if cli.headless || !cli.mock_backends {
        Some(glide_daemon::control::Instance::acquire(&cli.data_dir)?)
    } else {
        None
    };
    use tracing_subscriber::prelude::*;
    let (log, layer) = glide_daemon::logging::RollingLog::start(&cli.data_dir);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(tracing_subscriber::EnvFilter::try_new(&cli.log_level)?),
        )
        .with(layer.with_filter(tracing_subscriber::filter::LevelFilter::INFO))
        .init();
    tracing::info!("engine starting");
    let core = if cli.mock_backends {
        glide_daemon::Core::mock(&cli.data_dir, cli.port)
            .await
            .map_err(|_| {
                glide_proto::ipc::IpcError::new(
                    glide_proto::ipc::ErrorCode::InvalidParams,
                    "Could not initialize mock backends or load configuration.",
                )
            })
    } else {
        glide_daemon::Core::native_with_transfer_rate(
            &cli.data_dir,
            cli.port,
            cli.mock_platform,
            cli.no_discovery.then_some(false),
            cli.transfer_rate_mbps
                .map(|rate| u64::from(rate) * 1024 * 1024),
        )
        .await
    };
    let mut core = match core {
        Ok(core) => core,
        Err(error) => {
            glide_daemon::ipc::startup_failure(&mut tokio::io::stdout(), error).await?;
            anyhow::bail!("daemon initialization failed; see the IPC notification");
        }
    };
    core.configure_background(cli.ui, !cli.mock_backends && !cli.mock_platform)?;
    #[cfg(debug_assertions)]
    if let Some(text) = cli.mock_clipboard_text {
        core.mock_platform()
            .expect("clap requires mock platform")
            .clipboard
            .set_external_content(glide_platform::ClipboardContent::bytes(
                glide_platform::ClipboardFormat::Text,
                text.into_bytes(),
                glide_platform::ClipboardSensitivity::default(),
            )?)?;
    }
    let result = if cli.headless {
        glide_daemon::control::serve(core, instance.expect("headless instance lock")).await
    } else {
        let mut instance = instance;
        if let Some(instance) = &mut instance {
            instance.publish_stdio_owner()?;
        }
        glide_daemon::ipc::serve(tokio::io::stdin(), tokio::io::stdout(), core).await
    };
    tracing::info!("engine stopped");
    drop(log);
    result
}
#[cfg(any(glide_release, not(debug_assertions)))]
const _: () = assert!(
    !glide_net::TEST_SUPPORT_ENABLED,
    "glided release builds forbid glide-net/test-support"
);
