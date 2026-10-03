# Glide Rust engine

[`../docs/SPEC.md`](../docs/SPEC.md) defines the contract.
The CONTRACTS-v2 wire revision is **v3**, with ALPN `glide/3` and `glide/pair/3`.
The daemon now constructs native Windows/macOS platforms and NativePeerManager,
with an explicit --mock-platform mode using real secure networking. Eager clipboard
and input orchestration are wired. The approved shared APIs from
[glide-daemon-integration.md](docs/proposals/glide-daemon-integration.md) are available;
the daemon consumes native file streams, atomic clipboard snapshots and live metadata.
Keystore failure emits an IPC error/notification and fails closed. Mock identity
and pairing remain explicit development fixtures, never authentication.

The engine can run independently as `glided --headless --data-dir <dir>` with a
native Windows tray. Add `--ui <absolute app executable>` to open the app from
the tray. Local clients discover the protected `ipc.json`, authenticate its fresh
token, and then use the existing JSONL protocol. Engine-owned autostart is off
by default. See [always-on mode](../SPEC.md#9-always-on-mode) and the
[daemon README](crates/glide-daemon/README.md#always-on-control) for the security
boundary, test commands and Mac limitation. The existing Windows Electron
adapter attaches to this endpoint; Mac keeps its existing stdio/tray lifecycle.

## Crate boundaries

| Crate | Current responsibility | Integration boundary |
| --- | --- | --- |
| `glide-platform` | Copyable input/shared types, documented native contracts and scriptable mocks | Stable interface consumed by both native crates |
| `glide-proto` | Typed JSONL IPC, strict canonical postcard codecs, borrowed chunks and paged-manifest validation | Disconnect on every binary decode error; validate the complete manifest before consent/staging |
| `glide-platform-win` | Raw Input, bounded hooks, SendInput, native clipboard, monitors and autostart | Factory wired; Windows runtime acceptance remains |
| `glide-platform-mac` | CGEventTap, native injection, NSPasteboard, monitors and LaunchAgent | Factory wired; native Mac build/link/runtime acceptance remains |
| `glide-net` | Native pinned TLS 1.3 QUIC, exporter-bound PAKE, dual SAS confirmation, discovery; explicit mocks | Consume link and manager events; retain the manager for the link lifetime |
| `glide-xfer` | Verified resumable staging, compression, cancellation, leases and purge; concurrent paging/chunk work | Dedicated native streams and clipboard publication; existing send/receive API stays source compatible |
| `glide-daemon` | Native factories, bounded stdio/headless loop, persistence, edge/input safety, eager clipboard and staging purge | Acknowledged input epochs, verified native prefetch, atomic clipboard and live metadata orchestration |

`Link` defines `connect`, `accept`, `send_reliable`, `send_datagram`, `recv`, `recv_event`,
`peer_token`, `admit_input_epoch`, `mouse_ready`, and `close`, and implements `MouseReceiver`.
Reliable input, control, clipboard and files must use separate priority streams in the QUIC
implementation. `send_datagram` is a bounded synchronous latest-wins enqueue, with no mouse
allocation. Drain `MouseReceiver::try_recv_move()` after `mouse_ready()` notifications:
`ReceivedMove` carries a copyable `PeerToken { slot, generation }` and `Move`, without a
peer string or per-move future. `recv_event()` handles everything else; retain its pending
future across mouse wakeups. `recv()` remains the compatibility event API. Never invoke
daemon logic from a native hook.

`PeerManager` defines `pair_host`, `pair_join`, `confirm_pairing`, `cancel_pair_host`, `discover`,
`add_manual`, `unpair`, `events`. `pair_join` produces an unpinned `PairingSession` and verification
prompt; `confirm_pairing` yields a peer only after both users accept. `PeerManagerEvent` covers
pairing, discovery, peer snapshots, stats, heartbeats, takeover and loss of injection capability.
Use `Core::set_peer_interfaces` to install initialized network implementations;
`Core::receive_link`, `Core::receive_move` and `Core::capture_input` are orchestration entry points.
`Core::update_transfer` supplies validated transfer snapshots and throttled progress/confirmation
events. Production private keys must never enter `config.json`.

Platform contracts document ownership, callback threading, logical units, permission failures,
injection flags and queue-overflow escape. Input sinks and payloads are bounded and copyable.
`InputSink::mark_overflow()` reports native queue loss even when the shared sink has room.
Consume `InputBackend::capture_status_changes()` once and retain its bounded receiver;
`CaptureStatus::{SecureInput, PermissionLost, TapDisabled, UnsupportedInput, QueueOverflow}`
drive return-home/release, with `SecureInput(false)` reporting recovery only.
`secure_input_enabled() -> Option<bool>` supplies authoritative state where available.
Native implementations must immediately return capture to Local when enqueue fails. Windows
cannot normally inject into elevated/UAC/secure-desktop windows; macOS requires Accessibility
and Input Monitoring, and Secure Input blocks capture. Linux/Wayland is outside v1.

Permission setup is an explicit `permissions.request` IPC action (`params: {}`, result `{}`).
`InputBackend::request_permissions()` returns the immediate status and never waits for the user.
On macOS it requests Accessibility, Input Monitoring and event posting once per process;
the OS attributes the bundled engine's requests to the responsible Electron **Glide** app.
Missing grants leave the daemon available for setup. Permission workers run outside input/escape
handling: denied/unknown grants refresh every 2 seconds, granted capabilities every 10 seconds
for revocation; all `n/a` needs no polling. Changed grants publish state. Capture retries after
a grant without resuming remote control, with one recovery notification. If capture still fails,
`permissions.restart_required` (default false) tells the UI a relaunch may be needed.

Real Mac acceptance (unverified here; see the platform's [checklist](crates/glide-platform-mac/MANUAL_TEST.md)):

- [ ] Fresh Glide.app install: request permissions, confirm dialogs/settings list **Glide** and IPC stays responsive.
- [ ] Deny then grant Accessibility/Input Monitoring: state updates and capture starts without an engine restart where supported.
- [ ] Grant Input Monitoring while running: if capture still fails, verify `restart_required`, then relaunch Glide and verify capture.
- [ ] Revoke each grant while forwarding: return home, release holds, publish denial, and recover capture after regrant while keeping control local.

`glide-net/native-transport` and `glide-xfer/file-engine` are enabled by default. The only
approved dependency edge added here is optional, already-locked `quinn-proto = 0.11.19`;
its delegating crypto session selects exactly one ALPN before either certificate verifier.
Normal pinned handshakes and reconnects continue during pairing on the same port.
Clipboard uses direct native APIs. **v1 prefetches clipboard data and publishes real local
files only after verification; the daemon does not use delayed rendering.** Its trait methods
remain available for compatibility.

Binary decoders reject trailing bytes and noncanonical encodings centrally in `glide-proto`.
`encode_input_into` and `encode_control_into` reuse reliable-lane storage. `FileChunkView`,
`encode_file_chunk_into`, `decode_file_chunk_view`, `encode_file_chunk_prefix_into`,
`encode_file_chunk_trailer_into` and incremental `decode_file_chunk_header` enable bounded,
allocation-free chunk handling without changing the owned chunk's postcard bytes.
`FileManifest { chunk_size, page, final_page, .. }` uses ordered pages and contiguous IDs.
`ManifestValidator::push/finish` checks complete geometry/totals; `Consent::approve_pages`
binds the BLAKE3 digest of all canonical pages in order. The current file engine accepts
only page zero with `final_page = true`, using its configured chunk size.

`PairingHost.code` is `Zeroizing<String>`; typed IPC code copies use `PairingCode`, and daemon
JSON buffers wipe on drop. This covers owned copies, not app display memory or opaque
dependency internals. The native manager rejects unfinished transactions. Core
automatically calls, on a blocking worker before constructing that manager,
`NativePeerManager::repair_unfinished_pairing(&Path) -> Result<PairingRepair, NativeError>`
discards unfinished candidates/deny transactions and reports removed files/IDs; it never
auto-trusts. Core emits a notification if records were discarded. Storage requires
exclusive ownership. Damaged/orphan journals conservatively discard all persisted pins.

## Run and verify

Use the supplied external `CARGO_TARGET_DIR`; do not create a target directory in this repo.
Windows requires MSVC libraries. `.cargo/config.toml` selects the Rust-bundled MSVC-compatible
LLVM linker because the supplied scratch target path exceeds `link.exe`'s path limit.

From `core/`, PowerShell:

```powershell
'{"id":1,"method":"get_state","params":{}}' | cargo run --offline --locked -p glide-daemon --bin glided -- --mock-backends --data-dir .\mock-data
```

For an interactive UI session, keep stdin open. Stdout contains only responses and events;
tracing and CLI errors go to stderr. `ready` precedes the initial full state. Default EOF and
`app.shutdown` cancel pairing, restore local input, and release injected keys/buttons.
`--headless` continues after stdin EOF; Ctrl+C and Unix SIGTERM stop it safely.
`--mock-platform` uses real networking with mock OS input/clipboard/displays;
`--no-discovery` persists mDNS off. Names/monitors update live and replicate to authenticated peers; port changes call the manager and require a restart because native rebinding remains unsupported.
`--port N` overrides and persists the configured port; mock mode does not listen on it.

The mock has a reachable/discovered `Mock Mac` at `127.0.0.1:24801`, device ID consisting of
64 `e` characters, code `123456` (120 seconds from daemon start). These are fixtures, with no
socket activity. A mock join emits a three-word test phrase, waits for `pairing.confirm`, and
only then answers the original join request. The fixture explicitly preconfirms the simulated
remote screen; script both confirmations independently with `InMemoryPeerManager` in tests.
No mock phrase is cryptographically authenticated.

```json
{"id":2,"method":"pairing.join","params":{"address":"127.0.0.1:24801","code":"123456"}}
{"id":3,"method":"pairing.confirm","params":{"accepted":true}}
{"id":4,"method":"peer.configure","params":{"device_id":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","clipboard_enabled":false}}
```

```text
cargo build --workspace --offline --locked
cargo fmt --check
cargo clippy --workspace --all-targets --offline --locked -- -D warnings
cargo test --workspace --offline --locked
cargo check -p glide-platform -p glide-proto -p glide-platform-mac --target aarch64-apple-darwin --offline --locked
cargo check -p glide-platform -p glide-proto -p glide-platform-mac --target x86_64-apple-darwin --offline --locked
cargo test -p glide-net --release --offline --locked native::tests::loopback_latency_microbenchmark -- --ignored --exact --nocapture
```

`glide-bench` measures the pure geometry/mouse codec p99 and verifies raw TCP loopback bytes.
It is a foundation baseline, not proof of native capture/injection, QUIC file throughput,
process RSS/CPU, or the full SPEC performance budget. Hardware/native acceptance belongs to
later waves. CI checks Windows and macOS and packages release `glided`.

Current contract APIs, checks and remaining limitations are recorded in
[CONTRACTS_V2_VALIDATION.md](CONTRACTS_V2_VALIDATION.md). This pass fixes the net
fixture/session races and adds namespace isolation, native transfer streams,
metadata replication, atomic clipboard publication and acknowledged input epochs.
Real keystore access remains denied in this sandbox; production fails closed.
The transfer acceptance criterion is absolute loaded reliable-input and mouse-datagram p99 below 3 ms on loopback; all delivery, progress and hash assertions remain. Cross-checking Apple code is not native Mac execution or linking.

The two-process pairing recipe requires working OS keystore access. Each data
directory now selects an independent persistent identity. From core, in two terminals, keep stdin open:

```powershell
$env:CARGO_TARGET_DIR = '$env:TEMP\glide-contract-target'
cargo run -p glide-daemon --bin glided --offline --locked -- --mock-platform --no-discovery --port 24911 --data-dir $env:TEMP/glide-a
```

```powershell
$env:CARGO_TARGET_DIR = '$env:TEMP\glide-contract-target'
cargo run -p glide-daemon --bin glided --offline --locked -- --mock-platform --no-discovery --port 24912 --data-dir $env:TEMP/glide-b
```

Host terminal A, then join terminal B using the returned six-digit code:

```json
{"id":1,"method":"pairing.start_host","params":{}}
```

```json
{"id":2,"method":"pairing.join","params":{"address":"127.0.0.1:24911","code":"HOST_CODE"}}
```

After both pairing.verify phrases match, send this line to **each** terminal:

```json
{"id":3,"method":"pairing.confirm","params":{"accepted":true}}
{"id":4,"method":"get_state","params":{}}
```

The original join response resolves after both confirmations. Use accepted:false
to decline, pairing.cancel_host to cancel, and app.shutdown to stop. HOST_CODE is
a placeholder, not a working pairing code. The --mock-backends fixture above
remains available for UI development without OS-keystore or socket access.

## Limits and resolved ambiguities

* JSONL: 1 MiB including newline, checked before buffer growth. Postcard has lane-specific caps
  and bounded sequence deserializers; advertised collection lengths cannot trigger unbounded
  reservation. `glide_proto::wire` lists all caps (mouse datagrams use a 128-byte fixed buffer).
  `encode_move_into` uses a caller-owned buffer for the steady mouse path.
* Core/IPC caps: 32 paired/discovered devices, 64 monitors per peer, 64 UI transfers and
  **1,048,576 aggregate transfer items**, deliberately raised from 4096 for paged manifests.
  A manifest page still holds at most 4096 entries and 16 MiB encoded metadata; aggregate
  bytes are bounded to 64 GiB and chunk size to 1..=4 MiB. This is not an allocation allowance
  for one frame; the legacy single-page send/receive path caps entries at 4096,
  with additive paging APIs maintained by the xfer owner.
  The native network/file engines must additionally enforce concurrent stream, byte,
  staging, decompression and slow-peer limits before exposing data.
* Logical coordinates are fractional. Layout positions anchor the device monitor bounding-box
  top-left. Windows divides all OS physical virtual-desktop positions and extents by the
  primary scale; macOS uses uniform CG global points. Per-monitor scale is informational.
  Actual monitor overlap is rejected (bounding boxes may overlap); disconnected regions cannot be crossed through
  empty space. Corners require a real shared edge. A second edge attempt within 500 ms can
  bypass dwell; leaving the edge cancels dwell.
* The shortcut wording combines a Ctrl/Cmd swap with preserving physical GUI-key position.
  Preserve the latter explicitly: cross-OS `auto` maps left/right Ctrl to corresponding GUI
  usages, `always` applies that mapping even on the same OS, and `never` preserves Ctrl.
  Physical GUI and Alt/Option retain their HID usages. Colliding Ctrl/GUI source holds are
  reference-counted so one release cannot release the other.
* Unspecified defaults: name `Glide`, port 24800, discovery/sharing/clipboard formats enabled,
  edge dwell 350 ms, corner zone 5 logical px, double-tap disabled, startup options disabled.
  Required defaults retain the SPEC hotkeys, auto translation, 2048 MB approval threshold,
  and sensitive-clipboard exclusion.
* `TakeOver` includes the target cursor position; clipboard announcement has a timestamp for
  LWW, and file messages have explicit manifest/chunk/hash/resume metadata. No implicit execution.
* Configuration writes flush a same-directory temporary file and atomically replace the prior
  file; corrupt/oversized existing data is preserved and startup fails. Unix also syncs the
  containing directory. State snapshots are coalesced at 10/s, peer stats at 1/s, transfer
  progress at 10/s. Pairing verification expires after 60 seconds with no peer persisted.

## CONTRACTS-v2 handoff

`NativePeerManager::update_local_metadata(name, monitors)` updates Hello, mDNS and
connected peers. `update_listen_port(port)` permits the existing bound port and
rejects rebinding with a restart error. `PeerManagerEvent::DiscoveryRemoved`
removes an unpaired discovery entry without revoking authenticated trust.

`NativeLink::open_transfer(peer_id, transfer_id, lanes)` and `accept_transfer()`
return `TransferStreams { peer_id, peer_token, transfer_id, control, chunks, handle }`.
Use `TransferIo` directly or its cancellation-aware `split()` halves; pass streams
to `FileEngine::send_native` / `receive_native` (feature `native-transfer`). The
preamble authenticates the session and binds every lane to the transfer; verify
clipboard policy, announcement and consent before opening/accepting jobs. Retain
`Received` while publishing its verified local file paths.

`ClipboardBackend::read_snapshot()` returns content, sensitivity, self marker and
native change token together. `publish_snapshot(contents, marker, expected_change_token,
admission)` prepares the entire bundle and checks token/admission before one native
transaction. Handle `ClipboardPublish::{Published, ReplacedLocalChange, Revoked,
PartialFailure}` explicitly; only Published is success. Use a generation-backed
`ClipboardAdmission` and revoke it on cancellation, supersession or session loss.
Legacy delayed-render methods remain compatibility APIs only.

`Enter`, `Leave` and every reliable input carry a nonzero `epoch`. Native send of
Enter waits for `EnterAck`; Core admits via `Link::admit_input_epoch` only after
policy and initial injection succeed. Old epochs, unadmitted input and retired
`LinkEvent::Reliable.peer_token` generations are discarded. Leave revokes the epoch.

`glide-net/test-support` exposes `TestKeyStore` and
`NativePeerManager::with_test_keystore` for downstream tests; it retains real
PAKE, both SAS confirmations, persisted trust and pinned TLS. It is off by default;
release/optimized `glided` rejects feature unification enabling it at compile time.
No production keystore fallback or trust bypass is introduced.

Second-pass daemon operation, scheduling, real multi-Core fixtures, ignored two-process OS-keystore command and measurement methods are documented in [glide-daemon/README.md](crates/glide-daemon/README.md). Production never enables the isolated test keystore.
