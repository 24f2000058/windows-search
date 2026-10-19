//! Win32 implementation of [`JournalSource`] for NTFS volumes.
//!
//! Status: compile-checked against the Windows targets; NOT yet exercised on a
//! real volume (development so far ran on Linux). Behaviours marked
//! "unverified" below must be validated on the reference PC before relying
//! on them.
//!
//! Privileges: opening a volume handle for `FSCTL_ENUM_USN_DATA` /
//! `FSCTL_READ_USN_JOURNAL` requires administrator rights. Without them
//! [`NtfsVolume::open`] returns [`SourceError::AccessDenied`], which
//! `flint_index::model::validate` maps to the traversal fallback. See the
//! elevation ADR for the optional helper/service design.

use crate::mft::{data_runs_of, parse_record, Assembler};
use crate::records::{filetime_to_unix, parse_buffer};
use flint_index::model::{FileRecord, Frn, JournalInfo, SourceError, UsnRecord};
use flint_index::source::JournalSource;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::IO::DeviceIoControl;

const BUF_SIZE: usize = 1 << 20;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const GENERIC_READ: u32 = 0x8000_0000;
const VOLUME_NAME_NONE: u32 = 0x4;
/// `GetDriveTypeW` result for removable media (winbase.h).
pub const DRIVE_REMOVABLE: u32 = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn map_error(code: u32) -> SourceError {
    match code {
        ERROR_JOURNAL_ENTRY_DELETED => SourceError::EntryDeleted,
        // FAT/exFAT (and emulators) reject journal ioctls as unsupported.
        ERROR_JOURNAL_NOT_ACTIVE | ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED => SourceError::JournalNotActive,
        ERROR_JOURNAL_DELETE_IN_PROGRESS => SourceError::JournalDeleteInProgress,
        ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD => SourceError::AccessDenied,
        ERROR_NOT_READY | ERROR_DEV_NOT_EXIST | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_BAD_NETPATH => SourceError::Unavailable(format!("win32 error {code}")),
        c => SourceError::Io(format!("win32 error {c}")),
    }
}

fn last_error() -> SourceError {
    map_error(unsafe { GetLastError() })
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    /// `\\?\Volume{GUID}\` — stable identity used as the index key.
    pub guid_path: String,
    /// Mount points such as `C:\` (may be empty for unmounted volumes).
    pub mount_points: Vec<String>,
    pub filesystem: String,
    pub label: String,
    pub serial: u32,
    pub drive_type: u32,
}

impl VolumeInfo {
    pub fn is_ntfs(&self) -> bool {
        self.filesystem.eq_ignore_ascii_case("NTFS")
    }
    pub fn is_removable(&self) -> bool {
        self.drive_type == DRIVE_REMOVABLE
    }
    pub fn display_root(&self) -> String {
        self.mount_points.first().map_or_else(|| self.guid_path.clone(), |m| m.trim_end_matches('\\').to_owned())
    }
}

/// Lists all volumes. Volumes that fail to report information (e.g. an empty
/// card reader) are returned with an empty filesystem name, not dropped.
pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    let mut name = [0u16; 512];
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return out;
    }
    loop {
        let guid_path = from_wide(&name);
        let gw = wide(&guid_path);
        let mut paths = vec![0u16; 4096];
        let mut needed = 0u32;
        let mut mount_points = Vec::new();
        if unsafe { GetVolumePathNamesForVolumeNameW(gw.as_ptr(), paths.as_mut_ptr(), paths.len() as u32, &mut needed) } != 0 {
            for p in paths.split(|&c| c == 0).take_while(|p| !p.is_empty()) {
                mount_points.push(String::from_utf16_lossy(p));
            }
        }
        let (mut label, mut fs) = ([0u16; 261], [0u16; 261]);
        let (mut serial, mut maxlen, mut fsflags) = (0u32, 0u32, 0u32);
        let ok = unsafe { GetVolumeInformationW(gw.as_ptr(), label.as_mut_ptr(), label.len() as u32, &mut serial, &mut maxlen, &mut fsflags, fs.as_mut_ptr(), fs.len() as u32) } != 0;
        let drive_type = unsafe { GetDriveTypeW(gw.as_ptr()) };
        out.push(VolumeInfo {
            guid_path,
            mount_points,
            filesystem: if ok { from_wide(&fs) } else { String::new() },
            label: if ok { from_wide(&label) } else { String::new() },
            serial,
            drive_type,
        });
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) } == 0 {
            break;
        }
    }
    unsafe { FindVolumeClose(find) };
    out
}

/// Outcome of the last raw `$MFT` read (diagnostics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MftStats {
    pub records: u64,
    pub in_use: u64,
    /// Records failing fixup/structure checks.
    pub rejected: u64,
    pub bytes: u64,
    pub stopped: bool,
}

const MFT_CHUNK: usize = 4 << 20;

pub struct NtfsVolume {
    pub last_mft: Option<MftStats>,
    pub last_mft_error: Option<String>,
    volume: Handle,
    pub info: VolumeInfo,
    pub root_frn: Frn,
    buf: Vec<u8>,
}

fn file_info(h: HANDLE) -> Result<BY_HANDLE_FILE_INFORMATION, SourceError> {
    let mut fi: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(h, &mut fi) } == 0 {
        return Err(last_error());
    }
    Ok(fi)
}

fn frn_of(fi: &BY_HANDLE_FILE_INFORMATION) -> Frn {
    (fi.nFileIndexHigh as u64) << 32 | fi.nFileIndexLow as u64
}

fn mtime_of(fi: &BY_HANDLE_FILE_INFORMATION) -> i64 {
    filetime_to_unix(((fi.ftLastWriteTime.dwHighDateTime as i64) << 32) | fi.ftLastWriteTime.dwLowDateTime as i64)
}

fn open_path(path: &str, access: u32) -> Result<Handle, SourceError> {
    let w = wide(path);
    let h = unsafe { CreateFileW(w.as_ptr(), access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null_mut()) };
    if h == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    Ok(Handle(h))
}

// SAFETY: a volume handle may be used from any thread; NtfsVolume is only
// ever used by one thread at a time (it is moved into a catalog worker).
unsafe impl Send for NtfsVolume {}

impl NtfsVolume {
    /// Opens the volume for journal access (requires elevation).
    pub fn open(info: VolumeInfo) -> Result<Self, SourceError> {
        if !info.is_ntfs() {
            return Err(SourceError::JournalNotActive);
        }
        // Volume device path is the GUID path without its trailing slash.
        let dev = info.guid_path.trim_end_matches('\\').to_owned();
        let volume = open_path(&dev, GENERIC_READ)?;
        let root = open_path(&info.guid_path, FILE_READ_ATTRIBUTES)?;
        let root_frn = frn_of(&file_info(root.0)?);
        Ok(NtfsVolume { last_mft: None, last_mft_error: None, volume, info, root_frn, buf: vec![0u8; BUF_SIZE] })
    }

    fn ioctl<T>(&mut self, code: u32, input: &T) -> Result<usize, u32> {
        let mut ret = 0u32;
        let ok = unsafe { DeviceIoControl(self.volume.0, code, input as *const T as *const c_void, std::mem::size_of::<T>() as u32, self.buf.as_mut_ptr() as *mut c_void, self.buf.len() as u32, &mut ret, null_mut()) };
        if ok == 0 {
            Err(unsafe { GetLastError() })
        } else {
            Ok(ret as usize)
        }
    }

    fn open_by_id(&self, frn: Frn) -> Result<Option<Handle>, SourceError> {
        let mut desc: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
        desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
        desc.Type = FileIdType;
        desc.Anonymous.FileId = frn as i64;
        let h = unsafe { OpenFileById(self.volume.0, &desc, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, null(), FILE_FLAG_BACKUP_SEMANTICS) };
        if h == INVALID_HANDLE_VALUE {
            return match unsafe { GetLastError() } {
                // Deleted, or the segment was reused with another sequence.
                ERROR_INVALID_PARAMETER | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(None),
                c => Err(map_error(c)),
            };
        }
        let h = Handle(h);
        // Guard against sequence reuse: the opened file must be this FRN.
        if frn_of(&file_info(h.0)?) != frn {
            return Ok(None);
        }
        Ok(Some(h))
    }

    /// Uses `FSCTL_ENUM_USN_DATA`. Provides names/parents/attributes only
    /// (size and mtime are refreshed lazily). Unverified: whether files with
    /// several hard links yield one record per link; the index tolerates
    /// either, and HARD_LINK_CHANGE/link refresh reconcile later.
    pub fn enumerate_usn_data(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<(), SourceError> {
        let mut med = MFT_ENUM_DATA_V0 { StartFileReferenceNumber: 0, LowUsn: 0, HighUsn: i64::MAX };
        loop {
            let n = match self.ioctl(FSCTL_ENUM_USN_DATA, &med) {
                Ok(n) => n,
                Err(ERROR_HANDLE_EOF) => return Ok(()),
                Err(c) => return Err(map_error(c)),
            };
            let (header, recs, err) = parse_buffer(&self.buf[..n]);
            if let Some(e) = err {
                return Err(SourceError::Io(format!("malformed enumeration buffer: {e:?}")));
            }
            for p in recs {
                let r = p.record;
                if !sink(FileRecord { frn: r.frn, parent: r.parent, name: r.name, attrs: r.attrs, size: None, mtime: None }) {
                    return Ok(());
                }
            }
            match header {
                Some(next) if next != med.StartFileReferenceNumber => med.StartFileReferenceNumber = next,
                _ => return Ok(()),
            }
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        if unsafe { SetFilePointerEx(self.volume.0, offset as i64, null_mut(), FILE_BEGIN) } == 0 {
            return Err(last_error());
        }
        let mut got = 0u32;
        if unsafe { ReadFile(self.volume.0, buf.as_mut_ptr(), buf.len() as u32, &mut got, null_mut()) } == 0 {
            return Err(last_error());
        }
        Ok(got as usize)
    }

    /// Streams the `$MFT` sequentially in large cluster-aligned chunks.
    /// Status: compile-checked only; validate with `ntfs_probe` on hardware.
    pub fn enumerate_mft(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<MftStats, SourceError> {
        let mut vd: NTFS_VOLUME_DATA_BUFFER = unsafe { std::mem::zeroed() };
        let mut ret = 0u32;
        let ok = unsafe { DeviceIoControl(self.volume.0, FSCTL_GET_NTFS_VOLUME_DATA, null(), 0, &mut vd as *mut _ as *mut c_void, std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32, &mut ret, null_mut()) };
        if ok == 0 {
            return Err(last_error());
        }
        let (bpc, rs, sector) = (vd.BytesPerCluster as u64, vd.BytesPerFileRecordSegment as usize, vd.BytesPerSector as usize);
        let aligned = bpc.is_multiple_of(rs as u64) || (rs as u64).is_multiple_of(bpc.max(1));
        if bpc == 0 || rs < 512 || sector < 512 || !rs.is_multiple_of(sector) || !aligned {
            return Err(SourceError::Io(format!("unexpected NTFS geometry: cluster {bpc}, record {rs}, sector {sector}")));
        }
        let mut rec0 = vec![0u8; rs.max(sector)];
        self.read_at(vd.MftStartLcn as u64 * bpc, &mut rec0)?;
        let runs = data_runs_of(&mut rec0[..rs], sector).map_err(|e| SourceError::Io(format!("$MFT record 0: {e:?}")))?;
        let total_records = (vd.MftValidDataLength as u64) / rs as u64;
        let mut stats = MftStats::default();
        let mut asm = Assembler::default();
        let stop = std::cell::Cell::new(false);
        let mut emit = |f: FileRecord| {
            if !stop.get() && !sink(f) {
                stop.set(true);
            }
        };
        let chunk_bytes = (MFT_CHUNK as u64 / bpc.max(rs as u64)).max(1) * bpc.max(rs as u64);
        let mut buf = vec![0u8; chunk_bytes as usize];
        let mut segment = 0u64;
        'extents: for (lcn, clusters) in runs {
            let extent_bytes = clusters * bpc;
            let Some(lcn) = lcn else {
                segment += extent_bytes / rs as u64;
                continue;
            };
            let mut done = 0u64;
            while done < extent_bytes {
                if segment >= total_records {
                    break 'extents;
                }
                let want = (extent_bytes - done).min(chunk_bytes) as usize;
                let got = self.read_at(lcn as u64 * bpc + done, &mut buf[..want])?;
                if got == 0 {
                    break 'extents;
                }
                for rec in buf[..got - got % rs].chunks_exact_mut(rs) {
                    if segment >= total_records {
                        break;
                    }
                    match parse_record(rec, segment, sector) {
                        Ok(Some(e)) => {
                            stats.in_use += 1;
                            asm.add(e, &mut emit);
                        }
                        Ok(None) => {}
                        Err(_) => stats.rejected += 1,
                    }
                    segment += 1;
                    stats.records += 1;
                }
                done += got as u64;
                stats.bytes += got as u64;
                if stop.get() {
                    stats.stopped = true;
                    return Ok(stats);
                }
            }
        }
        asm.finish(&mut emit);
        stats.stopped = stop.get();
        Ok(stats)
    }

    fn mount(&self) -> Result<String, SourceError> {
        self.info.mount_points.first().map(|m| m.trim_end_matches('\\').to_owned()).ok_or_else(|| SourceError::Unavailable("volume has no mount point".into()))
    }
}

impl JournalSource for NtfsVolume {
    fn query(&mut self) -> Result<JournalInfo, SourceError> {
        let mut ret = 0u32;
        let mut d: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
        let ok = unsafe { DeviceIoControl(self.volume.0, FSCTL_QUERY_USN_JOURNAL, null(), 0, &mut d as *mut _ as *mut c_void, std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32, &mut ret, null_mut()) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(JournalInfo { journal_id: d.UsnJournalID, lowest_valid_usn: d.LowestValidUsn, first_usn: d.FirstUsn, next_usn: d.NextUsn })
    }

    /// Reads the raw `$MFT` (names of every link, sizes, times). If it
    /// cannot be read, or any record was rejected (torn by a concurrent write
    /// or corrupt), `FSCTL_ENUM_USN_DATA` runs as well so no file is missed;
    /// applying both is idempotent.
    fn enumerate(&mut self, sink: &mut dyn FnMut(FileRecord) -> bool) -> Result<(), SourceError> {
        match self.enumerate_mft(sink) {
            Ok(stats) => {
                self.last_mft = Some(stats);
                if stats.rejected == 0 || stats.stopped {
                    return Ok(());
                }
            }
            Err(e) => self.last_mft_error = Some(format!("{e:?}")),
        }
        self.enumerate_usn_data(sink)
    }

    fn read(&mut self, journal_id: u64, from_usn: i64, max: usize) -> Result<(Vec<UsnRecord>, i64), SourceError> {
        let rd = READ_USN_JOURNAL_DATA_V0 { StartUsn: from_usn, ReasonMask: 0xFFFF_FFFF, ReturnOnlyOnClose: 0, Timeout: 0, BytesToWaitFor: 0, UsnJournalID: journal_id };
        let n = self.ioctl(FSCTL_READ_USN_JOURNAL, &rd).map_err(map_error)?;
        let (header, recs, err) = parse_buffer(&self.buf[..n]);
        if let Some(e) = err {
            return Err(SourceError::Io(format!("malformed journal buffer: {e:?}")));
        }
        let next = header.map_or(from_usn, |h| h as i64);
        // The kernel fills at most one buffer; `max` only caps further.
        let mut out: Vec<UsnRecord> = recs.into_iter().map(|p| p.record).collect();
        if out.len() > max {
            let resume = out[max].usn;
            out.truncate(max);
            return Ok((out, resume));
        }
        Ok((out, next))
    }

    fn links(&mut self, frn: Frn) -> Result<Vec<FileRecord>, SourceError> {
        let Some(h) = self.open_by_id(frn)? else { return Ok(Vec::new()) };
        let fi = file_info(h.0)?;
        let mut rel = vec![0u16; 32_768];
        let n = unsafe { GetFinalPathNameByHandleW(h.0, rel.as_mut_ptr(), rel.len() as u32, VOLUME_NAME_NONE) } as usize;
        if n == 0 || n >= rel.len() {
            return Err(last_error());
        }
        let mount = self.mount()?;
        let first = format!("\\\\?\\{mount}{}", String::from_utf16_lossy(&rel[..n]));
        let fw = wide(&first);
        let mut names = Vec::new();
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        let find = unsafe { FindFirstFileNameW(fw.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
        if find == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        loop {
            names.push(from_wide(&buf[..len as usize]));
            len = buf.len() as u32;
            if unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) } == 0 {
                break;
            }
        }
        unsafe { FindClose(find) };
        let mut out = Vec::new();
        for link in names {
            // `link` is volume-relative, e.g. `\dir\file.txt`.
            let Some((dir, name)) = link.rsplit_once('\\') else { continue };
            let parent = if dir.is_empty() {
                self.root_frn
            } else {
                let ph = open_path(&format!("\\\\?\\{mount}{dir}"), FILE_READ_ATTRIBUTES)?;
                frn_of(&file_info(ph.0)?)
            };
            out.push(FileRecord { frn, parent, name: name.to_owned(), attrs: fi.dwFileAttributes, size: Some((fi.nFileSizeHigh as u64) << 32 | fi.nFileSizeLow as u64), mtime: Some(mtime_of(&fi)) });
        }
        Ok(out)
    }

    fn meta(&mut self, frn: Frn) -> Result<Option<(u32, u64, i64)>, SourceError> {
        let Some(h) = self.open_by_id(frn)? else { return Ok(None) };
        let fi = file_info(h.0)?;
        Ok(Some((fi.dwFileAttributes, (fi.nFileSizeHigh as u64) << 32 | fi.nFileSizeLow as u64, mtime_of(&fi))))
    }
}
