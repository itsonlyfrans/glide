//! Dedicated, authenticated transfer streams. No file data enters LinkEvent.
use super::{
    link::{Inner, Session},
    mutex, NativeLink, IO_TIMEOUT,
};
use crate::{Link, LinkError, PeerToken};
use quinn::{Connection, RecvStream, SendStream};
use std::{
    collections::{HashMap, HashSet},
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    task::{Context, Poll, Waker},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const MAGIC: &[u8; 6] = b"GLDX\0\x01";
const MAX_JOBS: usize = 4;

/// One bidi control stream and 1..=4 distinct bidi chunk lanes. Chunk callers
/// use the write halves on send and read halves on receive. Keep `handle` for
/// revocation/cancellation and close the connection on a codec failure.
pub struct TransferStreams {
    pub peer_id: String,
    pub peer_token: PeerToken,
    pub transfer_id: String,
    pub control: TransferIo,
    pub chunks: Vec<TransferIo>,
    pub handle: TransferHandle,
}

/// Owns both QUIC halves; implements Tokio AsyncRead/AsyncWrite for FileEngine.
pub struct TransferIo {
    pub(super) read: RecvStream,
    pub(super) write: SendStream,
    lease: Arc<Lease>,
    read_wake: Arc<Mutex<Option<Waker>>>,
    write_wake: Arc<Mutex<Option<Waker>>>,
    yield_write: bool,
}

/// Clonable job lifetime/cancellation handle; no identity or codec bypass.
#[derive(Clone)]
pub struct TransferHandle(Arc<Lease>);

struct Lease {
    id: String,
    ids: Arc<Mutex<HashSet<String>>>,
    conn: Connection,
    cancelled: AtomicBool,
    wakes: Mutex<Vec<Arc<Mutex<Option<Waker>>>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut ids) = self.ids.lock() {
            ids.remove(&self.id);
        }
    }
}

impl TransferHandle {
    /// Wakes blocked I/O. Every half resets/stops on its next poll or drop.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        if let Ok(wakes) = self.0.wakes.lock() {
            for wake in wakes.iter() {
                if let Ok(mut wake) = wake.lock() {
                    if let Some(waker) = wake.take() {
                        waker.wake();
                    }
                }
            }
        }
    }
    pub fn close_on_codec_failure(&self) {
        self.0.conn.close(1u32.into(), b"invalid transfer codec");
    }
    pub fn is_closed(&self) -> bool {
        self.0.conn.close_reason().is_some()
    }
}

impl TransferIo {
    /// Independent cancellation-aware read/write halves for control codecs.
    pub fn split(self) -> (tokio::io::ReadHalf<Self>, tokio::io::WriteHalf<Self>) {
        tokio::io::split(self)
    }
    pub fn read_id(&self) -> quinn::StreamId {
        self.read.id()
    }
    pub fn write_id(&self) -> quinn::StreamId {
        self.write.id()
    }
    fn new(read: RecvStream, write: SendStream, lease: &Arc<Lease>) -> Result<Self, LinkError> {
        let read_wake = Arc::new(Mutex::new(None));
        let write_wake = Arc::new(Mutex::new(None));
        mutex(&lease.wakes)?.extend([read_wake.clone(), write_wake.clone()]);
        Ok(Self {
            read,
            write,
            lease: lease.clone(),
            read_wake,
            write_wake,
            yield_write: false,
        })
    }
    fn check(&mut self, cx: &Context<'_>, write: bool) -> io::Result<()> {
        *(if write {
            &self.write_wake
        } else {
            &self.read_wake
        })
        .lock()
        .map_err(|_| io::Error::other("transfer state poisoned"))? = Some(cx.waker().clone());
        if self.lease.cancelled.load(Ordering::Acquire) {
            self.abort();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "transfer cancelled",
            ));
        }
        Ok(())
    }
    fn abort(&mut self) {
        let _ = self.read.stop(0u32.into());
        let _ = self.write.reset(0u32.into());
    }
}

impl Drop for TransferIo {
    fn drop(&mut self) {
        if self.lease.cancelled.load(Ordering::Acquire) {
            self.abort();
        } else {
            let _ = self.read.stop(0u32.into());
        }
        // SendStream's drop finishes queued bytes; resetting successful chunk
        // lanes here would discard data before their receiver consumed it.
    }
}
impl AsyncRead for TransferIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Err(error) = self.check(cx, false) {
            return Poll::Ready(Err(error));
        }
        AsyncRead::poll_read(Pin::new(&mut self.read), cx, buf)
    }
}
impl AsyncWrite for TransferIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Err(error) = self.check(cx, true) {
            return Poll::Ready(Err(error));
        }
        // Stream priority does not preempt a file task copying megabytes into
        // Quinn in one poll. Bound each turn and yield between bulk writes.
        if self.yield_write {
            self.yield_write = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        let result = AsyncWrite::poll_write(
            Pin::new(&mut self.write),
            cx,
            &buf[..buf.len().min(8 * 1024)],
        );
        if matches!(result, Poll::Ready(Ok(count)) if count > 0) {
            self.yield_write = true;
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.check(cx, true) {
            return Poll::Ready(Err(error));
        }
        AsyncWrite::poll_flush(Pin::new(&mut self.write), cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.check(cx, true) {
            return Poll::Ready(Err(error));
        }
        AsyncWrite::poll_shutdown(Pin::new(&mut self.write), cx)
    }
}

fn reserve(session: &Session, id: &str) -> Result<Arc<Lease>, LinkError> {
    let mut ids = mutex(&session.transfers)?;
    if ids.len() >= MAX_JOBS || !ids.insert(id.to_owned()) {
        return Err(LinkError::QueueFull);
    }
    Ok(Arc::new(Lease {
        id: id.to_owned(),
        ids: session.transfers.clone(),
        conn: session.conn.clone(),
        cancelled: AtomicBool::new(false),
        wakes: Mutex::new(Vec::with_capacity(10)),
    }))
}

fn validate(id: &str, lanes: u8, lane: u8) -> Result<(), LinkError> {
    if id.is_empty()
        || id.len() > 128
        || id.chars().any(char::is_control)
        || !(1..=4).contains(&lanes)
        || lane > lanes
    {
        return Err(LinkError::Internal("invalid transfer preamble".into()));
    }
    Ok(())
}

async fn preamble(send: &mut SendStream, id: &str, lanes: u8, lane: u8) -> Result<(), LinkError> {
    validate(id, lanes, lane)?;
    send.set_priority(-10).map_err(|_| LinkError::Closed)?;
    send.write_all(MAGIC).await.map_err(|_| LinkError::Closed)?;
    send.write_all(&[lanes, lane, id.len() as u8])
        .await
        .map_err(|_| LinkError::Closed)?;
    send.write_all(id.as_bytes())
        .await
        .map_err(|_| LinkError::Closed)
}

async fn read_preamble(recv: &mut RecvStream) -> Result<(String, u8, u8), LinkError> {
    let mut header = [0; 9];
    recv.read_exact(&mut header)
        .await
        .map_err(|_| LinkError::Closed)?;
    if &header[..6] != MAGIC || header[8] == 0 || header[8] > 128 {
        return Err(LinkError::Closed);
    }
    let mut id = vec![0; header[8] as usize];
    recv.read_exact(&mut id)
        .await
        .map_err(|_| LinkError::Closed)?;
    let id = String::from_utf8(id).map_err(|_| LinkError::Closed)?;
    validate(&id, header[6], header[7])?;
    Ok((id, header[6], header[7]))
}

impl NativeLink {
    pub async fn open_transfer(
        &self,
        peer_id: &str,
        transfer_id: &str,
        lanes: u8,
    ) -> Result<TransferStreams, LinkError> {
        validate(transfer_id, lanes, 0)?;
        let session = self.session(peer_id)?;
        let lease = reserve(&session, transfer_id)?;
        let mut guard = super::link::HandshakeGuard(Some(session.conn.clone()));
        let result = tokio::time::timeout(IO_TIMEOUT, async {
            let (mut write, mut read) = session
                .conn
                .open_bi()
                .await
                .map_err(|_| LinkError::Closed)?;
            preamble(&mut write, transfer_id, lanes, 0).await?;
            let mut ack = [0];
            read.read_exact(&mut ack)
                .await
                .map_err(|_| LinkError::Closed)?;
            if ack != *b"R" {
                return Err(LinkError::Closed);
            }
            let mut chunks = Vec::with_capacity(lanes as usize);
            for lane in 1..=lanes {
                let (mut write, read) = session
                    .conn
                    .open_bi()
                    .await
                    .map_err(|_| LinkError::Closed)?;
                preamble(&mut write, transfer_id, lanes, lane).await?;
                chunks.push(TransferIo::new(read, write, &lease)?);
            }
            read.read_exact(&mut ack)
                .await
                .map_err(|_| LinkError::Closed)?;
            if ack != *b"A"
                || self.peer_token(peer_id)? != *session.token.get().ok_or(LinkError::Closed)?
            {
                return Err(LinkError::Closed);
            }
            Ok(TransferStreams {
                peer_id: peer_id.to_owned(),
                peer_token: *session.token.get().ok_or(LinkError::Closed)?,
                transfer_id: transfer_id.to_owned(),
                control: TransferIo::new(read, write, &lease)?,
                chunks,
                handle: TransferHandle(lease.clone()),
            })
        })
        .await;
        match result {
            Ok(Ok(streams)) => {
                guard.0 = None;
                Ok(streams)
            }
            _ => {
                session
                    .conn
                    .close(1u32.into(), b"transfer handshake failed");
                Err(LinkError::Closed)
            }
        }
    }

    /// Cancellation-safe queue receive; rejects retired tokens and revoked pins.
    pub async fn accept_transfer(&self) -> Result<TransferStreams, LinkError> {
        let mut receiver = self.0.transfer_rx.lock().await;
        loop {
            let streams = receiver.recv().await.ok_or(LinkError::Closed)?;
            if self.peer_token(&streams.peer_id).ok() == Some(streams.peer_token) {
                return Ok(streams);
            }
        }
    }
}

struct Pending {
    control: TransferIo,
    chunks: Vec<Option<TransferIo>>,
    lease: Arc<Lease>,
    deadline: tokio::time::Instant,
}

pub(super) fn spawn_accept(inner: Weak<Inner>, session: Arc<Session>) {
    tokio::spawn(async move {
        let result = accept_jobs(inner, &session).await;
        if result.is_err() {
            session.conn.close(1u32.into(), b"invalid transfer streams");
        }
    });
}

async fn accept_jobs(inner: Weak<Inner>, session: &Session) -> Result<(), LinkError> {
    let mut pending = HashMap::<String, Pending>::new();
    loop {
        let deadline = pending
            .values()
            .map(|job| job.deadline)
            .min()
            .unwrap_or_else(|| tokio::time::Instant::now() + IO_TIMEOUT);
        let (mut write, mut read) = tokio::select! {
            _ = tokio::time::sleep_until(deadline), if !pending.is_empty() => return Err(LinkError::Closed),
            streams = session.conn.accept_bi() => streams.map_err(|_| LinkError::Closed)?,
        };
        let (id, lanes, lane) = tokio::time::timeout_at(deadline, read_preamble(&mut read))
            .await
            .map_err(|_| LinkError::Closed)??;
        if lane == 0 {
            let lease = reserve(session, &id)?;
            write.set_priority(-10).map_err(|_| LinkError::Closed)?;
            super::link::timed(write.write_all(b"R")).await?;
            pending.insert(
                id,
                Pending {
                    control: TransferIo::new(read, write, &lease)?,
                    chunks: (0..lanes).map(|_| None).collect(),
                    lease,
                    deadline: tokio::time::Instant::now() + IO_TIMEOUT,
                },
            );
            continue;
        }
        let job = pending.get_mut(&id).ok_or(LinkError::Closed)?;
        if job.chunks.len() != lanes as usize || job.chunks[lane as usize - 1].is_some() {
            return Err(LinkError::Closed);
        }
        write.set_priority(-10).map_err(|_| LinkError::Closed)?;
        job.chunks[lane as usize - 1] = Some(TransferIo::new(read, write, &job.lease)?);
        if job.chunks.iter().all(Option::is_some) {
            let mut job = pending.remove(&id).ok_or(LinkError::Closed)?;
            super::link::timed(job.control.write.write_all(b"A")).await?;
            let inner = inner.upgrade().ok_or(LinkError::Closed)?;
            inner
                .transfer_tx
                .try_send(TransferStreams {
                    peer_id: session.peer.device_id.clone(),
                    peer_token: *session.token.get().ok_or(LinkError::Closed)?,
                    transfer_id: id,
                    control: job.control,
                    chunks: job
                        .chunks
                        .into_iter()
                        .collect::<Option<Vec<_>>>()
                        .ok_or(LinkError::Closed)?,
                    handle: TransferHandle(job.lease),
                })
                .map_err(|_| LinkError::QueueFull)?;
        }
    }
}
