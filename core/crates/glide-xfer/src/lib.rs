//! Transport-independent verified file transfers for authenticated Glide peers.
//! See the crate README for protocol sequencing, resource ceilings and trust boundaries.

#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(feature = "file-engine")]
mod engine;
#[cfg(feature = "file-engine")]
mod filesystem;
#[cfg(feature = "file-engine")]
mod paged_engine;
#[cfg(feature = "file-engine")]
mod paging;
#[cfg(feature = "file-engine")]
mod receiver;
#[cfg(feature = "file-engine")]
mod stream;

#[cfg(feature = "file-engine")]
pub use engine::*;
#[cfg(feature = "file-engine")]
pub use paged_engine::*;
#[cfg(feature = "file-engine")]
pub use paging::*;
#[cfg(feature = "file-engine")]
pub use stream::{
    read_message, write_file_chunk, write_message, ChunkEncoder, ChunkMessage, ChunkReader,
    PreparedChunk,
};
