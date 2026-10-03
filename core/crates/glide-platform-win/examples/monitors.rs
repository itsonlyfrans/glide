#[cfg(windows)]
fn main() -> Result<(), glide_platform::BackendError> {
    glide_platform_win::enable_per_monitor_v2()?;
    for (m, raw) in glide_platform_win::monitor_geometry_snapshot()? {
        println!(
            "{} primary={} OS physical={raw:?} dpi={} scale={} engine=({}, {}) {}x{}",
            m.id,
            m.primary,
            m.scale * 96.0,
            m.scale,
            m.x,
            m.y,
            m.w,
            m.h
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This read-only monitor probe requires Windows.");
}
