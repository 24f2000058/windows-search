//! HTTPS GET through WinHTTP (the system's TLS, certificate store and
//! proxy settings). Redirects are followed, but never from https to http.
//! Responses are capped at the caller's size limit.
//!
//! Status: compile-checked and run under Wine against a local test server;
//! not validated against GitHub from Windows hardware.

use crate::Fetch;
use std::ffi::c_void;
use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::*;

struct H(*mut c_void);

impl Drop for H {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct WinHttp {
    pub user_agent: String,
}

impl Default for WinHttp {
    fn default() -> Self {
        WinHttp { user_agent: format!("Flint/{} (update check)", env!("CARGO_PKG_VERSION")) }
    }
}

/// (secure, host, port, path-and-query)
fn split_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let (true, Some(r)) = (crate::test_url_allowed(url), url.strip_prefix("http://")) {
        (false, r)
    } else {
        return Err("only https addresses are used".into());
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| "bad port")?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    if host.is_empty() || authority.contains('@') {
        return Err("bad address".into());
    }
    Ok((secure, host.to_owned(), port, path.to_owned()))
}

impl Fetch for WinHttp {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        let (secure, host, port, path) = split_url(url)?;
        unsafe {
            let ua = wide(&self.user_agent);
            let mut session = H(WinHttpOpen(PCWSTR(ua.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
            if session.0.is_null() {
                // Before Windows 8.1 (and on Wine): the configured proxy.
                session = H(WinHttpOpen(PCWSTR(ua.as_ptr()), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
            }
            if session.0.is_null() {
                return Err(format!("network unavailable: {}", windows::core::Error::from_thread().message()));
            }
            let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);
            let policy = 1u32.to_le_bytes(); // WINHTTP_OPTION_REDIRECT_POLICY_DISALLOW_HTTPS_TO_HTTP
            let _ = WinHttpSetOption(Some(session.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&policy));
            let h = wide(&host);
            let conn = H(WinHttpConnect(session.0, PCWSTR(h.as_ptr()), port, 0));
            if conn.0.is_null() {
                return Err(format!("cannot reach {host}: {}", windows::core::Error::from_thread().message()));
            }
            let p = wide(&path);
            let req = H(WinHttpOpenRequest(conn.0, w!("GET"), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), if secure { WINHTTP_FLAG_SECURE } else { WINHTTP_OPEN_REQUEST_FLAGS(0) }));
            if req.0.is_null() {
                return Err(windows::core::Error::from_thread().message());
            }
            WinHttpSendRequest(req.0, None, None, 0, 0, 0).map_err(|e| format!("request failed: {}", e.message()))?;
            WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| format!("no response: {}", e.message()))?;
            let mut status = 0u32;
            let mut len = 4u32;
            let mut idx = 0u32;
            WinHttpQueryHeaders(req.0, WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER, PCWSTR::null(), Some(&mut status as *mut u32 as *mut c_void), &mut len, &mut idx).map_err(|e| e.message())?;
            if status != 200 {
                return Err(format!("server answered HTTP {status}"));
            }
            let mut out = Vec::new();
            let mut buf = vec![0u8; 64 << 10];
            loop {
                let mut n = 0u32;
                WinHttpReadData(req.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut n).map_err(|e| format!("download interrupted: {}", e.message()))?;
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
                if out.len() as u64 > max_bytes {
                    return Err(format!("larger than the expected {max_bytes} bytes"));
                }
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_and_plain_http_is_refused() {
        assert_eq!(split_url("https://github.com/o/r/x.json").unwrap(), (true, "github.com".into(), 443, "/o/r/x.json".into()));
        assert_eq!(split_url("https://h:8443").unwrap(), (true, "h".into(), 8443, "/".into()));
        assert!(split_url("http://example.org/").is_err());
        assert!(split_url("https://user@host/").is_err());
    }
}
