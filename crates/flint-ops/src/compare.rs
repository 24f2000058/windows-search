//! Bounded, cancellable byte-for-byte file comparison.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Comparison {
    Identical,
    SizeDiffers { a: u64, b: u64 },
    /// First differing byte offset.
    DiffersAt(u64),
    /// Stopped by the byte budget or cancellation after `compared` equal bytes.
    Incomplete { compared: u64 },
}

pub const CHUNK: usize = 64 * 1024;

/// Compares two streams of known sizes, reading at most `budget` bytes from
/// each.
pub fn compare_streams(mut a: impl Read, size_a: u64, mut b: impl Read, size_b: u64, budget: u64, cancel: &AtomicBool) -> std::io::Result<Comparison> {
    if size_a != size_b {
        return Ok(Comparison::SizeDiffers { a: size_a, b: size_b });
    }
    let (mut ba, mut bb) = (vec![0u8; CHUNK], vec![0u8; CHUNK]);
    let mut off = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) || off >= budget && off < size_a {
            return Ok(Comparison::Incomplete { compared: off });
        }
        let want = CHUNK.min((budget - off.min(budget)) as usize).max(1);
        let na = read_full(&mut a, &mut ba[..want])?;
        let nb = read_full(&mut b, &mut bb[..want])?;
        if na != nb {
            // One file changed size while being read.
            return Ok(Comparison::DiffersAt(off + na.min(nb) as u64));
        }
        if na == 0 {
            return Ok(Comparison::Identical);
        }
        if let Some(i) = ba[..na].iter().zip(&bb[..nb]).position(|(x, y)| x != y) {
            return Ok(Comparison::DiffersAt(off + i as u64));
        }
        off += na as u64;
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Compares two files on disk.
pub fn compare_files(a: &std::path::Path, b: &std::path::Path, budget: u64, cancel: &AtomicBool) -> std::io::Result<Comparison> {
    let (fa, fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    let (sa, sb) = (fa.metadata()?.len(), fb.metadata()?.len());
    compare_streams(std::io::BufReader::new(fa), sa, std::io::BufReader::new(fb), sb, budget, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmp(a: &[u8], b: &[u8], budget: u64) -> Comparison {
        compare_streams(a, a.len() as u64, b, b.len() as u64, budget, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn identical_different_and_bounded() {
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(cmp(&big, &big, u64::MAX), Comparison::Identical);
        assert_eq!(cmp(b"", b"", 0), Comparison::Identical);
        let mut other = big.clone();
        other[200_001] ^= 1;
        assert_eq!(cmp(&big, &other, u64::MAX), Comparison::DiffersAt(200_001));
        assert_eq!(cmp(b"abc", b"abcd", u64::MAX), Comparison::SizeDiffers { a: 3, b: 4 });
        assert_eq!(cmp(&big, &other, 100_000), Comparison::Incomplete { compared: 100_000 }, "budget stops before the difference");
        let c = AtomicBool::new(true);
        assert_eq!(compare_streams(&big[..], big.len() as u64, &big[..], big.len() as u64, u64::MAX, &c).unwrap(), Comparison::Incomplete { compared: 0 });
        // Declared sizes match but one stream is shorter (file truncated
        // while reading).
        assert_eq!(compare_streams(&b"abcd"[..], 4, &b"ab"[..], 4, u64::MAX, &AtomicBool::new(false)).unwrap(), Comparison::DiffersAt(2));
    }

    #[test]
    fn compares_real_files() {
        let d = std::env::temp_dir().join(format!("flint-cmp-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a"), b"same").unwrap();
        std::fs::write(d.join("b"), b"same").unwrap();
        std::fs::write(d.join("c"), b"diff").unwrap();
        let no = AtomicBool::new(false);
        assert_eq!(compare_files(&d.join("a"), &d.join("b"), u64::MAX, &no).unwrap(), Comparison::Identical);
        assert_eq!(compare_files(&d.join("a"), &d.join("c"), u64::MAX, &no).unwrap(), Comparison::DiffersAt(0));
        assert!(compare_files(&d.join("a"), &d.join("missing"), u64::MAX, &no).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
