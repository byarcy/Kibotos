//! "Is there a newer Kibotós?": asks GitHub for the latest release.
//!
//! Off unless the user turns it on (when installing, or in Ayuda). Then
//! it runs at most once a day, on a worker thread a moment after the window
//! appears, so it never delays startup. It uses WinHTTP, which is part of
//! Windows (no TLS library inside Kibotós), and is delay-loaded: the DLL is
//! loaded only when a check runs. The request carries only what any
//! browser visit would (the IP address) and the user agent "Kibotos/x.y.z".

use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR, w};

use crate::install::{VERSION, version_key};
use crate::integrate;

const REPO: &str = "byarcy/Kibotos";
const ENABLED_SETTING: &str = "CheckUpdates";
const LAST_SETTING: &str = "LastUpdateCheck";
const SKIPPED_SETTING: &str = "SkippedVersion";
const DAY: u64 = 86_400;

pub fn enabled() -> bool {
    integrate::read_setting(ENABLED_SETTING).as_deref() == Some("1")
}

pub fn set_enabled(on: bool) {
    integrate::write_setting(ENABLED_SETTING, if on { "1" } else { "0" });
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether the daily automatic check should run now.
pub fn due() -> bool {
    enabled() && integrate::read_setting(LAST_SETTING).and_then(|s| s.parse::<u64>().ok()).is_none_or(|t| now() >= t + DAY)
}

pub fn mark_checked() {
    integrate::write_setting(LAST_SETTING, &now().to_string());
}

/// The user chose "Omitir esta versión".
pub fn skip(tag: &str) {
    integrate::write_setting(SKIPPED_SETTING, tag);
}

pub fn skipped(tag: &str) -> bool {
    integrate::read_setting(SKIPPED_SETTING).as_deref() == Some(tag)
}

pub fn release_page(tag: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/{tag}")
}

/// Whether `tag` ("v0.4") is newer than this build.
pub fn is_newer(tag: &str) -> bool {
    version_key(tag) > version_key(VERSION)
}

#[derive(Debug)]
pub enum CheckError {
    /// GitHub answered 404: no published release, or a private repository.
    NotFound,
    /// No connection, a timeout, or an unexpected answer.
    Failed(String),
}

/// The tag of the latest published release ("v0.4").
pub fn latest() -> Result<String, CheckError> {
    let body = get(&format!("/repos/{REPO}/releases/latest"))?;
    tag_name(&body).ok_or_else(|| CheckError::Failed("respuesta sin tag_name".into()))
}

/// Finds `"tag_name": "..."` in GitHub's JSON without a JSON parser.
fn tag_name(json: &str) -> Option<String> {
    let at = json.find("\"tag_name\"")? + "\"tag_name\"".len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned()).filter(|t| !t.is_empty() && t.len() < 64)
}

/// Closes a WinHTTP handle when dropped.
struct Handle(*mut core::ffi::c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn last_error(what: &str) -> CheckError {
    CheckError::Failed(format!("{what}: {}", std::io::Error::last_os_error()))
}

/// GET https://api.github.com{path}; the body as text (at most 1 MiB).
fn get(path: &str) -> Result<String, CheckError> {
    unsafe {
        let agent = HSTRING::from(format!("Kibotos/{VERSION}"));
        let session = Handle(WinHttpOpen(&agent, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        let _ = WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 5000);
        let connect = Handle(WinHttpConnect(session.0, w!("api.github.com"), INTERNET_DEFAULT_HTTPS_PORT, 0));
        if connect.0.is_null() {
            return Err(last_error("WinHttpConnect"));
        }
        let request = Handle(WinHttpOpenRequest(
            connect.0,
            w!("GET"),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err(last_error("WinHttpOpenRequest"));
        }
        let headers: Vec<u16> = "Accept: application/vnd.github+json".encode_utf16().collect();
        let _ = WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD);
        if WinHttpSendRequest(request.0, None, None, 0, 0, 0).is_err() {
            return Err(last_error("WinHttpSendRequest"));
        }
        if WinHttpReceiveResponse(request.0, std::ptr::null_mut()).is_err() {
            return Err(last_error("WinHttpReceiveResponse"));
        }
        let mut status = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        let _ = WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut _),
            &mut len,
            std::ptr::null_mut(),
        );
        if status == 404 {
            return Err(CheckError::NotFound);
        }
        if status != 200 {
            return Err(CheckError::Failed(format!("HTTP {status}")));
        }
        let mut body = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(request.0, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut read).is_err() {
                return Err(last_error("WinHttpReadData"));
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > 1 << 20 {
                return Err(CheckError::Failed("respuesta demasiado grande".into()));
            }
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tag() {
        let json = r#"{"url":"x","html_url":"https://github.com/a","id":1,"tag_name": "v0.4","name":"Kibotós v0.4"}"#;
        assert_eq!(tag_name(json).as_deref(), Some("v0.4"));
        assert_eq!(tag_name(r#"{"message":"Not Found"}"#), None);
        assert!(is_newer("v9.0") && !is_newer("v0.1"));
    }
}
