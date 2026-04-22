use crossbeam_channel::{Receiver, Sender};
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::error::{GappedError, Result};

pub const CHUNK_SIZE: usize = 256 * 1024;
pub const CHUNK_DEPTH: usize = 4;

pub type Chunk = io::Result<Vec<u8>>;

/// Best-effort worker count: available cores, capped to 8.
pub fn worker_count() -> usize {
    thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(4)
        .min(8)
}

/// Create a bounded chunk channel with pipelines standard detpth
pub fn chunk_channel() -> (Sender<Chunk>, Receiver<Chunk>) {
    crossbeam_channel::bounded(CHUNK_DEPTH)
}

/// Map a panicked worker thread into own error type.
pub fn join_worker<T>(handle: thread::JoinHandle<T>, kind: &'static str) -> Result<T> {
    handle
        .join()
        .map_err(|_| GappedError::WorkerPoolFailure(kind))
}

/// A `Read` over a bounded channel of byte chunks.
///
/// Caps per stream memory to `(CHUNK_DEPTH + 1) * CHUNK_SIZE` regardless of
/// the underlying file size. Producer blocks once the channel is full.
pub struct ContentReader {
    chunk_rx: Receiver<Chunk>,
    current: Vec<u8>,
    pos: usize,
    remaining: u64,
}

impl ContentReader {
    pub fn new(chunk_rx: Receiver<Chunk>, size: u64) -> Self {
        Self {
            chunk_rx,
            current: Vec::new(),
            pos: 0,
            remaining: size,
        }
    }

    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl Read for ContentReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.current.len() {
            if self.remaining == 0 {
                return Ok(0);
            }
            match self.chunk_rx.recv() {
                Ok(Ok(chunk)) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "producer closed channel before delivering full payload",
                    ));
                }
            }
        }
        let n = (self.current.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        self.remaining = self.remaining.saturating_sub(n as u64);
        Ok(n)
    }
}

/// A byte-sized semaphore bounding total in-flight payload memory.
///
/// Unlike a file-count semaphore, this caps memory directly: a caller about
/// to buffer an N-byte payload calls `acquire(N)`, which blocks until N
/// permits are free, then deducts them. The worker that eventually consumes
/// the payload calls `release(N)`, returning
/// the permits to the pool.
///
/// If a single requested `bytes` exceeds `capacity`, `acquire` holds to
/// `capacity` rather than deadlocking — the oversized payload becomes
/// exclusive in-flight for its duration.
pub struct ByteBudget {
    inner: Mutex<u64>,
    cv: Condvar,
    capacity: u64,
}

impl ByteBudget {
    pub fn new(capacity: u64) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            inner: Mutex::new(capacity),
            cv: Condvar::new(),
            capacity,
        })
    }

    pub fn acquire(&self, bytes: u64) -> u64 {
        let permits = bytes.min(self.capacity).max(1);
        let mut avail = self.inner.lock().unwrap();
        while *avail < permits {
            avail = self.cv.wait(avail).unwrap();
        }
        *avail -= permits;
        permits
    }

    pub fn release(&self, permits: u64) {
        if permits == 0 {
            return;
        }
        let mut avail = self.inner.lock().unwrap();
        *avail = (*avail + permits).min(self.capacity);
        self.cv.notify_all();
    }
}
