use std::io::Read;
use std::path::Path;

/// Compute BLAKE3 hash of a file by streaming the content.
pub fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];

    loop {
        let chunk_size = file.read(&mut buf)?;
        if chunk_size == 0 {
            break;
        }
        hasher.update(&buf[..chunk_size]);
    }

    Ok(*hasher.finalize().as_bytes())
}
