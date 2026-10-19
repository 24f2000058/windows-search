//! The interface every volume backend implements (NTFS adapter, simulator).

use crate::model::{FileRecord, Frn, JournalInfo, SourceError, UsnRecord};

pub trait JournalSource {
    fn query(&mut self) -> Result<JournalInfo, SourceError>;
    /// Streams every name link on the volume. `sink` returns `false` to stop
    /// early (cancellation); the caller then treats enumeration as incomplete.
    fn enumerate(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<(), SourceError>;
    /// Reads up to `max` records starting at `from_usn`; returns them and the
    /// USN to continue from.
    fn read(&mut self, journal_id: u64, from_usn: i64, max: usize) -> Result<(Vec<UsnRecord>, i64), SourceError>;
    /// Current hard links of a file (empty if it no longer exists).
    fn links(&mut self, frn: Frn) -> Result<Vec<FileRecord>, SourceError>;
    /// Current (attributes, size, mtime) of a file, if it exists.
    fn meta(&mut self, frn: Frn) -> Result<Option<(u32, u64, i64)>, SourceError>;
}
