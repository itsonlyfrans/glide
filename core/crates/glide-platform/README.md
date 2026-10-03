# glide-platform

Shared native traits, bounded copyable capture types and scriptable mocks.
Native callbacks enqueue samples only; daemon work never runs in a hook/tap.
Coordinates and extents use one uniform logical space per device, with origin at the
monitor bounding-box top-left. Windows applies `(physical - min_physical_origin) /
primary_scale` to every x/y/w/h and cursor position; the inverse uses that same divisor.
The unit is a primary-scale logical pixel. macOS CG global coordinates are already uniform
points, so only the bounding-box origin is subtracted/added. Per-monitor `scale` is
informational DPI/backing scale; it never rescales device geometry or movement.
Synthetic samples retain `injected = true`.

`InputSink::mark_overflow(&self)` uses a Release store to latch native queue
loss independently of sink capacity. `take_overflow` consumes the latch with
AcqRel semantics. Any native or sink loss immediately returns capture to Local;
the daemon releases remote/injected held input on its next control turn. Status
queue pressure cannot hide this escape: backends latch loss as well as notify.

`InputBackend::secure_input_enabled(&self) -> Option<bool>` defaults to `None`;
macOS returns authoritative `Some(bool)`, independent of permission grants.
`capture_status_changes() -> Receiver<CaptureStatus>` is bounded, with an empty
disconnected default for backends that never emit. Retain one receiver rather
than polling newly cloned receivers in separate consumers.

`CaptureStatus` variants are `SecureInput(bool)`, `PermissionLost`, `TapDisabled`,
`UnsupportedInput`, `QueueOverflow`. Every loss ends forwarding; only
`SecureInput(false)` is recovery. Recovery never implicitly resumes capture.
Windows reports queue loss/unsupported samples it can detect, and ordinary
permissions report OS grants independently of foreground UIPI blocking.
Injection errors have an explicit classification rather than treating every failure as denial. macOS maps Secure Input, permission and tap fallback
onto this shared trait rather than separate concrete-backend status methods.

`MockInput::script_capture_status` scripts all variants, updates Secure Input,
and returns Local/latches loss even if its bounded notification queue is full.
Tests cover this pressure case and daemon tests cover held-input release and
recovery without resumption.

Clipboard delayed-render methods remain in `ClipboardBackend` for compatibility.
**The v1 daemon does not call them**: it prefetches data and publishes verified
real local files. Native permission, keystore and capture/injection acceptance
must be exercised on each OS; cross-checks and mocks do not prove OS delivery.

```text
cargo test -p glide-platform --offline --locked
cargo clippy -p glide-platform --all-targets --offline --locked -- -D warnings
cargo check -p glide-platform --target aarch64-apple-darwin --offline --locked
```

Run from core with an external target directory.

`BackendError::injection_failure() -> InjectionFailure` separates `PermissionDenied`,
`TargetElevated`, `InvalidPosition`, and `Transient`; other backend errors classify as
transient. `clamp_cursor_to_monitors` finds the nearest real monitor point without
allocating. Native pointer injection clamps finite outside positions, including gaps;
non-finite input remains invalid. The daemon retries InvalidPosition once on a fresh
snapshot, drops transient events and counts them, and restores/releases holds only on
explicit denial or 50 consecutive/3 seconds of transient rejection. UIPI warnings are
limited to once per minute across sessions. There is no notification per mouse event.

`InputBackend::set_cursor_visible(&self, visible: bool) -> Result<(), BackendError>`
defaults to a no-op. Native backends guard repeated hides; visibility is independent
of capture mode. Missing cosmetic support cannot end a session. The daemon restores
on Enter, return-home, loss/timeout, teardown and shutdown, and retries failed restores
on its bounded safety turn. `MockInput::cursor_visible()` exposes requested visibility
for state-transition tests.

`ClipboardBackend::read_snapshot() -> Result<ClipboardSnapshot, BackendError>`
returns `contents`, aggregate `sensitivity`, owned `marker` and opaque native
`ClipboardChangeToken`. A raced read returns `BackendError::ClipboardChanged`.
`publish_snapshot(contents, marker, expected_change_token, admission)` validates
and prepares all formats first, then rechecks the native token and
`ClipboardAdmission::is_admitted()` immediately before ownership changes.
`ClipboardAdmission::for_generation(Arc<AtomicU64>, expected)` supports nonblocking
session/policy cancellation; `revoke()` invalidates only the captured generation.

Handle `ClipboardPublish::Published { change_token }`,
`ReplacedLocalChange { actual_change_token }`, `Revoked`, and
`PartialFailure { formats_written, cleared, error }` separately. Stale/revoked
publication preserves the existing clipboard; native partial failure never
counts as success. Bundles allow 32 distinct formats, 64 MiB aggregate bytes and
4096 absolute local file paths; neither API uses delayed rendering.
`MockClipboard::script_publish_race(ClipboardRace::{LocalCopy, RevokeAdmission,
NativeFailure})` scripts commit races and partial failure with the same contract.
