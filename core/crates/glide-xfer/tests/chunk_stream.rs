#![cfg(feature = "file-engine")]

use glide_proto::codec::{self, FileChunkView};
use glide_proto::wire::*;
use glide_xfer::{
    write_file_chunk, Cancel, ChunkEncoder, ChunkMessage, ChunkReader, Config, Error,
};
use proptest::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn frame(view: &FileChunkView<'_>) -> Vec<u8> {
    let mut prefix = [0u8; 256];
    let prefix = codec::encode_file_chunk_prefix_into(view, &mut prefix).expect("prefix");
    let mut trailer = [0u8; 38];
    let trailer = codec::encode_file_chunk_trailer_into(view, &mut trailer).expect("trailer");
    let length = prefix.len() + view.data.len() + trailer.len();
    let mut result = Vec::with_capacity(length + 4);
    result.extend_from_slice(&(length as u32).to_be_bytes());
    result.extend_from_slice(prefix);
    result.extend_from_slice(view.data);
    result.extend_from_slice(trailer);
    result
}

#[test]
fn mutable_encoder_access_invalidates_prepared_metadata() {
    let mut encoder = ChunkEncoder::new(&Config::default()).expect("encoder");
    encoder.buffer_mut(64).expect("source buffer");
    let prepared = encoder.prepare("data.jpg").expect("prepare");
    assert!(encoder.view("transfer", 0, 0, prepared).is_ok());
    encoder.buffer_mut(64).expect("next source buffer");
    assert!(encoder.view("transfer", 0, 0, prepared).is_err());
}

#[tokio::test]
async fn borrowed_chunk_writer_and_reader_reuse_the_frame_buffer() {
    let config = Config::default();
    let mut encoder = ChunkEncoder::new(&config).expect("encoder");
    let (mut writer, mut stream) = tokio::io::duplex(16 * 1024);
    let cancel = Cancel::new();
    let mut reader = ChunkReader::new(config.chunk_size).expect("reader");
    let mut first_data_ptr: *const u8 = std::ptr::null();
    let mut first_capacity = 0;

    for step in 0..2u32 {
        let index = step + 5;
        let source_ptr = {
            let source = encoder.buffer_mut(4096).expect("source buffer");
            for (offset, byte) in source.iter_mut().enumerate() {
                *byte = (offset as u8).wrapping_add(index as u8);
            }
            source.as_ptr()
        };
        let prepared = encoder.prepare("data.jpg").expect("prepare");
        let chunk = encoder
            .view("transfer", 0, index, prepared)
            .expect("borrowed view");
        assert_eq!(chunk.data.as_ptr(), source_ptr);
        write_file_chunk(&mut writer, &chunk, Duration::from_secs(1), &cancel)
            .await
            .expect("zero-copy frame write");

        {
            let ChunkMessage::Chunk(received) = reader
                .read(&mut stream, Duration::from_secs(1), &cancel)
                .await
                .expect("borrowed frame read")
            else {
                panic!("expected chunk");
            };
            assert_eq!(received.transfer_id, "transfer");
            assert_eq!(received.chunk_index, index);
            assert_eq!(received.data, chunk.data);
            assert_eq!(
                received.blake3_hash,
                *blake3::hash(received.data).as_bytes()
            );
            let received_data_ptr = received.data.as_ptr();
            if step == 0 {
                first_data_ptr = received_data_ptr;
            } else {
                assert_eq!(received_data_ptr, first_data_ptr);
            }
        }
        let capacity = reader.buffer_capacity();
        assert!(capacity <= config.chunk_size + MAX_FILE_CHUNK_FRAME_BYTES - MAX_FILE_CHUNK_BYTES);
        if step == 0 {
            first_capacity = capacity;
        } else {
            assert_eq!(capacity, first_capacity);
        }
    }
}

#[tokio::test]
async fn advertised_chunk_limits_are_checked_before_frame_allocation() {
    let mut reader = ChunkReader::with_limits(1024, 2048).expect("reader");
    let (mut writer, mut stream) = tokio::io::duplex(4096);
    writer
        .write_all(&((2049u32).to_be_bytes()))
        .await
        .expect("length");
    let error = reader
        .read(&mut stream, Duration::from_secs(1), &Cancel::new())
        .await
        .expect_err("oversized frame rejected");
    assert!(matches!(error, Error::Limit("chunk lane frame size")));
    assert_eq!(reader.buffer_capacity(), 0);

    let payload = vec![0x5a; 1025];
    let too_large = FileChunkView {
        transfer_id: "transfer",
        file_id: 0,
        chunk_index: 0,
        offset: 0,
        data: &payload,
        uncompressed_size: payload.len() as u32,
        compressed: false,
        blake3_hash: *blake3::hash(&payload).as_bytes(),
    };
    let mut prefix = [0u8; 256];
    let prefix = codec::encode_file_chunk_prefix_into(&too_large, &mut prefix).expect("prefix");
    let mut trailer = [0u8; 38];
    let trailer = codec::encode_file_chunk_trailer_into(&too_large, &mut trailer).expect("trailer");
    let length = (prefix.len() + payload.len() + trailer.len()) as u32;
    writer
        .write_all(&length.to_be_bytes())
        .await
        .expect("length");
    writer.write_all(prefix).await.expect("header");
    let error = reader
        .read(&mut stream, Duration::from_secs(1), &Cancel::new())
        .await
        .expect_err("oversized payload rejected");
    assert!(matches!(error, Error::Limit("chunk data size")));
    assert_eq!(reader.buffer_capacity(), 0);
}

#[tokio::test]
async fn chunk_reader_accepts_only_bounded_complete_and_cancel_controls() {
    let cancel = Cancel::new();
    let messages = [
        TransferMessage::FileComplete(FileComplete {
            transfer_id: "transfer".into(),
        }),
        TransferMessage::FileCancel(FileCancel {
            transfer_id: "transfer".into(),
        }),
    ];
    let (mut writer, mut stream) = tokio::io::duplex(1024);
    let mut reader = ChunkReader::with_limits(1024, 2048).expect("reader");
    for expected in messages {
        let bytes = codec::encode_transfer(&expected).expect("control encode");
        writer
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .expect("length");
        writer.write_all(&bytes).await.expect("control frame");
        let decoded = reader
            .read(&mut stream, Duration::from_secs(1), &cancel)
            .await
            .expect("bounded control decode");
        match (expected, decoded) {
            (TransferMessage::FileComplete(expected), ChunkMessage::Complete(actual)) => {
                assert_eq!(actual, expected);
            }
            (TransferMessage::FileCancel(expected), ChunkMessage::Cancel(actual)) => {
                assert_eq!(actual, expected);
            }
            _ => panic!("control type changed"),
        }
        assert_eq!(reader.buffer_capacity(), 0);
    }
}

#[tokio::test]
async fn truncated_and_garbage_frames_fail_without_growing_past_the_limit() {
    let good_data = [7u8; 64];
    let good = FileChunkView {
        transfer_id: "transfer",
        file_id: 0,
        chunk_index: 0,
        offset: 0,
        data: &good_data,
        uncompressed_size: good_data.len() as u32,
        compressed: false,
        blake3_hash: *blake3::hash(&good_data).as_bytes(),
    };
    let valid = frame(&good);
    let malformed: Vec<Vec<u8>> = vec![
        vec![0, 0, 0, 1, 0],
        vec![0, 0, 0, 1, 5],
        vec![0, 0, 0, 3, 1, 0x80, 0],
        vec![0, 0, 0, 1, 1],
        valid[..valid.len() - 1].to_vec(),
    ];

    for bytes in malformed
        .into_iter()
        .chain((0..valid.len()).map(|cut| valid[..cut].to_vec()))
    {
        let mut reader = ChunkReader::with_limits(1024, 2048).expect("reader");
        let (mut writer, mut stream) = tokio::io::duplex(4096);
        writer.write_all(&bytes).await.expect("malformed frame");
        drop(writer);
        let result = reader
            .read(&mut stream, Duration::from_secs(1), &Cancel::new())
            .await;
        assert!(result.is_err(), "malformed frame unexpectedly decoded");
        assert!(reader.buffer_capacity() <= 2048);
    }
}

proptest! {
    #[test]
    fn incremental_header_decoder_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let result = std::panic::catch_unwind(|| codec::decode_file_chunk_header(&bytes));
        prop_assert!(result.is_ok());
    }
}
