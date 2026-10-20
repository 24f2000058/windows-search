//! Minimal length-prefixed binary encoding for the host protocol.
//!
//! A frame is a little-endian `u32` byte length followed by the payload.
//! Payload fields are fixed-width little-endian integers and
//! length-prefixed UTF-8 strings / byte strings. The reader never trusts a
//! length: every read is bounds-checked against the frame, and frames above
//! [`MAX_FRAME`] are rejected before allocation, so a corrupt or hostile
//! peer cannot make the other side allocate or read out of bounds.

use std::io::{self, Read, Write};

/// Largest frame either side accepts (a thumbnail of 256×256 BGRA is 256 KiB;
/// text previews are capped well below this).
pub const MAX_FRAME: usize = 8 << 20;

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
        self
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub &'static str);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "malformed message: {}", self.0)
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or(WireError("truncated"))?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn bool(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError("bad bool")),
        }
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn str(&mut self) -> Result<String, WireError> {
        std::str::from_utf8(self.bytes()?).map(str::to_owned).map_err(|_| WireError("bad utf-8"))
    }
    /// A count of items, sanity-checked against the bytes left (each item
    /// takes at least `min_item` bytes), so a huge count cannot drive a huge
    /// allocation.
    pub fn count(&mut self, min_item: usize) -> Result<usize, WireError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_item.max(1)) > self.data.len() - self.pos {
            return Err(WireError("count exceeds message"));
        }
        Ok(n)
    }
    pub fn finish(&self) -> Result<(), WireError> {
        if self.pos == self.data.len() { Ok(()) } else { Err(WireError("trailing bytes")) }
    }
}

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` on clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_hostile_lengths() {
        let mut w = Writer::default();
        w.u8(7).u32(1).u64(u64::MAX).i64(-5).bool(true).str("héllo").bytes(&[1, 2, 3]);
        let mut r = Reader::new(&w.buf);
        assert_eq!((r.u8(), r.u32(), r.u64(), r.i64(), r.bool()), (Ok(7), Ok(1), Ok(u64::MAX), Ok(-5), Ok(true)));
        assert_eq!(r.str().unwrap(), "héllo");
        assert_eq!(r.bytes().unwrap(), &[1, 2, 3]);
        assert!(r.finish().is_ok());
        // A length pointing past the end is an error, not a panic.
        let mut bad = Writer::default();
        bad.u32(1_000_000);
        assert_eq!(Reader::new(&bad.buf).bytes(), Err(WireError("truncated")));
        assert_eq!(Reader::new(&bad.buf).count(8), Err(WireError("count exceeds message")));
        // Frames: oversized lengths are refused before allocating.
        let mut s = Vec::new();
        write_frame(&mut s, b"abc").unwrap();
        assert_eq!(read_frame(&mut &s[..]).unwrap().unwrap(), b"abc");
        assert_eq!(read_frame(&mut &b""[..]).unwrap(), None);
        let huge = (u32::MAX).to_le_bytes();
        assert!(read_frame(&mut &huge[..]).is_err());
    }
}
