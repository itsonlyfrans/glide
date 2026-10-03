# macOS native acceptance checklist

**All native behavior below is UNVERIFIED on the Windows implementation host.**
Cross-target `cargo check`/Clippy type-check Rust and dependency bindings; they do not link
Apple frameworks, validate handwritten FFI symbols, run AppKit, or demonstrate delivery,
latency, CPU, memory, permission handling, or cleanup. Use macOS 11 or later, on both an
Apple Silicon Mac and an Intel Mac. This crate exports no native backend on Windows.

Keep another local input device/session available during swallow tests. Use a small local
test harness that constructs `MacPlatform`, obtains the input and clipboard traits, and
drains their bounded receivers. Native daemon integration is owned by another engineer.
Do not connect to unpaired peers or log key/clipboard payloads in this harness.

## Build and integration

- [ ] On each Mac run `cargo build --offline --locked -p glide-platform-mac`,
  `cargo test --offline --locked -p glide-platform-mac`, and
  `cargo clippy --offline --locked -p glide-platform-mac --all-targets -- -D warnings`.
  Build/link a harness that calls the constructors to exercise all native framework symbols.
- [ ] Retain `InputBackend::capture_status_changes()` and wire its shared
  `CaptureStatus` values plus `InputSink::take_overflow()` into return-home/release.
  Poll the trait's `secure_input_enabled()` as authoritative state. Saturating
  the bounded status queue must not hide the sink loss latch. The old concrete
  status/fallback APIs are removed. Never resume remote control implicitly.
- [ ] Check backend construction without grants does not prompt; explicit
  `request_permissions()` and settings opening are user setup actions only.

## Permissions and Secure Input

Run these through the real packaged Electron **Glide.app**, whose child engine is
`Glide.app/Contents/Resources/bin/glided`. macOS should attribute these requests to
the responsible **Glide** app. A standalone harness does not prove app attribution.
All boxes remain unverified until executed on a real Mac.

- [ ] Fresh install/clean TCC state: daemon reaches `ready` with missing permissions;
  send `permissions.request` with `{}`. Verify Accessibility, Input Monitoring and
  event-posting requests (the OS may share Accessibility/posting consent), Glide attribution,
  empty success result and refreshed state. Leave dialogs unanswered and confirm
  `get_state`/return-home stay responsive. Repeat the request: no repeated prompt spam.
- [ ] Deny then grant each permission in System Settings: denied/unknown state refreshes
  within about 2 seconds. Once grants are available, capture starts without restarting
  where the OS permits; one "Glide can now share your mouse and keyboard" notification
  appears. Verify actual keyboard/mouse capture, not only reported grants.
- [ ] Grant Input Monitoring while Glide is running. If macOS requires relaunch,
  capture retry failure sets `permissions.restart_required: true` despite granted states.
  Relaunch the whole Glide app, then verify capture and `restart_required: false`.
  Repeated polls must not retry the failed tap or flood notifications.
- [ ] With grants available, revoke Accessibility/Input Monitoring/posting separately
  while forwarding with held keys/buttons. Check immediate native local fallback,
  remote hold release and changed state (slow daemon checks occur every 10 seconds).
  Regrant: 2-second checks resume, capture can recover, but forwarding stays local.
  No second recovery notice appears during the same daemon lifetime.

- [ ] Deny Accessibility and Input Monitoring independently: capture/swallow fails,
  input remains local, and permission states reflect the effective capability.
- [ ] Grant both and retry after any OS-required restart. Revoke each during forwarding;
  verify Local fallback, cursor recovery, and remote held-input release.
- [ ] Deny/revoke event-posting access: injection reports an error and forwarding stops.
  `CGEventPost` returns no delivery result; confirm actual application input separately.
- [ ] Enable Secure Input using a password field or Terminal secure keyboard entry:
  status changes surface, TCC grant states remain accurate, swallowing/new injection stop,
  and protected keystrokes are not enqueued. Disable it and explicitly restart sharing.
- [ ] Open Accessibility/Injection and Input Monitoring settings; verify the respective
  Privacy_Accessibility / Privacy_ListenEvent deep links on the tested OS version.

## Physical capture and recovery

**UNVERIFIED here: inactive/background cursor visibility (2026-10-02).**

- [ ] Cross from Mac to Windows while an ordinary foreground Mac app is active:
  the Mac cursor disappears on all Retina/non-Retina displays even though Glide is
  in the background. Windows remains visible. Return to Mac: the cursor reappears.
- [ ] When the Mac is a receiver, repeat Enter/Leave ten times, including duplicate
  Leave and return-home. Check no reference-count accumulation and normal local input.
- [ ] While Mac is inactive, unplug a display, lose the link, stop heartbeats, sleep/wake,
  revoke posting access, force a transient injection error, shut down, and trigger a Rust
  unwind. Check association/visibility restoration and held-key/button release.
- [ ] Exercise absent `CGSSetConnectionProperty` / `_CGSDefaultConnection` symbols
  (a test build masking lookup): cursor stays visible; pairing/forwarding still works.
- [ ] With displays above/left of primary and mixed backing scales, compare reported
  rectangles to CGDisplayBounds minus bounding-box origin; captured points and injected
  points agree at each actual monitor edge. Backing scale never changes these coordinates.

- [ ] Observe keyboard, mouse movement, all buttons, wheel and precise trackpad scrolling
  in Local mode without suppressing local application input.
- [ ] Call `start_capture` twice: exactly one tap exists, and the new sink receives input.
  Start with modifiers and Caps Lock already held/enabled; verify first releases/toggles.
- [ ] Enter both `Swallow { lock_pos: false }` and `Swallow { lock_pos: true }`:
  physical input is suppressed locally; call `set_cursor_visible(false)` separately.
  The cursor is hidden, locked mode detaches pointer
  association, and relative deltas continue. Confirm counts convert correctly to logical
  points on Retina and non-Retina monitors, at different pointer acceleration settings.
- [ ] Return Local repeatedly: cursor hide/show counts balance and mouse association returns.
- [ ] Fill the sink and disconnect its receiver while swallowing: the overflowing event passes
  locally, Local fallback occurs immediately, and overflow/fallback latches trigger cleanup.
- [ ] Trigger tap timeout/user-input disable (debugger suspension or controlled test):
  the tap re-enables while capture returns Local; explicitly re-enter forwarding afterward.
- [ ] Exercise an unsupported physical key/Fn/keypad variant while swallowing: fail locally
  and surface fallback rather than silently dropping it. Unmapped usages are unsupported.
- [ ] Drop the backend and exit normally while swallowing/holding injected keys/buttons;
  verify cursor restoration, tap removal, held-input releases and no lingering thread.
- [ ] Test a Rust unwind, abrupt process termination, sleep/wake, screen lock/logout and
  display loss. Drop guards cover orderly teardown/unwind; SIGKILL cannot execute Rust Drop.
  Verify the OS restores its process-owned cursor state after a hard crash before release.

## Injection and display geometry

- [ ] Observe application-visible letters, punctuation, number row, keypad, navigation,
  F1-F20, Caps Lock and every left/right modifier on ANSI, ISO and JIS keyboards/layouts.
  Check ISO section/non-US # and JIS Yen, Ro/underscore, keypad comma, Eisu and Kana.
- [ ] Hold both sides of a modifier and release one: the other stays active. Repeat ordinary
  keys and Caps ON/OFF. Hold both HID aliases for the ISO/backslash key; release one at a time.
- [ ] Check volume up/down/mute where the OS accepts their virtual keycodes. Consumer-page
  playback/brightness keys and Mac Fn have no page-0x07 shared representation and are unsupported.
- [ ] Inject moves while left/right/middle/extra buttons are held: proper drag event types,
  accurate cursor location, single/double/triple clicks and balanced button releases.
  Click grouping currently uses a fixed 500 ms / 4-point threshold; verify app behavior.
- [ ] Inject horizontal/vertical fractional scroll, line and precise pixel units; verify
  direction, trackpad behavior and no truncation surprises in multiple applications.
- [ ] Confirm every injected event carries the magic and never becomes a physical sample,
  triggers takeover, or is swallowed by the local capture tap. Other OS synthetic samples
  must retain `injected = true` and pass locally.
- [ ] Call `release_all` repeatedly and after a partial injection failure; every tracked
  key/button release is attempted and failed releases remain tracked for a later retry.
  Revoke posting access during a release and disconnect a display while a button is held;
  permission races retain held state and releases do not depend on fresh display geometry.
- [ ] Test negative/above-primary monitor origins, mixed scale, scaled resolutions, rotation,
  mirroring and multi-display edges. Snapshot dimensions are points, scale is backing pixels
  per point, and coordinates are normalized to the monitor-union top-left.
- [ ] Plug/unplug/rearrange/resize displays: reconfiguration notification and fresh snapshot,
  authoritative cursor conversion, safe Local fallback during topology changes. No stale moves
  should inject into absent displays; non-finite/off-display input must be rejected.

## Clipboard

The v1 daemon prefetches data/files and never uses delayed rendering. The delayed
provider checks below apply only to the retained native compatibility API.

- [ ] Copy/paste UTF-8 text (Unicode/empty/multiline), HTML and RTF in several native apps;
  multiple formats under one marker survive as representations of the same clipboard item.
- [ ] Copy PNG and TIFF-only images: PNG is preferred, TIFF becomes a valid PNG with correct
  dimensions/alpha/colors. Corrupt or oversized images fail safely; check decoder memory.
  TIFF conversion accepts a single classic TIFF image, up to 8 Mi pixels, 16,384 per side,
  four samples and 16 bits per sample within a 64 MiB decoded-data budget. BigTIFF and
  multi-page TIFF fail closed. Confirm supported normal TIFFs and rejected edge cases.
- [ ] Copy multiple files/directories in Finder, including spaces, Unicode and # in names;
  order, absolute local file URLs, names, sizes and directory flags survive. File writes expose
  real prefetched local URLs only. Reject malformed/remote URLs and unsafe symlink entries.
- [ ] Apply ConcealedType, TransientType and AutoGeneratedType independently and together:
  no payload is read/announced for sync. Rapidly swap ordinary/sensitive clipboard owners
  during reads; a change-count race must fail closed.
- [ ] Self writes are ignored by change-count ownership and marker. External copies, including
  a copied/forged Glide marker, still announce changes; no loop or mistaken suppression.
- [ ] Check change detection starts at 100 ms and backs off while idle without spinning.
  Saturate the bounded notification queue: ResyncRequired is delivered when space returns.
- [ ] Register delayed text/HTML/RTF/PNG; no fetch occurs before a paste. Wait longer than
  the fetch timeout before first paste to verify idle placeholders remain valid.
- [ ] Paste an uncached delayed format: callback promptly enqueues RenderRequested and never
  waits for network work. **First paste may fail** under the binding no-wait callback rule.
  Fulfill within the fetch timeout, retry and verify correct bytes. Register multiple formats.
- [ ] Exercise fetch timeout, cancellation, late completion, ownership replacement and shutdown:
  no stale payload replaces somebody else's clipboard; pending providers/payloads are released.
  Verify provider IPC is actually serviced by the owning run loop.
- [ ] Check maximum byte/type/item limits, malformed payloads and native read/write failures;
  clipboard contents/path names never enter logs, and retained Rust byte buffers are wiped.
- [ ] Native Objective-C exception containment is unavailable: `objc2/exception` needs the
  unprefetched/unlocked `objc2-exception-helper`. Test pasteboard-owner/server failures and
  process recovery on a Mac. Rust panic containment does not catch Objective-C exceptions.

## Platform and performance

- [ ] Data directory is `~/Library/Application Support/Glide`, owned by the signed-in user,
  private, and refuses unsafe home/path/symlink configurations without touching other files.
- [ ] LaunchAgent is absent/default-off initially. Enable/disable/idempotence work, plist paths
  escape XML, registration failures restore prior configuration, and symlink/foreign plist
  collisions are rejected. Confirm login persistence with `launchctl` and native logs.
- [ ] Toggle launch-at-login off while glided is running as its LaunchAgent. Confirm the engine
  stays alive, the plist is removed, launchd records the disabled override, and the next login
  does not start it. Re-enable and confirm registration returns without terminating the current
  process; if already loaded, updated arguments apply after the next login.
- [ ] LaunchAgent includes --headless: close stdin and confirm the service stays
  alive until app.shutdown/SIGTERM, releases holds and exits cleanly.
- [ ] Measure capture-to-inject p99, sustained mouse allocation behavior, idle CPU/RSS, thread
  shutdown, resource handles and clipboard polling. Neither host tests nor cross-checks establish
  the SPEC's <2 ms p99, <0.5% idle CPU or <40 MB RSS budgets.

Record hardware/OS/build, permission state, pass/failure and reproduction for each box.
Leave boxes unchecked until exercised on a Mac; do not infer a pass from pure logic tests.

## CONTRACTS-v2 atomic publication (native Mac checks still required)

- [ ] Snapshot text/HTML/RTF/PNG/files together; changeCount remains stable through
  all reads. Ordinary/sensitive owner swaps return ClipboardChanged without leaking data.
- [ ] Publish a complete multi-format bundle as ONE NSPasteboardItem. Native apps
  paste each representation; inspect types and marker under one changeCount.
- [ ] Replace local clipboard after snapshot, then publish with its old token: result
  is ReplacedLocalChange and the fresh local copy survives. Revoke ClipboardAdmission
  before commit: result is Revoked with no ownership change.
- [ ] Inject preparation/native write failure: no partially published success; verify
  PartialFailure count/clear status and preservation of a competing local owner.
- [ ] Publish several real files/directories in one item with public.file-url and
  complete NSFilenamesPboardType list. Paste all of them in Finder and native apps;
  verify order, Unicode, spaces, #, symlink rejection and staging lease retention.
- [ ] A forged external marker is not reported as owned in read_snapshot; repeated
  self publication does not loop. Confirm no delayed provider/network wait exists.
