//! Platform-neutral change-journal model shared by the NTFS adapter, the
//! traversal fallback, the simulator and the persistent store.

/// NTFS file reference number: low 48 bits = MFT segment, high 16 = sequence.
pub type Frn = u64;

#[inline]
pub fn segment(frn: Frn) -> u64 {
    frn & 0x0000_FFFF_FFFF_FFFF
}
#[inline]
pub fn sequence(frn: Frn) -> u16 {
    (frn >> 48) as u16
}
#[inline]
pub fn make_frn(segment: u64, sequence: u16) -> Frn {
    (segment & 0x0000_FFFF_FFFF_FFFF) | (sequence as u64) << 48
}

/// Win32 `FILE_ATTRIBUTE_*` bits we interpret.
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const REPARSE_POINT: u32 = 0x400;
    pub const OFFLINE: u32 = 0x1000;
}

/// `USN_REASON_*` bits.
pub mod reason {
    pub const DATA_OVERWRITE: u32 = 0x0000_0001;
    pub const DATA_EXTEND: u32 = 0x0000_0002;
    pub const DATA_TRUNCATION: u32 = 0x0000_0004;
    pub const NAMED_DATA_OVERWRITE: u32 = 0x0000_0010;
    pub const NAMED_DATA_EXTEND: u32 = 0x0000_0020;
    pub const NAMED_DATA_TRUNCATION: u32 = 0x0000_0040;
    pub const FILE_CREATE: u32 = 0x0000_0100;
    pub const FILE_DELETE: u32 = 0x0000_0200;
    pub const EA_CHANGE: u32 = 0x0000_0400;
    pub const SECURITY_CHANGE: u32 = 0x0000_0800;
    pub const RENAME_OLD_NAME: u32 = 0x0000_1000;
    pub const RENAME_NEW_NAME: u32 = 0x0000_2000;
    pub const INDEXABLE_CHANGE: u32 = 0x0000_4000;
    pub const BASIC_INFO_CHANGE: u32 = 0x0000_8000;
    pub const HARD_LINK_CHANGE: u32 = 0x0001_0000;
    pub const COMPRESSION_CHANGE: u32 = 0x0002_0000;
    pub const ENCRYPTION_CHANGE: u32 = 0x0004_0000;
    pub const OBJECT_ID_CHANGE: u32 = 0x0008_0000;
    pub const REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
    pub const STREAM_CHANGE: u32 = 0x0020_0000;
    pub const CLOSE: u32 = 0x8000_0000;

    /// Reasons after which size/time metadata must be re-read.
    pub const META: u32 = DATA_OVERWRITE | DATA_EXTEND | DATA_TRUNCATION | NAMED_DATA_OVERWRITE | NAMED_DATA_EXTEND | NAMED_DATA_TRUNCATION | BASIC_INFO_CHANGE | FILE_CREATE;
}

/// One name link of a file as produced by enumeration (MFT read or
/// traversal). A file with several hard links yields several records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    pub frn: Frn,
    pub parent: Frn,
    pub name: String,
    pub attrs: u32,
    /// Size and mtime (unix seconds), if the source provides them.
    pub size: Option<u64>,
    pub mtime: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsnRecord {
    pub usn: i64,
    pub frn: Frn,
    pub parent: Frn,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    /// Lowest USN still readable; older records were purged (wraparound).
    pub lowest_valid_usn: i64,
    pub first_usn: i64,
    pub next_usn: i64,
}

/// Errors a journal source can report; mirrors the Win32 conditions that
/// matter for recovery decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// `ERROR_JOURNAL_NOT_ACTIVE`: no change journal on the volume.
    JournalNotActive,
    /// `ERROR_JOURNAL_DELETE_IN_PROGRESS`.
    JournalDeleteInProgress,
    /// `ERROR_JOURNAL_ENTRY_DELETED`: requested records were purged.
    EntryDeleted,
    /// Journal ID differs from the one requested.
    JournalIdMismatch,
    AccessDenied,
    /// Volume disappeared (unplugged, network drop).
    Unavailable(String),
    Io(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RescanReason {
    NeverIndexed,
    /// The journal was deleted and recreated (new journal ID).
    JournalIdChanged,
    /// Records between the checkpoint and the oldest valid USN were purged.
    RecordsLost,
    /// The checkpoint is ahead of the journal (restored image, corruption).
    CheckpointAhead,
    /// A previous enumeration did not finish (crash, cancel, power loss).
    InterruptedEnumeration,
    JournalDeleted,
    /// Repair found inconsistencies it could not resolve incrementally.
    Inconsistent,
    UserRequested,
}

/// Persisted per-volume state. Only `Live` (journal-backed, caught up) and
/// `TraversalComplete` count as complete coverage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeState {
    NotIndexed,
    Enumerating { journal_id: u64, start_usn: i64 },
    Live { journal_id: u64, next_usn: i64 },
    NeedsRescan(RescanReason),
    /// Indexed by directory traversal (no journal). `skipped` counts
    /// directories that could not be read; coverage is complete only if 0.
    Traversal { complete: bool, skipped: u64 },
    Unavailable(String),
}

impl VolumeState {
    pub fn is_complete(&self) -> bool {
        matches!(self, VolumeState::Live { .. } | VolumeState::Traversal { complete: true, skipped: 0 })
    }

    pub fn encode(&self) -> (String, i64, i64, String) {
        match self {
            VolumeState::NotIndexed => ("not_indexed".into(), 0, 0, String::new()),
            VolumeState::Enumerating { journal_id, start_usn } => ("enumerating".into(), *journal_id as i64, *start_usn, String::new()),
            VolumeState::Live { journal_id, next_usn } => ("live".into(), *journal_id as i64, *next_usn, String::new()),
            VolumeState::NeedsRescan(r) => ("needs_rescan".into(), 0, 0, format!("{r:?}")),
            VolumeState::Traversal { complete, skipped } => ("traversal".into(), *complete as i64, *skipped as i64, String::new()),
            VolumeState::Unavailable(s) => ("unavailable".into(), 0, 0, s.clone()),
        }
    }

    /// Decodes a stored state; anything unrecognised becomes a rescan.
    pub fn decode(kind: &str, a: i64, b: i64, detail: &str) -> Self {
        match kind {
            "not_indexed" => VolumeState::NotIndexed,
            "enumerating" => VolumeState::Enumerating { journal_id: a as u64, start_usn: b },
            "live" => VolumeState::Live { journal_id: a as u64, next_usn: b },
            "needs_rescan" => VolumeState::NeedsRescan(match detail {
                "JournalIdChanged" => RescanReason::JournalIdChanged,
                "RecordsLost" => RescanReason::RecordsLost,
                "CheckpointAhead" => RescanReason::CheckpointAhead,
                "InterruptedEnumeration" => RescanReason::InterruptedEnumeration,
                "JournalDeleted" => RescanReason::JournalDeleted,
                "UserRequested" => RescanReason::UserRequested,
                "NeverIndexed" => RescanReason::NeverIndexed,
                _ => RescanReason::Inconsistent,
            }),
            "traversal" => VolumeState::Traversal { complete: a != 0, skipped: b as u64 },
            "unavailable" => VolumeState::Unavailable(detail.to_owned()),
            _ => VolumeState::NeedsRescan(RescanReason::Inconsistent),
        }
    }
}

/// What to do with a volume at startup or after a read error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Continue reading the journal from this USN.
    Resume { journal_id: u64, from_usn: i64 },
    Rescan(RescanReason),
    /// No journal: index by traversal instead (never claimed live).
    Traverse,
    /// Retry later; keep the existing (stale) entries searchable but report
    /// the volume as not current.
    Wait(String),
}

/// Validates a stored checkpoint against the current journal. Pure, so every
/// recovery path is unit-testable.
pub fn validate(state: &VolumeState, journal: &Result<JournalInfo, SourceError>) -> Decision {
    let j = match journal {
        Ok(j) => *j,
        Err(SourceError::JournalNotActive) => return Decision::Traverse,
        Err(SourceError::JournalDeleteInProgress) => return Decision::Wait("journal deletion in progress".into()),
        Err(SourceError::AccessDenied) => return Decision::Traverse,
        Err(SourceError::Unavailable(s)) => return Decision::Wait(format!("volume unavailable: {s}")),
        Err(e) => return Decision::Wait(format!("{e:?}")),
    };
    match state {
        VolumeState::NotIndexed => Decision::Rescan(RescanReason::NeverIndexed),
        VolumeState::Enumerating { .. } => Decision::Rescan(RescanReason::InterruptedEnumeration),
        VolumeState::NeedsRescan(r) => Decision::Rescan(*r),
        VolumeState::Traversal { .. } | VolumeState::Unavailable(_) => Decision::Rescan(RescanReason::NeverIndexed),
        VolumeState::Live { journal_id, next_usn } => {
            if *journal_id != j.journal_id {
                Decision::Rescan(RescanReason::JournalIdChanged)
            } else if *next_usn < j.lowest_valid_usn.max(j.first_usn) {
                Decision::Rescan(RescanReason::RecordsLost)
            } else if *next_usn > j.next_usn {
                Decision::Rescan(RescanReason::CheckpointAhead)
            } else {
                Decision::Resume { journal_id: j.journal_id, from_usn: *next_usn }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(id: u64, low: i64, next: i64) -> Result<JournalInfo, SourceError> {
        Ok(JournalInfo { journal_id: id, lowest_valid_usn: low, first_usn: low, next_usn: next })
    }

    #[test]
    fn frn_round_trip() {
        let f = make_frn(0x1234_5678_9abc, 7);
        assert_eq!(segment(f), 0x1234_5678_9abc);
        assert_eq!(sequence(f), 7);
    }

    #[test]
    fn checkpoint_validation_covers_every_recovery_path() {
        let live = VolumeState::Live { journal_id: 1, next_usn: 500 };
        assert_eq!(validate(&live, &j(1, 100, 900)), Decision::Resume { journal_id: 1, from_usn: 500 });
        assert_eq!(validate(&live, &j(1, 100, 500)), Decision::Resume { journal_id: 1, from_usn: 500 });
        assert_eq!(validate(&live, &j(2, 0, 900)), Decision::Rescan(RescanReason::JournalIdChanged));
        assert_eq!(validate(&live, &j(1, 600, 900)), Decision::Rescan(RescanReason::RecordsLost));
        assert_eq!(validate(&live, &j(1, 0, 400)), Decision::Rescan(RescanReason::CheckpointAhead));
        let enumerating = VolumeState::Enumerating { journal_id: 1, start_usn: 10 };
        assert_eq!(validate(&enumerating, &j(1, 0, 900)), Decision::Rescan(RescanReason::InterruptedEnumeration));
        assert_eq!(validate(&VolumeState::NotIndexed, &j(1, 0, 9)), Decision::Rescan(RescanReason::NeverIndexed));
        assert_eq!(validate(&live, &Err(SourceError::JournalNotActive)), Decision::Traverse);
        assert!(matches!(validate(&live, &Err(SourceError::JournalDeleteInProgress)), Decision::Wait(_)));
        assert!(matches!(validate(&live, &Err(SourceError::Unavailable("x".into()))), Decision::Wait(_)));
    }

    #[test]
    fn state_encoding_round_trips_and_rejects_garbage() {
        for s in [
            VolumeState::NotIndexed,
            VolumeState::Enumerating { journal_id: 9, start_usn: 77 },
            VolumeState::Live { journal_id: u64::MAX >> 1, next_usn: 123 },
            VolumeState::NeedsRescan(RescanReason::RecordsLost),
            VolumeState::Traversal { complete: false, skipped: 3 },
            VolumeState::Unavailable("gone".into()),
        ] {
            let (k, a, b, d) = s.encode();
            assert_eq!(VolumeState::decode(&k, a, b, &d), s);
        }
        assert_eq!(VolumeState::decode("??", 0, 0, ""), VolumeState::NeedsRescan(RescanReason::Inconsistent));
        assert!(!VolumeState::Traversal { complete: true, skipped: 1 }.is_complete());
        assert!(VolumeState::Live { journal_id: 1, next_usn: 1 }.is_complete());
    }
}
