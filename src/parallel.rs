use crossbeam_channel::{Receiver, Sender};
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::thread;

use crate::error::{GappedError, Result};

pub const CHUNK_SIZE: usize = 256 * 1024;
pub const CHUNK_DEPTH: usize = 4;

pub type Chunk = io::Result<Vec<u8>>;

/// Best-effort worker count: available cores, capped to 8. Used by the diff
/// prefetch pool and the walk hasher.
pub fn worker_count() -> usize {
    thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(4)
        .min(8)
}

/// Create a bounded chunk channel with pipelines standard depth.
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
