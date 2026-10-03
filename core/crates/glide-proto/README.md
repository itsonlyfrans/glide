# glide-proto

Shared typed JSONL IPC and canonical postcard wire contracts. Protocol version 3
uses `wire::ALPN = b"glide/3"` and `wire::PAIR_ALPN = b"glide/pair/3"`.
v1/v2 peers are incompatible and fail ALPN/version admission.

All binary `codec::decode_*` helpers reject trailing bytes and noncanonical
encodings. They use `postcard::take_from_bytes`, require an empty remainder and
compare canonical serialization directly against the input without a second
encoded buffer. Chunks use their strict borrowed header/trailer decoder instead
of reserializing opaque payload bytes. Variant/frame limits and finite-coordinate
checks still apply. Owned chunk decoders validate the complete borrowed frame
before allocating one exact-size payload copy; no second serde traversal is needed.
JSONL retains its bounded typed validation and does not require canonical JSON.

`encode_move_into`, `encode_input_into` and `encode_control_into` write validated
messages into caller-owned slices and return the encoded subslice. Input/control
signatures are `(&Message, &mut [u8]) -> Result<&[u8], CodecError>` with the output
lifetime tied to storage. Existing allocating encoders remain compatible.

`codec::FileChunkView<'a>` borrows `transfer_id: &'a str` and `data: &'a [u8]`,
with the owned chunk's `file_id`, `chunk_index`, `offset`, `uncompressed_size`,
`compressed` and `blake3_hash`. `encode_file_chunk_into` and
`decode_file_chunk_view` preserve exactly `TransferMessage::FileChunk` bytes.
For scatter writes use `encode_file_chunk_prefix_into`, the original payload
slice, then `encode_file_chunk_trailer_into`. No payload copy is needed.

Incremental `decode_file_chunk_header(bytes)` returns
`Result<Option<(FileChunkHeader<'_>, usize)>, CodecError>`: `None` means an
incomplete prefix; `Some` includes the payload-start offset. Pass the accumulated
bounded prefix again after reading more bytes; the decoder owns no buffering.
ID length/UTF-8, canonical varints, numeric file ID and data length are checked
before allocating receive payload storage. The full decoder additionally checks
trailer canonicality, compression geometry and offset overflow. Successful
borrowed decode/into encode paths allocate nothing; caller storage must remain
alive while borrowed views are used.

`FileManifest` adds `chunk_size: u32`, `page: u32`, `final_page: bool`.
`manifest::validate_manifest_page` checks individual page geometry;
`ManifestValidator::push` requires pages 0, 1, ... and globally contiguous file
IDs from zero, stable transfer/clip IDs and chunk size. `finish` requires a final
page and returns `ManifestSummary { items, bytes, chunk_size, pages }`.
Consumers must validate the complete sequence before approval/staging/chunks,
bind every canonical page in order into approval/resume digests, and apply
filesystem name/confinement rules in their file engine. `Consent::approve_pages`
in glide-xfer implements the complete-page BLAKE3 binding.

Limits: 4096 entries and 16 MiB encoded metadata per manifest page; 1,048,576
aggregate items, 64 GiB aggregate bytes, path depth 64, path length 4096 UTF-8
bytes, transfer/clip ID 128 bytes, and chunk size 1..=4 MiB. Core and IPC use the
same deliberately raised aggregate item cap, preserving `Transfer.items: u32`.
Bounded page containers do not reserve untrusted advertised lengths unchecked.

`ipc::PairingCode` is a redacted, serializable zeroizing string owner, used by
`PairingJoinParams` and `PairingStartHostResult`. `Request`/`Response` also wipe
their top-level code values on drop; daemon JSON buffers are separately guarded.
External UI memory and opaque dependency copies are outside that guarantee.

Run from core with an external target directory:

```text
cargo fmt --check
cargo test -p glide-proto --offline --locked
cargo clippy -p glide-proto --all-targets --offline --locked -- -D warnings
cargo check -p glide-proto --target aarch64-apple-darwin --offline --locked
```

Tests cover trailing/overlong encodings per family, reusable encoder equivalence,
chunk golden bytes and prefix/trailer composition, every truncated header/frame,
borrowed slice ownership, malformed/oversized lengths, page ordering/identity,
contiguous IDs, quotas and final-page requirements, and code JSON/redaction/wiping.
Apple checks type-check only.

CONTRACTS-v2 adds `Enter.epoch`, `Leave { epoch }`, `EnterAck { epoch }` and
`InputKey.epoch`, `InputButton.epoch`, `Wheel.epoch`, `ModifierSync.epoch`. All
epochs are nonzero; `InputMessage::epoch()` exposes them uniformly. Native Enter
acknowledgment fences cross-stream input ordering; Leave closes that epoch.
`MetadataUpdate { version, name, monitors }` is control variant 10; version must
match `PROTOCOL_VERSION`, names are bounded/nonempty/control-free, monitors are
bounded with finite positive dimensions/scales. Existing control variant ordinals
are retained, but changed epoch payloads make the wire incompatible with v2.
