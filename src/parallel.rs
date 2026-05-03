use crossbeam_channel::{Receiver, Sender};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::thread;

use crate::error::{GappedError, Result};

/// Bytes per chunk sent through a prefetch channel
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Bounded-channel depth per file.
pub const CHUNK_DEPTH: usize = 4;

pub type Chunk = io::Result<Vec<u8>>;

const DEFAULT_WORKERS: usize = 4;
const MAX_WORKERS: usize = 8;

/// Best-effort worker count: available cores, capped to [`MAX_WORKERS`].
pub fn worker_count() -> usize {
    thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(DEFAULT_WORKERS)
        .min(MAX_WORKERS)
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

// ---------------------------------------------------------------------------
// PrefetchPool — parallel file reader that hands out ContentReaders in order
// ---------------------------------------------------------------------------

struct ReadJob {
    path: PathBuf,
    size_tx: Sender<io::Result<u64>>,
    chunk_tx: Sender<Chunk>,
}

struct Pending {
    path: PathBuf,
    size_rx: Receiver<io::Result<u64>>,
    chunk_rx: Receiver<Chunk>,
}

/// Prefetches file content in parallel, handing out `ContentReader`s in
/// queue order. At most `n_workers` files are streamed concurrently,
/// each bounded to `CHUNK_DEPTH` buffered chunks.
pub struct PrefetchPool {
    paths: Vec<PathBuf>,
    in_flight: VecDeque<Pending>,
    next_spawn: usize,
    max_in_flight: usize,
    job_tx: Option<Sender<ReadJob>>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl PrefetchPool {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let n_workers = worker_count();
        let (job_tx, job_rx) = crossbeam_channel::bounded::<ReadJob>(n_workers);

        let handles: Vec<_> = (0..n_workers)
            .map(|_| {
                let job_rx = job_rx.clone();
                thread::spawn(move || {
                    for job in job_rx.iter() {
                        stream_file(job);
                    }
                })
            })
            .collect();
        drop(job_rx);

        let mut pool = Self {
            paths,
            in_flight: VecDeque::new(),
            next_spawn: 0,
            max_in_flight: n_workers,
            job_tx: Some(job_tx),
            handles,
        };
        pool.top_up();
        pool
    }

    fn top_up(&mut self) {
        let Some(tx) = self.job_tx.as_ref() else {
            return;
        };
        while self.in_flight.len() < self.max_in_flight && self.next_spawn < self.paths.len() {
            let path = self.paths[self.next_spawn].clone();
            self.next_spawn += 1;
            let (size_tx, size_rx) = crossbeam_channel::bounded(1);
            let (chunk_tx, chunk_rx) = chunk_channel();
            if tx
                .send(ReadJob {
                    path: path.clone(),
                    size_tx,
                    chunk_tx,
                })
                .is_err()
            {
                break;
            }
            self.in_flight.push_back(Pending {
                path,
                size_rx,
                chunk_rx,
            });
        }
    }

    /// Pop the next file in queue order, blocking until its size is known.
    /// Returns `Ok(None)` once every queued path has been handed out.
    pub fn next(&mut self) -> Result<Option<ContentReader>> {
        let Some(pending) = self.in_flight.pop_front() else {
            return Ok(None);
        };
        self.top_up();
        let size = pending.size_rx.recv().map_err(|_| {
            GappedError::WorkerPoolFailure("prefetch worker exited before reporting size")
        })?;
        let size = size.map_err(|source| GappedError::IoPath {
            path: pending.path,
            source,
        })?;
        Ok(Some(ContentReader::new(pending.chunk_rx, size)))
    }

    /// Close job channel and join all worker threads.
    pub fn finish(mut self) -> Result<()> {
        self.job_tx.take();
        for handle in self.handles.drain(..) {
            join_worker(handle, "prefetch worker thread panicked")?;
        }
        Ok(())
    }
}

fn stream_file(job: ReadJob) {
    let ReadJob {
        path,
        size_tx,
        chunk_tx,
    } = job;

    let opened = File::open(&path).and_then(|f| {
        let size = f.metadata()?.len();
        Ok((f, size))
    });

    let (file, size) = match opened {
        Ok(v) => {
            if size_tx.send(Ok(v.1)).is_err() {
                return;
            }
            v
        }
        Err(e) => {
            let _ = size_tx.send(Err(e));
            return;
        }
    };

    let mut reader = BufReader::with_capacity(CHUNK_SIZE, file);
    let mut remaining = size;
    while remaining > 0 {
        let want = CHUNK_SIZE.min(remaining as usize);
        let mut buf = vec![0u8; want];
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = chunk_tx.send(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file truncated while streaming",
                )));
                return;
            }
            Ok(n) => {
                buf.truncate(n);
                if chunk_tx.send(Ok(buf)).is_err() {
                    return;
                }
                remaining -= n as u64;
            }
            Err(e) => {
                let _ = chunk_tx.send(Err(e));
                return;
            }
        }
    }
}
