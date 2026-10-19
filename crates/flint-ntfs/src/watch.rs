//! `ReadDirectoryChangesW` change feed for journal-less volumes and folders.
//!
//! A dedicated thread blocks in a synchronous, recursive
//! `ReadDirectoryChangesW` and forwards the changed directories. A zero-byte
//! completion or `ERROR_NOTIFY_ENUM_DIR` means the kernel buffer overflowed
//! and events were lost, which is reported as [`FeedEvent::Overflow`] so the
//! catalog falls back to a full traversal. Dropping the watcher cancels the
//! blocked call (`CancelIoEx`) and joins the thread.
//!
//! Status: compile-checked; not yet exercised on Windows. Network shares
//! support this only if the server does (SMB generally does); failures are
//! reported, and the catalog then retries with backoff.

use flint_index::catalog::{ChangeFeed, FeedEvent};
use flint_index::model::SourceError;
use flint_index::reconcile::{dirs_for_events, parse_notify_buffer};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::CancelIoEx;

const BUF_BYTES: usize = 64 * 1024;
/// Coalesce bursts (e.g. a copy of many files) into one reconcile.
const DEBOUNCE: Duration = Duration::from_millis(200);

pub struct DirWatcher {
    rx: Receiver<FeedEvent>,
    stop: Arc<AtomicBool>,
    handle: usize,
    thread: Option<JoinHandle<()>>,
}

impl DirWatcher {
    pub fn start(root: &Path) -> Result<Self, SourceError> {
        let w: Vec<u16> = root.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let h = unsafe { CreateFileW(w.as_ptr(), FILE_LIST_DIRECTORY, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null_mut()) };
        if h == INVALID_HANDLE_VALUE {
            return Err(SourceError::Unavailable(format!("cannot watch {}: win32 error {}", root.display(), unsafe { GetLastError() })));
        }
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, hbits) = (stop.clone(), h as usize);
        let thread = std::thread::Builder::new()
            .name("flint-watch".into())
            .spawn(move || {
                // u32 storage keeps the buffer DWORD-aligned as required.
                let mut buf = vec![0u32; BUF_BYTES / 4];
                let filter = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_ATTRIBUTES;
                loop {
                    let mut got = 0u32;
                    let ok = unsafe { ReadDirectoryChangesW(hbits as HANDLE, buf.as_mut_ptr().cast(), BUF_BYTES as u32, 1, filter, &mut got, null_mut(), None) };
                    if stop2.load(Ordering::SeqCst) {
                        return;
                    }
                    let ev = if ok == 0 {
                        match unsafe { GetLastError() } {
                            ERROR_NOTIFY_ENUM_DIR => FeedEvent::Overflow,
                            ERROR_OPERATION_ABORTED => return,
                            e => FeedEvent::Failed(format!("win32 error {e}")),
                        }
                    } else if got == 0 {
                        FeedEvent::Overflow
                    } else {
                        let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize) };
                        FeedEvent::Dirs(dirs_for_events(&parse_notify_buffer(bytes)))
                    };
                    let fatal = matches!(ev, FeedEvent::Failed(_));
                    if tx.send(ev).is_err() || fatal {
                        return;
                    }
                }
            })
            .map_err(|e| SourceError::Io(e.to_string()))?;
        Ok(DirWatcher { rx, stop, handle: hbits, thread: Some(thread) })
    }
}

impl ChangeFeed for DirWatcher {
    fn poll(&mut self, timeout: Duration) -> FeedEvent {
        let first = match self.rx.recv_timeout(timeout) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => return FeedEvent::Idle,
            Err(RecvTimeoutError::Disconnected) => return FeedEvent::Failed("watcher thread ended".into()),
        };
        let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
        let mut overflow = false;
        let mut absorb = |e: FeedEvent| -> Option<FeedEvent> {
            match e {
                FeedEvent::Dirs(d) => dirs.extend(d),
                FeedEvent::Overflow => overflow = true,
                FeedEvent::Failed(f) => return Some(FeedEvent::Failed(f)),
                FeedEvent::Idle => {}
            }
            None
        };
        if let Some(f) = absorb(first) {
            return f;
        }
        let until = Instant::now() + DEBOUNCE;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(e) => {
                    if let Some(f) = absorb(e) {
                        return f;
                    }
                }
                Err(_) => break,
            }
        }
        if overflow { FeedEvent::Overflow } else { FeedEvent::Dirs(dirs.into_iter().collect()) }
    }
}

impl Drop for DirWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        unsafe {
            CancelIoEx(self.handle as HANDLE, null());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            CloseHandle(self.handle as HANDLE);
        }
    }
}
