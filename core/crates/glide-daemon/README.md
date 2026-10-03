# glide-daemon

The daemon uses the v3 contracts for pinned QUIC input, live metadata, atomic eager
clipboard bundles and verified file/large-format prefetch. The original integration
proposal is [glide-daemon-integration.md](../../docs/proposals/glide-daemon-integration.md).
Production keystore failure reports an IPC notification/error and fails closed.
The test-support keystore retains real PAKE, dual SAS, pinned TLS and trust storage;
it is never a production fallback. Optimized/release glided rejects that feature.

## Run and lifetime

Run from core with an external CARGO_TARGET_DIR. For example, in PowerShell:

```powershell
$env:CARGO_TARGET_DIR = '$env:TEMP/glide-integration-target'
```

All build/test/check Cargo commands are offline/locked.
For native networking with mock OS input/displays/clipboard, keep stdin open:

```text
cargo run -p glide-daemon --bin glided --offline --locked -- --mock-platform --no-discovery --port 24911 --data-dir $env:TEMP/glide-a
```

Use a different data directory and port for each instance. Each directory selects
an independent OS-keystore identity; only native pins restore trust. config.json
contains public preferences/layout and clipboard overrides, never private keys.
Startup repairs unfinished pairing records without trusting candidates and warns
when records were discarded. ready precedes state and recovery notices. Startup
failure emits notification and error response id 0 without ready.

| Flag | Behavior |
| --- | --- |
| --mock-backends | Mock OS/network, unauthenticated UI fixture |
| --mock-platform | Mock OS, real keystore/PAKE/pinned QUIC |
| --data-dir PATH | Persistent namespace, settings, pins and protected staging |
| --headless | Authenticated local control and Windows tray, independent of stdin |
| --ui PATH | Validate/store an absolute existing app executable for Open Glide |
| --no-discovery | Persist mDNS off; manual pairing still works |
| --port N | Override and persist the UDP listening port at startup |
| --transfer-rate-mbps N | Aggregate file-send cap in MiB/s |
| --log-level LEVEL | stderr tracing; content/keys/paths are never info logs |

Mock flags are mutually exclusive. The debug-only --mock-clipboard-text flag
seeds an explicit mock-platform test; it is omitted from optimized builds.
app.shutdown, EOF in normal mode, Ctrl+C, Unix SIGTERM and Core drop release held
input. The Mac LaunchAgent uses --headless. build.rs embeds the Windows
PerMonitorV2 manifest; the stdio process fixture verifies its PE resource.

Pair with pairing.start_host on A, pairing.join(address/code) on B, then
pairing.confirm(accepted:true) on **both** screens only after the SAS phrases match.
The join response waits for both confirmations. Decline/cancel/expiry never pins
an unconfirmed device. Pairing work runs outside the capture loop.

## Input and state

`permissions.request` takes `{}` and returns `{}` after a blocking worker calls
`InputBackend::request_permissions()` and refreshes state. It never waits for a user
to answer macOS prompts or runs prompt work on the capture/escape task. Accepted
requests are serialized in a bounded queue; each calls the backend once. Constructors
and passive polling do not prompt. The bundled `glided` child requests permissions
for the responsible **Glide** app.

Missing input grants keep IPC running. Denied/unknown capabilities are checked every
2 seconds; granted ones are checked every 10 seconds for revocation (all `n/a` stops
polling). Only changes publish passive state events; an explicit request also publishes
a refreshed snapshot. Permission loss returns control home and releases held input.
Capture retries after the grants become available, without resuming remote forwarding,
and emits "Glide can now share your mouse and keyboard" once per daemon lifetime.
`State.permissions.restart_required` defaults to false, including older JSON snapshots.
It becomes true if grants are available but capture still fails, so the UI can offer a
Glide relaunch; repeated polls do not retry a failed tap or repeat the notification.
Secure Input remains separate from grants and is not itself a reason to ask for relaunch.

Real Mac permission acceptance (unchecked on the Windows host):

- [ ] Fresh packaged Glide.app: send `permissions.request`, check all permission prompts/settings identify Glide, response `{}` and refreshed state; repeat without dialog spam.
- [ ] Deny then grant Accessibility/Input Monitoring: verify 2-second updates, automatic capture retry and one recovery notice.
- [ ] Grant Input Monitoring then relaunch if required: failed retry sets `restart_required`; relaunch clears it when capture works.
- [ ] Revoke Accessibility, Input Monitoring and posting separately while forwarding: local fallback and release, changed state, 2-second regrant tracking; regrant never resumes remote control automatically.

Follow [the native Mac checklist](../glide-platform-mac/MANUAL_TEST.md) for both architectures.

permissions, self.monitors, peer connection/monitors/latency/stats and discovered
are platform/authenticated manager facts. Rename and hotplug call
update_local_metadata, which updates discovery and connected peers. DiscoveryRemoved
removes stale candidates. The authenticated accept queue is drained throughout
the daemon lifetime; name-only or duplicate metadata retains active input epochs. Port changes call update_listen_port; its current
unsupported rebinding returns a restart instruction without mutating settings.
Use the same data directory with --port to preserve pins on restart.

Layout/settings/per-peer clipboard policy persist atomically. Layout uses Lamport
and device-ID LWW replication. Local edits reject overlap, including known offline
monitors. Accepted remote layouts are reconciled against local paired membership:
unknown IDs are removed, missing devices are added at the rightmost edge, and
overlapping monitor placements move to the nearest free actual monitor edge.
Bounding boxes may overlap when their screens do not. New devices touch an actual
monitor edge, preferring the rightmost/primary right edge; gaps inside a device are
not reserved. Reconciliation caps rectangle comparisons on unusually complex
untrusted topologies and rejects an exhausted search rather than accepting overlap. Reconciliation rebroadcasts a bumped local Lamport
version; unchanged proposals retain their incoming version. Version-only updates
preserve active input and held keys. Layouts contain at
most 32 devices including self. Malformed positions, duplicate IDs, oversized
layouts and exhausted clocks are dropped without changing state or notifying.
If changed
peer monitors make the existing layout overlap, the daemon returns home, retains
the true monitor state and uses a safe local desktop until the layout is repaired.

Mouse uses the latest-wins datagram path; keys/buttons/wheel use ordered input.
Enter carries a monotonic epoch; forwarding waits for EnterAck. The receiver
admits only after policy and initial injection succeed; stale epochs, old tokens
and unpaired input are discarded. Leave closes admission. Windows/macOS HID
shortcut translation is applied at the receiver. Return-home/TakeOver hotkeys are
checked locally even with the network down. Link loss, capture loss, denied
injection permission, queue overflow, unpair and shutdown restore home and release
holds. Local restoration precedes bounded remote teardown. Reliable sends have a
100 ms deadline; stalled delivery closes the session.

IPC notifications are reserved for actionable local failures: startup/keystore/
port, a locally attempted bad or expired pairing code or lockout, transfer consent,
local size/storage limits, OS clipboard failures and missing input permissions.
Never notify for a rejected peer message, unsolicited pairing failure, peer
disconnect, stale token, cancelled operation, transport timeout or backpressure.
Those conditions are debug diagnostics (and request responses/transfer state when
applicable). A peer cannot supply notification text or turn protocol rejection
into a toast. A failure of a local resource remains actionable even when discovered
while processing a peer event.

Core drains the bounded manager queue before dispatching link messages. If Paired
has not arrived or was lost to broadcast lag, it applies the authoritative native
paired snapshot first. The native pin/paired-record lock makes this snapshot an
admission barrier. Authenticated pinned sessions are never closed for missing Core
state; unknown unpinned senders are closed with a 100 ms deadline and silently
dropped. There is no early-message buffer: zero retained messages/bytes, no expiry
queue, and no per-sender allocation in Core. Existing transport queues and decoder
limits remain in force; each manager drain processes at most 128 events.

Transfer progress is bounded to ten events/second and
completed history to 64 entries.

## Clipboard and prefetch

Every clipboard read/write uses read_snapshot/publish_snapshot. Text/HTML/RTF/PNG
bundles up to 8 MiB travel eagerly; files and larger bundles use dedicated
NativeLink transfer streams with FileEngine. The receiver inspects the full
manifest before comparing current max_auto_mb policy. Above the threshold it
emits awaiting_confirm; transfer.confirm approves the exact manifest.
transfer.cancel, unpair, supersession and policy changes revoke publication
admission and cancel work. A completion cannot publish under stale admission.

Publication waits for full chunk/file verification and every announced
representation. File snapshots contain verified local paths, exposed by native
CF_HDROP/file URLs. No delayed-render call is used. Completed staging leases remain
held while the clipboard refers to them; shutdown clears only Glide's marked file
snapshot before releasing leases. Reconnect reannounces the current clip and uses
FileEngine's resumable staging. Manifest-probe/full-fetch handoffs use the bounded
sender queue, and old-session transport failures retain the same transfer identity
for retry. Stale staging purge runs outside the input path.

Global/format/per-peer policy and sensitive-content flags gate reads, announcements,
transfers and publication. Origin/clip_id/timestamp LWW plus source markers prevent
loops. Local changes coalesce to at most four jobs/second. Clipboard wire IDs are
at most 63 bytes so peer-qualified IPC transfer IDs fit the 128-byte limit. Files/manifests,
staging quotas, free-space reserve and consent digests retain FileEngine's bounds.
Byte representations total at most 64 MiB; files retain the 64 GiB transfer limit.
Manifest consent waits at most 120 seconds.

Bulk defaults use 1 MiB chunks, two lanes and two concurrent engine jobs. Hashing,
compression and filesystem work use the bounded blocking pool: at most four
workers and below logical core count where possible. Async workers are bounded
to two through four. No rate cap is imposed by default. Windows sockets reserve a bounded 2 MiB UDP receive buffer to absorb bulk bursts; the separate outgoing mouse queue stays latest-wins.

## Verify

```text
cargo fmt --check
cargo clippy --workspace --all-targets --offline --locked -- -D warnings
cargo test --workspace --offline --locked
cargo test --workspace --offline --locked -- --test-threads=1
cargo check -p glide-platform -p glide-proto -p glide-platform-mac --target aarch64-apple-darwin --offline --locked
cargo check -p glide-platform -p glide-proto -p glide-platform-mac --target x86_64-apple-darwin --offline --locked
cargo test -p glide-daemon --lib native_core_capture_to_inject_loopback_p50_p99 --offline --locked -- --nocapture
cargo test -p glide-xfer --features test-support --release --test native_transfer bulk_quic_transfer_keeps_input_p99_under_three_ms --offline --locked -- --ignored --exact --nocapture
```

The loaded test retains complete 512 MiB file/hash verification, progress during
sampling, per-event delivery/deadlines and exact sequence assertions. Acceptance
is an absolute **reliable-input and mouse-datagram p99 below 3 ms** on loopback,
replacing the misleading 2x-idle ratio. Core capture-to-inject timing includes
orchestration and real QUIC, with mock platform calls rather than hardware.

The process-level test starts two real debug glided binaries with separate
namespaces/ports and mock OS platforms; it drives JSONL pairing, both SAS
confirmations, clipboard consent/publication, unpair and shutdown. It uses the
real OS keystore and is ignored with reason "needs the OS keystore, not available
in a sandbox". Run outside the sandbox:

```powershell
$env:CARGO_TARGET_DIR = '$env:TEMP/glide-integration-target'
cargo test -p glide-daemon --test process_native --offline --locked -- --ignored --exact two_native_daemons_pair_clipboard_and_unpair --nocapture
```

Idle measurement: release glided, ready then one-second warmup, ten-second
TotalProcessorTime delta plus WorkingSet64/PrivateMemorySize64. Run
[tools/measure-idle.ps1](tools/measure-idle.ps1) with -Mode mock-platform; if real
keystore startup is unavailable, -Mode mock-backends measures only that fixture.
The script changes no host policy and stops only its own process. This host blocks
.ps1 files; the fresh sample used permitted inline process-counter commands with
the same method. Native keystore startup emitted permission_denied without ready.
The release --mock-backends sample recorded 0 ms CPU over 10.003 s (below counter
resolution, not a claim of zero work), 9.71 MiB working set and 2.10 MiB private bytes
on a 32-logical-processor host. Native-network idle resources remain unmeasured.

Three consecutive release 512 MiB loopback runs passed with loaded reliable p99
0.9274-1.2516 ms, mouse p99 0.9479-1.1432 ms, and 172.30-220.69 MiB/s throughput.
Original 64 KiB Windows socket buffers lost sampled datagrams under load; the
2 MiB buffer passed all delivery checks. Kernel drops were not directly traced.

Final command results and fresh measurements are recorded in
[INTEGRATION_WORKLOG.md](INTEGRATION_WORKLOG.md). Apple checks are typechecks on
Windows, not Mac linking/runtime. Real LAN, Mac/Keychain, hardware input,
Windows UAC/secure desktops, Mac Secure Input, native clipboard rendering and
native hardware hotplug remain separate acceptance work.

## Always-on control

Run `glided --data-dir <dir> --headless [--ui <absolute app executable>]`.
Headless never reads stdin and writes no protocol frames to stdout. Without
`--headless`, the original stdio protocol, development fixtures and EOF behavior
remain. All real engines take the per-directory lock; stdio owners publish an
informational `endpoint: "stdio"` which cannot accept clients. A second headless
instance reports "Glide is already running" with exit 75 after checking a live
same-user glided PID/image. Startup waits briefly if another engine is still
publishing its metadata; an unidentifiable lock owner fails closed.

The persistent random directory name lives in `control.id`. Windows endpoint:
`\\.\pipe\glide-ctl-<16 lowercase hex digits>`. Unix/macOS endpoint:
`<data-dir>/ipc/ctl.sock`. Never use a TCP listener for local control.

Owner-only `ipc.json` is written atomically and removed on clean exit:

```json
{"endpoint":"<pipe or socket>","token":"<64 hex characters>","pid":1234,"version":"0.1.0","protocol":1,"started_at_ms":0}
```

Read this file without printing its token. Connect and send a newline-terminated
`{"auth":"<token>"}` within two seconds. UI clients may add `"client":"ui"`;
Windows also recognizes plain authentication from the verified client process
whose image matches the configured `--ui` executable, as used by the existing
packaged Electron adapter. Other unlabelled clients and explicit
`"client":"tool"` clients are tools. This is only a notification hint,
not a privilege difference. The engine returns `{"auth":"ok"}`, then `ready`, a
full `state` snapshot, and exactly the existing JSONL request/response/event
protocol. `get_state` and `ready` retain the engine version. Events broadcast to
all clients; responses (including deferred pairing responses) return only to
the requester with its original ID. At most four authenticated clients are
admitted; a fifth is closed without an auth acknowledgement. Up to 16 pending
handshakes have independent two-second first-line deadlines and do not consume
those four slots. A correct login behind one batch of silent clients can proceed
when a pending slot expires, within about two seconds plus OS scheduling. Retry
Windows pipe-busy opens; an arbitrary continuous OS backlog has no fairness
promise. Authentication is limited to 1024 bytes; protocol frames retain the
1 MiB limit. Each client has a 16-frame, 2 MiB outbound budget and a 500 ms write
deadline; a slow client is dropped without blocking engine input or other clients.
Per-client pending requests and the shared engine bridge are also bounded.
A correct token is accepted regardless of earlier failures, using the existing
fixed-width constant-time comparison. Wrong tokens and malformed first lines
close silently after the same fixed 250 ms delay; silent clients close at the
two-second deadline. Failure counts are debug-only; tokens are never logged.
There is no authentication lockout or global back-off.

Windows creates byte-stream pipes with an explicit protected DACL containing
only the current user's SID. `PIPE_REJECT_REMOTE_CLIENTS` and first-instance
creation are mandatory; a listener handle stays alive across accepts. Before
authentication, `GetNamedPipeClientProcessId`, `OpenProcessToken`, and `EqualSid`
verify the connecting user. `ipc.json`, temporary files, the directory ID and
lock file get explicit owner-only DACLs too. A per-session `Local\\GlideEngine`
mutex includes the user SID and directory key; an exclusive lock file also
excludes other sessions. Unix verifies owned regular files/directories, uses
0700 directories, 0600 metadata/socket files, `flock`, and same-effective-UID
peer credentials (`getpeereid` on macOS, `SO_PEERCRED` on Linux). Symlink/wrong-owner
socket paths fail closed. PID liveness and image name are checked before stale
metadata is removed under the OS lock. A crash rotates the authentication token.

The endpoint trusts authenticated processes belonging to the same OS user.
Such processes can read the token and exercise every existing engine action;
it is not a sandbox for malicious same-user software. Other users and remote
clients are denied by OS access control and peer verification independently of
the token. Root/administrator privilege escalation and an already-compromised
user session are outside this boundary. The PAKE, both human SAS confirmations,
keystore protection and pinned TLS are unchanged.

`--ui` must be an absolute existing file without control characters and is
stored as `ui_path` in config.json. It is used only by Open Glide; absent UI paths
disable that item. Engine-owned `settings.startup.launch_at_login` stays off by
default. Windows repairs/removes the HKCU Run value `Glide`; the enabled command
quotes the engine, data-directory and UI paths and includes `--headless`.
macOS repairs the existing LaunchAgent with those same arguments. Removing
Mac autostart disables future launches without booting out the running engine;
arguments for an already-loaded job take effect at the next login. Explicit
mock OS backends do not mutate the real startup entry. Registry tests create
and clean a unique `Glide-Test-*` value, never the user's real `Glide` value.
That integration test is ignored by default because sandboxed runners can deny
HKCU Run writes; run it explicitly on a host with that permission:
`cargo test -p glide-platform-win --offline --locked custom_run_value_is_written_repaired_and_removed -- --ignored`.

Windows tray uses embedded 16/32 px ICO frames derived from `app/assets/tray.png`,
a message-only icon owner, a hidden broadcast window for Explorer restart/DPI/
session shutdown, and blocking GetMessage on its own thread. Tooltip is
`Glide - N connected`, updated at most once per second. Menu: Open Glide, checked
Share keyboard and mouse, Return to this computer, disabled status, Open logs folder, separator,
Quit Glide. Left click opens the configured executable. Error/warning balloons
and pairing requests without a UI client are limited to one every ten seconds.
Tray failure is nonfatal; the tray has no idle polling. Drop has a 200 ms deadline.

Both headless and stdio engines write info/warn/error diagnostics to
`<data-dir>/logs/glided.log`, rotating before a write would exceed 1,000,000
bytes, keeping `glided.1.log` through `glided.3.log`. Engine log files older than
seven days are removed at startup and every 24 hours, including an expired
current file. The Windows tray's **Open logs folder** opens this directory.
A 64-record background queue drops new log records on saturation; filesystem
failures never fail engine startup or block input handling. Shutdown drains and
flushes accepted records with a 200 ms deadline; a hung disk cannot delay safety
cleanup. Stdio human logs still go to stderr with `--log-level`; the file layer
always receives info/warn/error. Only audited static messages go to disk. Event
fields, unreviewed messages and debug events are redacted/omitted, including
control tokens, pairing codes, SAS words, clipboard data, keys, filenames and
key material. New static diagnostic messages should be reviewed and added to
`logging.rs`'s allowlist; unknown events retain their severity with a redaction
marker.

**Mac status:** `tray_mac` is a compiling stub returning "not available yet".
Electron still owns the Mac tray. A real NSStatusItem needs main-thread
NSApplication run-loop ownership, menu/action dispatch, icon/menu lifetime,
DPI/theme assets and notifications; this round does not change Mac threading.
The existing Windows Electron adapter already connects/authenticates to this
endpoint and disconnects when its window closes. `app/` was left untouched;
packaged UI coexistence remains a live verification item on an interactive host.

`app.shutdown` delivers its response before disconnecting its client. Core
restores Local mode and releases held input before awaiting anything, cancels
transfer/publication/pairing work, limits the initial remote leave to 100 ms,
and limits asynchronous cleanup to one second.
Drop guards preserve input safety and protected clipboard lease behavior on
errors/timeouts. Signals, Windows console-close/logoff/shutdown and tray Quit
use cleanup. OS hard termination cannot run Rust cleanup; existing remote
heartbeat/input guards remain the crash escape path.

Verification commands, with an external CARGO_TARGET_DIR:

```text
cargo fmt --check
cargo clippy --workspace --all-targets --offline --locked -- -D warnings
cargo test --workspace --offline --locked
cargo test -p glide-daemon --test headless --offline --locked
cargo test -p glide-daemon --lib --offline --locked native_tray_readds_on_explorer_message_and_handles_session_shutdown -- --ignored
cargo run -p glide-daemon --example tray_probe --offline --locked
cargo test -p glide-daemon --test process_native --offline --locked -- --ignored --exact two_headless_native_daemons_auth_pair_clipboard_and_shutdown --nocapture
```

The ignored headless process test uses an overlapped Tokio named-pipe client
in a dedicated runtime (Unix async socket on Mac), with bounded writes and
named response/state/exit deadlines. A regular pipe-only regression verifies
that a parked reader cannot block a write, without requiring a keystore.
The ignored headless process test needs the real OS keystore and covers two
engines, metadata/auth/get_state, PAKE + both SAS confirmations, clipboard
consent/publication, unpair and shutdown. The regular native test uses the
isolated test keystore to interrupt an active 512 MiB transfer with held input,
then checks cancellation, key release, Local mode and shutdown under two seconds.
Read-back Windows DACL tests assert one current-user ACE for pipes and atomically
replaced metadata; flag/squatting tests verify local-only first-instance creation.
Regular headless process tests exercise four/five authenticated clients, 100 wrong
logins followed by an immediate valid login, 16 stalled handshakes, failures/timeouts,
state/event/ID routing, slow-client removal, EOF survival, stale metadata,
crashed lock recovery, token rotation, UI validation and shutdown deadlines.

For a 30-second warmup with the real Windows tray, run
`core/tools/measure-headless.ps1 -Executable <release glided.exe> -Mode mock-platform`
from the project root. It samples working set/private bytes and processor-time
delta for another ten seconds, then uses authenticated shutdown. `mock-platform`
retains real secure networking but needs the OS keystore; `mock-backends` measures
only the explicit fixture. A failed keystore or tray startup produces no accepted
measurement. Native mode uses normal native engine behavior, including startup
reconciliation. Counter resolution can report zero CPU time without proving zero
work. Fresh validation and measurements are recorded in [HEADLESS_REPORT.md](HEADLESS_REPORT.md).
