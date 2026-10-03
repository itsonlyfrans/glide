//! Bounded stdio JSONL server. Stdout contains protocol JSON only.

use crate::Core;
use glide_proto::ipc::{ErrorCode, Event, IpcError, Notification, Response};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub use glide_proto::codec::MAX_IPC_LINE_BYTES;

/// Read one UTF-8 protocol frame, checking the cap before extending the buffer.
/// Oversized input terminates the session rather than draining an unbounded attacker stream.
pub async fn read_line<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<u8>>> {
    read_line_limited(reader, MAX_IPC_LINE_BYTES).await
}

pub async fn read_line_limited<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = SecretBuffer(Vec::new());
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if line.0.is_empty() {
                Ok(None)
            } else {
                Ok(Some(std::mem::take(&mut line.0)))
            };
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(chunk.len(), |index| index + 1);
        if line.0.len() + count > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IPC frame too large",
            ));
        }
        line.0.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(std::mem::take(&mut line.0)));
        }
    }
}

async fn write_json<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    let bytes = SecretBuffer(glide_proto::codec::encode_jsonl(value)?);
    writer.write_all(&bytes.0).await?;
    writer.flush().await?;
    Ok(())
}

struct SecretBuffer(Vec<u8>);
enum NativeEvent {
    Input(glide_platform::InputEvent),
    Clipboard(glide_platform::ClipboardEvent),
}
impl Drop for SecretBuffer {
    fn drop(&mut self) {
        glide_proto::ipc::wipe_bytes(&mut self.0);
    }
}

/// Serve until EOF or shutdown; release input and cancel pairing on every exit path.
pub async fn serve<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: R,
    writer: W,
    core: Core,
) -> anyhow::Result<()> {
    serve_with_mode(reader, writer, core, false).await
}

/// Report initialization failure without claiming ready or exposing backend diagnostics.
pub async fn startup_failure<W: AsyncWrite + Unpin>(
    writer: &mut W,
    error: IpcError,
) -> anyhow::Result<()> {
    write_json(
        writer,
        &Event::Notification(Notification {
            level: "error".into(),
            title: "Glide could not start".into(),
            body: error.message.clone(),
            action: None,
        }),
    )
    .await?;
    write_json(writer, &Response::failure(0, error)).await
}

pub async fn serve_with_mode<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: R,
    mut writer: W,
    mut core: Core,
    headless: bool,
) -> anyhow::Result<()> {
    let result = async {
        write_json(&mut writer, &Event::Ready { version: env!("CARGO_PKG_VERSION").into() }).await?;
        write_json(&mut writer, &Event::State(Box::new(core.snapshot()))).await?;
        for event in core.take_events() { write_json(&mut writer, &event).await?; }
        let mut reader = BufReader::new(reader);
        let mut stdin_closed = false;
        let mut mouse_failure_count = 0u64;
        let termination = shutdown_signal();
        tokio::pin!(termination);
        let capture = core.capture_receiver();
        let clipboard = core.clipboard_receiver();
        let clipboard_ready = core.clipboard_ready();
        let (capture_tx, mut capture_rx) = tokio::sync::mpsc::channel(1024);
        tokio::task::spawn_blocking(move || {
            while !capture_tx.is_closed() {
                let event = crossbeam_channel::select! {
                    recv(capture) -> event => match event { Ok(event) => NativeEvent::Input(event), Err(_) => break },
                    recv(clipboard) -> event => match event { Ok(event) => NativeEvent::Clipboard(event), Err(_) => break },
                    default(std::time::Duration::from_millis(100)) => continue,
                };
                if capture_tx.blocking_send(event).is_err() { break; }
            }
        });
        let link = core.link();
        let mut pulse = tokio::time::interval(std::time::Duration::from_millis(100));
        pulse.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // Keep the read future alive across pulses: canceling it would discard a partial line.
            let line = {
                let pending = read_line(&mut reader);
                tokio::pin!(pending);
                let mut peer_event = link.recv_event();
                loop {
                    let mouse_ready = link.mouse_ready().notified();
                    tokio::pin!(mouse_ready);
                    mouse_ready.as_mut().enable();
                    tokio::select! {
                        line = &mut pending, if !stdin_closed => {
                            let line = line?;
                            if line.is_none() && headless { stdin_closed = true; }
                            else { break line; }
                        }
                        signal = &mut termination => { signal?; break None; }
                        Some(event) = capture_rx.recv() => {
                            match event {
                                NativeEvent::Input(event) => { if let Err(error) = core.capture_input(event).await { core.notify_failure(&error); tracing::debug!(code = ?error.code, "capture operation failed"); } }
                                NativeEvent::Clipboard(event) => { core.clipboard_changed(event); core.poll_clipboard().await; }
                            }
                            for event in core.take_events() { write_json(&mut writer, &event).await?; }
                            if core.shutdown_requested() { break None; }
                        }
                        _ = clipboard_ready.notified() => {
                            core.poll_clipboard().await;
                            for event in core.take_events() { write_json(&mut writer, &event).await?; }
                        }
                        _ = &mut mouse_ready => {
                            // At most one slot per peer; a producer refills/notifies.
                            // Bound each drain turn so reliable input/control stays fair.
                            for _ in 0..32 {
                                match link.try_recv_move() {
                                    Ok(Some(movement)) => { if let Err(error) = core.receive_move(movement).await {
                                        mouse_failure_count = mouse_failure_count.saturating_add(1);
                                        if mouse_failure_count.is_power_of_two() { tracing::debug!(failures = mouse_failure_count, code = ?error.code, "mouse injection failed"); }
                                    } }
                                    Ok(None) => break,
                                    Err(glide_net::LinkError::Busy) => {
                                        link.mouse_ready().notify_one();
                                        tokio::task::yield_now().await;
                                        break;
                                    }
                                    Err(_) => { core.shutdown().await?; break; }
                                }
                            }
                        }
                        event = &mut peer_event => {
                            match event {
                                Ok(event) => { if let Err(error) = core.receive_link(event).await { tracing::debug!(code = ?error.code, "peer operation failed"); } }
                                Err(_) => { core.shutdown().await?; break None; }
                            }
                            for event in core.take_events() { write_json(&mut writer, &event).await?; }
                            if core.shutdown_requested() { break None; }
                            peer_event = link.recv_event();
                        }
                        _ = pulse.tick() => {
                            if let Err(failure) = core.tick().await {
                                core.notify_failure(&IpcError { code: ErrorCode::PermissionDenied,
                                    message: "Glide stopped sharing because input, displays or protected storage became unavailable. Check OS permissions and restart Glide.".into() });
                                for event in core.take_events() { write_json(&mut writer, &event).await?; }
                                return Err(failure);
                            }
                            for response in core.take_responses() { write_json(&mut writer,&response).await?; }
                            for event in core.take_events() { write_json(&mut writer, &event).await?; }
                            if core.shutdown_requested() { break None; }
                        }
                    }
                }
            };
                    let Some(line) = line else { break; };
                    let line = SecretBuffer(line);
                    let response = match glide_proto::codec::decode_jsonl_request(&line.0) {
                        Ok(request) => core.handle(request).await,
                        Err(_) => Some(Response::failure(0, IpcError { code: ErrorCode::InvalidParams, message: "invalid request JSON".into() })),
                    };
                    if let Some(response)=response { write_json(&mut writer, &response).await?; }
                    for response in core.take_responses() { write_json(&mut writer,&response).await?; }
                    for event in core.take_events() { write_json(&mut writer, &event).await?; }
                    if core.shutdown_requested() { break; }
        }
        Ok(())
    }.await;
    core.shutdown().await?;
    result
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(windows)]
    {
        let mut close = tokio::signal::windows::ctrl_close()?;
        let mut shutdown = tokio::signal::windows::ctrl_shutdown()?;
        let mut logoff = tokio::signal::windows::ctrl_logoff()?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = close.recv() => Ok(()),
            _ = shutdown.recv() => Ok(()),
            _ = logoff.recv() => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn input_is_bounded_before_allocation_and_last_line_is_accepted() {
        let mut reader = BufReader::new(&b"{}\n{}"[..]);
        assert_eq!(
            read_line(&mut reader).await.expect("line"),
            Some(b"{}\n".to_vec())
        );
        assert_eq!(
            read_line(&mut reader).await.expect("line"),
            Some(b"{}".to_vec())
        );
        assert_eq!(read_line(&mut reader).await.expect("EOF"), None);
        let oversized = vec![b'x'; MAX_IPC_LINE_BYTES + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        assert_eq!(
            read_line(&mut reader).await.expect_err("limit").kind(),
            std::io::ErrorKind::InvalidData
        );
    }
}
