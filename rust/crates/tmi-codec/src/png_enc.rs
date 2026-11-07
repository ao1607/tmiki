//! Parallel PNG encode (P3-c) via mtpng.
//!
//! The image is split into fixed-size deflate chunks that are filtered and
//! compressed on [`pool`], so the output depends on the chunk size and the
//! pixels only — never on the number of threads.

use mtpng::encoder::{Encoder, Options};
use mtpng::{ColorType, CompressionLevel, Header};
use std::sync::OnceLock;

const CHUNK_SIZE: usize = 256 * 1024;

/// The pool mtpng's filter and deflate jobs run on. mtpng queues them and
/// blocks the calling thread until they are done; on the global pool (its
/// default) an encode called from that pool's own workers deadlocks once
/// every worker is waiting in one, e.g. the composite and the thumbnail PNG
/// side by side on a two-thread pool. The jobs here never wait.
pub fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("tmi-png-{i}"))
            .build()
            .expect("start the PNG encode threads")
    })
}

pub fn encode(rgba: &[u8], width: u32, height: u32, text: Option<&(String, String)>) -> std::io::Result<Vec<u8>> {
    let mut header = Header::new();
    header.set_size(width, height)?;
    header.set_color(ColorType::TruecolorAlpha, 8)?;

    let mut options = Options::new();
    options.set_compression_level(CompressionLevel::Fast)?;
    options.set_chunk_size(CHUNK_SIZE)?;
    options.set_thread_pool(pool())?;

    let mut encoder = Encoder::new(Vec::with_capacity(rgba.len() / 2), &options);
    encoder.write_header(&header)?;
    if let Some((key, value)) = text {
        // tEXt goes before IDAT so readers see it in the header pass.
        let mut data = Vec::with_capacity(key.len() + 1 + value.len());
        data.extend_from_slice(key.as_bytes());
        data.push(0);
        data.extend_from_slice(value.as_bytes());
        encoder.write_chunk(b"tEXt", &data)?;
    }
    encoder.write_image_rows(rgba)?;
    encoder.finish()
}

/// Why `encode` must not be called with these arguments, if anything.
pub fn check_args(rgba: &[u8], width: u32, height: u32, text: Option<&(String, String)>) -> Result<(), String> {
    if let Some((key, value)) = text {
        let latin1 = |s: &str| s.chars().all(|c| (c as u32) < 256 && c != '\0');
        if key.is_empty() || key.len() > 79 || !latin1(key) || !latin1(value) {
            return Err("tEXt key/value must be Latin-1 without NUL (key 1-79 chars)".into());
        }
    }
    if width == 0 || height == 0 || rgba.len() != width as usize * height as usize * 4 {
        return Err(format!("rgba buffer is {} bytes, expected {}x{}x4", rgba.len(), width, height));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Encodes side by side on the workers of a one-thread pool: mtpng's
    /// jobs must not wait for those workers.
    #[test]
    fn encodes_on_the_workers_of_a_busy_pool() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
            let rgba: Vec<u8> = (0..300 * 400 * 4).map(|i| (i * 7 % 251) as u8).collect();
            let (a, b) = pool.install(|| rayon::join(|| encode(&rgba, 300, 400, None), || encode(&rgba, 300, 400, None)));
            tx.send(a.is_ok() && b.is_ok() && a.unwrap() == b.unwrap()).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(60)), Ok(true), "PNG encode hung on a busy pool");
    }
}
