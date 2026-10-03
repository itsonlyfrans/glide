# glide-net native transport

`native-transport` is enabled by default. Existing workspace daemon dependencies
therefore compile the native implementation; the daemon still needs its separate
integration step. `InMemoryLink` and `InMemoryPeerManager` remain explicit,
unauthenticated test backends, including with `--no-default-features`.

## Initialization

Call inside a Tokio runtime and retain the manager for the link's lifetime:

```rust,no_run
use glide_net::{NativeConfig, NativePeerManager, PeerManager};
use glide_platform::Os;
use std::{net::SocketAddr, path::PathBuf};

async fn initialize(data_dir: PathBuf, name: String)
    -> Result<NativePeerManager, glide_net::NativeError>
{
    NativePeerManager::new(NativeConfig {
        data_dir,
        bind_addr: SocketAddr::from(([0, 0, 0, 0], 24801)),
        name,
        os: Os::Windows, // Os::Macos on macOS
        monitors: Vec::new(), // use the native platform's validated monitor list
        discovery: true,      // settings.network.discovery
    }).await
}
```

`manager.link()` returns a cloneable `NativeLink` implementing `Link`.
`manager.events()` supplies the existing `PeerManagerEvent` broadcast stream.
`identity()` exposes only public ID/fingerprint metadata. `local_addr()` returns
the bound UDP address. `paired_peers()` restores offline peer snapshots for the
daemon. `set_discovery(bool)` changes active mDNS advertisement and browsing.
The opt-in test provider below is excluded from production glided release builds.
Dropping the manager closes its endpoint and discovery service.

The daemon must consume `recv_event()` and the `MouseReceiver` seam, plus manager events, resolve its
pending join request on `Paired`/`PairingResult`, and restore local capture/release
injected keys on loss. Those responsibilities are outside this crate.

## Identity and trust

Ed25519 self-signed certificates are regenerated from a stable private PKCS#8
key. The ID is lowercase hex SHA-256 of the complete DER SubjectPublicKeyInfo;
the displayed fingerprint is uppercase hex in groups of four. Windows
Credential Manager entries are additionally wrapped with machine-bound DPAPI,
preventing a roaming credential from cloning the identity onto another device.
Old raw Windows credentials are rejected and require an explicit identity reset.
macOS uses Keychain through the native `keyring` backend.
Entries use service `com.glide.identity.v1` and user `<namespace>-device-pkcs8` /
`<namespace>-device-wrap-key`. keyring 4 (`v1` feature) keeps the keyring 3
layout: a Windows generic credential with target `<user>.<service>`, enterprise
persistence and a UTF-16LE blob; a macOS login-keychain generic password with
that service/account. `keyring3_credentials_are_read_by_upgraded_keystore`
(ignored, Windows) proves an entry written the keyring 3 way still loads.
Known-answer tests pin the device ID/fingerprint and every pairing derivation.

The direct credential is preferred. If its write fails, the only fallback is
`identity.key.enc`, AES-256-GCM with a random key held in the OS keystore (also
machine-bound DPAPI on Windows). Keystore failure is an error, never a plaintext
fallback. Owned private-key, code, exporter, wrapping-key and PAKE-key buffers are
zeroizing; secrets are not logged. The fixed dependencies retain opaque internal
secret copies without a universal zeroization guarantee; see the proposal below.

Normal ALPN `glide/3` uses TLS 1.3 mutual authentication with live pin verifiers,
handshake signature verification, no tickets/session cache/resumption/0-RTT.
Pins are checked again at installation, enqueue and delivery. Generation checks
discard events from replaced connections. Unknown/revoked certificates cannot
enter the normal application protocol. Discovery names/IPs never confer trust.

Unpair first removes live trust and closes sessions, then commits a durable deny
record before rewriting peer snapshots. A failed snapshot write cannot resurrect
the old pin after restart. Any unresolved storage transaction fails startup
closed and preserves evidence for explicit repair; it is never silently discarded.
Public files are `network-peers.json`, `revoked-peers.json` and `identity.namespace`.
The latter contains exactly 64 lowercase hex digits from 32 random PUBLIC bytes,
scoping direct/wrapping credential usernames independently for each protected data
directory. It contains no private key. Do not copy namespaces between installations;
protect and exclusively own the data directory. No legacy migration is provided.

Before constructing the manager, an explicitly requested recovery may call
`NativePeerManager::repair_unfinished_pairing(&Path) -> Result<PairingRepair, NativeError>`
on a blocking worker. `PairingRepair { removed_files, removed_peer_ids }` reports the
discarded unfinished state. A valid journal removes its candidate pin; malformed,
partial or orphan state conservatively removes all persisted pins. Committed deny
records remain. A durable `pairing-repair.json` denial marker gates startup until
cleanup completes, including after an interrupted repair. The data directory must
be exclusively owned; symlink/directory journal collisions fail without deletion.
Startup never runs repair or activates an unfinished candidate automatically.

## Pairing

The host displays one random six-digit code for 120 seconds. Three failed
exchanges burn it; each source IP has 12 probe/pair attempts per 120 seconds, with
a bounded source table. A successful PAKE also burns the code. The first bidi
stream permits only a non-sensitive hello/probe or pairing request. A second
stream runs SPAKE2, deriving its password/identities from the code, TLS exporter
and both certificate IDs. Transcript HMACs authenticate both fingerprint/metadata
sets. HKDF derives a 33-bit, three-word SAS from a checked-in 2048-word list.
The noncanonical English list has unique four-letter prefixes, no one-edit
word pairs and tested sensitive-term exclusions. Phonetic/cross-cultural
confusability still needs native-speaker review.

`PairingHost.code` and returned host copies own `Zeroizing<String>`. Debug output
redacts the code. `glide-proto::ipc::PairingCode` owns typed IPC copies, and the
daemon wipes request/response JSON buffers after use. These guarantees do not
extend to external app display buffers or dependency-internal opaque copies.

`pair_join` returns an **unpinned** `PairingSession`. Both screens receive the
existing verification prompt. Both must call `confirm_pairing(true)` within
60 seconds; decline, timeout, bad SAS authentication or disconnect aborts.
Authenticated Confirm/Ready/Commit/Complete barriers protect persistence.
Staged metadata is guarded by `pairing-pending.json` and cannot admit a normal
handshake. Only both completion proofs permit activation; interrupted staging
rolls back, and a crash leaving the journal prevents startup trust. Distributed
power-loss atomicity across two hosts is not proven by the loopback tests.

One endpoint admits both ALPNs without changing configurations during pairing.
The delegating `DispatchSession` buffers a bounded first ClientHello (16 KiB),
requires exactly one recognized ALPN and selects its crypto session before
feeding rustls or running a certificate verifier. Missing, malformed, mixed,
duplicate and unknown ALPNs fail closed. `glide/3` always uses strict live pins;
`glide/pair/3` uses pairing-only verification gated by the live pairing window.
Established normal sessions and new normal reconnects continue while pairing
is open. The approved optional direct `quinn-proto = 0.11.19` dependency exposes
the delegating crypto-session interface; the lockfile changes only that edge.

## Streams, limits and latency

Normal framing is a four-byte big-endian length plus the existing postcard
payload. Control uses dialer bidi stream zero, priority 100. Uni streams begin
with a lane byte: input 1 (priority 90), clipboard 2 (priority 20), files 3
(priority 0, one bounded message and FIN per stream). Four file streams may run
per peer. Pairing uses two-byte lengths for PAKE frames (maximum 1024 bytes);
its first hello stream uses four-byte JSON lengths (maximum 1024 bytes).

Variant-specific limits are checked before payload allocation. `glide-proto`
alone rejects trailing/overlong and other noncanonical postcard encodings;
there is no redundant network re-encode/compare allocation. Every bad frame
closes its connection. Queues are bounded; input and control delivery are
separate from the two-slot bulk queue. A 96 MiB byte budget bounds owned bulk
payload admission (including credit for the frame and decoded payload),
alongside Quinn's flow control. Caller-owned source buffers and decoded metadata
overhead are additional. Slow frame bodies/writes time out at
five seconds. Admission is limited to 32 peers/handshakes, 21 bidi and eight uni
streams per connection, 256 KiB stream receive windows and 8 MiB connection
receive/send windows. Congestion control is Cubic; this Quinn API has no pacing
disable switch. Transport idle timeout is five seconds.

Persistent input/clipboard stream credit is reserved before publishing a session.
File reader tasks wait asynchronously for four decode slots; an independent
14-task cap prevents unfinished stream floods from allocating unbounded tasks.
Bulk pressure therefore neither rejects a legal subsequent file stream nor
consumes the first input stream's credit. Five-second slow-reader protection
still applies. Reliable control/input actors encode typed messages, including
heartbeats, into reused lane buffers with `encode_control_into` and
`encode_input_into`; clipboard/file messages retain their owned encoding path.

Heartbeat/ACK runs every 500 ms; missing matching application ACKs for 1500 ms
closes the session and emits loss even if QUIC packets or inbound heartbeats
continue. RTT/throughput stats emit at most once per second. Automatic reconnect
uses 500 ms to 5 second backoff; the smaller device ID initiates automatic
dials at once and the other side joins in after the peer has been unreachable for 5
seconds, so one failing side cannot leave both waiting. Simultaneous dials dedupe to the
connection initiated by the smaller ID.

Mouse enqueue uses a single fixed latest-wins slot, synchronous try-locks and
returns `Busy` immediately on contention. The pump sends as soon as notified:
there is no periodic 1 ms pacing. Old sequence numbers are discarded. Eight
prewarmed/reclaimable payload buffers and one queued Quinn datagram avoid owned
payload growth. Windows requests 1 ms timer resolution with a balanced RAII
request only while authenticated sessions exist. Allocation instrumentation
covers the synchronous enqueue and receive seams, not Quinn/runtime internals.
`MouseReceiver::try_recv_move() -> Result<Option<ReceivedMove>, LinkError>`
returns `ReceivedMove { peer: PeerToken { slot, generation }, movement: Move }`
without allocation for both native and in-memory links. Replaced connections
receive a fresh generation. `Link::peer_token` resolves a connected peer's token;
`mouse_ready` notifies its bounded latest-wins receive slot. The daemon drains
this seam and checks token freshness in `Core::receive_move`. `recv_event` excludes
mouse data, so its boxed future survives mouse wakeups; compatibility `recv`
still includes mouse events and allocates a peer string. Rare transport buffer
exhaustion backs off without spinning, rather than pacing normal sends.

mDNS advertises only device ID, name, OS and port on `_glide._udp.local.`.
The instance/host names and TXT keys are pinned by a test because older
releases must keep finding newer ones. Loopback interfaces stay disabled (the
mdns-sd default before 0.17); mdns-sd 0.18+ also skips point-to-point (tunnel)
and Apple peer-to-peer interfaces.
Disabling discovery creates no mDNS service/browser; manual `host:port` probes
remain available only against an open pairing window. Manual/discovered hello
metadata remains untrusted until PAKE and both human confirmations succeed.

## Verification

Run from `core/` with the supplied external target directory, never a repo target:

```text
cargo fmt -p glide-net --check
cargo clippy -p glide-net --all-targets --offline --locked -- -D warnings
cargo test -p glide-net --offline --locked
cargo clippy -p glide-net --no-default-features --all-targets --offline --locked -- -D warnings
cargo test -p glide-net --no-default-features --offline --locked
cargo test -p glide-net --release --offline --locked native::tests::loopback_latency_microbenchmark -- --ignored --exact --nocapture
cargo test -p glide-net --release --offline --locked native::tests::saturated_file_streams_do_not_block_input -- --exact --nocapture
```

Tests use actual Windows loopback QUIC with ephemeral, test-only identities and
discovery disabled. They cover dual SAS equality/confirmation, incorrect code,
three strikes, expiry, a TLS-terminating relay without the code, decline/SAS
mismatch/disconnect, pre-application unknown-certificate rejection, revocation,
restart fail-closed storage, fault injection at journal/peer/deny create, write,
sync, rename and directory-sync boundaries, repair interruption/retry, source
rate limiting, malformed/oversize frames, exact ALPN selection, pairing-window
normal traffic/reconnects and revocation, latest-wins datagrams, measured input
during saturated file streams, reverse-path heartbeat failure, dedupe and
allocation-free synchronous enqueue/receive (calling-thread Rust allocator).

DPAPI encryption/tamper tests run on Windows. The real isolated Credential
Manager identity test is explicit/ignored because it writes OS credentials. Its
current sandbox attempt fails with secure-storage access denied before identity
creation. Do not substitute the test provider for that OS acceptance result.
See [the contract validation report](../../CONTRACTS_V2_VALIDATION.md) for current
workspace stress runs and real QUIC/FileEngine latency measurements. Live LAN mDNS,
Keychain, native Mac execution and device capture/injection remain manual checks.

## CONTRACTS-v2 APIs

Protocol/ALPN v3 is incompatible with v1/v2. Reliable key/button/wheel/modifier
messages and Enter/Leave carry nonzero epochs. Sending Enter waits up to five
seconds for EnterAck; the receiver must call `Link::admit_input_epoch(peer_id, epoch)`
after policy and initial injection. No reliable input or mouse is admitted until
then; Leave revokes it. Pending Enter cancellation closes the connection.
Reliable events include `peer_token: Option<PeerToken>` (Some for native transport);
consumers recheck it at dispatch to reject retired generations.

`NativePeerManager::update_local_metadata(name: String, monitors: Vec<Monitor>)`
validates and publishes versioned `ControlMessage::MetadataUpdate(MetadataUpdate)`
to current peers, the mDNS service, pairing metadata and future Hellos. Installation
replays current metadata if Hello overlapped a change. `update_listen_port(port)`
rejects any change to the bound port. `PeerManagerEvent::DiscoveryRemoved { device_id }`
reports removal/disable of unpaired discoveries without disconnecting active peers.

`NativeLink::open_transfer(peer_id: &str, transfer_id: &str, lanes: u8)` and
`accept_transfer()` return `TransferStreams` with authenticated `peer_id`, current
`peer_token`, bound `transfer_id`, `control: TransferIo`, `chunks: Vec<TransferIo>`
and `handle: TransferHandle`. TransferIo implements Tokio AsyncRead/AsyncWrite and
`split()` returns cancellation-aware control halves; `read_id()` / `write_id()`
allow stream identity checks. Do not bypass the wrapper with raw Quinn I/O.

All 1..=4 distinct chunk lanes and control carry preamble `GLDX\0\x01`, lane count,
lane number (control 0, chunks 1..=4) and bounded UTF-8 transfer ID. Root reservation
and ready/all-lanes acknowledgments precede payload delivery. At most four total
incoming/outgoing jobs per peer reserve 20 bidi credits, separate from persistent
input/clipboard credits. Handshakes/preambles expire after five seconds. Malformed,
unknown, duplicate, excessive or stalled lanes close the offending session.
Priority is -10, below input 90 and clipboard 20; bulk writes yield between 8 KiB
quanta. This reduces contention but does not establish the 2x-idle p99 target.
`handle.cancel()` wakes both split halves and stops/resets on their next poll/drop.
`close_on_codec_failure()` closes the session; FileEngine native adapters call it
for codec, integrity and protocol failures. Dropping a successful writer finishes
queued bytes. `accept_transfer` discards retired/revoked generations.

Feature `test-support` (default off) exposes isolated `TestKeyStore::load_or_create`
and `NativePeerManager::with_test_keystore(config, &store)`. Clones share one store;
different stores do not. Secrets are zeroizing; identity, PAKE, dual SAS confirmation,
persisted trust and pinned TLS remain real. `TEST_SUPPORT_ENABLED` permits the
daemon compile-time release guard even when Cargo unifies dependency features.

```text
cargo test -p glide-net --features test-support --offline --locked
cargo test -p glide-net --offline --locked two_data_directories_have_independent_native_identities -- --ignored --nocapture
cargo test -p glide-net --offline --locked keyring3_credentials_are_read_by_upgraded_keystore -- --ignored --nocapture
cargo test -p glide-xfer --features test-support --test native_transfer --offline --locked
cargo test -p glide-xfer --features test-support --release --test native_transfer bulk_quic_transfer_keeps_input_p99_within_twice_idle --offline --locked -- --ignored --nocapture
```
