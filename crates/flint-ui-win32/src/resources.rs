//! Process/thread resource controls (Windows).

use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::Threading::*;

/// Puts the calling thread in background mode: lowest scheduling priority,
/// very low I/O priority and low memory priority (its pages are trimmed
/// first under memory pressure). On Windows 11 it also opts the thread into
/// EcoQoS (efficiency cores, lower clock); older systems ignore that call.
/// Used for index workers only; search and UI threads stay interactive.
pub fn enter_background_mode() {
    unsafe {
        let t = GetCurrentThread();
        SetThreadPriority(t, THREAD_MODE_BACKGROUND_BEGIN);
        let state = THREAD_POWER_THROTTLING_STATE { Version: THREAD_POWER_THROTTLING_CURRENT_VERSION, ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED, StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED };
        SetThreadInformation(t, ThreadPowerThrottling, &state as *const _ as *const _, std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32);
    }
}

/// (private bytes, working set) of this process.
pub fn memory_usage() -> Option<(usize, usize)> {
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        (GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb) != 0).then_some((c.PrivateUsage, c.WorkingSetSize))
    }
}

/// Offset of local wall-clock time from UTC at instant `utc` (unix
/// seconds), using the system's time-zone rules for that date (so past and
/// future daylight-saving transitions are honoured, not today's offset).
pub fn local_offset(utc: i64) -> i64 {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    const EPOCH: i64 = 116_444_736_000_000_000;
    let Some(t) = utc.checked_mul(10_000_000).and_then(|v| v.checked_add(EPOCH)).filter(|v| *v >= 0) else { return 0 };
    let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    unsafe {
        let (mut su, mut sl): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&ft, &mut su) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &su, &mut sl) == 0 {
            return 0;
        }
        let secs = |s: &SYSTEMTIME| flint_fuzzy::query::days_from_civil(s.wYear as i64, s.wMonth as u32, s.wDay as u32) * 86_400 + s.wHour as i64 * 3600 + s.wMinute as i64 * 60 + s.wSecond as i64;
        secs(&sl) - secs(&su)
    }
}
