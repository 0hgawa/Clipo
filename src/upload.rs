//! Upload a screenshot to the user's own Cloudflare R2 (or any
//! S3-compatible) bucket via WinHTTP + a hand-rolled AWS Signature V4.
//!
//! Bring-your-own-keys: every credential lives in the user's settings,
//! never embedded in the binary. Returns the public URL of the stored object.

use std::ffi::c_void;
use std::fmt::Write as _;
use std::ptr;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts, WinHttpWriteData, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
    WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_ADDREQ_FLAG_REPLACE, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::System::SystemInformation::GetSystemTime;

type HmacSha256 = Hmac<Sha256>;

const USER_AGENT: &str = "Clipo/0.1 (+https://github.com/0hgawa/Clipo)";
const SERVICE: &str = "s3";
const CHUNK: usize = 64 * 1024;

const DNS_TIMEOUT_MS: i32 = 10_000;
const CONNECT_TIMEOUT_MS: i32 = 15_000;
const SEND_TIMEOUT_MS: i32 = 90_000;
const RECEIVE_TIMEOUT_MS: i32 = 30_000;

/// Borrowed view of an S3-compatible target. Any S3 provider works: Cloudflare
/// R2, AWS S3, Backblaze B2, Wasabi, MinIO — only endpoint + region differ.
pub struct S3Target<'a> {
    pub endpoint: &'a str,
    pub region: &'a str,
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub bucket: &'a str,
    pub public_url: &'a str,
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("winhttp: {0}")]
    WinHttp(String),
    #[error("upload rejected (HTTP {status}): {body}")]
    Rejected { status: u32, body: String },
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
        // SAFETY: `self.0` is a handle WinHTTP returned (non-null, checked in
        // `Handle::new`) and is closed exactly once — Drop runs once per Handle.
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// PUT `bytes` to `<bucket>/<key>`, signed with SigV4. Returns the public URL
/// (`<public_url>/<key>`). Blocking — call from a worker thread.
pub fn upload(
    target: &S3Target<'_>,
    bytes: &[u8],
    key: &str,
    content_type: &str,
) -> Result<String, UploadError> {
    let host = host_of(target.endpoint);
    let canonical_uri = format!("/{}/{}", uri_encode(target.bucket), uri_encode(key));
    let payload_hash = sha256_hex(bytes);
    let (amz_date, date_stamp) = timestamps();

    let canonical_headers = format!(
        "content-type:{content_type}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
    );
    let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date";
    let canonical_request =
        format!("PUT\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", target.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signing_key = signing_key(target.secret_access_key, &date_stamp, target.region);
    let signature = hex(&hmac(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        target.access_key_id
    );

    let user_agent = wide(USER_AGENT);
    let session = Handle::new(
        // SAFETY: `user_agent` is a NUL-terminated UTF-16 buffer that outlives
        // the call; the remaining arguments are null or constants.
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
    // SAFETY: `session` is a live, non-null WinHTTP handle; the timeouts are
    // plain integers.
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

    let host_wide = wide(&host);
    let connection = Handle::new(
        // SAFETY: `session` is live; `host_wide` is a NUL-terminated UTF-16
        // buffer that outlives the call.
        unsafe { WinHttpConnect(session.raw(), PCWSTR(host_wide.as_ptr()), 443, 0) },
        "WinHttpConnect",
    )?;

    let path_wide = wide(&canonical_uri);
    let verb_wide = wide("PUT");
    let request = Handle::new(
        // SAFETY: `connection` is live; `verb_wide`/`path_wide` are NUL-
        // terminated UTF-16 buffers that outlive the call; the rest is null/flags.
        unsafe {
            WinHttpOpenRequest(
                connection.raw(),
                PCWSTR(verb_wide.as_ptr()),
                PCWSTR(path_wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null_mut(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;

    add_header(&request, &format!("Content-Type: {content_type}"))?;
    add_header(&request, &format!("x-amz-content-sha256: {payload_hash}"))?;
    add_header(&request, &format!("x-amz-date: {amz_date}"))?;
    add_header(&request, &format!("Authorization: {authorization}"))?;

    let total_len = u32::try_from(bytes.len())
        .map_err(|_| UploadError::WinHttp("body larger than 4 GiB".into()))?;
    // SAFETY: `request` is live; no optional buffers are passed (`None`), only
    // the body length that follows via WriteData.
    unsafe { WinHttpSendRequest(request.raw(), None, None, 0, total_len, 0) }
        .map_err(|e| UploadError::WinHttp(format!("SendRequest: {e}")))?;
    for chunk in bytes.chunks(CHUNK) {
        write_all(&request, chunk)?;
    }
    // SAFETY: `request` is a live request handle; the reserved pointer is null
    // as the API requires.
    unsafe { WinHttpReceiveResponse(request.raw(), ptr::null_mut()) }
        .map_err(|e| UploadError::WinHttp(format!("ReceiveResponse: {e}")))?;

    let status = status_code(&request)?;
    if status == 200 || status == 204 {
        let base = target.public_url.trim_end_matches('/');
        Ok(format!("{base}/{key}"))
    } else {
        let raw = read_response_bytes(&request, 16 * 1024).unwrap_or_default();
        let body = String::from_utf8_lossy(&raw).into_owned();
        let detail = s3_error_message(&body).unwrap_or(body);
        Err(UploadError::Rejected { status, body: detail })
    }
}

/// Plain HTTPS GET as text (follows redirects automatically; 16 KiB cap).
/// Used by the update check. Blocking — call from a worker thread.
pub fn get(url: &str) -> Result<String, UploadError> {
    String::from_utf8(http_get_bytes(url, 16 * 1024)?).map_err(|_| UploadError::NotUtf8)
}

/// Download `url` as raw bytes (the update `clipo.exe`; 256 MiB ceiling). Follows
/// the GitHub release redirect automatically. Blocking — call from a worker.
pub fn download(url: &str) -> Result<Vec<u8>, UploadError> {
    http_get_bytes(url, 256 * 1024 * 1024)
}

fn http_get_bytes(url: &str, max: usize) -> Result<Vec<u8>, UploadError> {
    let host = host_of(url);
    let path = path_of(url);
    let user_agent = wide(USER_AGENT);
    let session = Handle::new(
        // SAFETY: `user_agent` is a NUL-terminated UTF-16 buffer that outlives
        // the call; the remaining arguments are null or constants.
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
    // SAFETY: `session` is a live, non-null WinHTTP handle; the timeouts are
    // plain integers.
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

    let host_wide = wide(&host);
    let connection = Handle::new(
        // SAFETY: `session` is live; `host_wide` is a NUL-terminated UTF-16
        // buffer that outlives the call.
        unsafe { WinHttpConnect(session.raw(), PCWSTR(host_wide.as_ptr()), 443, 0) },
        "WinHttpConnect",
    )?;
    let path_wide = wide(&path);
    let verb_wide = wide("GET");
    let request = Handle::new(
        // SAFETY: `connection` is live; `verb_wide`/`path_wide` are NUL-
        // terminated UTF-16 buffers that outlive the call; the rest is null/flags.
        unsafe {
            WinHttpOpenRequest(
                connection.raw(),
                PCWSTR(verb_wide.as_ptr()),
                PCWSTR(path_wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null_mut(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;
    // SAFETY: `request` is live; a GET carries no body (all lengths 0, no buffers).
    unsafe { WinHttpSendRequest(request.raw(), None, None, 0, 0, 0) }
        .map_err(|e| UploadError::WinHttp(format!("SendRequest: {e}")))?;
    // SAFETY: `request` is a live request handle; the reserved pointer is null
    // as the API requires.
    unsafe { WinHttpReceiveResponse(request.raw(), ptr::null_mut()) }
        .map_err(|e| UploadError::WinHttp(format!("ReceiveResponse: {e}")))?;
    let status = status_code(&request)?;
    let body = read_response_bytes(&request, max).unwrap_or_default();
    if status == 200 {
        Ok(body)
    } else {
        Err(UploadError::Rejected { status, body: String::from_utf8_lossy(&body).into_owned() })
    }
}

fn path_of(url: &str) -> String {
    let e = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    e.find('/').map_or_else(|| "/".to_owned(), |i| e[i..].to_owned())
}

fn s3_error_message(body: &str) -> Option<String> {
    for tag in ["Message", "Code"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let Some(start) = body.find(&open) {
            let from = start + open.len();
            if let Some(rel) = body[from..].find(&close) {
                let msg = body[from..from + rel].trim();
                if !msg.is_empty() {
                    return Some(msg.to_owned());
                }
            }
        }
    }
    None
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, date_stamp: &str, region: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, SERVICE.as_bytes());
    hmac(&k_service, b"aws4_request")
}

fn host_of(endpoint: &str) -> String {
    let e = endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    e.split('/').next().unwrap_or(e).to_owned()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// `(amz_date, date_stamp)` in UTC, e.g. `("20260529T143210Z", "20260529")`.
fn timestamps() -> (String, String) {
    // SAFETY: GetSystemTime takes no arguments and only returns the current
    // time by value — always sound to call.
    let st = unsafe { GetSystemTime() };
    let amz_date = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    );
    let date_stamp = format!("{:04}{:02}{:02}", st.wYear, st.wMonth, st.wDay);
    (amz_date, date_stamp)
}

fn add_header(request: &Handle, header: &str) -> Result<(), UploadError> {
    let header_wide = wide(header);
    // SAFETY: `request` is live; the header slice (without the trailing NUL)
    // points into `header_wide`, which outlives the call.
    unsafe {
        WinHttpAddRequestHeaders(
            request.raw(),
            &header_wide[..header_wide.len() - 1],
            WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
        )
    }
    .map_err(|e| UploadError::WinHttp(format!("AddRequestHeaders: {e}")))
}

fn write_all(request: &Handle, data: &[u8]) -> Result<(), UploadError> {
    let mut written: u32 = 0;
    let len =
        u32::try_from(data.len()).map_err(|_| UploadError::WinHttp("write chunk > 4 GiB".into()))?;
    // SAFETY: `request` is live; `data` outlives the call and `len` is its exact
    // length; `written` is a valid out-pointer.
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

fn status_code(request: &Handle) -> Result<u32, UploadError> {
    let mut code: u32 = 0;
    let mut size = u32::try_from(std::mem::size_of::<u32>()).expect("size_of u32 fits u32");
    // SAFETY: `request` is live; `code`/`size` are valid out-pointers sized for a
    // u32 (NUMBER flag) and the header-name pointer is null for a status query.
    unsafe {
        WinHttpQueryHeaders(
            request.raw(),
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(std::ptr::from_mut(&mut code).cast()),
            &raw mut size,
            ptr::null_mut(),
        )
    }
    .map_err(|e| UploadError::WinHttp(format!("QueryHeaders status: {e}")))?;
    Ok(code)
}

fn read_response_bytes(request: &Handle, max: usize) -> Result<Vec<u8>, UploadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    loop {
        let mut available: u32 = 0;
        // SAFETY: `request` is live; `available` is a valid out-pointer.
        unsafe { WinHttpQueryDataAvailable(request.raw(), &raw mut available) }
            .map_err(|e| UploadError::WinHttp(format!("QueryDataAvailable: {e}")))?;
        if available == 0 {
            break;
        }
        let want = (available as usize).min(chunk.len());
        let mut read: u32 = 0;
        let want_u32 = u32::try_from(want).expect("want <= chunk.len() <= u32::MAX");
        // SAFETY: `request` is live; `chunk` is a valid buffer of `want_u32`
        // bytes and `read` is a valid out-pointer.
        unsafe { WinHttpReadData(request.raw(), chunk.as_mut_ptr().cast(), want_u32, &raw mut read) }
            .map_err(|e| UploadError::WinHttp(format!("ReadData: {e}")))?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        if buf.len() > max {
            break;
        }
    }
    Ok(buf)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn sha256_matches_known_vector() {
        // SHA-256("") well-known digest.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn signing_key_matches_aws_reference() {
        // AWS SigV4 reference vector (docs.aws.amazon.com "derive-signing-key"):
        // secret=wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY, date=20150830,
        // region=us-east-1, service=iam.
        let k_date = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20150830");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"iam");
        let k_signing = hmac(&k_service, b"aws4_request");
        assert_eq!(
            hex(&k_signing),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn uri_encode_passes_unreserved_and_escapes_the_rest() {
        assert_eq!(uri_encode("clipo-2026.png"), "clipo-2026.png");
        assert_eq!(uri_encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn s3_error_message_extracts_message_then_code() {
        let xml = "<?xml version=\"1.0\"?><Error><Code>NoSuchBucket</Code>\
                   <Message>The specified bucket does not exist.</Message></Error>";
        assert_eq!(
            s3_error_message(xml).as_deref(),
            Some("The specified bucket does not exist.")
        );
        assert_eq!(
            s3_error_message("<Error><Code>AccessDenied</Code></Error>").as_deref(),
            Some("AccessDenied")
        );
        assert_eq!(s3_error_message("not xml"), None);
    }
}
