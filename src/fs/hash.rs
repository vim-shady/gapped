use std::io::Read;
use std::path::Path;
use xxhash_rust::xxh3::Xxh3;

const STREAM_BUF: usize = 64 * 1024;

/// Compute XXH3-128 hash of a file by streaming the content.
pub fn hash_file(path: &Path) -> std::io::Result<[u8; 16]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = [0u8; STREAM_BUF];

    loop {
        let chunk_size = file.read(&mut buf)?;
        if chunk_size == 0 {
            break;
        }
        hasher.update(&buf[..chunk_size]);
    }

    Ok(hasher.digest128().to_le_bytes())
}
