# glide-platform-mac

Native macOS `MacPlatform`, `MacInput` and clipboard/display/autostart backends.
Construct on macOS; Windows builds retain portable logic tests without exporting
a native backend. CGEventTap callbacks perform bounded nonblocking sample work.
AppKit/pasteboard operations are marshalled to their owning native thread.

`MacInput` implements `InputBackend::secure_input_enabled() -> Some(bool)` and
`capture_status_changes() -> Receiver<CaptureStatus>`. The old concrete
`status_changes`/`take_capture_fallback` workaround is removed. Secure Input is
independent of grants: `SecureInput(true)` ends forwarding, `false` only reports
recovery. Permission loss, tap disable, unsupported physical samples and native
or sink queue overflow emit shared statuses, latch `InputSink::mark_overflow`
on loss while forwarding and immediately restore Local/cursor association.
The daemon observes both the bounded receiver and authoritative sink latch.
It must release held input and never resume remote control automatically.

Accessibility and Input Monitoring grants are required; posting can fail
independently. `CGEventPost` does not provide application-delivery confirmation.
Synthetic events carry native markers. `release_all` attempts all held releases
and retains failures for retry. Coordinates and extents use one uniform logical space per device, with origin at the
monitor bounding-box top-left. Windows applies `(physical - min_physical_origin) /
primary_scale` to every x/y/w/h and cursor position; the inverse uses that same divisor.
The unit is a primary-scale logical pixel. macOS CG global coordinates are already uniform
points, so only the bounding-box origin is subtracted/added. Per-monitor `scale` is
informational DPI/backing scale; it never rescales device geometry or movement.
Unsupported layouts/samples and unsafe file URLs fail explicitly.

The daemon prefetches clipboard formats/files. Delayed-render methods remain
implemented for trait compatibility, but are not used in v1. The LaunchAgent runs
`glided --headless` with its data directory and optional UI executable.

Disabling launch-at-login persists launchd's disabled override and removes the plist
without booting out or terminating the running engine. Enabling repairs the plist and
clears that override. If the job is already loaded, updated arguments take effect at
the next login; the current daemon is left running. Confirm this lifecycle on macOS
using [MANUAL_TEST.md](MANUAL_TEST.md).

On the Windows contract host (2026-10-02), offline/locked checks pass for both
`aarch64-apple-darwin` and `x86_64-apple-darwin`. Portable mac logic tests pass
in the workspace. **Native linking/FFI ABI, permission changes, Secure Input,
tap fallback, clipboard delivery and teardown remain unverified on a Mac.**
Use [MANUAL_TEST.md](MANUAL_TEST.md) on Apple Silicon and Intel before acceptance.
Objective-C exception containment is still limited by the locked dependencies.

From core with an external target directory:

Cursor visibility is independent of capture. `set_cursor_visible(false)` enables
background hiding through optional runtime-resolved `CGSSetConnectionProperty` /
`_CGSDefaultConnection` with `SetsCursorInBackground = kCFBooleanTrue`, then calls
`CGDisplayHideCursor` on every active display. Each successful hide is recorded and
balanced once by show, including repeat transitions and Drop/unwind. Missing symbols
leave the cursor visible without failing a link. **This native behavior is UNVERIFIED
on macOS here**; the Apple target check only verifies types.

```text
cargo check -p glide-platform-mac --target aarch64-apple-darwin --offline --locked
cargo check -p glide-platform-mac --target x86_64-apple-darwin --offline --locked
```

`read_snapshot` captures pasteboard types/data/sensitivity, owned marker and
changeCount, then rejects raced reads. `publish_snapshot` prepares one
NSPasteboardItem with every representation and commits with one writeObjects call
after change-token/admission checks. Partial native write failure is explicit.
Files include real local public.file-url plus NSFilenamesPboardType for the complete
ordered list in that one item; both forms are accepted on read. No delayed provider
is installed. Finder/native-app compatibility remains a manual Mac acceptance check
in MANUAL_TEST.md. LaunchAgent arguments now include --headless before --data-dir.
