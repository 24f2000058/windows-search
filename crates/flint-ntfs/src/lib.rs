//! NTFS volume adapter: MFT enumeration and USN change-journal reading.
//!
//! [`records`] (buffer parsing) is platform-independent and tested anywhere;
//! [`win`] performs the Win32 calls and only builds on Windows.

pub mod records;
#[cfg(windows)]
pub mod win;
