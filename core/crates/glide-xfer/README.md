# glide-xfer

Default feature `file-engine` supplies the transport-independent file/folder engine.
No new dependencies were added. The existing unused `glide-net` dependency is optional;
the engine does not construct QUIC, identities, clipboard entries or executable processes.
Every existing public item/signature, `Config` field and error variant stays source-compatible.
Negotiation, paging and fragmented-resume coalescing use additive APIs or opt-in options.

## Daemon integration

1. Construct one `FileEngine` per protected, pre-existing daemon data directory. It
   purges stale sessions on construction; spawn/own `run_purge` for scheduled cleanup.
2. Sender calls `build_manifest` with selected paths. DFS retains only bounded manifest
   metadata and at most 32 directory iterators, not directory listings or file contents.
   Sources are prehashed because the wire manifest requires whole-file hashes.
3. Supply a bidirectional control stream and 1–4 dedicated chunk streams to `send` /
   `receive`. Any `AsyncRead`/`AsyncWrite` implementations work, including QUIC halves.
   Both sides must agree on lane count and timeout before calling. The v2 manifest
   carries `chunk_size`. Existing engines require the exact configured size; opt-in
   receivers use the sender's size within `ChunkSizeBounds`. Streams belong only to this transfer; discard them on any error
   or cancellation. Do not retry a partially consumed frame on the same stream.
   The daemon must close the **peer connection** on codec/decode failures and invalid
   frame discriminants/lengths, per SPEC; transport-independent xfer cannot close QUIC
   itself. Ordinary valid-message quota rejection can fail just that transfer.
4. `receive` requires the device ID from a completed pinned/authenticated transport,
   not discovery or a peer-supplied identity. The daemon must also match the expected
   clipboard announcement, sharing policy and clip/transfer IDs before invoking it.
   The engine cannot authenticate arbitrary byte streams itself. QUIC authentication,
   no 0-RTT and input-stream priority remain glide-net responsibilities.
5. For `Consent::Automatic`, over 2048 MiB returns `ConfirmationRequired` with the
   actual manifest before staging. Emit an awaiting-confirm transfer snapshot, close
   those streams, then retry after human approval with `Consent::approve(&manifest)`.
   Approval binds every name, size, file digest, clip/transfer ID, chunk size and
   page/final marker; substitutions fail. `Consent::approve_pages` defines the
   complete ordered-page BLAKE3 digest contract through `ManifestValidator`.
   Missing/reordered pages fail. Existing `send`/`receive` retain their single-page
   contract; use `send_paged`/`receive_paged` for larger trees (see below).
6. Poll `Progress::snapshot`, `transfer_snapshot` or `transfer_snapshot_paged` at most 10 Hz and feed snapshots to
   `Core::update_transfer`. Callers set active/done/failed/cancelled state and associate
   the authenticated peer. Progress counts verified resumed bytes but rate excludes them.
   A sender is done only after the receiver's completion acknowledgement.
7. Retain `Received` or `ReceivedPaged` while clipboard file URLs/CF_HDROP reference its `paths`. Its
   owned OS lease prevents purge of live publication. `Received.paths` contains only
   top-level files/folders after **all** files and the final tree were verified/published.
   The engine never executes files. Callers must clear stale clipboard references
   before releasing this lease.

Wire: big-endian u32 byte length + existing postcard `TransferMessage`. Control sequence
is complete manifest page(s) → one `FileResume` for each regular file → sender `FileComplete` → receiver
`FileComplete` acknowledgement. Chunk lanes carry `FileChunk` and lane `FileComplete`.
`FileCancel` is accepted on either control or chunk lanes. Control stays idle while chunk
lanes are active; once its first byte arrives, the complete frame has a timeout. After
lanes finish, completion has the normal idle timeout as well. Partial frames are never
restarted by a select loop. Chunk index/offset/size are checked against the selected manifest
chunk size; arbitrary offset writes and overlapping/duplicate chunks are rejected.

## Additive APIs for large trees and negotiated chunks

`FileEngine::with_chunk_size_bounds(ChunkSizeBounds { min, max })` opts the existing
receiver into sender-selected chunk geometry. Validate bounds before staging/chunk-buffer
allocation; offsets, journals and verification use the manifest size. It is bound exactly
into resume metadata. Without this option, the old exact-size behavior stays unchanged.

`build_manifest_paged(roots, transfer_id, clip_id, config, paging, spool_dir, cancel)`
returns `PagedSendPlan`. Use `FileEngine::send_paged` with its existing control/lane
arguments and `receive_paged(peer, control, lanes, paging, consent, cancel, progress)`.
The latter returns `PagedReceive::Received(ReceivedPaged)` or
`PagedReceive::NeedsApproval(PagedManifest)`. Retain the spool for review, then retry
on fresh streams with `manifest.approve()` after human approval. That approval equals
`Consent::approve_pages` over the complete canonical pages, in order.

`PagedManifest` cheaply clones its disk owner. `summary`, `first_page`, `read_page`,
`entry`, `source`, `digest`, `limits`, `spool_bytes`, `journal_bytes` and `approve`
support bounded review without collecting the tree. Run disk methods on blocking workers.
`PageSpool::{new,push,finish}` supports custom/synthetic page producers. Provide a
dedicated, protected, existing spool directory. Outside `FileEngine`, callers must
serialize reservation changes when several spools share a directory.

The DFS sender retains one page/source-path batch, a 64 KiB hashing buffer and at most
`max_depth` directory iterators. Pages, individual entry/source records and resume
ranges are disk-spooled. The receiver checks every page with `ManifestValidator`,
spooling all pages before approval/chunks. Skip/replay/order/contiguity/identity errors
fail closed. A disk hash table and exact lowercase keys enforce uniqueness and preceding
directory parents without a tree-sized RAM set. Probing is expected O(1), capped at
4096 probes to reject crafted clusters. Bounded per-entry disk indexes avoid redecoding a
whole page for each file. Chunk geometry and the complete ordered digest bind reconnects.
`Progress::transfer_snapshot_paged` reports aggregate count/bytes.

`ChunkEncoder::{prepare,view}`, opaque `PreparedChunk`, `write_file_chunk`,
`ChunkReader::{new,with_limits,read,chunk,buffer_capacity}` and `ChunkMessage` expose
the borrowed hot path. Existing `encode`/`recycle` and owned message helpers remain.
Sender writes a stack prefix, original raw/compressed slice and stack trailer. Receiver
incrementally checks the header before allocation and reads the payload directly into
one reused frame buffer, borrowing its view. Buffers move to blocking storage work and
back to their lane. No per-chunk transfer-ID allocation, owned-payload copy or payload
zero-fill remains in the engine. Transport/kernel copies still exist. Borrowed manifest
preflight checks entry counts, path lengths/depth, file/chunk/byte limits and field shapes
before owned postcard decoding; frame bounds precede buffer allocation.

## Safety, staging and resume

`data_dir/glide-xfer/<BLAKE3(peer,id)>` contains metadata, a lease, numeric `.part` files,
33-byte-per-chunk verification journals and a private `incoming` tree. Names are validated
component by component: separators, colon/ADS, `..`, control characters, Windows reserved
devices (including superscript digits), trailing dots/spaces and excessive lengths fail.
Parents must be explicit directories in a preceding manifest entry. IDs are contiguous;
case collisions fail. Exclusive placeholder creation also detects filesystem-specific
Unicode/case aliases. Unicode names are preserved. Symlinks, junctions, other reparse
points and special source files are rejected, not followed or created. Final file opens
use no-follow/reparse-point flags and validate the owned file handle. Every OS handle has
RAII ownership; unsafe code is limited to documented disk-space/lease OS calls.

The daemon must own the data directory and prevent untrusted local writers. Portable
path-component checks and no-follow final opens do **not** provide a capability sandbox
against a local actor concurrently replacing ancestor directories. A local hostile
writer requires platform openat/directory-handle confinement, outside the current shared
platform trait. Selected source ancestors also must remain stable while walking. This
limitation does not permit peer-controlled path traversal: peers never select ancestors.

Reconnection binds the exact complete manifest, authenticated peer and selected chunk size.
Every journal-marked chunk is reread and BLAKE3-verified before reporting it present;
changed/short chunks are cleared and retransmitted. Missing ranges remain bounded at
4096. Existing engines keep the old restart behavior. `with_resume_coalescing` and
`receive_paged` retain the first 4095 exact missing ranges and widen only the overflow
tail into the final range. Only verified chunks covered by that tail are cleared for
explicit retransmission; the verified prefix stays present. This uses the unchanged
`FileResume` contract and never restarts the whole file just because of range count.
A fragmented tail can retransmit substantial valid data; exact range paging would need
a new shared contract. The 10,000-chunk alternating-gap test retains 4096 verified bytes
across a cut, then completes with final range 8191..10000. No chunk-index set grows with file size in
RAM. Large resumes on slow disks may require raising the sender's configurable handshake
timeout (up to 300 seconds) to allow rehashing. Journal writes can be lost on power failure,
but are never trusted without rehashing.
Final data is synced, checked against the manifest's whole-file hash, renamed over the
private placeholder, then the entire incoming tree is atomically renamed to `ready`.

Transport I/O interruption retains private resumable staging. Storage I/O is classified
separately and purged. Corruption, protocol errors, decompression errors, timeouts and
explicit cancellation discard staging, reporting cleanup failures. Dropped receiver
futures attempt cleanup via RAII; OS deletion failures are retried by purge after expiry.
Startup/scheduled purge skips live leases.
Manifest/review/resume spools have live leases and 8-byte reservation markers and count
against staging session/byte quotas, including held prompts. Purge also scans expired
`glide-manifest-*` / `glide-resume-*` crash remnants. All file handles close before their
temporary directory is removed, including on Windows.

A published ID is rejected
as a replay; disconnect after publication but before acknowledgement requires a new ID.
Completed IDs are not permanent replay tombstones after purge; transport freshness and
daemon clipboard-transaction ownership remain required. A received tree is not exposed
until integrity checks succeed, even if sender progress already reaches 100%.
No completion-status request/response was added: the approved policy remains a
new transfer ID after a lost completion acknowledgement.

## Resource ceilings

Defaults: 4 MiB chunks, 4 lanes, 4 concurrent send/receive sessions per engine, 4096 entries,
depth 32, 1024 UTF-8 bytes per relative path, 255 UTF-8 bytes/UTF-16 units per component,
4096 encoded bytes per absolute source path, 64 GiB per file/transfer, 1,048,576 chunks per
file, 32 staging sessions, 64 GiB staging reservation quota, 64 MiB free-space reserve,
30-second whole-frame/queue deadlines, 24-hour expiry and hourly purge. Staging quota
includes chunk journals and a 16 MiB metadata allowance per job, so an exactly 64 GiB
payload cannot fit the default staging quota. Disk space is checked before staging and
again before each chunk write; disk/OS errors never expose partial files. Configure quotas
deliberately rather than accepting arbitrary peer estimates. Rate limits apply globally
to all sender lanes using uncompressed bytes; waits are timer-driven and cancellable.
Configuration rejects rates whose worst first-chunk wait across the configured lanes
already exceeds the peer deadline; lower rates require smaller agreed chunks/timeouts.

Read-ahead is one chunk per lane; receiver queue capacity is one. Raw/packed/decompression
buffers are reused. zstd level 1 is used only if a ≤64 KiB sample and the whole chunk both
save at least 10%; known compressed/media extensions bypass it. Decoder output is a fixed
trusted-size slice; oversized output, incorrect lengths, excessive windows and trailing
or invalid compressed data fail. Encoder/decoder windows are capped at 4 MiB.
hashing, compression, large page/control codecs and staging filesystem work run on
blocking workers; the small borrowed chunk codec runs inline.

`Config::payload_memory_ceiling()` is a conservative **Rust payload/metadata** bound:

```text
4 × 16 MiB + (4 × lanes + 2) × (4 MiB + 4096)
  + max_entries × (4 × max_path_bytes + 4096 + 512)
```

`PagingConfig` defaults: 1,048,576 aggregate entries, 1 GiB canonical manifest bytes,
4096 entries / 16 MiB per page, 4096 roots, receiver chunk bounds 1 byte–4 MiB and a
4 GiB spool cap. Hard ceilings are 1,048,576 entries, 4 GiB manifest, 8 GiB spool,
4096 page entries/roots, 16 MiB frames, depth 64 and 4096-byte paths; `Config` can lower
path/depth/file/transfer/chunk limits. Receiver spool capacity is also capped by the
staging quota. The default sparse uniqueness table consumes 48 MiB of disk while
validating and is removed on finish. Sender resume spools share the manifest's spool
budget and obey staging/disk-space limits. Legacy `Config::max_entries` remains 4096.

Default: 178,331,648 bytes (170.07 MiB) per transfer, independent of file size/chunk count.
For paging, `PagingConfig::payload_memory_ceiling(&config)` substitutes the bounded
page-entry count and adds a page key-map allowance: default 195,108,864 bytes
(186.07 MiB), independent of aggregate tree size. Both ceilings exclude caller transport
buffers, OS/Tokio stacks, allocator overhead and native zstd allocations; neither is
an RSS guarantee. The engine uses the shared `FileChunkView`, prefix/trailer encoders,
borrowed decoder and incremental header decoder without changing postcard bytes.

## Offline checks and benchmark

Run from `core`, using an external target directory with a short Windows path
such as `$env:TEMP/glide-xfer-v2-target`; any scratch path outside the repository works
exceeds `cl.exe` limits. Do not change shared `.cargo/config.toml`:

```powershell
$env:CARGO_TARGET_DIR = '$env:TEMP/glide-xfer-v2-target'
cargo fmt -p glide-xfer --check
cargo clippy --offline --locked -p glide-xfer --all-targets -- -D warnings
cargo test --offline --locked -p glide-xfer
cargo check --offline --locked -p glide-xfer --no-default-features
cargo check --offline --locked -p glide-xfer --target aarch64-apple-darwin
$env:GLIDE_XFER_PROFILE = '1' # optional CPU/memcpy reference measurements
cargo run --offline --locked --release -p glide-xfer --example throughput
```

Tests exercise real staging directories and duplex streams: zero/one-byte and 4 MiB ±1
boundaries, deep Unicode trees, malicious names, aliases/limits/approval, corruption and
whole-file mismatches, resume after partial frames/on-disk modification, duplicate chunks,
fragmented resumes, cancellation/abandoned futures, concurrent-session limits, rate limits,
decompression bombs/windows, disk-space failures, Windows junctions, lease-aware startup
and scheduled purge. A 2 GiB compressed synthetic stream checks both chunk/whole hashes
and counts actual Rust heap allocations. Full four-lane staging transfers also have an
allocation-counted ceiling check. Native zstd and OS allocations are not counted.

macOS runtime/ABI, QUIC integration, daemon clipboard behavior, real LAN throughput,
process RSS and simultaneous input latency have not been verified here. Linux disk-space
queries fail closed (Linux is outside SPEC v1). No dependencies added by the file-engine changes.

Fresh release measurements (2026-10-02), same benchmark before/after,
1,073,741,824 plaintext bytes per case. Decimal GB/s:

| Path | Raw before | Raw zero-copy | Compressible before | Compressible zero-copy |
| --- | ---: | ---: | ---: | ---: |
| Framed synthetic duplex | 0.771 | 2.603 | 2.426 | 2.754 |
| Framed synthetic loopback TCP | 0.673 | 1.768 | 2.551 | 2.655 |
| Full disk engine, four duplex lanes | 1.060 | 1.152 | 0.385 | 0.413 |
| Full disk engine, four loopback TCP lanes | 0.271 | 0.972 | 0.534 | 0.349 |

The preceding engineer's raw disk measurements were 0.887 duplex / 0.795 TCP,
historical observations rather than this run's baseline. The new raw run exceeds the
1.0 / 0.8 GB/s targets. These are observations, not sustained-speed guarantees: this is
a shared Windows host and disk/cache/sync/scheduling variation is material. Compressible
TCP staging regressed in this sample and does not meet the raw-data throughput targets.
Framed tests include chunk and whole-stream verification without staging/TLS/QUIC.
Full engine tests include journals, verification, final sync and publication; source
creation/sync/prehash are untimed and reads may hit OS caches.

Single-thread reference measurements: BLAKE3 chunk hash 13.361 GB/s, whole-file hash
13.294 GB/s, one memcpy 28.853 GB/s. Removing codec payload copies substantially
improves framed raw speed. Full staging retains three timed hashing passes (sender chunk,
receiver chunk, receiver whole-file), journal/file operations, sync, queue and blocking
task handoffs; these microbenchmarks do not attribute each operation's individual cost.

Fresh Rust allocator peaks: 2 GiB compressed synthetic stream 17,009,680 bytes;
four-lane engine pairs 54,878,751 / 54,880,809 bytes for 16 / 64 MiB (previously
71,779,941 / 105,207,156). A 1M-entry synthetic tree peaked at 1,323,363 bytes
(1,319,034 above harness baseline), spooled 130,772,611 bytes and took 38.78–46.58 seconds.
It exercises complete page/name/parent validation and spooling, rather than creating
a million filesystem objects. The actual paged transfer test publishes a 4101-entry
tree, including digest-bound confirmation. Native zstd/OS memory is excluded. Runnable
synthetic heap checks enforce 48 MiB; full engine checks use the conservative computed bound.
Final checks: workspace and crate `cargo fmt --check`, all-target strict Clippy,
all 46 tests and the no-default-feature check passed offline/locked. The
`aarch64-apple-darwin` Rust target is installed, but its default-feature check is
blocked in existing BLAKE3/zstd native builds: `cc` is missing. macOS runtime
behavior remains unverified. Detailed task evidence is recorded in WORKLOG.md.

## Native transport adapter (CONTRACTS-v2)

Enable existing optional edge with `native-transfer` (also enables `file-engine`).
`FileEngine::send_native(plan, TransferStreams, &Cancel, &Progress)` and
`receive_native(TransferStreams, Consent, &Cancel, &Progress)` consume authenticated
control/chunk streams from NativeLink. The manifest ID must equal the versioned
preamble transfer ID; receive derives peer identity from authenticated streams.
Codec, integrity or protocol failure closes the connection; other errors cancel
the job. Existing generic send/receive and paging APIs remain available unchanged.
The daemon must still bind announcement/consent/current PeerToken and recheck
clipboard admission before publishing Received.paths; retain Received for its lease.
On connection loss, reopen a job with the same verified manifest and fresh streams
after pinned reconnect; resume journals remain bound to that authenticated peer.

Opt-in `test-support` enables the isolated network keystore solely for downstream
real PAKE/dual SAS/pinned-TLS tests, and is forbidden in release glided. The real
QUIC 32 MiB mid-connection-kill/resume regression is regular under that feature.
The explicit 512 MiB QUIC benchmark runs file engines as independent tasks with two
chunk lanes while measuring 1000 idle/loaded reliable and mouse samples, verifies both
file hashes, and requires each loaded p99 to stay below 3 ms. The daemon uses two lanes
by default; the earlier four-lane measurement reached 1.28 ms p99 for both paths, below
the absolute ceiling despite its large idle-relative increase. See
[CONTRACTS_V2_VALIDATION.md](../../CONTRACTS_V2_VALIDATION.md) for recorded results.
Historical duplex/file benchmarks above are not evidence of this native QUIC criterion.
