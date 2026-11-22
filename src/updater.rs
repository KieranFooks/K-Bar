//! Ultra-lightweight `#![no_std]` auto-updater primitives.
//!
//! Uses ephemeral `LoadLibraryW` + `GetProcAddress` + `FreeLibrary` for `winhttp.dll`,
//! `bcrypt.dll`, `comctl32.dll`, and `shell32.dll` so idle RAM stays unchanged and
//! static PE imports stay restricted to katt's core DLL allowlist.

use core::ffi::c_void;
use core::mem::transmute;
use core::ptr::{null, null_mut};

use crate::w;
use crate::win32::{
    ActivateActCtx, CloseHandle, CreateActCtxW, CreateFileW, CreateProcessW, DeactivateActCtx,
    DeleteFileW, FreeLibrary, GetModuleFileNameW, GetProcAddress, GetTempPathW, LoadLibraryW,
    MessageBoxW, MoveFileExW, ReadFile, RegGetValueW, ReleaseActCtx, Sleep, WriteFile, ACTCTXW,
    BOOL, CREATE_ALWAYS, ERROR_SUCCESS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, GENERIC_READ,
    GENERIC_WRITE, HANDLE, HINSTANCE, HKEY_CURRENT_USER, HWND, IDYES, INVALID_HANDLE_VALUE,
    MB_ICONINFORMATION, MB_YESNO, MOVEFILE_REPLACE_EXISTING, OPEN_EXISTING, PROCESS_INFORMATION,
    RRF_RT_REG_SZ, STARTUPINFOW,
};

pub type CalVer = [u16; 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateError {
    InvalidUrl,
    DllLoadFailed,
    NetworkFailed,
    BadHttpStatus(u32),
    MissingLocation,
    InvalidTag,
    InvalidHashFile,
    HashMismatch,
    InvalidPeBinary,
    FileIoFailed,
    SwapFailed,
    SpawnFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptOutcome {
    pub action: PromptAction,
    pub skip_version: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAction {
    UpdateNow,
    Later,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagCheck {
    pub version: CalVer,
    pub tag_utf8: [u8; 48],
    pub tag_len: usize,
    pub release_url_utf8: [u8; 256],
    pub release_url_len: usize,
}

impl TagCheck {
    pub fn tag_str(&self) -> &str {
        core::str::from_utf8(&self.tag_utf8[..self.tag_len]).unwrap_or("")
    }

    pub fn release_url_str(&self) -> &str {
        core::str::from_utf8(&self.release_url_utf8[..self.release_url_len]).unwrap_or("")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrlParts {
    pub secure: bool,
    pub port: u16,
    pub host_utf16: [u16; 128],
    pub path_utf16: [u16; 256],
}

/// Parse CalVer tags like `v2026.11.23`, `2026.11.23.1`, or URLs ending in `/tag/v2026.11.23`.
pub fn parse_calver(raw: &str) -> Option<CalVer> {
    let trimmed = raw.trim();
    let tail = match trimmed.rfind('/') {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    };
    let ver_str = tail.strip_prefix('v').unwrap_or(tail);
    if ver_str.is_empty() {
        return None;
    }

    let mut out = [0u16; 4];
    let mut count = 0usize;
    for part in ver_str.split('.') {
        if count >= 4 || part.is_empty() {
            return None;
        }
        let mut val: u32 = 0;
        for &b in part.as_bytes() {
            if !b.is_ascii_digit() {
                return None;
            }
            val = val.saturating_mul(10).saturating_add((b - b'0') as u32);
        }
        if val > u16::MAX as u32 {
            return None;
        }
        out[count] = val as u16;
        count += 1;
    }

    if count < 3 || out[0] == 0 {
        return None;
    }
    Some(out)
}

#[inline]
pub fn is_newer(remote: CalVer, local: CalVer) -> bool {
    remote > local
}

#[inline]
pub fn is_dev_version(local: CalVer) -> bool {
    local == [0, 1, 0, 0]
}

/// Parse an ASCII `http://` or `https://` URL into host, port, and path (`UTF-16Z`).
pub fn parse_url(url: &str) -> Option<UrlParts> {
    let (secure, default_port, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, 443u16, r)
    } else {
        let r = url.strip_prefix("http://")?;
        (false, 80u16, r)
    };

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }

    let (host, port) = match authority.rfind(':') {
        Some(idx) if !authority[idx + 1..].is_empty() => {
            let mut p: u32 = 0;
            for &b in &authority.as_bytes()[idx + 1..] {
                if !b.is_ascii_digit() {
                    return None;
                }
                p = p * 10 + (b - b'0') as u32;
                if p > 65535 {
                    return None;
                }
            }
            if p == 0 {
                return None;
            }
            (&authority[..idx], p as u16)
        }
        _ => (authority, default_port),
    };

    let mut parts = UrlParts {
        secure,
        port,
        host_utf16: [0u16; 128],
        path_utf16: [0u16; 256],
    };
    if !ascii_to_utf16z(host, &mut parts.host_utf16) {
        return None;
    }
    if !ascii_to_utf16z(path, &mut parts.path_utf16) {
        return None;
    }
    Some(parts)
}

pub fn parse_sha256_hex(text: &str) -> Option<[u8; 32]> {
    let s = text.trim();
    if s.len() < 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0usize;
    while i < 32 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

#[inline]
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn ascii_to_utf16z(src: &str, dst: &mut [u16]) -> bool {
    let bytes = src.as_bytes();
    if bytes.len() + 1 > dst.len() {
        return false;
    }
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] >= 0x80 {
            return false;
        }
        dst[i] = bytes[i] as u16;
        i += 1;
    }
    dst[i] = 0;
    true
}

fn utf16_to_ascii_slice(src: &[u16], dst: &mut [u8]) -> usize {
    let mut i = 0usize;
    while i < src.len() && i < dst.len() && src[i] != 0 {
        dst[i] = if src[i] < 0x80 { src[i] as u8 } else { b'?' };
        i += 1;
    }
    i
}

// ============================================================================
// Ephemeral WinHTTP (`winhttp.dll`)
// ============================================================================

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_OPTION_REDIRECT_POLICY: u32 = 88;
const WINHTTP_OPTION_REDIRECT_POLICY_NEVER: u32 = 0;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_LOCATION: u32 = 33;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

type FnWinHttpOpen =
    unsafe extern "system" fn(*const u16, u32, *const u16, *const u16, u32) -> HANDLE;
type FnWinHttpConnect = unsafe extern "system" fn(HANDLE, *const u16, u16, u32) -> HANDLE;
type FnWinHttpOpenRequest = unsafe extern "system" fn(
    HANDLE,
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    *const *const u16,
    u32,
) -> HANDLE;
type FnWinHttpSetOption = unsafe extern "system" fn(HANDLE, u32, *const c_void, u32) -> BOOL;
type FnWinHttpSendRequest =
    unsafe extern "system" fn(HANDLE, *const u16, u32, *const c_void, u32, u32, usize) -> BOOL;
type FnWinHttpReceiveResponse = unsafe extern "system" fn(HANDLE, *mut c_void) -> BOOL;
type FnWinHttpQueryHeaders =
    unsafe extern "system" fn(HANDLE, u32, *const u16, *mut c_void, *mut u32, *mut u32) -> BOOL;
type FnWinHttpReadData = unsafe extern "system" fn(HANDLE, *mut u8, u32, *mut u32) -> BOOL;
type FnWinHttpCloseHandle = unsafe extern "system" fn(HANDLE) -> BOOL;

struct WinHttpApi {
    module: HINSTANCE,
    open: FnWinHttpOpen,
    connect: FnWinHttpConnect,
    open_request: FnWinHttpOpenRequest,
    set_option: FnWinHttpSetOption,
    send_request: FnWinHttpSendRequest,
    receive_response: FnWinHttpReceiveResponse,
    query_headers: FnWinHttpQueryHeaders,
    read_data: FnWinHttpReadData,
    close_handle: FnWinHttpCloseHandle,
}

impl WinHttpApi {
    unsafe fn load() -> Option<Self> {
        let module = LoadLibraryW(w!("winhttp.dll"));
        if module.is_null() {
            return None;
        }
        macro_rules! proc {
            ($name:literal, $ty:ty) => {{
                let p = GetProcAddress(module, concat!($name, "\0").as_ptr());
                if p.is_null() {
                    let _ = FreeLibrary(module);
                    return None;
                }
                transmute::<*const c_void, $ty>(p)
            }};
        }
        Some(Self {
            module,
            open: proc!("WinHttpOpen", FnWinHttpOpen),
            connect: proc!("WinHttpConnect", FnWinHttpConnect),
            open_request: proc!("WinHttpOpenRequest", FnWinHttpOpenRequest),
            set_option: proc!("WinHttpSetOption", FnWinHttpSetOption),
            send_request: proc!("WinHttpSendRequest", FnWinHttpSendRequest),
            receive_response: proc!("WinHttpReceiveResponse", FnWinHttpReceiveResponse),
            query_headers: proc!("WinHttpQueryHeaders", FnWinHttpQueryHeaders),
            read_data: proc!("WinHttpReadData", FnWinHttpReadData),
            close_handle: proc!("WinHttpCloseHandle", FnWinHttpCloseHandle),
        })
    }
}

impl Drop for WinHttpApi {
    fn drop(&mut self) {
        if !self.module.is_null() {
            unsafe {
                let _ = FreeLibrary(self.module);
            }
        }
    }
}

/// Perform a `HEAD` request to `latest_url` without following redirects, parsing the
/// CalVer tag from the `Location` header (e.g. `.../releases/tag/v2026.11.23`).
pub fn check_latest_tag(latest_url: &str) -> Result<TagCheck, UpdateError> {
    let parts = parse_url(latest_url).ok_or(UpdateError::InvalidUrl)?;
    unsafe {
        let api = WinHttpApi::load().ok_or(UpdateError::DllLoadFailed)?;
        let session = (api.open)(
            w!("katt-updater/1.0"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        );
        if session.is_null() {
            return Err(UpdateError::NetworkFailed);
        }

        let res = check_latest_tag_inner(&api, session, &parts);
        let _ = (api.close_handle)(session);
        res
    }
}

unsafe fn check_latest_tag_inner(
    api: &WinHttpApi,
    session: HANDLE,
    parts: &UrlParts,
) -> Result<TagCheck, UpdateError> {
    let conn = (api.connect)(session, parts.host_utf16.as_ptr(), parts.port, 0);
    if conn.is_null() {
        return Err(UpdateError::NetworkFailed);
    }

    let flags = if parts.secure { WINHTTP_FLAG_SECURE } else { 0 };
    let req = (api.open_request)(
        conn,
        w!("HEAD"),
        parts.path_utf16.as_ptr(),
        null(),
        null(),
        null(),
        flags,
    );
    if req.is_null() {
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::NetworkFailed);
    }

    let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
    let _ = (api.set_option)(
        req,
        WINHTTP_OPTION_REDIRECT_POLICY,
        &policy as *const u32 as *const c_void,
        core::mem::size_of::<u32>() as u32,
    );

    let ok = (api.send_request)(req, null(), 0, null(), 0, 0, 0) != 0
        && (api.receive_response)(req, null_mut()) != 0;
    if !ok {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::NetworkFailed);
    }

    let mut status: u32 = 0;
    let mut status_sz: u32 = core::mem::size_of::<u32>() as u32;
    let status_ok = (api.query_headers)(
        req,
        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
        null(),
        &mut status as *mut u32 as *mut c_void,
        &mut status_sz,
        null_mut(),
    ) != 0;
    if !status_ok {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::NetworkFailed);
    }
    if !(300..400).contains(&status) {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::BadHttpStatus(status));
    }

    let mut loc_w = [0u16; 256];
    let mut loc_bytes: u32 = (loc_w.len() * 2) as u32;
    let loc_ok = (api.query_headers)(
        req,
        WINHTTP_QUERY_LOCATION,
        null(),
        loc_w.as_mut_ptr() as *mut c_void,
        &mut loc_bytes,
        null_mut(),
    ) != 0;

    let _ = (api.close_handle)(req);
    let _ = (api.close_handle)(conn);
    if !loc_ok {
        return Err(UpdateError::MissingLocation);
    }

    let mut loc_u8 = [0u8; 256];
    let loc_len = utf16_to_ascii_slice(&loc_w, &mut loc_u8);
    let loc_str = core::str::from_utf8(&loc_u8[..loc_len]).map_err(|_| UpdateError::InvalidTag)?;
    let version = parse_calver(loc_str).ok_or(UpdateError::InvalidTag)?;

    let tag_slice = match loc_str.rfind('/') {
        Some(idx) => &loc_str[idx + 1..],
        None => loc_str,
    };
    let mut result = TagCheck {
        version,
        tag_utf8: [0u8; 48],
        tag_len: tag_slice.len().min(48),
        release_url_utf8: [0u8; 256],
        release_url_len: loc_len.min(256),
    };
    result.tag_utf8[..result.tag_len].copy_from_slice(&tag_slice.as_bytes()[..result.tag_len]);
    result.release_url_utf8[..result.release_url_len]
        .copy_from_slice(&loc_u8[..result.release_url_len]);
    Ok(result)
}

// ============================================================================
// Ephemeral BCrypt (`bcrypt.dll`) SHA-256 + Download
// ============================================================================

type FnBCryptOpenAlgorithmProvider =
    unsafe extern "system" fn(*mut HANDLE, *const u16, *const u16, u32) -> i32;
type FnBCryptCloseAlgorithmProvider = unsafe extern "system" fn(HANDLE, u32) -> i32;
type FnBCryptCreateHash =
    unsafe extern "system" fn(HANDLE, *mut HANDLE, *mut u8, u32, *const u8, u32, u32) -> i32;
type FnBCryptHashData = unsafe extern "system" fn(HANDLE, *const u8, u32, u32) -> i32;
type FnBCryptFinishHash = unsafe extern "system" fn(HANDLE, *mut u8, u32, u32) -> i32;
type FnBCryptDestroyHash = unsafe extern "system" fn(HANDLE) -> i32;

struct BCryptSha256 {
    module: HINSTANCE,
    alg: HANDLE,
    hash: HANDLE,
    close_alg: FnBCryptCloseAlgorithmProvider,
    hash_data: FnBCryptHashData,
    finish_hash: FnBCryptFinishHash,
    destroy_hash: FnBCryptDestroyHash,
}

impl BCryptSha256 {
    unsafe fn new() -> Option<Self> {
        let module = LoadLibraryW(w!("bcrypt.dll"));
        if module.is_null() {
            return None;
        }
        macro_rules! proc {
            ($name:literal, $ty:ty) => {{
                let p = GetProcAddress(module, concat!($name, "\0").as_ptr());
                if p.is_null() {
                    let _ = FreeLibrary(module);
                    return None;
                }
                transmute::<*const c_void, $ty>(p)
            }};
        }
        let open_alg = proc!("BCryptOpenAlgorithmProvider", FnBCryptOpenAlgorithmProvider);
        let close_alg = proc!(
            "BCryptCloseAlgorithmProvider",
            FnBCryptCloseAlgorithmProvider
        );
        let create_hash = proc!("BCryptCreateHash", FnBCryptCreateHash);
        let hash_data = proc!("BCryptHashData", FnBCryptHashData);
        let finish_hash = proc!("BCryptFinishHash", FnBCryptFinishHash);
        let destroy_hash = proc!("BCryptDestroyHash", FnBCryptDestroyHash);

        let mut alg: HANDLE = null_mut();
        if open_alg(&mut alg, w!("SHA256"), null(), 0) < 0 || alg.is_null() {
            let _ = FreeLibrary(module);
            return None;
        }
        let mut hash: HANDLE = null_mut();
        if create_hash(alg, &mut hash, null_mut(), 0, null(), 0, 0) < 0 || hash.is_null() {
            let _ = close_alg(alg, 0);
            let _ = FreeLibrary(module);
            return None;
        }
        Some(Self {
            module,
            alg,
            hash,
            close_alg,
            hash_data,
            finish_hash,
            destroy_hash,
        })
    }

    unsafe fn update(&self, data: &[u8]) -> bool {
        (self.hash_data)(self.hash, data.as_ptr(), data.len() as u32, 0) >= 0
    }

    unsafe fn finish(self, out: &mut [u8; 32]) -> bool {
        (self.finish_hash)(self.hash, out.as_mut_ptr(), 32, 0) >= 0
    }
}

impl Drop for BCryptSha256 {
    fn drop(&mut self) {
        unsafe {
            if !self.hash.is_null() {
                let _ = (self.destroy_hash)(self.hash);
            }
            if !self.alg.is_null() {
                let _ = (self.close_alg)(self.alg, 0);
            }
            if !self.module.is_null() {
                let _ = FreeLibrary(self.module);
            }
        }
    }
}

/// Fetch a `.sha256` file via HTTP/HTTPS and parse its 64-hex-digit digest.
pub fn fetch_sha256(sha_url: &str) -> Result<[u8; 32], UpdateError> {
    let parts = parse_url(sha_url).ok_or(UpdateError::InvalidUrl)?;
    unsafe {
        let api = WinHttpApi::load().ok_or(UpdateError::DllLoadFailed)?;
        let session = (api.open)(
            w!("katt-updater/1.0"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        );
        if session.is_null() {
            return Err(UpdateError::NetworkFailed);
        }

        let res = fetch_small_text(&api, session, &parts);
        let _ = (api.close_handle)(session);
        let (buf, len) = res?;
        let text = core::str::from_utf8(&buf[..len]).map_err(|_| UpdateError::InvalidHashFile)?;
        parse_sha256_hex(text).ok_or(UpdateError::InvalidHashFile)
    }
}

unsafe fn open_get_request(
    api: &WinHttpApi,
    session: HANDLE,
    parts: &UrlParts,
) -> Result<(HANDLE, HANDLE), UpdateError> {
    let conn = (api.connect)(session, parts.host_utf16.as_ptr(), parts.port, 0);
    if conn.is_null() {
        return Err(UpdateError::NetworkFailed);
    }
    let flags = if parts.secure { WINHTTP_FLAG_SECURE } else { 0 };
    let req = (api.open_request)(
        conn,
        w!("GET"),
        parts.path_utf16.as_ptr(),
        null(),
        null(),
        null(),
        flags,
    );
    if req.is_null() {
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::NetworkFailed);
    }

    let ok = (api.send_request)(req, null(), 0, null(), 0, 0, 0) != 0
        && (api.receive_response)(req, null_mut()) != 0;
    if !ok {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::NetworkFailed);
    }

    let mut status: u32 = 0;
    let mut status_sz: u32 = core::mem::size_of::<u32>() as u32;
    if (api.query_headers)(
        req,
        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
        null(),
        &mut status as *mut u32 as *mut c_void,
        &mut status_sz,
        null_mut(),
    ) == 0
    {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::NetworkFailed);
    }
    if status != 200 {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::BadHttpStatus(status));
    }
    Ok((conn, req))
}

unsafe fn fetch_small_text(
    api: &WinHttpApi,
    session: HANDLE,
    parts: &UrlParts,
) -> Result<([u8; 256], usize), UpdateError> {
    let (conn, req) = open_get_request(api, session, parts)?;
    let mut buf = [0u8; 256];
    let mut total = 0usize;
    while total < buf.len() {
        let mut got: u32 = 0;
        let ok = (api.read_data)(
            req,
            buf[total..].as_mut_ptr(),
            (buf.len() - total) as u32,
            &mut got,
        );
        if ok == 0 {
            let _ = (api.close_handle)(req);
            let _ = (api.close_handle)(conn);
            return Err(UpdateError::NetworkFailed);
        }
        if got == 0 {
            break;
        }
        total += got as usize;
    }
    let _ = (api.close_handle)(req);
    let _ = (api.close_handle)(conn);
    Ok((buf, total))
}

/// Download `download_url` into `dst_path_utf16`, verifying SHA-256 against `expected_sha256`
/// and checking the PE (`MZ` + `PE\0\0`) signature before returning.
pub fn download_and_verify_pe(
    download_url: &str,
    dst_path_utf16: &[u16],
    expected_sha256: &[u8; 32],
) -> Result<u64, UpdateError> {
    let parts = parse_url(download_url).ok_or(UpdateError::InvalidUrl)?;
    unsafe {
        let api = WinHttpApi::load().ok_or(UpdateError::DllLoadFailed)?;
        let hasher = BCryptSha256::new().ok_or(UpdateError::DllLoadFailed)?;
        let session = (api.open)(
            w!("katt-updater/1.0"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        );
        if session.is_null() {
            return Err(UpdateError::NetworkFailed);
        }

        let res = stream_download_inner(&api, session, &parts, dst_path_utf16, hasher, expected_sha256);
        let _ = (api.close_handle)(session);
        if res.is_err() {
            let _ = DeleteFileW(dst_path_utf16.as_ptr());
        }
        res
    }
}

unsafe fn stream_download_inner(
    api: &WinHttpApi,
    session: HANDLE,
    parts: &UrlParts,
    dst_path_utf16: &[u16],
    hasher: BCryptSha256,
    expected_sha256: &[u8; 32],
) -> Result<u64, UpdateError> {
    let (conn, req) = open_get_request(api, session, parts)?;
    let fh = CreateFileW(
        dst_path_utf16.as_ptr(),
        GENERIC_WRITE | GENERIC_READ,
        0,
        null(),
        CREATE_ALWAYS,
        FILE_ATTRIBUTE_NORMAL,
        null_mut(),
    );
    if fh == INVALID_HANDLE_VALUE {
        let _ = (api.close_handle)(req);
        let _ = (api.close_handle)(conn);
        return Err(UpdateError::FileIoFailed);
    }

    let mut chunk = [0u8; 1024];
    let mut total: u64 = 0;
    loop {
        let mut got: u32 = 0;
        let ok = (api.read_data)(req, chunk.as_mut_ptr(), chunk.len() as u32, &mut got);
        if ok == 0 {
            let _ = CloseHandle(fh);
            let _ = (api.close_handle)(req);
            let _ = (api.close_handle)(conn);
            return Err(UpdateError::NetworkFailed);
        }
        if got == 0 {
            break;
        }
        let slice = &chunk[..got as usize];
        if !hasher.update(slice) {
            let _ = CloseHandle(fh);
            let _ = (api.close_handle)(req);
            let _ = (api.close_handle)(conn);
            return Err(UpdateError::DllLoadFailed);
        }
        let mut written: u32 = 0;
        if WriteFile(fh, slice.as_ptr(), got, &mut written, null_mut()) == 0 || written != got {
            let _ = CloseHandle(fh);
            let _ = (api.close_handle)(req);
            let _ = (api.close_handle)(conn);
            return Err(UpdateError::FileIoFailed);
        }
        total += got as u64;
    }

    let _ = (api.close_handle)(req);
    let _ = (api.close_handle)(conn);
    let _ = CloseHandle(fh);

    let mut actual_sha256 = [0u8; 32];
    if !hasher.finish(&mut actual_sha256) {
        return Err(UpdateError::DllLoadFailed);
    }

    let mut diff = 0u8;
    let mut i = 0usize;
    while i < 32 {
        diff |= actual_sha256[i] ^ expected_sha256[i];
        i += 1;
    }
    if diff != 0 {
        return Err(UpdateError::HashMismatch);
    }

    if total < 256 || !verify_pe_header(dst_path_utf16) {
        return Err(UpdateError::InvalidPeBinary);
    }

    Ok(total)
}

unsafe fn verify_pe_header(path_utf16: &[u16]) -> bool {
    let fh = CreateFileW(
        path_utf16.as_ptr(),
        GENERIC_READ,
        FILE_SHARE_READ,
        null(),
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        null_mut(),
    );
    if fh == INVALID_HANDLE_VALUE {
        return false;
    }

    let mut hdr = [0u8; 512];
    let mut got: u32 = 0;
    let ok = ReadFile(fh, hdr.as_mut_ptr(), hdr.len() as u32, &mut got, null_mut()) != 0;
    let _ = CloseHandle(fh);
    if !ok || (got as usize) < 0x44 {
        return false;
    }
    if hdr[0] != b'M' || hdr[1] != b'Z' {
        return false;
    }
    let pe_off = u32::from_le_bytes([hdr[0x3C], hdr[0x3D], hdr[0x3E], hdr[0x3F]]) as usize;
    if pe_off + 4 > got as usize {
        return false;
    }
    hdr[pe_off..pe_off + 4] == *b"PE\0\0"
}

// ============================================================================
// Notification Dialog (`comctl32.dll` TaskDialogIndirect + `shell32.dll`)
// ============================================================================

const TDF_ENABLE_HYPERLINKS: i32 = 0x0001;
const TDF_ALLOW_DIALOG_CANCELLATION: i32 = 0x0008;
const TDF_POSITION_RELATIVE_TO_WINDOW: i32 = 0x1000;
const TDN_HYPERLINK_CLICKED: u32 = 3;
const TD_INFORMATION_ICON: *const u16 = 65533usize as *const u16;

const BTN_UPDATE_NOW: i32 = 1001;
const BTN_LATER: i32 = 1002;

#[repr(C, packed(1))]
struct TaskDialogButton {
    n_button_id: i32,
    psz_button_text: *const u16,
}

type FnTaskDialogCallback =
    Option<unsafe extern "system" fn(HWND, u32, usize, isize, isize) -> i32>;

#[repr(C, packed(1))]
struct TaskDialogConfig {
    cb_size: u32,
    hwnd_parent: HWND,
    h_instance: HINSTANCE,
    dw_flags: i32,
    dw_common_buttons: i32,
    psz_window_title: *const u16,
    h_main_icon: *const u16,
    psz_main_instruction: *const u16,
    psz_content: *const u16,
    c_buttons: u32,
    p_buttons: *const TaskDialogButton,
    n_default_button: i32,
    c_radio_buttons: u32,
    p_radio_buttons: *const TaskDialogButton,
    n_default_radio_button: i32,
    psz_verification_text: *const u16,
    psz_expanded_information: *const u16,
    psz_expanded_control_text: *const u16,
    psz_collapsed_control_text: *const u16,
    h_footer_icon: *const u16,
    psz_footer: *const u16,
    pf_callback: FnTaskDialogCallback,
    lp_callback_data: isize,
    cx_width: u32,
}

const _: () = assert!(core::mem::size_of::<TaskDialogButton>() == 12);
const _: () = assert!(core::mem::size_of::<TaskDialogConfig>() == 160);

type FnTaskDialogIndirect =
    unsafe extern "system" fn(*const TaskDialogConfig, *mut i32, *mut i32, *mut BOOL) -> i32;
type FnShellExecuteW = unsafe extern "system" fn(
    HWND,
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    i32,
) -> HINSTANCE;

unsafe extern "system" fn task_dialog_callback(
    hwnd: HWND,
    msg: u32,
    _wparam: usize,
    lparam: isize,
    _ref_data: isize,
) -> i32 {
    if msg == TDN_HYPERLINK_CLICKED && lparam != 0 {
        let url_ptr = lparam as *const u16;
        let shell32 = LoadLibraryW(w!("shell32.dll"));
        if !shell32.is_null() {
            let p = GetProcAddress(shell32, c"ShellExecuteW".as_ptr().cast::<u8>());
            if !p.is_null() {
                let shell_exec: FnShellExecuteW = transmute(p);
                let _ = shell_exec(hwnd, w!("open"), url_ptr, null(), null(), 1);
            }
            let _ = FreeLibrary(shell32);
        }
    }
    0
}

struct Utf16Builder<const N: usize> {
    buf: [u16; N],
    len: usize,
}

impl<const N: usize> Utf16Builder<N> {
    const fn new() -> Self {
        Self {
            buf: [0u16; N],
            len: 0,
        }
    }

    fn push_ascii(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if self.len + 1 >= N {
                break;
            }
            self.buf[self.len] = if b < 0x80 { b as u16 } else { b'?' as u16 };
            self.len += 1;
        }
        self.buf[self.len] = 0;
    }

    fn as_ptr(&self) -> *const u16 {
        self.buf.as_ptr()
    }
}

const COMCTL6_MANIFEST: &[u8] = b"<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<assembly xmlns=\"urn:schemas-microsoft-com:asm.v1\" manifestVersion=\"1.0\">\r\n\
  <dependency><dependentAssembly><assemblyIdentity type=\"win32\" \
name=\"Microsoft.Windows.Common-Controls\" version=\"6.0.0.0\" \
processorArchitecture=\"*\" publicKeyToken=\"6595b64144ccf1df\" language=\"*\"/>\
</dependentAssembly></dependency>\r\n</assembly>\r\n";

struct ComCtl6Guard {
    act_ctx: HANDLE,
    cookie: usize,
    active: bool,
}

impl ComCtl6Guard {
    #[inline(never)]
    unsafe fn enter() -> Self {
        let mut guard = Self {
            act_ctx: INVALID_HANDLE_VALUE,
            cookie: 0,
            active: false,
        };
        let mut path = [0u16; 270];
        let dir_len = GetTempPathW(250, path.as_mut_ptr()) as usize;
        if dir_len == 0 || dir_len >= 250 {
            return guard;
        }
        let suffix = b"katt_cc6.manifest";
        if dir_len + suffix.len() + 1 > path.len() {
            return guard;
        }
        let mut i = 0usize;
        while i < suffix.len() {
            path[dir_len + i] = suffix[i] as u16;
            i += 1;
        }
        path[dir_len + i] = 0;

        let fh = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null(),
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if fh != INVALID_HANDLE_VALUE {
            let mut written: u32 = 0;
            let _ = WriteFile(
                fh,
                COMCTL6_MANIFEST.as_ptr(),
                COMCTL6_MANIFEST.len() as u32,
                &mut written,
                null_mut(),
            );
            let _ = CloseHandle(fh);

            let act = ACTCTXW {
                lp_source: path.as_ptr(),
                ..Default::default()
            };
            let ctx = CreateActCtxW(&act);
            let _ = DeleteFileW(path.as_ptr());
            if ctx != INVALID_HANDLE_VALUE && !ctx.is_null() {
                let mut cookie = 0usize;
                if ActivateActCtx(ctx, &mut cookie) != 0 {
                    guard.act_ctx = ctx;
                    guard.cookie = cookie;
                    guard.active = true;
                } else {
                    ReleaseActCtx(ctx);
                }
            }
        }
        guard
    }
}

impl Drop for ComCtl6Guard {
    fn drop(&mut self) {
        unsafe {
            if self.active {
                let _ = DeactivateActCtx(0, self.cookie);
            }
            if self.act_ctx != INVALID_HANDLE_VALUE && !self.act_ctx.is_null() {
                ReleaseActCtx(self.act_ctx);
            }
        }
    }
}

/// Show the English update prompt with a clickable release link.
pub fn show_update_dialog(
    parent_hwnd: usize,
    current_tag: &str,
    new_tag: &str,
    release_url: &str,
) -> PromptOutcome {
    let parent = parent_hwnd as HWND;
    let mut content = Utf16Builder::<512>::new();
    content.push_ascii("Installed: ");
    content.push_ascii(current_tag);
    content.push_ascii("\nAvailable: ");
    content.push_ascii(new_tag);
    content.push_ascii("\n\n<a href=\"");
    content.push_ascii(release_url);
    content.push_ascii("\">View release notes</a>");

    unsafe {
        let _cc6 = ComCtl6Guard::enter();
        let comctl32 = LoadLibraryW(w!("comctl32.dll"));
        if !comctl32.is_null() {
            let p = GetProcAddress(comctl32, c"TaskDialogIndirect".as_ptr().cast::<u8>());
            if !p.is_null() {
                let td_indirect: FnTaskDialogIndirect = transmute(p);
                let buttons = [
                    TaskDialogButton {
                        n_button_id: BTN_UPDATE_NOW,
                        psz_button_text: w!("Update now"),
                    },
                    TaskDialogButton {
                        n_button_id: BTN_LATER,
                        psz_button_text: w!("Later"),
                    },
                ];
                let cfg = TaskDialogConfig {
                    cb_size: core::mem::size_of::<TaskDialogConfig>() as u32,
                    hwnd_parent: parent,
                    h_instance: null_mut(),
                    dw_flags: TDF_ENABLE_HYPERLINKS
                        | TDF_ALLOW_DIALOG_CANCELLATION
                        | TDF_POSITION_RELATIVE_TO_WINDOW,
                    dw_common_buttons: 0,
                    psz_window_title: w!("katt"),
                    h_main_icon: TD_INFORMATION_ICON,
                    psz_main_instruction: w!("A new version of katt is available"),
                    psz_content: content.as_ptr(),
                    c_buttons: buttons.len() as u32,
                    p_buttons: buttons.as_ptr(),
                    n_default_button: BTN_UPDATE_NOW,
                    c_radio_buttons: 0,
                    p_radio_buttons: null(),
                    n_default_radio_button: 0,
                    psz_verification_text: w!("Skip this version"),
                    psz_expanded_information: null(),
                    psz_expanded_control_text: null(),
                    psz_collapsed_control_text: null(),
                    h_footer_icon: null(),
                    psz_footer: null(),
                    pf_callback: Some(task_dialog_callback),
                    lp_callback_data: 0,
                    cx_width: 0,
                };

                let mut pressed: i32 = BTN_LATER;
                let mut verified: BOOL = 0;
                let hr = td_indirect(&cfg, &mut pressed, null_mut(), &mut verified);
                let _ = FreeLibrary(comctl32);
                if hr >= 0 {
                    return PromptOutcome {
                        action: if pressed == BTN_UPDATE_NOW {
                            PromptAction::UpdateNow
                        } else {
                            PromptAction::Later
                        },
                        skip_version: verified != 0,
                    };
                }
            } else {
                let _ = FreeLibrary(comctl32);
            }
        }

        // Fallback if ComCtl v6 manifest is not active.
        show_messagebox_fallback(parent, current_tag, new_tag, release_url)
    }
}

#[inline(never)]
unsafe fn show_messagebox_fallback(
    parent: HWND,
    current_tag: &str,
    new_tag: &str,
    release_url: &str,
) -> PromptOutcome {
    let mut msg = Utf16Builder::<384>::new();
    msg.push_ascii("A new version of katt is available.\n\nInstalled: ");
    msg.push_ascii(current_tag);
    msg.push_ascii("\nAvailable: ");
    msg.push_ascii(new_tag);
    msg.push_ascii("\nRelease: ");
    msg.push_ascii(release_url);
    msg.push_ascii("\n\nUpdate now?");
    let ans = MessageBoxW(parent, msg.as_ptr(), w!("katt"), MB_YESNO | MB_ICONINFORMATION);
    PromptOutcome {
        action: if ans == IDYES {
            PromptAction::UpdateNow
        } else {
            PromptAction::Later
        },
        skip_version: false,
    }
}

// ============================================================================
// Atomic Self-Replace & Cleanup (`MoveFileExW` + `CreateProcessW`)
// ============================================================================

fn append_utf16_suffix(base_z: &[u16], suffix: &str, out: &mut [u16]) -> bool {
    let mut len = 0usize;
    while len < base_z.len() && base_z[len] != 0 {
        if len + 1 >= out.len() {
            return false;
        }
        out[len] = base_z[len];
        len += 1;
    }
    let sbytes = suffix.as_bytes();
    if len + sbytes.len() + 1 > out.len() {
        return false;
    }
    let mut i = 0usize;
    while i < sbytes.len() {
        out[len + i] = sbytes[i] as u16;
        i += 1;
    }
    out[len + i] = 0;
    true
}

/// Atomically replace `exe_path_utf16` with `new_path_utf16` using `.old` staging,
/// then launch the updated binary with optional `extra_args`. Returns the new process handle.
pub fn apply_update_and_spawn(
    exe_path_utf16: &[u16],
    new_path_utf16: &[u16],
    extra_args: &str,
) -> Result<HANDLE, UpdateError> {
    let mut old_path = [0u16; 270];
    if !append_utf16_suffix(exe_path_utf16, ".old", &mut old_path) {
        return Err(UpdateError::SwapFailed);
    }

    unsafe {
        if MoveFileExW(
            exe_path_utf16.as_ptr(),
            old_path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING,
        ) == 0
        {
            return Err(UpdateError::SwapFailed);
        }

        if MoveFileExW(
            new_path_utf16.as_ptr(),
            exe_path_utf16.as_ptr(),
            MOVEFILE_REPLACE_EXISTING,
        ) == 0
        {
            // Roll back on failure.
            let _ = MoveFileExW(
                old_path.as_ptr(),
                exe_path_utf16.as_ptr(),
                MOVEFILE_REPLACE_EXISTING,
            );
            return Err(UpdateError::SwapFailed);
        }

        let mut cmdline = [0u16; 512];
        let mut pos = 0usize;
        cmdline[pos] = b'"' as u16;
        pos += 1;
        let mut i = 0usize;
        while i < exe_path_utf16.len() && exe_path_utf16[i] != 0 && pos + 4 < cmdline.len() {
            cmdline[pos] = exe_path_utf16[i];
            pos += 1;
            i += 1;
        }
        cmdline[pos] = b'"' as u16;
        pos += 1;

        if !extra_args.is_empty() {
            cmdline[pos] = b' ' as u16;
            pos += 1;
            for &b in extra_args.as_bytes() {
                if pos + 1 >= cmdline.len() {
                    break;
                }
                cmdline[pos] = b as u16;
                pos += 1;
            }
        }
        cmdline[pos] = 0;

        let si = STARTUPINFOW {
            cb: core::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        if CreateProcessW(
            exe_path_utf16.as_ptr(),
            cmdline.as_mut_ptr(),
            null(),
            null(),
            0,
            0,
            null(),
            null(),
            &si,
            &mut pi,
        ) == 0
        {
            return Err(UpdateError::SpawnFailed);
        }
        if !pi.h_thread.is_null() {
            let _ = CloseHandle(pi.h_thread);
        }
        Ok(pi.h_process)
    }
}

/// Remove `<exe_path>.old` left behind by an atomic self-update, retrying briefly while
/// the previous instance finishes exiting.
pub fn cleanup_old_binary(exe_path_utf16: &[u16]) -> bool {
    let mut old_path = [0u16; 270];
    if !append_utf16_suffix(exe_path_utf16, ".old", &mut old_path) {
        return false;
    }
    unsafe {
        let mut attempt = 0;
        while attempt < 15 {
            if DeleteFileW(old_path.as_ptr()) != 0 {
                return true;
            }
            Sleep(100);
            attempt += 1;
        }
    }
    false
}

/// Remove `<exe_path>.old` without waiting (single attempt at normal startup).
pub fn try_delete_old_once(exe_path_utf16: &[u16]) {
    let mut old_path = [0u16; 270];
    if append_utf16_suffix(exe_path_utf16, ".old", &mut old_path) {
        unsafe {
            let _ = DeleteFileW(old_path.as_ptr());
        }
    }
}

/// Read the current executable path as UTF-16Z into `out`.
pub fn current_exe_utf16(out: &mut [u16; 260]) -> bool {
    unsafe {
        let n = GetModuleFileNameW(null_mut(), out.as_mut_ptr(), out.len() as u32) as usize;
        n > 0 && n < out.len() - 8
    }
}

/// Build `<exe_path>.new` UTF-16Z into `out`.
pub fn make_new_path_utf16(exe_path_utf16: &[u16], out: &mut [u16; 270]) -> bool {
    append_utf16_suffix(exe_path_utf16, ".new", out)
}

/// Format `https://github.com/KieranFooks/katt/releases/download/<tag>/<asset>` into `out`.
pub fn format_github_asset_url<'a>(
    tag: &str,
    asset: &str,
    out: &'a mut [u8; 256],
) -> Option<&'a str> {
    let prefix = b"https://github.com/KieranFooks/katt/releases/download/";
    let t = tag.as_bytes();
    let a = asset.as_bytes();
    let total = prefix.len() + t.len() + 1 + a.len();
    if total > out.len() {
        return None;
    }
    let mut pos = 0usize;
    out[pos..pos + prefix.len()].copy_from_slice(prefix);
    pos += prefix.len();
    out[pos..pos + t.len()].copy_from_slice(t);
    pos += t.len();
    out[pos] = b'/';
    pos += 1;
    out[pos..pos + a.len()].copy_from_slice(a);
    pos += a.len();
    core::str::from_utf8(&out[..pos]).ok()
}

/// Check `HKCU\Software\katt\SkippedVersion` via `kernelbase!RegGetValueW`.
pub fn is_version_skipped(tag: &str) -> bool {
    let mut buf = [0u16; 64];
    let mut cb = (buf.len() * 2) as u32;
    let mut ty = 0u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\katt"),
            w!("SkippedVersion"),
            RRF_RT_REG_SZ,
            &mut ty,
            buf.as_mut_ptr() as *mut c_void,
            &mut cb,
        )
    };
    if r != ERROR_SUCCESS {
        return false;
    }
    let mut ascii = [0u8; 64];
    let len = utf16_to_ascii_slice(&buf, &mut ascii);
    &ascii[..len] == tag.as_bytes()
}

/// Persist `HKCU\Software\katt\SkippedVersion` via ephemeral `advapi32.dll`.
#[inline(never)]
pub fn save_skipped_version(tag: &str) {
    const REG_OPTION_NON_VOLATILE: u32 = 0;
    const KEY_SET_VALUE: u32 = 0x0002;
    const REG_SZ: u32 = 1;

    type FnRegCreateKeyExW = unsafe extern "system" fn(
        isize,
        *const u16,
        u32,
        *const u16,
        u32,
        u32,
        *const c_void,
        *mut isize,
        *mut u32,
    ) -> i32;
    type FnRegSetValueExW =
        unsafe extern "system" fn(isize, *const u16, u32, u32, *const u8, u32) -> i32;
    type FnRegCloseKey = unsafe extern "system" fn(isize) -> i32;

    let mut tag_w = [0u16; 64];
    if !ascii_to_utf16z(tag, &mut tag_w) {
        return;
    }
    let byte_len = ((tag.len() + 1) * 2) as u32;

    unsafe {
        let adv = LoadLibraryW(w!("advapi32.dll"));
        if adv.is_null() {
            return;
        }
        let p_create = GetProcAddress(adv, c"RegCreateKeyExW".as_ptr().cast::<u8>());
        let p_set = GetProcAddress(adv, c"RegSetValueExW".as_ptr().cast::<u8>());
        let p_close = GetProcAddress(adv, c"RegCloseKey".as_ptr().cast::<u8>());
        if !p_create.is_null() && !p_set.is_null() && !p_close.is_null() {
            let reg_create: FnRegCreateKeyExW = transmute(p_create);
            let reg_set: FnRegSetValueExW = transmute(p_set);
            let reg_close: FnRegCloseKey = transmute(p_close);

            let mut hkey: isize = 0;
            if reg_create(
                HKEY_CURRENT_USER,
                w!("Software\\katt"),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut hkey,
                null_mut(),
            ) == ERROR_SUCCESS
            {
                let _ = reg_set(
                    hkey,
                    w!("SkippedVersion"),
                    0,
                    REG_SZ,
                    tag_w.as_ptr() as *const u8,
                    byte_len,
                );
                let _ = reg_close(hkey);
            }
        }
        let _ = FreeLibrary(adv);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_calver_tags_and_urls() {
        assert_eq!(parse_calver("v2026.11.23"), Some([2026, 11, 23, 0]));
        assert_eq!(parse_calver("2026.11.23.2"), Some([2026, 11, 23, 2]));
        assert_eq!(
            parse_calver("https://github.com/KieranFooks/katt/releases/tag/v2026.12.01.1"),
            Some([2026, 12, 1, 1])
        );
        assert_eq!(parse_calver("v2026.11"), None);
        assert_eq!(parse_calver("invalid"), None);
    }

    #[test]
    fn compares_calver_correctly() {
        assert!(is_newer([2026, 11, 23, 0], [2026, 11, 22, 0]));
        assert!(is_newer([2026, 11, 22, 1], [2026, 11, 22, 0]));
        assert!(!is_newer([2026, 11, 22, 0], [2026, 11, 22, 0]));
        assert!(!is_newer([2026, 11, 21, 9], [2026, 11, 22, 0]));
    }

    #[test]
    fn parses_http_and_https_urls() {
        let http = parse_url("http://127.0.0.1:8080/releases/latest").unwrap();
        assert!(!http.secure);
        assert_eq!(http.port, 8080);

        let https = parse_url("https://github.com/KieranFooks/katt/releases/latest").unwrap();
        assert!(https.secure);
        assert_eq!(https.port, 443);
    }

    #[test]
    fn parses_sha256_hex_digest() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  katt.exe\n";
        let digest = parse_sha256_hex(hex).unwrap();
        assert_eq!(digest[0], 0x01);
        assert_eq!(digest[1], 0x23);
        assert_eq!(digest[31], 0xEF);
        assert!(parse_sha256_hex("too_short").is_none());
    }

    #[test]
    fn comctl6_activation_context_exposes_task_dialog_indirect() {
        unsafe {
            let _cc6 = ComCtl6Guard::enter();
            let comctl32 = LoadLibraryW(w!("comctl32.dll"));
            assert!(!comctl32.is_null());
            let p = GetProcAddress(comctl32, c"TaskDialogIndirect".as_ptr().cast::<u8>());
            let _ = FreeLibrary(comctl32);
            assert!(!p.is_null(), "TaskDialogIndirect should resolve under ComCtl v6 ActCtx");
        }
    }
}
