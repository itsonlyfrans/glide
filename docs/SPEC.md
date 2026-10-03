# Glide — architecture & contracts

Software KVM: one keyboard/mouse drives several Windows/macOS machines, with seamless
cursor crossing, shared clipboard (text, rich text, images, files, video) and secure transport.

Two processes per machine:

| Part | Tech |
|---|---|
| `core/` — engine `glided` | Rust, tokio, quinn (QUIC), rustls |
| `desktop/` — the Glide window | Tauri 2 (the OS web view), vanilla HTML/CSS/JS screens in `desktop/ui/` |

The window never touches the network or OS input. On Windows the engine runs on its own (always on, with its own
tray) and the window attaches to it over a same-user named pipe with a token; on macOS the window starts the engine
and talks to it over **stdio JSON Lines**. Both use the same protocol (§7).

## 1. Topology: symmetric peers, one "brain" at a time

Every device runs the same daemon and holds the same **layout**. Full mesh of QUIC connections
between paired, online devices. The device where physical input is happening is the **brain**
("active source"). Brain responsibilities:

* Tracks one **virtual cursor** in the global virtual desktop (union of all monitors of all devices,
  placed by layout, in *logical pixels*: Windows physical px / primary monitor DPI scale, macOS CG global points).
* Clamps/slides the virtual cursor inside the union of monitors (non-rectangular unions handled).
* When the virtual cursor leaves the local monitors into another device's monitor → **forward mode**:
  swallow local input, send to that device. Targets are dumb injectors: they receive absolute
  positions in *their own* logical coordinates (brain converts) plus key/button/wheel events.
* **Local-input preemption**: a target that sees *non-injected* physical input while it is receiving
  forwarded input sends `TakeOver`; brain stops forwarding, target becomes brain with cursor where it is.
  Injected events must be tagged/filtered (Win: `LLMHF_INJECTED`/`LLKHF_INJECTED` + dwExtraInfo magic;
  mac: `kCGEventSourceUserData` magic) so injection never triggers preemption or loops.
* **Escape hatches (mandatory, never lock the user out)**: hotkey `return_home` (default
  `Ctrl+Alt+Shift+Home`) forces control back to the local machine; auto-return if the link drops,
  heartbeat times out (3 missed @ 500 ms), peer vanishes, or the daemon exits/crashes (OS hooks die with it).
  Forward mode also ends if the target reports it cannot inject (permissions revoked).
* Edge settings: `edge_delay_ms` (dwell before switching), `corner_dead_zone_px`, optional double-tap-edge.
  Screen corners/edges that touch no neighbour never switch.

## 2. Inter-device wire protocol (`glide-proto`, postcard + serde, versioned)

ALPN `glide/1` (mutual TLS 1.3 over QUIC). Streams/datagrams:

* **Control stream** (bidi, opened by the dialer, id 0): `Hello{proto_version, device_id, name, os, monitors[], app_version}`,
  `LayoutUpdate{version:(lamport,device_id), devices[]}` (last-writer-wins, rebroadcast on change),
  `Heartbeat{seq,ts}`/`HeartbeatAck` (also RTT), `Bye{reason}`, `Unpaired`, `TakeOver`, `Enter{pos, modifiers_down[]}`, `Leave`.
  Since 0.2.2: `Details{model, kind, builtin_monitor}` (what the computer is, for its picture on the Desk) and
  `Arrange{screens[]}` (arrange the receiver's own screens, chosen on another computer's Desk; applied exactly like a
  local `set_settings` arrangement). Both are sent **only** to peers whose `Hello.app_version` is 0.2.2 or newer,
  because older versions drop the connection on an unknown message.
* **Wake-on-LAN**: while connected, each side reads the other's hardware address from its own ARP table (no extra
  traffic) and stores it with the pairing. Pushing the cursor toward a sleeping computer's place on the desk, or
  `peer.wake`, broadcasts the standard magic packet (ports 9 and 7), at most once a minute per computer.
* **Input reliable stream** (uni, high priority): `Key{hid_usage, down}`, `Button{button, down}`,
  `Wheel{dx,dy}` (hi-res units, carry both vertical+horizontal, `precise` flag for trackpad), `ModifierSync`.
  Each carries `seq`. On `Enter`/`Leave`, brain sends modifier state so no key is ever stuck; on `Leave`/disconnect
  target **releases every key/button it injected** (stuck-key guard).
* **Mouse-move datagrams**: `Move{seq, x, y}` absolute in target logical coords, latest-wins, stale seq dropped.
  Coalesce to the display refresh/1 kHz max; never queue. On macOS the receiver paces moves that arrive in bursts
  (typical on Wi-Fi) over the following 240 Hz frames, never longer than 16 ms; steady arrivals post immediately and a
  click first snaps to the newest position.
* **Clipboard streams** (see §4) and **file transfer streams** (one uni stream per chunk group) — never share a
  stream with input so large transfers cannot add input latency. Input streams get QUIC priority > clipboard > files;
  congestion control: BBR or Cubic with pacing off for datagrams if the lib allows.
* Keys travel as **USB HID usage codes** (page 0x07) = physical key. Receiver maps to its scancode/virtual key.
  **Modifier translation** (`keyboard.swap_ctrl_cmd`: `auto|always|never`): when source OS ≠ target OS, `auto` maps
  the shortcut key to the target's habit: Ctrl on a Windows keyboard ⇒ Cmd on a Mac target (Ctrl+C ⇒ Cmd+C), and
  Cmd on a Mac keyboard ⇒ Ctrl on a Windows target (Cmd+C ⇒ Ctrl+C, never the Win key). Alt↔Option, Shift, the
  Win key (to a Mac) and Ctrl (to a Windows PC) are left as they are. The app switcher follows each system's habit:
  Alt+Tab on a Windows keyboard ⇒ Cmd+Tab on a Mac, and Cmd+Tab on a Mac keyboard ⇒ Alt+Tab on Windows (the held
  modifier is re-pointed when Tab goes down and released with it). Must be unit tested.
* Max message sizes enforced on decode (reject > limits before allocating); every decode failure drops the connection.

## 3. Security model (non-negotiable)

1. **Identity**: per-device self-signed cert (ECDSA P-256 or Ed25519, `rcgen`), generated on first run.
   `device_id` = lowercase hex SHA-256 of the cert's SubjectPublicKeyInfo. Private key stored in the OS keystore
   (Windows DPAPI-protected / macOS Keychain via `keyring`), fallback to a file encrypted with a keystore-held key;
   never plaintext on disk, never logged, wrapped in `zeroize`.
2. **Pairing** (ALPN `glide/pair/1`, only accepted while the host has pairing mode open and shows a code):
   6-digit one-time code, expires in 120 s, **3 wrong attempts burn the code**, global rate limit per source IP.
   Uses a **PAKE (SPAKE2 or CPace)** keyed by the code, bound to the TLS exporter (channel binding) so a MITM
   without the code cannot succeed and offline brute force is impossible. After PAKE success each side sends its
   cert fingerprint authenticated under the PAKE key. **Then a mandatory human check ("this is me")**: both devices derive a
   3-word phrase (SAS) from HKDF(PAKE key, both fingerprints, TLS exporter) over a 2048-word list and show it
   (`pairing.verify` event). **Pairing only completes and pins keys after the user confirms "they match" on BOTH screens**
   (`pairing.confirm`); either side saying no, or a 60 s timeout, aborts and pins nothing. Unpaired peers have no other way in.
   Setup must stay "stupid easy": discovery finds the device, user enters 6 digits, confirms 3 words match. No IPs, certs or ports.
3. **Normal sessions** (ALPN `glide/1`): custom rustls `ServerCertVerifier`/`ClientCertVerifier` accept **only pinned
   fingerprints**; anything else is rejected during the handshake, before any application data is parsed. TLS 1.3 only,
   no resumption/0-RTT (no replay surface). Every byte on the wire is encrypted + authenticated (QUIC).
4. **Discovery** (mDNS `_glide._udp`) advertises only `device_id`, name, os, port. Nothing secret, nothing trusted:
   spoofed advertisements are harmless because trust is the pinned cert, not the name/IP.
5. **Unpair/revoke**: removes the pin, closes connections immediately, tells the peer (`Unpaired`) best-effort; a peer
   unpaired from one side stays blocked from that side.
6. **Clipboard hygiene**: never sync content flagged sensitive/concealed (Windows: `ExcludeClipboardContentFromMonitorProcessing`,
   `CanIncludeInClipboardHistory=0`; macOS: `org.nspasteboard.ConcealedType`, `.TransientType`, `.AutoGeneratedType`)
   when `clipboard.exclude_sensitive` (default true). Clipboard/file sync can be disabled globally and per peer.
7. **Files received**: written only under a per-session staging dir inside the data dir; names sanitised (no path
   separators, no `..`, no reserved Windows names, no ADS `:`), symlinks never followed or created, size/count limits,
   blake3 verified before exposure, never auto-executed, staging purged on a schedule.
8. **Resource limits**: caps on concurrent connections, streams, message sizes, transfer sizes; slow-peer timeouts.
9. **Process hygiene**: no telemetry, no outbound traffic other than to paired peers (+ mDNS). Logs never contain
   key material, clipboard content, typed keys or file names above `debug` level.
10. **Window (historical: Electron; now Tauri)**: `contextIsolation:true`, `sandbox:true`, `nodeIntegration:false`, strict CSP, preload exposes a
    narrow allowlisted API, no remote content, navigation/new-window blocked, single instance lock.

## 4. Clipboard & files

* Watch the local clipboard event-driven (Win: `AddClipboardFormatListener`; mac: `changeCount` poll at 100–250 ms
  on a cheap timer, or a timer + NSPasteboard notification). Ignore changes the daemon itself made (sequence number / marker).
* On change → `ClipAnnounce{clip_id, origin, formats[{kind,mime,size}], files[{name,size,is_dir}]}` to all enabled peers.
  Loop prevention via `(origin, clip_id)` + self-write marker; last-writer-wins by timestamp.
* **Text / HTML / RTF / images (PNG; convert DIB/TIFF)**: if total ≤ eager threshold (default 8 MB) push eagerly on
  the clipboard stream, so the receiver's clipboard is updated within a few ms. Larger payloads use the same
  **prefetch** path as files (below): background transfer with progress, clipboard updated only when complete.
  **v1 decision: delayed rendering is NOT used.** Neither Win32 `WM_RENDERFORMAT` nor macOS `NSPasteboardItemDataProvider`
  can wait for the network without stalling the pasting app or failing the first paste (see `core/docs/proposals/`).
  The `set_delayed_render` platform hooks stay in the trait but the daemon never calls them.
* **Files & folders (incl. multi-GB video)**: v1 = *prefetch*: as soon as the copy happens, the source streams the files
  to the receiver's staging dir in the background (progress visible in UI); when complete the receiver puts a real
  file list on its clipboard (Win `CF_HDROP`/`Preferred DropEffect=copy`, mac file URLs `public.file-url`).
  If the user pastes before completion the clipboard simply is not updated yet (UI/notification shows progress).
  Engine: chunked (default 4 MiB), parallel streams, **zstd only for compressible types** (skip mp4/mov/jpg/png/zip/7z…),
  **blake3** per chunk + whole file, resumable after reconnect, cancel, directory trees preserved, rate limit option.
  v2 (documented, not required): virtual files (Win `CFSTR_FILEDESCRIPTORW`/`FILECONTENTS`, mac `NSFilePromiseProvider`) for pure on-demand streaming.
* Limit: `max_auto_mb` (default 2048) — above it the transfer asks for confirmation via a `notification` with action.

## 5. Platform layer (`glide-platform`)

Traits (in `glide-platform`, with `Mock*` implementations used by tests and `--mock-backends`):

* `InputBackend`: `start_capture(sink) / set_mode(Local | Swallow{lock_pos})`, `inject(Event)`, `release_all()`,
  `monitors() -> Vec<Monitor>` + change notification, `local_cursor_pos()`, `permissions() -> Permissions`.
  Raw high-resolution deltas while swallowing (Win: Raw Input + `ClipCursor`/recenter; mac: `CGEventTap` with
  `CGAssociateMouseAndMouseCursorPosition(false)` + `kCGMouseEventDeltaX/Y`). Tap/hook threads must never block:
  push into a lock-free/bounded channel; hook callbacks do O(1) work (Windows kills slow low-level hooks).
* `ClipboardBackend`: read/write all supported formats, change notification, files list, delayed render, sensitivity flags.
* `Platform`: autostart toggle (Win: `HKCU\...\Run`, mac: LaunchAgent), data dir, open permission settings pane.
* Known limits to document (not hide): Windows can't inject into elevated/UAC/secure-desktop windows unless the
  daemon runs elevated; macOS needs Accessibility + Input Monitoring grants and "Secure Input" (password fields)
  blocks key capture; Wayland/Linux out of scope for v1 (traits keep the door open).

## 6. Performance budget

* Added processing latency (capture→inject, excl. network) p99 < 2 ms; LAN end-to-end typically < 5 ms.
* Idle: < 0.5 % CPU, daemon RSS < 40 MB; no busy loops (event-driven everywhere; timers ≥ 100 ms when idle).
* Mouse path allocation-free in steady state. File transfer ≥ 80 % of link rate on gigabit (loopback bench ≥ 1 GB/s ok).
* Startup < 200 ms to IPC ready. Provide `cargo bench`/a loopback bench binary measuring input latency + file throughput.

## 7. UI ⇄ daemon IPC (JSON Lines on stdio)

Daemon CLI: `glided --data-dir <dir> [--mock-backends] [--port N] [--log-level L]`. stdin: requests, stdout: responses + events
(only protocol JSON on stdout), stderr: human logs. One JSON object per line, UTF-8. Unknown fields ignored. Camel-free: **snake_case** keys.

```
→ {"id":1,"method":"get_state","params":{}}
← {"id":1,"ok":true,"result":{...}}   |   {"id":1,"ok":false,"error":{"code":"bad_code","message":"..."}}
← {"event":"state","data":{...}}      (full snapshot; debounced ≤ 20/s; also sent once at startup, "ready" first)
```

Methods: `get_state`, `set_settings{patch}` (deep merge), `set_sharing{enabled}`, `set_layout{devices:[{device_id,x,y}]}`,
`pairing.start_host{}` → `{code,expires_at_ms}`, `pairing.cancel_host`, `pairing.join{address|device_id, code}` → `{device_id}` (resolves after BOTH sides confirmed),
`pairing.confirm{accepted: bool}` (answer to the `pairing.verify` prompt, valid on both host and joiner),
`peer.add_manual{address}` (host:port → adds to `discovered` if reachable), `peer.unpair{device_id}`,
`peer.configure{device_id, clipboard_enabled?}` (per-peer overrides, persisted), `return_home`,
`transfer.cancel{id}`, `transfer.confirm{id, accept}`, `permissions.open_settings{kind}`, `app.shutdown`, `permissions.request{}`.
Error codes: `bad_code, code_expired, locked_out, unreachable, not_paired, invalid_params, permission_denied, internal`.

Events: `ready{version}`, `state`, `pairing.incoming{name,os,address}`, `pairing.verify{phrase:[w1,w2,w3], peer:{name,os}, expires_at_ms}`, `pairing.result{ok,device_id?,error?}`,
`peer.stats{device_id,latency_ms,rx_bps,tx_bps}` (≤ 1 Hz), `transfer.progress{id,bytes_done,rate_bps}` (≤ 10 Hz),
`notification{level,title,body,action?}`, `active_changed{device_id,reason}`.

**State**
```
State {
  self: { device_id, name, os: "windows"|"macos", fingerprint, listen_port, version,
          monitors: [ {id,x,y,w,h,scale,primary} ] },            // this machine's own screens (live, updates on hotplug)
  sharing_enabled: bool,
  active_device_id: string,                       // current brain's target: who the cursor is on
  permissions: { accessibility: "granted"|"denied"|"unknown"|"n/a", input_monitoring: same, injection: same, restart_required: bool }, // default false; capture still failed after grants; relaunch may be needed
  peers: [ { device_id, name, os, fingerprint, online, connection: "offline"|"connecting"|"connected",
             address?, latency_ms?, monitors: [ {id,x,y,w,h,scale,primary} ], clipboard_enabled: bool } ],
  discovered: [ { device_id, name, os, address } ],     // nearby, not yet paired
  layout: { devices: [ { device_id, x, y } ] },          // origin of each device's monitor bounding box; logical px; includes self
  settings: Settings,
  transfers: [ { id, direction: "send"|"receive", peer_id, name, items, bytes_total, bytes_done, rate_bps,
                 state: "queued"|"awaiting_confirm"|"active"|"done"|"failed"|"cancelled", error? } ]
}
Settings {
  device_name,
  hotkeys:   { return_home: "Ctrl+Alt+Shift+Home", toggle_sharing: "Ctrl+Alt+Shift+S" },
  clipboard: { enabled, sync_text, sync_images, sync_files, max_auto_mb, exclude_sensitive },
  switching: { edge_delay_ms, corner_dead_zone_px, double_tap },
  keyboard:  { swap_ctrl_cmd: "auto"|"always"|"never" },
  startup:   { launch_at_login, start_minimized },
  network:   { port, discovery }
}
```
Coordinates and extents use one uniform logical space per device, with origin at the
monitor bounding-box top-left. Windows applies `(physical - min_physical_origin) /
primary_scale` to every x/y/w/h and cursor position; the inverse uses that same divisor.
The unit is a primary-scale logical pixel. macOS CG global coordinates are already uniform
points, so only the bounding-box origin is subtracted/added. Per-monitor `scale` is
informational DPI/backing scale; it never rescales device geometry or movement.
`layout.devices[].x/y` place the device bounding-box origin in the shared space.
The brain persists `layout` and `settings` under `--data-dir`; layout is replicated to peers via `LayoutUpdate`.

## 8. Repo layout

```
core/ Cargo workspace
  crates/glide-proto     wire + IPC types, codecs, limits, fuzz-friendly decoders
  crates/glide-platform  traits + mocks, shared types (Event, HidUsage, Monitor, Permissions)
  crates/glide-platform-win   (cfg windows)   crates/glide-platform-mac   (cfg macos)
  crates/glide-net       identity, keystore, pairing PAKE, QUIC transport, mDNS discovery
  crates/glide-xfer      file transfer engine (chunk/zstd/blake3/resume/staging/sanitise)
  crates/glide-daemon    binary glided: orchestration, layout & edge engine, key translation, IPC, config
desktop/  Tauri app; desktop/ui the screens; desktop/dev the mock engine and checks
```
CI: GitHub Actions matrix (windows-latest, macos-latest): fmt, clippy -D warnings, tests, build release, package.

## 9. Always-on mode

`glided --data-dir <dir> --headless [--ui <absolute app executable>]` runs without
stdin or the Glide window. `--ui` is validated and stored in engine config;
it is used only to open the app on demand. Existing stdio JSONL remains available
without `--headless`, including explicit development mocks.

Local control is a byte-stream named pipe on Windows,
`\\.\pipe\glide-ctl-<16 lowercase random hex digits>`, or a Unix socket at
`<data-dir>/ipc/ctl.sock`. Windows uses an explicit protected current-user-only
DACL, rejects remote clients, checks the connecting process token SID, and uses
the first-pipe-instance flag. Unix uses an owner-only 0700 IPC directory, 0600
socket, and a peer effective-UID check (`getpeereid` on macOS).

At startup the engine atomically publishes owner-only `ipc.json` containing
`endpoint`, a fresh CSPRNG 32-byte hex `token`, `pid`, `version`, `protocol: 1`,
and `started_at_ms`. First line must be `{"auth":"<token>"}` within two seconds;
success is `{"auth":"ok"}`, followed by the unchanged ready/state/request/
response/event JSONL protocol.
The optional authentication field `"client":"ui"` identifies the window for
notification suppression. On Windows, plain authentication from the configured
`--ui` executable is recognized by its verified client process image; other
unlabelled clients and explicit `"tool"` clients remain tools. This hint
grants no additional capability, and all four slots share the same control API.
Authentication compares fixed-size tokens in constant time and caps the first
line at 1024 bytes. A correct token is accepted regardless of earlier failures;
there is no global lockout. Wrong tokens and malformed first lines receive no
reply and close after a fixed 250 ms delay. Failures are counted only in debug
logs, without tokens. Silent clients close at the two-second first-line deadline.
There are at most 16 active unauthenticated handshakes, separate from the four
authenticated client slots. A correct login queued behind one full batch of
silent handshakes can proceed when a pending slot expires, within about two
seconds (plus OS scheduling); retries are needed if the OS reports pipe busy.
This is an admission/handshake bound, not a fairness guarantee for an arbitrary
OS connection backlog under continuous flooding. Other protocol lines retain
the 1 MiB limit. Bounded queues and write deadlines disconnect slow authenticated
clients, and events reach every admitted client. Tokens are never logged.

One engine owns each data directory: Windows holds a per-user session mutex
and an exclusive lock file; Unix holds `flock` on `engine.lock`. A live same-user
engine is checked against its PID/image before reporting "Glide is already
running" and exit 75. Stale metadata is removed only after acquiring the OS
lock and checking liveness/image. Clean exit removes `ipc.json`; crashes rotate
the token at the next start. Stdio engine ownership metadata uses endpoint
`stdio`, which is informational and cannot accept a connection.

The engine reconciles launch-at-login with the persisted default-off setting:
Windows HKCU Run value `Glide` launches the quoted engine path with `--headless`,
the data-directory argument and optional quoted `--ui`; macOS uses its existing
LaunchAgent helper with the same arguments. No control characters are accepted
in executable paths. Explicit mock OS platforms do not change host autostart.

Windows owns a native notification-area icon on a blocking message-loop thread,
with Open Glide, sharing toggle, return-home, connection status and clean Quit.
Explorer restarts re-register the icon. Warning/error and unattended pairing
balloons are throttled. Tray creation failure leaves the engine running.
On macOS the Glide app (Tauri) owns the menu-bar icon;
an NSStatusItem implementation requires a main-thread NSApplication run loop.

`app.shutdown` flushes its response before closing the requesting connection,
cancels transfer work, releases all injected input and restores local capture.
Signals, Windows console close/logoff/shutdown, and drop guards share cleanup.
Cleanup has a deadline so connected clients and bulk work cannot hold exit open.

Threat boundary: other users and remote clients are denied independently of the
token. Processes already running as the same OS user can read the token and
control Glide; this does not sandbox malicious same-user software. OS-level
administrators/root, privilege escalation and a compromised current-user
session are outside this boundary. Pairing still requires the existing PAKE,
dual human SAS confirmation and pinned TLS; local control does not bypass them.
