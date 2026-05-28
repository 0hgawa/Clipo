//! Anonymous file upload via WinHTTP. Zero new deps (winhttp.dll is
//! part of Windows since Vista, bindings already in workspace).
//!
//! Catbox: `reqtype=fileupload` + `fileToUpload=<binary>`.
//! 0x0.st:  `file=<binary>` (no reqtype).
//!
//! Both return plain-text URLs or plain-text errors. Driven from
//! `spawn_blocking` — every call is synchronous.

use std::ffi::c_void;
use std::ptr;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_ADDREQ_FLAG_REPLACE,
    WINHTTP_FLAG_SECURE, WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen,
    WinHttpOpenRequest, WinHttpQueryDataAvailable, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts, WinHttpWriteData,
};
use windows::core::PCWSTR;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UploadService {
    #[default]
    Catbox,
    Zerox0,
}

const USER_AGENT: &str = "Clipo/0.1 (+https://github.com/0hgawa/Clipo)";
const BOUNDARY: &str = "----ClipoBoundary7F0E1A2B3C4D5E6F";
const CHUNK: usize = 64 * 1024;

// WinHTTP timeouts (ms). Default `dwResolveTimeout` is 0 = INFINITE per
// the API docs — a DNS hang freezes the worker indefinitely with no
// user-visible recourse. Setting an explicit DNS deadline is the whole
// reason for `WinHttpSetTimeouts` here; the others are tuned
// loose-but-bounded so a slow-but-working upload completes while a
// dead one fails inside a reasonable wait.
const DNS_TIMEOUT_MS: i32 = 10_000;
const CONNECT_TIMEOUT_MS: i32 = 15_000;
// 90s covers a multi-MB 4K capture at ~1 Mbps (~80s). 60s false-
// positived on slow links during testing.
const SEND_TIMEOUT_MS: i32 = 90_000;
const RECEIVE_TIMEOUT_MS: i32 = 30_000;

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("winhttp: {0}")]
    WinHttp(String),
    #[error("upload rejected: {0}")]
    Rejected(String),
    #[error("response not utf-8")]
    NotUtf8,
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &'static str) -> Result<Self, UploadError> {
        if raw.is_null() {
            return Err(UploadError::WinHttp(format!("{what} returned null")));
        }
        Ok(Self(raw))
    }

    const fn raw(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: handle is non-null (enforced by new); closed once via RAII.
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// Upload `bytes` as `filename`. Returns the public URL. Blocking —
/// drive from `spawn_blocking`.
pub fn upload(service: UploadService, bytes: &[u8], filename: &str) -> Result<String, UploadError> {
    let shape = upload_shape(service);
    // Envelope only — the PNG itself streams by reference through
    // WriteData. Saves ~10 MB of allocation per 4K capture.
    let (header, footer) = build_envelope(shape.text_fields, shape.file_field, filename);

    // Hold UTF-16 buffers in named locals so as_ptr() stays valid.
    let user_agent = wide(USER_AGENT);
    // SAFETY: user_agent lives until end of scope; both proxy params
    // null per AUTOMATIC_PROXY docs.
    let session = Handle::new(
        unsafe {
            WinHttpOpen(
                PCWSTR(user_agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        },
        "WinHttpOpen",
    )?;

    // SAFETY: session is live; timeouts are scalars.
    unsafe {
        WinHttpSetTimeouts(
            session.raw(),
            DNS_TIMEOUT_MS,
            CONNECT_TIMEOUT_MS,
            SEND_TIMEOUT_MS,
            RECEIVE_TIMEOUT_MS,
        )
    }
    .map_err(|e| UploadError::WinHttp(format!("SetTimeouts: {e}")))?;

    let host_wide = wide(shape.host);
    // SAFETY: session live; host is NUL-terminated UTF-16.
    let connection = Handle::new(
        unsafe { WinHttpConnect(session.raw(), PCWSTR(host_wide.as_ptr()), 443, 0) },
        "WinHttpConnect",
    )?;

    let path_wide = wide(shape.path);
    let verb_wide = wide("POST");
    // Referer is required for Catbox (filtered), unset for 0x0.st.
    let referer_wide = shape.referer.map(wide);
    let referer_ptr = referer_wide
        .as_ref()
        .map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
    // SAFETY: connection live; PCWSTRs reference buffers alive through call.
    let request = Handle::new(
        unsafe {
            WinHttpOpenRequest(
                connection.raw(),
                PCWSTR(verb_wide.as_ptr()),
                PCWSTR(path_wide.as_ptr()),
                PCWSTR::null(),
                referer_ptr,
                ptr::null_mut(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;

    let content_type = format!("Content-Type: multipart/form-data; boundary={BOUNDARY}");
    let header_wide = wide(&content_type);
    // SAFETY: request live; slice carries length, exclude trailing NUL.
    unsafe {
        WinHttpAddRequestHeaders(
            request.raw(),
            &header_wide[..header_wide.len() - 1],
            WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
        )
    }
    .map_err(|e| UploadError::WinHttp(format!("AddRequestHeaders: {e}")))?;

    let total_len = u32::try_from(header.len() + bytes.len() + footer.len())
        .map_err(|_| UploadError::WinHttp("body larger than 4 GiB".into()))?;

    // SAFETY: request live; None headers reuses what we added.
    unsafe { WinHttpSendRequest(request.raw(), None, None, 0, total_len, 0) }
        .map_err(|e| UploadError::WinHttp(format!("SendRequest: {e}")))?;

    // Stream body: envelope header → PNG (chunked) → envelope footer.
    write_all(&request, &header)?;
    for chunk in bytes.chunks(CHUNK) {
        write_all(&request, chunk)?;
    }
    write_all(&request, &footer)?;

    // SAFETY: request live; lpReserved null per API.
    unsafe { WinHttpReceiveResponse(request.raw(), ptr::null_mut()) }
        .map_err(|e| UploadError::WinHttp(format!("ReceiveResponse: {e}")))?;

    let mut response = read_response(&request)?;
    // Trim in place — avoids the second String allocation.
    let end = response.trim_end().len();
    response.truncate(end);
    let start = response.len() - response.trim_start().len();
    if start > 0 {
        response.drain(..start);
    }
    if !response.starts_with("https://") && !response.starts_with("http://") {
        return Err(UploadError::Rejected(response));
    }
    Ok(response)
}

struct UploadShape {
    host: &'static str,
    path: &'static str,
    text_fields: &'static [(&'static str, &'static str)],
    file_field: &'static str,
    referer: Option<&'static str>,
}

fn upload_shape(service: UploadService) -> UploadShape {
    match service {
        UploadService::Catbox => UploadShape {
            host: "catbox.moe",
            path: "/user/api.php",
            text_fields: &[("reqtype", "fileupload")],
            file_field: "fileToUpload",
            referer: Some("https://catbox.moe/"),
        },
        UploadService::Zerox0 => UploadShape {
            host: "0x0.st",
            path: "/",
            text_fields: &[],
            file_field: "file",
            referer: None,
        },
    }
}

fn write_all(request: &Handle, data: &[u8]) -> Result<(), UploadError> {
    let mut written: u32 = 0;
    let len = u32::try_from(data.len())
        .map_err(|_| UploadError::WinHttp("write chunk > 4 GiB".into()))?;
    // SAFETY: request live; data slice valid for len bytes; written is stack.
    unsafe {
        WinHttpWriteData(
            request.raw(),
            Some(data.as_ptr().cast()),
            len,
            &raw mut written,
        )
    }
    .map_err(|e| UploadError::WinHttp(format!("WriteData: {e}")))?;
    if written != len {
        return Err(UploadError::WinHttp(format!(
            "WriteData short: {written}/{len}"
        )));
    }
    Ok(())
}

/// Build envelope (multipart headers + closing boundary). The PNG
/// bytes ride between header and footer via separate WriteData calls,
/// avoiding a copy into a composite buffer.
fn build_envelope(
    text_fields: &[(&str, &str)],
    file_field: &str,
    filename: &str,
) -> (Vec<u8>, Vec<u8>) {
    let mut header = Vec::with_capacity(512);
    for (name, value) in text_fields {
        write_text_field(&mut header, name, value);
    }
    // Sanitize: drop non-ASCII-printable, quotes, CR/LF. Filename's
    // only role downstream is preserving the extension.
    let safe_name: String = filename
        .chars()
        .map(|c| {
            if (c.is_ascii_graphic() && c != '"') || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    header.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    header.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{safe_name}\"\r\n"
        )
        .as_bytes(),
    );
    header.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");

    let footer = format!("\r\n--{BOUNDARY}--\r\n").into_bytes();
    (header, footer)
}

fn write_text_field(out: &mut Vec<u8>, name: &str, value: &str) {
    out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    out.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn read_response(request: &Handle) -> Result<String, UploadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    loop {
        let mut available: u32 = 0;
        // SAFETY: request live; available is stack-owned.
        unsafe { WinHttpQueryDataAvailable(request.raw(), &raw mut available) }
            .map_err(|e| UploadError::WinHttp(format!("QueryDataAvailable: {e}")))?;
        if available == 0 {
            break;
        }
        let want = (available as usize).min(chunk.len());
        let mut read: u32 = 0;
        let want_u32 = u32::try_from(want).expect("want <= chunk.len() <= u32::MAX");
        // SAFETY: request live; chunk valid for chunk.len() bytes.
        unsafe {
            WinHttpReadData(
                request.raw(),
                chunk.as_mut_ptr().cast(),
                want_u32,
                &raw mut read,
            )
        }
        .map_err(|e| UploadError::WinHttp(format!("ReadData: {e}")))?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        // Sanity cap — responses are <100 chars.
        if buf.len() > 64 * 1024 {
            break;
        }
    }
    String::from_utf8(buf).map_err(|_| UploadError::NotUtf8)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catbox_envelope_carries_reqtype() {
        let (header, footer) = build_envelope(&[("reqtype", "fileupload")], "fileToUpload", "a.png");
        let h = std::str::from_utf8(&header).unwrap();
        let f = std::str::from_utf8(&footer).unwrap();
        assert!(h.contains("name=\"reqtype\""));
        assert!(h.contains("name=\"fileToUpload\"; filename=\"a.png\""));
        assert!(h.ends_with("\r\n\r\n"));
        assert_eq!(f, format!("\r\n--{BOUNDARY}--\r\n"));
    }

    #[test]
    fn zerox0_envelope_uses_file_field_and_no_text_fields() {
        let (header, _) = build_envelope(&[], "file", "a.png");
        let h = std::str::from_utf8(&header).unwrap();
        assert!(!h.contains("name=\"reqtype\""));
        assert!(h.contains("name=\"file\"; filename=\"a.png\""));
    }

    #[test]
    fn upload_shape_catbox_uses_referer() {
        let s = upload_shape(UploadService::Catbox);
        assert_eq!(s.host, "catbox.moe");
        assert_eq!(s.file_field, "fileToUpload");
        assert!(s.referer.is_some());
    }

    #[test]
    fn upload_shape_zerox0_is_self_contained() {
        let s = upload_shape(UploadService::Zerox0);
        assert_eq!(s.host, "0x0.st");
        assert_eq!(s.file_field, "file");
        assert!(s.text_fields.is_empty());
        assert!(s.referer.is_none());
    }

    #[test]
    fn filename_strips_control_chars_and_quotes() {
        let (header, _) = build_envelope(&[("reqtype", "fileupload")], "fileToUpload", "a\"b\nc.png");
        let h = std::str::from_utf8(&header).unwrap();
        assert!(h.contains("filename=\"a_b_c.png\""));
    }

    #[test]
    fn wide_appends_nul() {
        let w = wide("ab");
        assert_eq!(w, vec![u16::from(b'a'), u16::from(b'b'), 0]);
    }
}
