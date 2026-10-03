# Windows native backend

`WindowsPlatform::new()` implements the foundation traits. `WindowsInput` owns one
dedicated hook thread, a Raw Input message-only window, and an invisible top-level
window for display-change broadcasts. A second input backend is rejected because
Windows Raw Input registration is per process. Capture begins in Local mode.
Callbacks enqueue fixed-size samples in a bounded channel; translation happens
outside the hooks. Synthetic input retains `injected=true`, including input from
other injectors; callers must never forward these samples or use them for takeover.

Swallow uses physical Raw Input deltas divided by the device primary monitor DPI scale;
queued samples are coalesced with a one-shot 1ms high-resolution waitable timer
before delivery; reliable events flush pending movement to preserve ordering.
This requires Windows 10 1803 or newer. A one-pixel ClipCursor rectangle
holds the cursor. Visibility is independent of capture: `set_cursor_visible(false)`
replaces all standard system cursor shapes with transparent cursors, guarded against
double-hide; `true` reloads the user scheme with `SPI_SETCURSORS`. Startup, Local
return, console shutdown, tray Quit and Drop restore the scheme. Clipping is restored
on Local, enqueue failure, display changes and Drop. Both native sample-queue and input-sink loss call
`InputSink::mark_overflow()`, even when the sink queue has room, and immediately
disable Swallow. `InputBackend::capture_status_changes()` emits bounded
`CaptureStatus::QueueOverflow` or `UnsupportedInput` for detected failures;
the overflow latch remains authoritative if that notification queue fills.
`capture_faulted()` is diagnostic and clears on an explicit mode request.
After daemon return-home/release, re-entry may reuse the existing sink; replacing
it is no longer required. Queue loss does not falsely revoke OS permission or
injection capability. Windows leaves `secure_input_enabled()` as `None`.
Absolute Raw Input devices (for example, tablets) return capture to Local rather
than pretending their coordinates are relative mouse counts.

`SendInput` uses USB HID Set-1 scan-code mappings, E0 flags, a VK_PAUSE special
case for the E1 sequence, and E0 PrintScreen. Unsupported usages fail explicitly.
Keys/buttons are tracked until successfully released. `release_all()` tries all
held inputs even when one release fails. `sync_modifiers()` handles left/right
Ctrl, Alt, Shift and Win; the daemon still owns cross-OS shortcut translation.
The trait's pointer event injects an absolute logical position; `inject_relative()`
uses the same logical units and converts deltas with the primary divisor. Wheel values use Win32 units (120 per
detent); fractional amounts accumulate without losing sub-unit precision.

Call `enable_per_monitor_v2()` at process startup **before any UI/window is created**.
`glide-daemon/build.rs` embeds `Glide.manifest` in glided.exe with MSVC linker
manifest arguments; its PE-resource integration test checks PerMonitorV2. A Rust library cannot set an executable's manifest. Constructors
verify PMv2 and fail if the host already selected a different awareness mode.
Coordinates and extents use one uniform logical space per device, with origin at the
monitor bounding-box top-left. Windows applies `(physical - min_physical_origin) /
primary_scale` to every x/y/w/h and cursor position; the inverse uses that same divisor.
The unit is a primary-scale logical pixel. macOS CG global coordinates are already uniform
points, so only the bounding-box origin is subtracted/added. Per-monitor `scale` is
informational DPI/backing scale; it never rescales device geometry or movement.
No edge-walking or component shifting is used; physical gaps/offsets are preserved.
`cargo run --offline --locked -p glide-platform-win --example monitors` prints the
engine snapshot beside raw PMv2 rectangles/DPI without moving or hiding the cursor.

Windows UIPI prevents injection into higher-integrity applications. The foreground
integrity and active input desktop are checked on injection; permission reports
reflect device access, not the integrity of the foreground application. Inability
to query cursor access is Unknown. UAC, the lock screen and secure desktops are unsupported,
including when elevated. Elevation alone is not a promise of secure-desktop access.
Each injection checks foreground integrity and the active desktop without
allocating. The foreground can change between inspection and SendInput; failed
injection is classified as PermissionDenied (known secure desktop), TargetElevated
(known higher-integrity foreground), or Transient (unknown/other SendInput failure).
Only explicit permission loss ends forwarding immediately. No UIAccess bypass
or privilege elevation is attempted. Ctrl+Alt+Delete cannot be injected.

Clipboard access is direct Win32: Unicode text, CF_HTML, RTF, PNG and DIB/DIBV5
through WIC, and real CF_HDROP paths with copy drop effect. Clipboard-change events
are bounded, and markers identify self writes. Sensitivity metadata must be honored
by the caller's exclusion policy. File contents remain the transfer crate's job.
Delayed-render methods remain for compatibility and their callbacks never wait
for the daemon/network. **The v1 daemon does not use them**: it prefetches formats
and verified files before publication, as SPEC section 4 now requires. Pending
requests in the optional native API still expire.

Data storage resolves to `%APPDATA%\Glide`. HKCU Run helpers are implemented;
construction never enables autostart. The engine reconciles the default-off
launch-at-login setting with `Glide` and registers its quoted executable plus
`--headless --data-dir` and optional `--ui`. Registry tests use a unique temporary
value and never mutate the real `Glide` startup value.
Opening permission settings is a successful no-op on Windows.

From `core/`, use the supplied external `CARGO_TARGET_DIR`:

```powershell
cargo fmt -p glide-platform-win --check
cargo clippy --offline --locked -p glide-platform-win --all-targets -- -D warnings
cargo test --offline --locked -p glide-platform-win -- --test-threads=1
cargo test --offline --locked -p glide-platform-win -- --ignored --test-threads=1
cargo run --offline --locked -p glide-platform-win --example probe
```

The ignored mouse loopback test nudges the real pointer by one pixel, verifies a
synthetic captured event, reverses the nudge and restores the original position
with a drop guard. Run it on an interactive desktop. It never types or clicks.
Clipboard integration tests temporarily replace content and restore saved text;
close clipboard-sensitive applications while running them. Rich third-party/delayed
formats may not be recoverable as text. The probe prints live input, including key
usages, only for explicit manual diagnostics.

Verification on this machine (2026-10-02): formatting, all-target Clippy with
warnings denied, and 26 regular tests passed using the offline locked workspace.
The contract regression installs a non-full sink, overflows the native sample
queue, and verifies both the sink latch and bounded QueueOverflow status.
The regular tests include live monitor enumeration, read-only desktop diagnostics,
WIC PNG/DIBV5 round-trips and synthetic input translation. The empty non-Windows
library also passed `cargo check --target aarch64-apple-darwin`.

The ignored clipboard test passed after repairing a command/event-snapshot queue
race: text, HTML, RTF, PNG, CF_HDROP and delayed text were written/read, then saved
representations were restored and original text checked exactly when present.
The ignored mouse test was attempted and returned Unavailable before its nudge;
read-only diagnostics confirmed `GetCursorPos` fails with Windows error 5 (access
denied). Injection capability was Unknown and the backend refuses it. No mouse
loopback or typed-key verification succeeded in this session. Retry on an
interactive desktop with:

```powershell
cargo test --offline --locked -p glide-platform-win injected_mouse_loopback_is_synthetic_and_reversed -- --ignored --test-threads=1
```

Hardware raw-delta accuracy, 1 kHz+ coalescing, display hotplug, mixed-DPI hardware,
actual elevation/secure-desktop transitions and the SPEC latency/CPU budgets need
interactive hardware acceptance beyond unit and loopback tests. Autostart registry
mutations, actual key/modifier injection and cursor hide/pin behavior were not run.

Atomic clipboard APIs use one OpenClipboard scope for the snapshot and one
OpenClipboard/EmptyClipboard/SetClipboardData batch for publication. Every format
and HGLOBAL (including CF_HDROP, copy effect and marker) is prepared before locking
the clipboard; sequence/admission checks immediately precede EmptyClipboard.
SetClipboardData failure reports PartialFailure and attempts to clear partial data
while still locked. No delayed rendering occurs. The extended explicit save/restore
round-trip test verifies complete text/HTML/RTF/PNG/files publication, ownership,
revoked admission and a replaced-local-copy race. See the contract validation report
for the latest real Windows result; this does not prove cross-app image rendering.
