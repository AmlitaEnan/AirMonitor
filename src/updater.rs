use std::cmp::Ordering;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use windows::core::{s, w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, FreeLibrary, GetLastError, HANDLE, HMODULE};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const REPO_URL: &str = "https://github.com/AmlitaEnan/AirMonitor";
pub const RELEASES_URL: &str = "https://github.com/AmlitaEnan/AirMonitor/releases";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

const REPO_URL_PREFIX: &str = "https://github.com/AmlitaEnan/AirMonitor/";
const ALLOWED_DOWNLOAD_PREFIXES: &[&str] = &[
    "https://github.com/AmlitaEnan/AirMonitor/",
    "https://objects.githubusercontent.com/",
    "https://release-assets.githubusercontent.com/",
];

const GITHUB_API_HOST: &str = "api.github.com";
const GITHUB_LATEST_RELEASE_PATH: &str = "/repos/AmlitaEnan/AirMonitor/releases/latest";
const HTTPS_PORT: u16 = 443;

const TIMEOUT_RESOLVE_MS: i32 = 5000;
const TIMEOUT_CONNECT_MS: i32 = 5000;
const TIMEOUT_SEND_MS: i32 = 5000;
const TIMEOUT_RECEIVE_API_MS: i32 = 8000;
const TIMEOUT_RECEIVE_DOWNLOAD_MS: i32 = 15000;

const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_SHA256_BYTES: usize = 64 * 1024;
const MIN_EXE_BYTES: u64 = 100 * 1024;
const MAX_EXE_BYTES: u64 = 25 * 1024 * 1024;
const DOWNLOAD_CHUNK_BYTES: usize = 16 * 1024;
const PROGRESS_MIN_BYTES: u64 = 64 * 1024;
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);
const MAX_DOWNLOAD_REDIRECTS: usize = 5;
const MAX_JSON_DEPTH: usize = 128;

const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_OPTION_REDIRECT_POLICY: u32 = 88;
const WINHTTP_OPTION_REDIRECT_POLICY_NEVER: u32 = 0;
const WINHTTP_QUERY_CONTENT_LENGTH: u32 = 5;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_LOCATION: u32 = 33;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const ERROR_INSUFFICIENT_BUFFER_CODE: u32 = 122;
const ERROR_WINHTTP_HEADER_NOT_FOUND_CODE: u32 = 12150;

const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_EXECUTABLE_IMAGE: u16 = 0x0002;
const IMAGE_FILE_DLL: u16 = 0x2000;
const STAGED_NEW_FILE_NAME: &str = "airmonitor.exe.new";
const STAGED_OLD_FILE_NAME: &str = "airmonitor.exe.old";
const OLD_EXE_RETRY_DELAY: Duration = Duration::from_millis(250);
const WAIT_OLD_PID_TIMEOUT_MS: u32 = 5000;

static WRITE_CHECK_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate {
        current: String,
        latest: String,
    },
    UpdateAvailable {
        current: String,
        latest: String,
        release_url: String,
        published_at: Option<String>,
        download_url: Option<String>,
        asset_name: Option<String>,
        asset_size: Option<u64>,
        sha256_url: Option<String>,
    },
    Downloading {
        latest: String,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    Verifying {
        latest: String,
    },
    Applying {
        latest: String,
    },
    NoReleaseYet {
        current: String,
    },
    Error(String),
}

type HInternet = *mut c_void;

type FnWinHttpOpen =
    unsafe extern "system" fn(*const u16, u32, *const u16, *const u16, u32) -> HInternet;
type FnWinHttpSetTimeouts = unsafe extern "system" fn(HInternet, i32, i32, i32, i32) -> i32;
type FnWinHttpSetOption = unsafe extern "system" fn(HInternet, u32, *const c_void, u32) -> i32;
type FnWinHttpConnect = unsafe extern "system" fn(HInternet, *const u16, u16, u32) -> HInternet;
type FnWinHttpOpenRequest = unsafe extern "system" fn(
    HInternet,
    *const u16,
    *const u16,
    *const u16,
    *const u16,
    *const *const u16,
    u32,
) -> HInternet;
type FnWinHttpSendRequest =
    unsafe extern "system" fn(HInternet, *const u16, u32, *mut c_void, u32, u32, usize) -> i32;
type FnWinHttpReceiveResponse = unsafe extern "system" fn(HInternet, *mut c_void) -> i32;
type FnWinHttpQueryHeaders =
    unsafe extern "system" fn(HInternet, u32, *const u16, *mut c_void, *mut u32, *mut u32) -> i32;
type FnWinHttpQueryDataAvailable = unsafe extern "system" fn(HInternet, *mut u32) -> i32;
type FnWinHttpReadData = unsafe extern "system" fn(HInternet, *mut c_void, u32, *mut u32) -> i32;
type FnWinHttpCloseHandle = unsafe extern "system" fn(HInternet) -> i32;

struct WinHttpApi {
    _module: ModuleGuard,
    open: FnWinHttpOpen,
    set_timeouts: FnWinHttpSetTimeouts,
    set_option: FnWinHttpSetOption,
    connect: FnWinHttpConnect,
    open_request: FnWinHttpOpenRequest,
    send_request: FnWinHttpSendRequest,
    receive_response: FnWinHttpReceiveResponse,
    query_headers: FnWinHttpQueryHeaders,
    query_data_available: FnWinHttpQueryDataAvailable,
    read_data: FnWinHttpReadData,
    close_handle: FnWinHttpCloseHandle,
}

struct ModuleGuard(HMODULE);

impl Drop for ModuleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            unsafe {
                let _ = FreeLibrary(self.0);
            }
        }
    }
}

struct HInternetGuard {
    handle: HInternet,
    close_fn: FnWinHttpCloseHandle,
}

impl HInternetGuard {
    fn new(handle: HInternet, close_fn: FnWinHttpCloseHandle) -> Option<Self> {
        if handle.is_null() {
            None
        } else {
            Some(Self { handle, close_fn })
        }
    }

    fn raw(&self) -> HInternet {
        self.handle
    }
}

impl Drop for HInternetGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = (self.close_fn)(self.handle);
            }
        }
    }
}

unsafe fn load_proc<T>(module: HMODULE, name: PCSTR) -> Option<T> {
    let proc = GetProcAddress(module, name)?;
    Some(std::mem::transmute_copy(&proc))
}

impl WinHttpApi {
    fn load() -> Result<Self, String> {
        unsafe {
            let module = LoadLibraryExW(
                w!("winhttp.dll"),
                HANDLE::default(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
            .map_err(|_| "无法加载系统 WinHTTP 组件".to_string())?;

            if module.is_invalid() || module.0.is_null() {
                return Err("无法加载系统 WinHTTP 组件".to_string());
            }

            let guard = ModuleGuard(module);

            let open = load_proc::<FnWinHttpOpen>(module, s!("WinHttpOpen"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpOpen".to_string())?;
            let set_timeouts = load_proc::<FnWinHttpSetTimeouts>(module, s!("WinHttpSetTimeouts"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpSetTimeouts".to_string())?;
            let set_option = load_proc::<FnWinHttpSetOption>(module, s!("WinHttpSetOption"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpSetOption".to_string())?;
            let connect = load_proc::<FnWinHttpConnect>(module, s!("WinHttpConnect"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpConnect".to_string())?;
            let open_request = load_proc::<FnWinHttpOpenRequest>(module, s!("WinHttpOpenRequest"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpOpenRequest".to_string())?;
            let send_request = load_proc::<FnWinHttpSendRequest>(module, s!("WinHttpSendRequest"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpSendRequest".to_string())?;
            let receive_response =
                load_proc::<FnWinHttpReceiveResponse>(module, s!("WinHttpReceiveResponse"))
                    .ok_or_else(|| "WinHTTP 缺少 WinHttpReceiveResponse".to_string())?;
            let query_headers =
                load_proc::<FnWinHttpQueryHeaders>(module, s!("WinHttpQueryHeaders"))
                    .ok_or_else(|| "WinHTTP 缺少 WinHttpQueryHeaders".to_string())?;
            let query_data_available =
                load_proc::<FnWinHttpQueryDataAvailable>(module, s!("WinHttpQueryDataAvailable"))
                    .ok_or_else(|| "WinHTTP 缺少 WinHttpQueryDataAvailable".to_string())?;
            let read_data = load_proc::<FnWinHttpReadData>(module, s!("WinHttpReadData"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpReadData".to_string())?;
            let close_handle = load_proc::<FnWinHttpCloseHandle>(module, s!("WinHttpCloseHandle"))
                .ok_or_else(|| "WinHTTP 缺少 WinHttpCloseHandle".to_string())?;

            Ok(Self {
                _module: guard,
                open,
                set_timeouts,
                set_option,
                connect,
                open_request,
                send_request,
                receive_response,
                query_headers,
                query_data_available,
                read_data,
                close_handle,
            })
        }
    }
}

type BCryptAlgHandle = *mut c_void;
type BCryptHashHandle = *mut c_void;
type FnBCryptOpenAlgorithmProvider =
    unsafe extern "system" fn(*mut BCryptAlgHandle, *const u16, *const u16, u32) -> i32;
type FnBCryptGetProperty =
    unsafe extern "system" fn(*mut c_void, *const u16, *mut u8, u32, *mut u32, u32) -> i32;
type FnBCryptCreateHash = unsafe extern "system" fn(
    BCryptAlgHandle,
    *mut BCryptHashHandle,
    *mut u8,
    u32,
    *const u8,
    u32,
    u32,
) -> i32;
type FnBCryptHashData = unsafe extern "system" fn(BCryptHashHandle, *const u8, u32, u32) -> i32;
type FnBCryptFinishHash = unsafe extern "system" fn(BCryptHashHandle, *mut u8, u32, u32) -> i32;
type FnBCryptDestroyHash = unsafe extern "system" fn(BCryptHashHandle) -> i32;
type FnBCryptCloseAlgorithmProvider = unsafe extern "system" fn(BCryptAlgHandle, u32) -> i32;

struct BCryptApi {
    _module: ModuleGuard,
    open_algorithm_provider: FnBCryptOpenAlgorithmProvider,
    get_property: FnBCryptGetProperty,
    create_hash: FnBCryptCreateHash,
    hash_data: FnBCryptHashData,
    finish_hash: FnBCryptFinishHash,
    destroy_hash: FnBCryptDestroyHash,
    close_algorithm_provider: FnBCryptCloseAlgorithmProvider,
}

struct BCryptAlgGuard {
    handle: BCryptAlgHandle,
    close_fn: FnBCryptCloseAlgorithmProvider,
}

impl Drop for BCryptAlgGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = (self.close_fn)(self.handle, 0);
            }
        }
    }
}

struct BCryptHashGuard {
    handle: BCryptHashHandle,
    destroy_fn: FnBCryptDestroyHash,
}

impl Drop for BCryptHashGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = (self.destroy_fn)(self.handle);
            }
        }
    }
}

impl BCryptApi {
    fn load() -> Result<Self, String> {
        unsafe {
            let module = LoadLibraryExW(
                w!("bcrypt.dll"),
                HANDLE::default(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
            .map_err(|_| "无法加载系统 BCrypt 加密组件".to_string())?;

            if module.is_invalid() || module.0.is_null() {
                return Err("无法加载系统 BCrypt 加密组件".to_string());
            }

            let guard = ModuleGuard(module);
            let open_algorithm_provider = load_proc::<FnBCryptOpenAlgorithmProvider>(
                module,
                s!("BCryptOpenAlgorithmProvider"),
            )
            .ok_or_else(|| "BCrypt 缺少 BCryptOpenAlgorithmProvider".to_string())?;
            let get_property = load_proc::<FnBCryptGetProperty>(module, s!("BCryptGetProperty"))
                .ok_or_else(|| "BCrypt 缺少 BCryptGetProperty".to_string())?;
            let create_hash = load_proc::<FnBCryptCreateHash>(module, s!("BCryptCreateHash"))
                .ok_or_else(|| "BCrypt 缺少 BCryptCreateHash".to_string())?;
            let hash_data = load_proc::<FnBCryptHashData>(module, s!("BCryptHashData"))
                .ok_or_else(|| "BCrypt 缺少 BCryptHashData".to_string())?;
            let finish_hash = load_proc::<FnBCryptFinishHash>(module, s!("BCryptFinishHash"))
                .ok_or_else(|| "BCrypt 缺少 BCryptFinishHash".to_string())?;
            let destroy_hash = load_proc::<FnBCryptDestroyHash>(module, s!("BCryptDestroyHash"))
                .ok_or_else(|| "BCrypt 缺少 BCryptDestroyHash".to_string())?;
            let close_algorithm_provider = load_proc::<FnBCryptCloseAlgorithmProvider>(
                module,
                s!("BCryptCloseAlgorithmProvider"),
            )
            .ok_or_else(|| "BCrypt 缺少 BCryptCloseAlgorithmProvider".to_string())?;

            Ok(Self {
                _module: guard,
                open_algorithm_provider,
                get_property,
                create_hash,
                hash_data,
                finish_hash,
                destroy_hash,
                close_algorithm_provider,
            })
        }
    }
}

struct TempFileCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempFileCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn to_wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn format_winhttp_error(stage: &str, code: u32) -> String {
    match code {
        12002 => format!("{}：连接 GitHub 超时", stage),
        12007 => format!("{}：无法解析 GitHub 域名，请检查网络", stage),
        12029 => format!("{}：无法连接到 GitHub 服务器", stage),
        12030 | 12031 => format!("{}：与 GitHub 的连接中断", stage),
        12175 => format!("{}：TLS 安全连接失败", stage),
        _ => format!("{} (WinHTTP 错误 {})", stage, code),
    }
}

fn last_win32_error() -> u32 {
    unsafe { GetLastError().0 }
}

fn open_winhttp_session(
    api: &WinHttpApi,
    receive_timeout_ms: i32,
) -> Result<HInternetGuard, String> {
    let user_agent = to_wide_null(&format!("AirMonitor/{}", CURRENT_VERSION));
    let session_raw = unsafe {
        let h = (api.open)(
            user_agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if h.is_null() {
            (api.open)(
                user_agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        } else {
            h
        }
    };
    let session = HInternetGuard::new(session_raw, api.close_handle)
        .ok_or_else(|| format_winhttp_error("初始化网络会话失败", last_win32_error()))?;

    let timeout_ok = unsafe {
        (api.set_timeouts)(
            session.raw(),
            TIMEOUT_RESOLVE_MS,
            TIMEOUT_CONNECT_MS,
            TIMEOUT_SEND_MS,
            receive_timeout_ms,
        )
    };
    if timeout_ok == 0 {
        return Err(format_winhttp_error("设置网络超时失败", last_win32_error()));
    }

    Ok(session)
}

fn fetch_latest_release_http(api: &WinHttpApi) -> Result<(u32, Vec<u8>), String> {
    let session = open_winhttp_session(api, TIMEOUT_RECEIVE_API_MS)?;
    let host = to_wide_null(GITHUB_API_HOST);
    let verb = to_wide_null("GET");
    let path = to_wide_null(GITHUB_LATEST_RELEASE_PATH);
    let headers: Vec<u16> =
        "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n"
            .encode_utf16()
            .collect();

    let connect_raw = unsafe { (api.connect)(session.raw(), host.as_ptr(), HTTPS_PORT, 0) };
    let connect = HInternetGuard::new(connect_raw, api.close_handle)
        .ok_or_else(|| format_winhttp_error("建立连接失败", last_win32_error()))?;

    let request_raw = unsafe {
        (api.open_request)(
            connect.raw(),
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    let request = HInternetGuard::new(request_raw, api.close_handle)
        .ok_or_else(|| format_winhttp_error("创建请求失败", last_win32_error()))?;

    let policy: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
    let opt_ok = unsafe {
        (api.set_option)(
            request.raw(),
            WINHTTP_OPTION_REDIRECT_POLICY,
            (&policy as *const u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if opt_ok == 0 {
        return Err(format_winhttp_error(
            "禁用自动重定向失败",
            last_win32_error(),
        ));
    }

    let send_ok = unsafe {
        (api.send_request)(
            request.raw(),
            headers.as_ptr(),
            headers.len() as u32,
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    if send_ok == 0 {
        return Err(format_winhttp_error("发送请求失败", last_win32_error()));
    }

    let recv_ok = unsafe { (api.receive_response)(request.raw(), std::ptr::null_mut()) };
    if recv_ok == 0 {
        return Err(format_winhttp_error("接收响应失败", last_win32_error()));
    }

    let status_code = query_status_code(api, request.raw())?;

    let mut body = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let mut available: u32 = 0;
        let avail_ok = unsafe { (api.query_data_available)(request.raw(), &mut available) };
        if avail_ok == 0 {
            return Err(format_winhttp_error("查询响应数据失败", last_win32_error()));
        }
        if available == 0 {
            break;
        }

        let mut remaining = available as usize;
        while remaining > 0 {
            if body.len() >= MAX_RESPONSE_BYTES {
                return Err("服务器响应体超出大小限制".to_string());
            }
            let room = MAX_RESPONSE_BYTES - body.len();
            let to_read = remaining.min(chunk.len()).min(room);
            let mut bytes_read: u32 = 0;
            let read_ok = unsafe {
                (api.read_data)(
                    request.raw(),
                    chunk.as_mut_ptr().cast::<c_void>(),
                    to_read as u32,
                    &mut bytes_read,
                )
            };
            if read_ok == 0 {
                return Err(format_winhttp_error("读取响应体失败", last_win32_error()));
            }
            if bytes_read == 0 {
                break;
            }
            let n = bytes_read as usize;
            body.extend_from_slice(&chunk[..n]);
            remaining = remaining.saturating_sub(n);
        }
    }

    Ok((status_code, body))
}

fn query_status_code(api: &WinHttpApi, request: HInternet) -> Result<u32, String> {
    let mut status_code: u32 = 0;
    let mut status_len: u32 = std::mem::size_of::<u32>() as u32;
    let query_ok = unsafe {
        (api.query_headers)(
            request,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status_code as *mut u32).cast::<c_void>(),
            &mut status_len,
            std::ptr::null_mut(),
        )
    };
    if query_ok == 0 {
        return Err(format_winhttp_error("读取响应头失败", last_win32_error()));
    }
    Ok(status_code)
}

fn query_header_string(
    api: &WinHttpApi,
    request: HInternet,
    info_level: u32,
) -> Result<Option<String>, String> {
    let mut len_bytes: u32 = 0;
    let ok = unsafe {
        (api.query_headers)(
            request,
            info_level,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut len_bytes,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = last_win32_error();
        if err == ERROR_WINHTTP_HEADER_NOT_FOUND_CODE {
            return Ok(None);
        }
        if err != ERROR_INSUFFICIENT_BUFFER_CODE {
            return Err(format_winhttp_error("读取响应头失败", err));
        }
    }
    if len_bytes == 0 {
        return Ok(None);
    }

    let wide_len = (len_bytes as usize).div_ceil(2);
    let mut buf = vec![0u16; wide_len];
    let ok2 = unsafe {
        (api.query_headers)(
            request,
            info_level,
            std::ptr::null(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut len_bytes,
            std::ptr::null_mut(),
        )
    };
    if ok2 == 0 {
        return Err(format_winhttp_error("读取响应头失败", last_win32_error()));
    }

    let actual_wides = (len_bytes as usize / 2).min(buf.len());
    let trimmed = match buf[..actual_wides].iter().position(|&c| c == 0) {
        Some(idx) => &buf[..idx],
        None => &buf[..actual_wides],
    };
    let s = String::from_utf16(trimmed).map_err(|_| "响应头包含无效 UTF-16".to_string())?;
    Ok(Some(s))
}

fn parse_https_url(url: &str) -> Result<(&str, String), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "仅支持 HTTPS 下载地址".to_string())?;
    let slash = rest
        .find('/')
        .ok_or_else(|| "下载地址缺少路径".to_string())?;
    let host = &rest[..slash];
    let path_and_query = rest[slash..].split('#').next().unwrap_or(&rest[slash..]);
    if host.is_empty() || path_and_query.is_empty() {
        return Err("下载地址格式无效".to_string());
    }
    Ok((host, path_and_query.to_string()))
}

fn resolve_redirect_url(current_host: &str, location: &str) -> Result<String, String> {
    let loc = location.trim();
    if loc.is_empty() || loc.starts_with("//") {
        return Err("重定向 Location 头无效".to_string());
    }
    let next_url = if loc.starts_with('/') {
        format!("https://{}{}", current_host, loc)
    } else {
        loc.to_string()
    };
    if !is_allowed_download_url(&next_url) {
        return Err("下载重定向目标未通过安全白名单校验".to_string());
    }
    Ok(next_url)
}

struct OpenDownloadResponse {
    // Field order matters: drop child request handle before parent connect handle.
    request: HInternetGuard,
    _connect: HInternetGuard,
}

fn open_download_with_redirects(
    api: &WinHttpApi,
    session: &HInternetGuard,
    initial_url: &str,
) -> Result<OpenDownloadResponse, String> {
    if !is_allowed_download_url(initial_url) {
        return Err("下载地址未通过安全白名单校验".to_string());
    }

    let verb = to_wide_null("GET");
    let mut current_url = initial_url.to_string();

    for hop in 0..=MAX_DOWNLOAD_REDIRECTS {
        let (host, path_and_query) = parse_https_url(&current_url)?;
        let host_w = to_wide_null(host);
        let path_w = to_wide_null(&path_and_query);

        let connect_raw = unsafe { (api.connect)(session.raw(), host_w.as_ptr(), HTTPS_PORT, 0) };
        let connect = HInternetGuard::new(connect_raw, api.close_handle)
            .ok_or_else(|| format_winhttp_error("建立下载连接失败", last_win32_error()))?;

        let request_raw = unsafe {
            (api.open_request)(
                connect.raw(),
                verb.as_ptr(),
                path_w.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        };
        let request = HInternetGuard::new(request_raw, api.close_handle)
            .ok_or_else(|| format_winhttp_error("创建下载请求失败", last_win32_error()))?;

        let policy: u32 = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
        let opt_ok = unsafe {
            (api.set_option)(
                request.raw(),
                WINHTTP_OPTION_REDIRECT_POLICY,
                (&policy as *const u32).cast::<c_void>(),
                std::mem::size_of::<u32>() as u32,
            )
        };
        if opt_ok == 0 {
            return Err(format_winhttp_error(
                "禁用自动重定向失败",
                last_win32_error(),
            ));
        }

        let send_ok = unsafe {
            (api.send_request)(
                request.raw(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                0,
                0,
            )
        };
        if send_ok == 0 {
            return Err(format_winhttp_error("发送下载请求失败", last_win32_error()));
        }

        let recv_ok = unsafe { (api.receive_response)(request.raw(), std::ptr::null_mut()) };
        if recv_ok == 0 {
            return Err(format_winhttp_error("接收下载响应失败", last_win32_error()));
        }

        let status = query_status_code(api, request.raw())?;
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if hop >= MAX_DOWNLOAD_REDIRECTS {
                return Err("下载重定向次数过多".to_string());
            }
            let location = query_header_string(api, request.raw(), WINHTTP_QUERY_LOCATION)?
                .ok_or_else(|| "下载重定向缺少 Location 头".to_string())?;
            current_url = resolve_redirect_url(host, &location)?;
            continue;
        }

        if status != 200 {
            return Err(format!("下载失败，服务器返回 HTTP {}", status));
        }

        return Ok(OpenDownloadResponse {
            _connect: connect,
            request,
        });
    }

    Err("下载重定向次数过多".to_string())
}

fn download_file_with_redirects<F>(
    api: &WinHttpApi,
    url: &str,
    dest_path: &Path,
    latest: &str,
    on_progress: &mut F,
) -> Result<(), String>
where
    F: FnMut(UpdateStatus),
{
    let session = open_winhttp_session(api, TIMEOUT_RECEIVE_DOWNLOAD_MS)?;
    let resp = open_download_with_redirects(api, &session, url)?;

    let total_bytes = query_header_string(api, resp.request.raw(), WINHTTP_QUERY_CONTENT_LENGTH)?
        .and_then(|s| s.trim().parse::<u64>().ok());

    if let Some(len) = total_bytes {
        if !(MIN_EXE_BYTES..=MAX_EXE_BYTES).contains(&len) {
            return Err(format!("安装包大小超出允许范围 ({} 字节)", len));
        }
    }

    let mut file = File::create(dest_path).map_err(|e| format!("无法创建更新临时文件: {e}"))?;
    let mut chunk = [0u8; DOWNLOAD_CHUNK_BYTES];
    let mut downloaded_bytes: u64 = 0;
    let mut last_reported_bytes: u64 = 0;
    let mut last_report_at = Instant::now();

    on_progress(UpdateStatus::Downloading {
        latest: latest.to_string(),
        downloaded_bytes: 0,
        total_bytes,
    });

    loop {
        let mut available: u32 = 0;
        let avail_ok = unsafe { (api.query_data_available)(resp.request.raw(), &mut available) };
        if avail_ok == 0 {
            return Err(format_winhttp_error("查询下载数据失败", last_win32_error()));
        }
        if available == 0 {
            break;
        }

        let mut remaining = available as usize;
        while remaining > 0 {
            let to_read = remaining.min(chunk.len());
            let mut bytes_read: u32 = 0;
            let read_ok = unsafe {
                (api.read_data)(
                    resp.request.raw(),
                    chunk.as_mut_ptr().cast::<c_void>(),
                    to_read as u32,
                    &mut bytes_read,
                )
            };
            if read_ok == 0 {
                return Err(format_winhttp_error("读取下载数据失败", last_win32_error()));
            }
            if bytes_read == 0 {
                break;
            }
            let n = bytes_read as usize;
            let next_total = downloaded_bytes.saturating_add(n as u64);
            if next_total > MAX_EXE_BYTES {
                return Err("安装包大小超出 25 MiB 限制".to_string());
            }

            file.write_all(&chunk[..n])
                .map_err(|e| format!("写入更新临时文件失败: {e}"))?;

            downloaded_bytes = next_total;
            remaining = remaining.saturating_sub(n);

            if downloaded_bytes.saturating_sub(last_reported_bytes) >= PROGRESS_MIN_BYTES
                || last_report_at.elapsed() >= PROGRESS_MIN_INTERVAL
            {
                on_progress(UpdateStatus::Downloading {
                    latest: latest.to_string(),
                    downloaded_bytes,
                    total_bytes,
                });
                last_reported_bytes = downloaded_bytes;
                last_report_at = Instant::now();
            }
        }
    }

    file.flush()
        .map_err(|e| format!("刷新更新临时文件失败: {e}"))?;
    drop(file);

    if downloaded_bytes < MIN_EXE_BYTES {
        return Err(format!(
            "下载文件过小 ({} 字节)，可能不是有效的安装程序",
            downloaded_bytes
        ));
    }
    if let Some(expected_len) = total_bytes {
        if downloaded_bytes != expected_len {
            return Err(format!(
                "下载不完整（已下载 {} / {} 字节）",
                downloaded_bytes, expected_len
            ));
        }
    }

    if downloaded_bytes > last_reported_bytes {
        on_progress(UpdateStatus::Downloading {
            latest: latest.to_string(),
            downloaded_bytes,
            total_bytes,
        });
    }

    Ok(())
}

fn download_small_text_with_redirects(
    api: &WinHttpApi,
    url: &str,
    max_bytes: usize,
) -> Result<String, String> {
    let session = open_winhttp_session(api, TIMEOUT_RECEIVE_API_MS)?;
    let resp = open_download_with_redirects(api, &session, url)?;

    if let Some(len) = query_header_string(api, resp.request.raw(), WINHTTP_QUERY_CONTENT_LENGTH)?
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        if len > max_bytes as u64 {
            return Err("校验文件超出 64 KiB 大小限制".to_string());
        }
    }

    let mut body = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let mut available: u32 = 0;
        let avail_ok = unsafe { (api.query_data_available)(resp.request.raw(), &mut available) };
        if avail_ok == 0 {
            return Err(format_winhttp_error("查询校验文件失败", last_win32_error()));
        }
        if available == 0 {
            break;
        }

        let mut remaining = available as usize;
        while remaining > 0 {
            if body.len() >= max_bytes {
                return Err("校验文件超出 64 KiB 大小限制".to_string());
            }
            let room = max_bytes - body.len();
            let to_read = remaining.min(chunk.len()).min(room);
            let mut bytes_read: u32 = 0;
            let read_ok = unsafe {
                (api.read_data)(
                    resp.request.raw(),
                    chunk.as_mut_ptr().cast::<c_void>(),
                    to_read as u32,
                    &mut bytes_read,
                )
            };
            if read_ok == 0 {
                return Err(format_winhttp_error("读取校验文件失败", last_win32_error()));
            }
            if bytes_read == 0 {
                break;
            }
            let n = bytes_read as usize;
            body.extend_from_slice(&chunk[..n]);
            remaining = remaining.saturating_sub(n);
        }
    }

    String::from_utf8(body).map_err(|_| "校验文件不是有效的 UTF-8 文本".to_string())
}

pub fn check_latest_release() -> UpdateStatus {
    let api = match WinHttpApi::load() {
        Ok(api) => api,
        Err(err) => return UpdateStatus::Error(err),
    };

    match fetch_latest_release_http(&api) {
        Ok((status, body)) => process_release_response(status, &body, CURRENT_VERSION),
        Err(err) => UpdateStatus::Error(err),
    }
}

fn has_safe_url_path(url: &str) -> bool {
    let path_part = url.split(['?', '#']).next().unwrap_or(url);
    if path_part.contains("/../")
        || path_part.ends_with("/..")
        || path_part.contains("/./")
        || path_part.ends_with("/.")
    {
        return false;
    }
    let lower = path_part.to_ascii_lowercase();
    !(lower.contains("%2e%2e") || lower.contains("%2f") || lower.contains("%5c"))
}

pub fn is_allowed_repo_url(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    if url
        .bytes()
        .any(|b| b <= 0x20 || b >= 0x7F || b == b'\\' || b == b'"' || b == b'\'')
    {
        return false;
    }
    if url != REPO_URL && !url.starts_with(REPO_URL_PREFIX) {
        return false;
    }
    has_safe_url_path(url)
}

pub fn is_allowed_download_url(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    if url
        .bytes()
        .any(|b| b <= 0x20 || b >= 0x7F || b == b'\\' || b == b'"' || b == b'\'')
    {
        return false;
    }
    let Some(prefix) = ALLOWED_DOWNLOAD_PREFIXES
        .iter()
        .copied()
        .find(|p| url.starts_with(p))
    else {
        return false;
    };
    let suffix = &url[prefix.len()..];
    if suffix.is_empty() || suffix.starts_with('?') || suffix.starts_with('#') {
        return false;
    }
    has_safe_url_path(url)
}

pub fn open_repo_url(url: &str) -> Result<(), String> {
    if !is_allowed_repo_url(url) {
        return Err("拒绝打开未受信任的 URL".to_string());
    }

    let wide = to_wide_null(url);
    let hinst = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };

    let code = hinst.0 as isize;
    if code <= 32 {
        return Err(format!("无法打开浏览器 (ShellExecuteW 错误 {})", code));
    }

    Ok(())
}

pub fn is_self_update_dir_writable() -> bool {
    let Ok(exe_path) = std::env::current_exe() else {
        return false;
    };
    let Some(dir) = exe_path.parent() else {
        return false;
    };
    is_dir_writable(dir)
}

fn is_dir_writable(dir: &Path) -> bool {
    let pid = std::process::id();
    for _ in 0..4 {
        let seq = WRITE_CHECK_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let probe = dir.join(format!(".airmonitor_write_check_{}_{}.tmp", pid, seq));
        match OpenOptions::new().write(true).create_new(true).open(&probe) {
            Ok(file) => {
                drop(file);
                let _ = fs::remove_file(&probe);
                return true;
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return false,
        }
    }
    false
}

pub fn download_and_apply_update<F>(
    latest: &str,
    download_url: &str,
    asset_name: Option<&str>,
    sha256_url: Option<&str>,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(UpdateStatus),
{
    if !is_self_update_dir_writable() {
        return Err("当前程序目录无写入权限，请点击「手动下载」".to_string());
    }
    if !is_allowed_download_url(download_url) {
        return Err("下载地址未通过安全白名单校验".to_string());
    }
    if let Some(url) = sha256_url {
        if !is_allowed_download_url(url) {
            return Err("校验文件地址未通过安全白名单校验".to_string());
        }
    }

    let current_exe = std::env::current_exe().map_err(|e| format!("无法获取当前程序路径: {e}"))?;
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| "无法获取当前程序目录".to_string())?;
    let new_exe = exe_dir.join(STAGED_NEW_FILE_NAME);
    let old_exe = exe_dir.join(STAGED_OLD_FILE_NAME);

    let _ = fs::remove_file(&new_exe);
    let mut new_cleanup = TempFileCleanup::new(new_exe.clone());

    let checksum_text = {
        let api = WinHttpApi::load()?;
        download_file_with_redirects(&api, download_url, &new_exe, latest, &mut on_progress)?;

        on_progress(UpdateStatus::Verifying {
            latest: latest.to_string(),
        });

        match sha256_url {
            Some(url) => Some(download_small_text_with_redirects(
                &api,
                url,
                MAX_SHA256_BYTES,
            )?),
            None => None,
        }
    };

    validate_pe_x64_file(&new_exe)?;

    if let Some(text) = checksum_text {
        let expected_hash = parse_sha256_for_asset(&text, asset_name)
            .ok_or_else(|| "校验文件格式无效或未找到匹配目标的 SHA-256".to_string())?;
        let actual_hash = compute_file_sha256(&new_exe)?;
        if !actual_hash.eq_ignore_ascii_case(&expected_hash) {
            return Err("安装包 SHA-256 校验失败，文件可能已损坏或被篡改".to_string());
        }
    }

    on_progress(UpdateStatus::Applying {
        latest: latest.to_string(),
    });

    new_cleanup.disarm();
    let pid_arg = std::process::id().to_string();
    apply_staged_update_with_spawner(&current_exe, &new_exe, &old_exe, |exe| {
        Command::new(exe)
            .arg("--wait-pid")
            .arg(&pid_arg)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
}

pub fn handle_post_update_startup() {
    if let Some(old_pid) = parse_wait_pid_from_args(std::env::args()) {
        if old_pid != 0 && old_pid != std::process::id() {
            unsafe {
                if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, old_pid) {
                    if !handle.is_invalid() {
                        let _ = WaitForSingleObject(handle, WAIT_OLD_PID_TIMEOUT_MS);
                        let _ = CloseHandle(handle);
                    }
                }
            }
        }
    }

    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };
    let Some(exe_dir) = current_exe.parent() else {
        return;
    };

    cleanup_staged_files_in_dir(exe_dir, &current_exe);
}

fn parse_wait_pid_from_args<I, S>(args: I) -> Option<u32>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        let s = arg.as_ref();
        if s == "--wait-pid" {
            return iter
                .next()
                .and_then(|v| v.as_ref().trim().parse::<u32>().ok());
        }
        if let Some(rest) = s.strip_prefix("--wait-pid=") {
            return rest.trim().parse::<u32>().ok();
        }
    }
    None
}

fn cleanup_staged_files_in_dir(exe_dir: &Path, current_exe: &Path) {
    remove_old_file_with_retry(&exe_dir.join(STAGED_OLD_FILE_NAME));
    let _ = fs::remove_file(exe_dir.join(STAGED_NEW_FILE_NAME));

    if let Some(name) = current_exe.file_name().and_then(|n| n.to_str()) {
        if !name.eq_ignore_ascii_case("airmonitor.exe") {
            remove_old_file_with_retry(&exe_dir.join(format!("{name}.old")));
            let _ = fs::remove_file(exe_dir.join(format!("{name}.new")));
        }
    }
}

fn remove_old_file_with_retry(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(_) => {
            std::thread::sleep(OLD_EXE_RETRY_DELAY);
            let _ = fs::remove_file(path);
        }
    }
}

fn move_file_replace(src: &Path, dst: &Path) -> Result<(), String> {
    let src_w = to_wide_path(src);
    let dst_w = to_wide_path(dst);
    unsafe {
        MoveFileExW(
            PCWSTR(src_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|e| format!("移动文件失败 ({} -> {}): {e}", src.display(), dst.display()))
    }
}

fn apply_staged_update_with_spawner<S>(
    current_exe: &Path,
    new_exe: &Path,
    old_exe: &Path,
    mut spawn_fn: S,
) -> Result<(), String>
where
    S: FnMut(&Path) -> Result<(), String>,
{
    let _ = fs::remove_file(old_exe);

    if let Err(err) = move_file_replace(current_exe, old_exe) {
        let _ = fs::remove_file(new_exe);
        return Err(format!("备份旧版程序失败: {err}"));
    }

    if let Err(err) = move_file_replace(new_exe, current_exe) {
        let _ = move_file_replace(old_exe, current_exe);
        let _ = fs::remove_file(new_exe);
        return Err(format!("替换为新版程序失败，已回滚旧版: {err}"));
    }

    if let Err(err) = spawn_fn(current_exe) {
        let _ = move_file_replace(old_exe, current_exe);
        let _ = fs::remove_file(new_exe);
        return Err(format!("启动新版程序失败，已自动回滚旧版: {err}"));
    }

    Ok(())
}

fn validate_pe_x64_file(path: &Path) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| format!("读取安装包信息失败: {e}"))?;
    let len = meta.len();
    if !(MIN_EXE_BYTES..=MAX_EXE_BYTES).contains(&len) {
        return Err(format!("安装包大小异常 ({} 字节)", len));
    }
    let mut file = File::open(path).map_err(|e| format!("无法打开安装包进行校验: {e}"))?;
    validate_pe_x64_reader(&mut file)
}

fn validate_pe_x64_reader<R: Read + Seek>(reader: &mut R) -> Result<(), String> {
    let mut dos = [0u8; 64];
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| "无法读取安装包 DOS 头".to_string())?;
    reader
        .read_exact(&mut dos)
        .map_err(|_| "安装包文件过短，不是有效的可执行文件".to_string())?;

    if &dos[0..2] != b"MZ" {
        return Err("安装包缺少 MZ 文件头，可能不是有效的可执行文件".to_string());
    }

    let e_lfanew = u32::from_le_bytes([dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F]]) as u64;
    if !(64..=1024 * 1024).contains(&e_lfanew) {
        return Err("安装包 PE 头偏移无效".to_string());
    }

    reader
        .seek(SeekFrom::Start(e_lfanew))
        .map_err(|_| "安装包 PE 头偏移越界".to_string())?;
    let mut pe_header = [0u8; 24];
    reader
        .read_exact(&mut pe_header)
        .map_err(|_| "安装包 PE 头不完整".to_string())?;

    if &pe_header[0..4] != b"PE\0\0" {
        return Err("安装包缺少 PE\\0\\0 签名".to_string());
    }

    let machine = u16::from_le_bytes([pe_header[4], pe_header[5]]);
    if machine != IMAGE_FILE_MACHINE_AMD64 {
        return Err(format!(
            "安装包架构不匹配（Machine=0x{machine:04X}，需要 x64）"
        ));
    }

    let characteristics = u16::from_le_bytes([pe_header[22], pe_header[23]]);
    if (characteristics & IMAGE_FILE_EXECUTABLE_IMAGE) == 0
        || (characteristics & IMAGE_FILE_DLL) != 0
    {
        return Err(format!(
            "安装包不是有效的可执行映像（Characteristics=0x{characteristics:04X}）"
        ));
    }

    Ok(())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn file_basename(path_token: &str) -> &str {
    path_token.rsplit(['/', '\\']).next().unwrap_or(path_token)
}

fn parse_sha256_for_asset(text: &str, asset_name: Option<&str>) -> Option<String> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let non_empty: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    if non_empty.len() == 1 && is_hex64(non_empty[0]) {
        return Some(non_empty[0].to_ascii_lowercase());
    }

    let target = asset_name.unwrap_or("airmonitor.exe");
    for line in &non_empty {
        let mut parts = line.splitn(2, |c: char| c.is_ascii_whitespace());
        let hash_tok = parts.next()?.trim();
        let rest = parts.next().unwrap_or("").trim();
        if !is_hex64(hash_tok) || rest.is_empty() {
            continue;
        }
        let cleaned = rest.strip_prefix('*').unwrap_or(rest).trim();
        if file_basename(cleaned).eq_ignore_ascii_case(target) {
            return Some(hash_tok.to_ascii_lowercase());
        }
    }

    None
}

fn compute_file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("无法打开安装包计算 SHA-256: {e}"))?;
    compute_sha256_reader(&mut file)
}

fn compute_sha256_reader<R: Read>(reader: &mut R) -> Result<String, String> {
    let api = BCryptApi::load()?;

    let mut alg_handle: BCryptAlgHandle = std::ptr::null_mut();
    let status = unsafe {
        (api.open_algorithm_provider)(&mut alg_handle, w!("SHA256").as_ptr(), std::ptr::null(), 0)
    };
    if status < 0 || alg_handle.is_null() {
        return Err(format!("初始化 SHA-256 算法失败 (0x{:08X})", status as u32));
    }
    let alg = BCryptAlgGuard {
        handle: alg_handle,
        close_fn: api.close_algorithm_provider,
    };

    let mut obj_len: u32 = 0;
    let mut cb_result: u32 = 0;
    let prop_status = unsafe {
        (api.get_property)(
            alg.handle,
            w!("ObjectLength").as_ptr(),
            (&mut obj_len as *mut u32).cast::<u8>(),
            std::mem::size_of::<u32>() as u32,
            &mut cb_result,
            0,
        )
    };
    if prop_status < 0 || obj_len == 0 {
        return Err(format!(
            "读取 SHA-256 对象长度失败 (0x{:08X})",
            prop_status as u32
        ));
    }

    let mut hash_obj = vec![0u8; obj_len as usize];
    let mut hash_handle: BCryptHashHandle = std::ptr::null_mut();
    let create_status = unsafe {
        (api.create_hash)(
            alg.handle,
            &mut hash_handle,
            hash_obj.as_mut_ptr(),
            obj_len,
            std::ptr::null(),
            0,
            0,
        )
    };
    if create_status < 0 || hash_handle.is_null() {
        return Err(format!(
            "创建 SHA-256 哈希上下文失败 (0x{:08X})",
            create_status as u32
        ));
    }
    let hash = BCryptHashGuard {
        handle: hash_handle,
        destroy_fn: api.destroy_hash,
    };

    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("读取文件计算 SHA-256 失败: {e}"))?;
        if n == 0 {
            break;
        }
        let hash_status = unsafe { (api.hash_data)(hash.handle, buf.as_ptr(), n as u32, 0) };
        if hash_status < 0 {
            return Err(format!("SHA-256 计算失败 (0x{:08X})", hash_status as u32));
        }
    }

    let mut digest = [0u8; 32];
    let finish_status = unsafe { (api.finish_hash)(hash.handle, digest.as_mut_ptr(), 32, 0) };
    if finish_status < 0 {
        return Err(format!(
            "完成 SHA-256 计算失败 (0x{:08X})",
            finish_status as u32
        ));
    }

    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedAssets {
    download_url: String,
    asset_name: String,
    asset_size: Option<u64>,
    sha256_url: Option<String>,
}

fn select_release_assets(assets: &[ReleaseAsset]) -> Option<SelectedAssets> {
    let mut valid_assets = Vec::new();
    for asset in assets {
        let Some(name) = asset.name.as_deref() else {
            continue;
        };
        let Some(url) = asset.browser_download_url.as_deref() else {
            continue;
        };
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            continue;
        }
        if !is_allowed_download_url(url) {
            continue;
        }
        valid_assets.push((name, url, asset.size));
    }

    let selected_exe = valid_assets
        .iter()
        .find(|&&(name, _, _)| name.eq_ignore_ascii_case("airmonitor.exe"))
        .or_else(|| {
            valid_assets.iter().find(|&&(name, _, _)| {
                let lower = name.to_ascii_lowercase();
                lower.starts_with("airmonitor")
                    && lower.ends_with(".exe")
                    && !lower.contains("probe")
            })
        })
        .copied()?;

    let (exe_name, exe_url, exe_size) = selected_exe;
    let exact_sha_name = format!("{exe_name}.sha256");

    let sha256_url = valid_assets
        .iter()
        .find(|&&(name, _, _)| name.eq_ignore_ascii_case(&exact_sha_name))
        .or_else(|| {
            valid_assets
                .iter()
                .find(|&&(name, _, _)| name.eq_ignore_ascii_case("sha256sums.txt"))
        })
        .or_else(|| {
            valid_assets
                .iter()
                .find(|&&(name, _, _)| name.eq_ignore_ascii_case("sha256sums"))
        })
        .or_else(|| {
            valid_assets
                .iter()
                .find(|&&(name, _, _)| name.eq_ignore_ascii_case("checksums.txt"))
        })
        .map(|&(_, url, _)| url.to_string())
        .or_else(|| {
            let mut sha_files = valid_assets.iter().filter(|&&(name, _, _)| {
                let lower = name.to_ascii_lowercase();
                lower.ends_with(".sha256") && !lower.contains("probe")
            });
            let first = sha_files.next()?;
            if sha_files.next().is_none() {
                Some(first.1.to_string())
            } else {
                None
            }
        });

    Some(SelectedAssets {
        download_url: exe_url.to_string(),
        asset_name: exe_name.to_string(),
        asset_size: exe_size,
        sha256_url,
    })
}

fn process_release_response(status: u32, body: &[u8], current_version: &str) -> UpdateStatus {
    match status {
        200 => {
            let text = match std::str::from_utf8(body) {
                Ok(s) => s,
                Err(_) => {
                    return UpdateStatus::Error("服务器响应不是有效的 UTF-8 文本".to_string())
                }
            };
            let release = match parse_release_json(text) {
                Ok(r) => r,
                Err(e) => return UpdateStatus::Error(e),
            };
            let Some(raw_tag) = release.tag_name else {
                return UpdateStatus::Error("发布信息缺少 tag_name 字段".to_string());
            };
            let latest_ver = match SemVer::parse(&raw_tag) {
                Some(v) => v,
                None => return UpdateStatus::Error("无法解析最新版本号".to_string()),
            };
            let current_ver = match SemVer::parse(current_version) {
                Some(v) => v,
                None => return UpdateStatus::Error("无法解析当前版本号".to_string()),
            };

            let normalized_latest = normalize_version_tag(&raw_tag);
            let normalized_current = normalize_version_tag(current_version);

            if latest_ver > current_ver {
                let release_url = release
                    .html_url
                    .filter(|u| is_allowed_repo_url(u))
                    .unwrap_or_else(|| RELEASES_URL.to_string());
                let selected = select_release_assets(&release.assets);
                let (download_url, asset_name, asset_size, sha256_url) = match selected {
                    Some(a) => (
                        Some(a.download_url),
                        Some(a.asset_name),
                        a.asset_size,
                        a.sha256_url,
                    ),
                    None => (None, None, None, None),
                };

                UpdateStatus::UpdateAvailable {
                    current: normalized_current,
                    latest: normalized_latest,
                    release_url,
                    published_at: release.published_at,
                    download_url,
                    asset_name,
                    asset_size,
                    sha256_url,
                }
            } else {
                UpdateStatus::UpToDate {
                    current: normalized_current,
                    latest: normalized_latest,
                }
            }
        }
        404 => UpdateStatus::NoReleaseYet {
            current: normalize_version_tag(current_version),
        },
        403 | 429 => {
            UpdateStatus::Error("GitHub API 限流，请稍后重试或点击「查看 Releases」".to_string())
        }
        other => UpdateStatus::Error(format!("服务器返回 HTTP {}", other)),
    }
}

fn normalize_version_tag(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed)
        .to_string()
}

#[derive(Debug, Clone, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<Vec<PreId>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreId {
    Numeric(u64),
    Alpha(String),
}

impl Ord for PreId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PreId::Numeric(a), PreId::Numeric(b)) => a.cmp(b),
            (PreId::Numeric(_), PreId::Alpha(_)) => Ordering::Less,
            (PreId::Alpha(_), PreId::Numeric(_)) => Ordering::Greater,
            (PreId::Alpha(a), PreId::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SemVer {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => {
                    for (ia, ib) in a.iter().zip(b.iter()) {
                        let ord = ia.cmp(ib);
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    a.len().cmp(&b.len())
                }
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl SemVer {
    fn parse(raw: &str) -> Option<Self> {
        let s = raw.trim();
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);
        let without_build = s.split('+').next()?;
        let (core, pre_str) = match without_build.split_once('-') {
            Some((c, p)) => {
                if p.is_empty() {
                    return None;
                }
                (c, Some(p))
            }
            None => (without_build, None),
        };

        let mut parts = core.split('.');
        let major = parse_semver_num(parts.next()?)?;
        let minor = parse_semver_num(parts.next()?)?;
        let patch = parse_semver_num(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }

        let pre = match pre_str {
            Some(p) => {
                let mut ids = Vec::new();
                for seg in p.split('.') {
                    if seg.is_empty()
                        || !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        return None;
                    }
                    if seg.bytes().all(|b| b.is_ascii_digit()) {
                        let num = parse_semver_num(seg)?;
                        ids.push(PreId::Numeric(num));
                    } else {
                        ids.push(PreId::Alpha(seg.to_string()));
                    }
                }
                Some(ids)
            }
            None => None,
        };

        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

fn parse_semver_num(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse::<u64>().ok()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ReleaseAsset {
    name: Option<String>,
    browser_download_url: Option<String>,
    size: Option<u64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ReleaseJson {
    tag_name: Option<String>,
    html_url: Option<String>,
    published_at: Option<String>,
    assets: Vec<ReleaseAsset>,
}

fn parse_release_json(input: &str) -> Result<ReleaseJson, String> {
    let input = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    let mut parser = JsonParser::new(input);
    parser.skip_ws();
    parser.expect_byte(b'{')?;
    parser.skip_ws();

    let mut out = ReleaseJson::default();
    if parser.peek() == Some(b'}') {
        parser.pos += 1;
        parser.skip_ws();
        if !parser.is_eof() {
            return Err("JSON 末尾存在多余内容".to_string());
        }
        return Ok(out);
    }

    loop {
        parser.skip_ws();
        let key = parser.parse_string()?;
        parser.skip_ws();
        parser.expect_byte(b':')?;
        parser.skip_ws();

        match key.as_str() {
            "tag_name" => match parser.peek() {
                Some(b'"') => out.tag_name = Some(parser.parse_string()?),
                _ => parser.skip_value()?,
            },
            "html_url" => match parser.peek() {
                Some(b'"') => out.html_url = Some(parser.parse_string()?),
                _ => parser.skip_value()?,
            },
            "published_at" => match parser.peek() {
                Some(b'"') => {
                    let s = parser.parse_string()?;
                    if !s.is_empty() {
                        out.published_at = Some(s);
                    }
                }
                _ => parser.skip_value()?,
            },
            "assets" => match parser.peek() {
                Some(b'[') => out.assets = parser.parse_assets_array()?,
                _ => parser.skip_value()?,
            },
            _ => parser.skip_value()?,
        }

        parser.skip_ws();
        match parser.peek() {
            Some(b',') => {
                parser.pos += 1;
            }
            Some(b'}') => {
                parser.pos += 1;
                break;
            }
            _ => return Err("JSON 对象语法无效".to_string()),
        }
    }

    parser.skip_ws();
    if !parser.is_eof() {
        return Err("JSON 末尾存在多余内容".to_string());
    }

    Ok(out)
}

struct JsonParser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), String> {
        match self.peek() {
            Some(b) if b == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err("JSON 语法错误".to_string()),
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("JSON \\u 转义不完整".to_string());
        }
        let mut val: u16 = 0;
        for &b in &self.bytes[self.pos..self.pos + 4] {
            let nibble = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err("JSON \\u 转义包含非法十六进制字符".to_string()),
            };
            val = (val << 4) | (nibble as u16);
        }
        self.pos += 4;
        Ok(val)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect_byte(b'"')?;
        let mut out = String::new();
        let mut chunk_start = self.pos;

        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                out.push_str(&self.src[chunk_start..self.pos]);
                self.pos += 1;
                return Ok(out);
            }
            if b < 0x20 {
                return Err("JSON 字符串包含未转义控制字符".to_string());
            }
            if b == b'\\' {
                out.push_str(&self.src[chunk_start..self.pos]);
                self.pos += 1;
                let esc = self
                    .peek()
                    .ok_or_else(|| "JSON 字符串转义未闭合".to_string())?;
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            if self.pos + 2 > self.bytes.len()
                                || self.bytes[self.pos] != b'\\'
                                || self.bytes[self.pos + 1] != b'u'
                            {
                                return Err("JSON 字符串包含孤立 UTF-16 高代理项".to_string());
                            }
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err("JSON 字符串包含无效 UTF-16 代理对".to_string());
                            }
                            let cp =
                                0x10000 + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                            let ch = char::from_u32(cp)
                                .ok_or_else(|| "JSON Unicode 码位无效".to_string())?;
                            out.push(ch);
                        } else if (0xDC00..=0xDFFF).contains(&code) {
                            return Err("JSON 字符串包含孤立 UTF-16 低代理项".to_string());
                        } else {
                            let ch = char::from_u32(code as u32)
                                .ok_or_else(|| "JSON Unicode 码位无效".to_string())?;
                            out.push(ch);
                        }
                    }
                    _ => return Err("JSON 字符串包含非法转义序列".to_string()),
                }
                chunk_start = self.pos;
            } else {
                self.pos += 1;
            }
        }

        Err("JSON 字符串未闭合".to_string())
    }

    fn parse_assets_array(&mut self) -> Result<Vec<ReleaseAsset>, String> {
        self.expect_byte(b'[')?;
        self.skip_ws();
        let mut assets = Vec::new();

        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(assets);
        }

        loop {
            self.skip_ws();
            if self.peek() == Some(b'{') {
                assets.push(self.parse_asset_object()?);
            } else {
                self.skip_value()?;
            }

            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("JSON 数组语法无效".to_string()),
            }
        }

        Ok(assets)
    }

    fn parse_asset_object(&mut self) -> Result<ReleaseAsset, String> {
        self.expect_byte(b'{')?;
        self.skip_ws();
        let mut asset = ReleaseAsset::default();

        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(asset);
        }

        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect_byte(b':')?;
            self.skip_ws();

            match key.as_str() {
                "name" => match self.peek() {
                    Some(b'"') => asset.name = Some(self.parse_string()?),
                    _ => self.skip_value()?,
                },
                "browser_download_url" => match self.peek() {
                    Some(b'"') => asset.browser_download_url = Some(self.parse_string()?),
                    _ => self.skip_value()?,
                },
                "size" => match self.peek() {
                    Some(b'0'..=b'9') => asset.size = self.parse_u64_json()?,
                    _ => self.skip_value()?,
                },
                _ => self.skip_value()?,
            }

            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err("JSON 对象语法无效".to_string()),
            }
        }

        Ok(asset)
    }

    fn parse_u64_json(&mut self) -> Result<Option<u64>, String> {
        let start = self.pos;
        self.skip_number()?;
        let slice = &self.src[start..self.pos];
        if slice.bytes().all(|b| b.is_ascii_digit()) {
            Ok(slice.parse::<u64>().ok())
        } else {
            Ok(None)
        }
    }

    fn skip_string(&mut self) -> Result<(), String> {
        self.expect_byte(b'"')?;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(());
            }
            if b < 0x20 {
                return Err("JSON 字符串包含未转义控制字符".to_string());
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = self
                    .peek()
                    .ok_or_else(|| "JSON 字符串转义未闭合".to_string())?;
                self.pos += 1;
                match esc {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                    b'u' => {
                        let _ = self.parse_hex4()?;
                    }
                    _ => return Err("JSON 字符串包含非法转义序列".to_string()),
                }
            } else {
                self.pos += 1;
            }
        }
        Err("JSON 字符串未闭合".to_string())
    }

    fn skip_literal(&mut self, lit: &[u8]) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err("JSON 字面量无效".to_string())
        }
    }

    fn skip_number(&mut self) -> Result<(), String> {
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err("JSON 数字格式无效".to_string()),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("JSON 小数部分无效".to_string());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err("JSON 指数部分无效".to_string());
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        Ok(())
    }

    fn skip_value(&mut self) -> Result<(), String> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => self.skip_string(),
            Some(b't') => self.skip_literal(b"true"),
            Some(b'f') => self.skip_literal(b"false"),
            Some(b'n') => self.skip_literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.skip_number(),
            Some(b'{' | b'[') => self.skip_composite(),
            _ => Err("JSON 值无效".to_string()),
        }
    }

    fn skip_composite(&mut self) -> Result<(), String> {
        let mut stack: Vec<u8> = Vec::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            match b {
                b'"' => {
                    self.skip_string()?;
                }
                b'{' => {
                    if stack.len() >= MAX_JSON_DEPTH {
                        return Err("JSON 嵌套过深".to_string());
                    }
                    stack.push(b'}');
                    self.pos += 1;
                }
                b'[' => {
                    if stack.len() >= MAX_JSON_DEPTH {
                        return Err("JSON 嵌套过深".to_string());
                    }
                    stack.push(b']');
                    self.pos += 1;
                }
                b'}' | b']' => {
                    if stack.pop() != Some(b) {
                        return Err("JSON 括号不匹配".to_string());
                    }
                    self.pos += 1;
                    if stack.is_empty() {
                        return Ok(());
                    }
                }
                _ => {
                    self.pos += 1;
                }
            }
        }
        Err("JSON 复合结构未闭合".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_synthetic_pe(machine: u16, characteristics: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 256];
        buf[0] = b'M';
        buf[1] = b'Z';
        let lfanew: u32 = 0x80;
        buf[0x3C..0x40].copy_from_slice(&lfanew.to_le_bytes());
        buf[0x80..0x84].copy_from_slice(b"PE\0\0");
        buf[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        buf[0x96..0x98].copy_from_slice(&characteristics.to_le_bytes());
        buf
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let seq = WRITE_CHECK_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "airmonitor_updater_{}_{}_{}",
            name,
            std::process::id(),
            seq
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test temp dir");
        dir
    }

    #[test]
    fn semver_parsing_and_ordering() {
        let v010 = SemVer::parse("v0.1.0").unwrap();
        let plain010 = SemVer::parse("0.1.0").unwrap();
        let upper_v020 = SemVer::parse("V0.2.0").unwrap();
        let v0100 = SemVer::parse("0.10.0").unwrap();
        let beta1 = SemVer::parse("0.2.0-beta.1").unwrap();
        let alpha = SemVer::parse("0.2.0-alpha").unwrap();
        let alpha1 = SemVer::parse("0.2.0-alpha.1").unwrap();
        let num_pre = SemVer::parse("0.2.0-1").unwrap();

        assert_eq!(v010, plain010);
        assert!(v0100 > upper_v020);
        assert!(upper_v020 > v010);
        assert!(upper_v020 > beta1);
        assert!(beta1 > alpha1);
        assert!(alpha1 > alpha);
        assert!(alpha > num_pre);
        assert_eq!(
            SemVer::parse("v1.2.3+build.1").unwrap(),
            SemVer::parse("1.2.3+build.2").unwrap()
        );

        assert!(SemVer::parse("").is_none());
        assert!(SemVer::parse("v1.2").is_none());
        assert!(SemVer::parse("1.2.3.4").is_none());
        assert!(SemVer::parse("01.2.3").is_none());
        assert!(SemVer::parse("1.2.3-").is_none());
    }

    #[test]
    fn json_parser_ignores_nested_keys_and_parses_assets() {
        let payload = r#"{
            "url": "https://api.github.com/repos/AmlitaEnan/AirMonitor/releases/1",
            "author": {
                "login": "octocat",
                "html_url": "https://github.com/other-user",
                "tag_name": "v99.99.99"
            },
            "assets": [
                {
                    "name": "airmonitor-probe.exe",
                    "size": 524288,
                    "browser_download_url": "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor-probe.exe"
                },
                {
                    "name": "airmonitor.exe",
                    "size": 1122334,
                    "html_url": "https://github.com/wrong/asset",
                    "browser_download_url": "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe",
                    "uploader": {
                        "name": "bot",
                        "size": 999,
                        "published_at": "1999-01-01T00:00:00Z",
                        "note": "string with } and ] and \"html_url\": \"trap\""
                    }
                },
                {
                    "name": "SHA256SUMS.txt",
                    "size": 256,
                    "browser_download_url": "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/SHA256SUMS.txt"
                }
            ],
            "tag_name": "v0.2.0",
            "html_url": "https:\/\/github.com\/AmlitaEnan\/AirMonitor\/releases\/tag\/v0.2.0",
            "published_at": "2026-11-20T08:30:00Z",
            "body": "Line1\nLine2\t\"quoted\" \\slash \u0041 \uD83D\uDE80"
        }"#;

        let parsed = parse_release_json(payload).expect("valid release json");
        assert_eq!(parsed.tag_name.as_deref(), Some("v0.2.0"));
        assert_eq!(
            parsed.html_url.as_deref(),
            Some("https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.2.0")
        );
        assert_eq!(parsed.published_at.as_deref(), Some("2026-11-20T08:30:00Z"));
        assert_eq!(parsed.assets.len(), 3);
        assert_eq!(parsed.assets[1].name.as_deref(), Some("airmonitor.exe"));
        assert_eq!(parsed.assets[1].size, Some(1_122_334));

        let selected = select_release_assets(&parsed.assets).expect("should pick airmonitor.exe");
        assert_eq!(selected.asset_name, "airmonitor.exe");
        assert_eq!(selected.asset_size, Some(1_122_334));
        assert_eq!(
            selected.download_url,
            "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
        );
        assert_eq!(
            selected.sha256_url.as_deref(),
            Some(
                "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/SHA256SUMS.txt"
            )
        );
    }

    #[test]
    fn asset_selection_excludes_probe_and_untrusted_urls() {
        let probe_only = vec![ReleaseAsset {
            name: Some("airmonitor-probe.exe".to_string()),
            browser_download_url: Some(
                "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor-probe.exe"
                    .to_string(),
            ),
            size: Some(500_000),
        }];
        assert!(select_release_assets(&probe_only).is_none());

        let evil_exe = vec![ReleaseAsset {
            name: Some("airmonitor.exe".to_string()),
            browser_download_url: Some("https://evil.example.com/airmonitor.exe".to_string()),
            size: Some(900_000),
        }];
        assert!(select_release_assets(&evil_exe).is_none());

        let fallback_prefixed = vec![
            ReleaseAsset {
                name: Some("airmonitor-probe-x64.exe".to_string()),
                browser_download_url: Some(
                    "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor-probe-x64.exe"
                        .to_string(),
                ),
                size: Some(400_000),
            },
            ReleaseAsset {
                name: Some("AirMonitor-v0.2.0-x64.exe".to_string()),
                browser_download_url: Some(
                    "https://objects.githubusercontent.com/github-production-release-asset/12345/AirMonitor-v0.2.0-x64.exe"
                        .to_string(),
                ),
                size: Some(1_050_000),
            },
            ReleaseAsset {
                name: Some("AirMonitor-v0.2.0-x64.exe.sha256".to_string()),
                browser_download_url: Some(
                    "https://release-assets.githubusercontent.com/github-production-release-asset/12345/AirMonitor-v0.2.0-x64.exe.sha256"
                        .to_string(),
                ),
                size: Some(96),
            },
        ];
        let sel = select_release_assets(&fallback_prefixed).expect("should match prefixed exe");
        assert_eq!(sel.asset_name, "AirMonitor-v0.2.0-x64.exe");
        assert_eq!(
            sel.sha256_url.as_deref(),
            Some("https://release-assets.githubusercontent.com/github-production-release-asset/12345/AirMonitor-v0.2.0-x64.exe.sha256")
        );
    }

    #[test]
    fn json_parser_handles_escapes_null_and_malformed() {
        let payload = r#"{
            "tag_name": "v0.\u0033.0",
            "html_url": "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.3.0\"\\\/\uD83D\uDE80",
            "published_at": null
        }"#;
        let parsed = parse_release_json(payload).expect("valid escaped json");
        assert_eq!(parsed.tag_name.as_deref(), Some("v0.3.0"));
        assert_eq!(parsed.published_at, None);
        assert!(parsed.html_url.unwrap().contains("v0.3.0\"\\/🚀"));

        assert!(parse_release_json("").is_err());
        assert!(parse_release_json("[]").is_err());
        assert!(parse_release_json(r#"{"tag_name": "v1.0.0",}"#).is_err());
        assert!(parse_release_json(r#"{"tag_name": "unclosed"#).is_err());
        assert!(parse_release_json(r#"{"tag_name": "\uD800"}"#).is_err());
        assert!(parse_release_json(r#"{"tag_name": "\u中中"}"#).is_err());
        assert!(parse_release_json(r#"{"body": "\u中中", "tag_name": "v0.1.0"}"#).is_err());
        assert!(parse_release_json(r#"{"tag_name": "\u12"}"#).is_err());
        assert!(parse_release_json(r#"{"tag_name": "\uGGGG"}"#).is_err());
        assert!(parse_release_json(r#"{"nested": {"a": [1, 2}"#).is_err());
    }

    #[test]
    fn status_mapping_and_url_fallback() {
        let newer = br#"{
            "author": { "html_url": "https://github.com/attacker" },
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.2.0",
            "published_at": "2026-11-20T10:00:00Z",
            "assets": [
                {
                    "name": "airmonitor.exe",
                    "size": 1048576,
                    "browser_download_url": "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
                },
                {
                    "name": "SHA256SUMS.txt",
                    "size": 128,
                    "browser_download_url": "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/SHA256SUMS.txt"
                }
            ]
        }"#;
        assert_eq!(
            process_release_response(200, newer, "0.1.0"),
            UpdateStatus::UpdateAvailable {
                current: "0.1.0".to_string(),
                latest: "0.2.0".to_string(),
                release_url: "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.2.0"
                    .to_string(),
                published_at: Some("2026-11-20T10:00:00Z".to_string()),
                download_url: Some(
                    "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
                        .to_string()
                ),
                asset_name: Some("airmonitor.exe".to_string()),
                asset_size: Some(1_048_576),
                sha256_url: Some(
                    "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/SHA256SUMS.txt"
                        .to_string()
                ),
            }
        );

        let unsafe_url = br#"{
            "tag_name": "v0.2.0",
            "html_url": "https://evil.example.com/payload.exe",
            "published_at": null
        }"#;
        assert_eq!(
            process_release_response(200, unsafe_url, "0.1.0"),
            UpdateStatus::UpdateAvailable {
                current: "0.1.0".to_string(),
                latest: "0.2.0".to_string(),
                release_url: RELEASES_URL.to_string(),
                published_at: None,
                download_url: None,
                asset_name: None,
                asset_size: None,
                sha256_url: None,
            }
        );

        let same = br#"{"tag_name": "v0.1.0", "html_url": "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.1.0"}"#;
        assert_eq!(
            process_release_response(200, same, "0.1.0"),
            UpdateStatus::UpToDate {
                current: "0.1.0".to_string(),
                latest: "0.1.0".to_string(),
            }
        );

        assert_eq!(
            process_release_response(404, b"{}", "0.1.0"),
            UpdateStatus::NoReleaseYet {
                current: "0.1.0".to_string(),
            }
        );

        assert_eq!(
            process_release_response(403, b"{}", "0.1.0"),
            UpdateStatus::Error("GitHub API 限流，请稍后重试或点击「查看 Releases」".to_string())
        );
        assert_eq!(
            process_release_response(429, b"{}", "0.1.0"),
            UpdateStatus::Error("GitHub API 限流，请稍后重试或点击「查看 Releases」".to_string())
        );
        assert_eq!(
            process_release_response(500, b"{}", "0.1.0"),
            UpdateStatus::Error("服务器返回 HTTP 500".to_string())
        );
    }

    #[test]
    fn allowed_repo_url_whitelist() {
        assert!(is_allowed_repo_url(REPO_URL));
        assert!(is_allowed_repo_url(RELEASES_URL));
        assert!(is_allowed_repo_url(
            "https://github.com/AmlitaEnan/AirMonitor/releases/tag/v0.1.0"
        ));

        assert!(!is_allowed_repo_url(""));
        assert!(!is_allowed_repo_url(
            "http://github.com/AmlitaEnan/AirMonitor"
        ));
        assert!(!is_allowed_repo_url(
            "https://github.com/AmlitaEnan/AirMonitorEvil"
        ));
        assert!(!is_allowed_repo_url(
            "https://github.com/AmlitaEnan/AirMonitor/../Other"
        ));
        assert!(!is_allowed_repo_url(
            "https://github.com/AmlitaEnan/AirMonitor/..?a=1"
        ));
        assert!(!is_allowed_repo_url(
            "https://github.com/AmlitaEnan/AirMonitor/%2e%2e/Other"
        ));
        assert!(!is_allowed_repo_url("https://github.com/other/AirMonitor"));
        assert!(!is_allowed_repo_url("file:///C:/Windows/System32/cmd.exe"));
        assert!(!is_allowed_repo_url("cmd.exe"));
        assert!(!is_allowed_repo_url(
            "https://github.com/AmlitaEnan/AirMonitor/releases calc.exe"
        ));
    }

    #[test]
    fn allowed_download_url_whitelist() {
        assert!(is_allowed_download_url(
            "https://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
        ));
        assert!(is_allowed_download_url(
            "https://objects.githubusercontent.com/github-production-release-asset/12345/airmonitor.exe?X-Amz-Signature=abc"
        ));
        assert!(is_allowed_download_url(
            "https://release-assets.githubusercontent.com/github-production-release-asset/12345/airmonitor.exe"
        ));

        assert!(!is_allowed_download_url(""));
        assert!(!is_allowed_download_url(
            "http://github.com/AmlitaEnan/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
        ));
        assert!(!is_allowed_download_url(
            "http://objects.githubusercontent.com/abc/airmonitor.exe"
        ));
        assert!(!is_allowed_download_url(
            "https://github.com/other/AirMonitor/releases/download/v0.2.0/airmonitor.exe"
        ));
        assert!(!is_allowed_download_url(
            "https://evil.example.com/airmonitor.exe"
        ));
        assert!(!is_allowed_download_url(
            "https://objects.githubusercontent.com.evil.com/airmonitor.exe"
        ));
        assert!(!is_allowed_download_url(
            "https://github.com/AmlitaEnan/AirMonitor/"
        ));
        assert!(!is_allowed_download_url(
            "https://github.com/AmlitaEnan/AirMonitor/releases/../evil.exe"
        ));
        assert!(!is_allowed_download_url(
            "https://github.com/AmlitaEnan/AirMonitor/releases/%2E%2E/evil.exe"
        ));
        assert!(!is_allowed_download_url(
            "https://objects.githubusercontent.com/path/airmonitor.exe calc.exe"
        ));
    }

    #[test]
    fn sha256sums_format_parsing() {
        let h_main = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let h_probe = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

        let gnu_text = format!("{h_probe}  airmonitor-probe.exe\n{h_main}  airmonitor.exe\n");
        assert_eq!(
            parse_sha256_for_asset(&gnu_text, Some("airmonitor.exe")).as_deref(),
            Some(h_main)
        );
        assert_eq!(
            parse_sha256_for_asset(&gnu_text, Some("airmonitor-probe.exe")).as_deref(),
            Some(h_probe)
        );

        let gnu_binary = format!("{h_main} *./dist/AirMonitor.exe\r\n");
        assert_eq!(
            parse_sha256_for_asset(&gnu_binary, Some("airmonitor.exe")).as_deref(),
            Some(h_main)
        );

        let single_upper = format!("\u{FEFF}{}\n", h_main.to_ascii_uppercase());
        assert_eq!(
            parse_sha256_for_asset(&single_upper, Some("airmonitor-v0.2.0.exe")).as_deref(),
            Some(h_main)
        );

        assert!(
            parse_sha256_for_asset("not-a-hash  airmonitor.exe", Some("airmonitor.exe")).is_none()
        );
        assert!(parse_sha256_for_asset(&gnu_text, Some("missing.exe")).is_none());
    }

    #[test]
    fn bcrypt_sha256_nist_vectors() {
        let mut empty = Cursor::new(b"" as &[u8]);
        assert_eq!(
            compute_sha256_reader(&mut empty).expect("sha256 empty"),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut abc = Cursor::new(b"abc" as &[u8]);
        assert_eq!(
            compute_sha256_reader(&mut abc).expect("sha256 abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pe_x64_header_validation() {
        let valid = make_synthetic_pe(IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_EXECUTABLE_IMAGE);
        assert!(validate_pe_x64_reader(&mut Cursor::new(&valid)).is_ok());

        let dll = make_synthetic_pe(
            IMAGE_FILE_MACHINE_AMD64,
            IMAGE_FILE_EXECUTABLE_IMAGE | IMAGE_FILE_DLL,
        );
        assert!(validate_pe_x64_reader(&mut Cursor::new(&dll)).is_err());

        let non_exec = make_synthetic_pe(IMAGE_FILE_MACHINE_AMD64, 0);
        assert!(validate_pe_x64_reader(&mut Cursor::new(&non_exec)).is_err());

        let x86 = make_synthetic_pe(0x014C, IMAGE_FILE_EXECUTABLE_IMAGE);
        assert!(validate_pe_x64_reader(&mut Cursor::new(&x86)).is_err());

        let html = b"<html><body>404 Not Found</body></html>".repeat(4);
        assert!(validate_pe_x64_reader(&mut Cursor::new(&html)).is_err());

        let truncated = b"MZ\x90\x00";
        assert!(validate_pe_x64_reader(&mut Cursor::new(truncated)).is_err());
    }

    #[test]
    fn atomic_replace_and_rollback_helpers() {
        let dir = unique_test_dir("atomic");
        let current_exe = dir.join("airmonitor.exe");
        let new_exe = dir.join("airmonitor.exe.new");
        let old_exe = dir.join("airmonitor.exe.old");

        // Case 1: spawn succeeds -> current_exe has new bytes, new_exe gone, old_exe exists
        fs::write(&current_exe, b"old-v1").unwrap();
        fs::write(&new_exe, b"new-v2").unwrap();
        apply_staged_update_with_spawner(&current_exe, &new_exe, &old_exe, |_| Ok(()))
            .expect("atomic apply should succeed");
        assert_eq!(fs::read(&current_exe).unwrap(), b"new-v2");
        assert_eq!(fs::read(&old_exe).unwrap(), b"old-v1");
        assert!(!new_exe.exists());

        cleanup_staged_files_in_dir(&dir, &current_exe);
        assert!(!old_exe.exists());
        assert!(!new_exe.exists());

        // Case 2: spawn fails -> current_exe rolled back to old bytes, new_exe removed
        fs::write(&current_exe, b"old-v1").unwrap();
        fs::write(&new_exe, b"broken-v2").unwrap();
        let err = apply_staged_update_with_spawner(&current_exe, &new_exe, &old_exe, |_| {
            Err("simulated launch failure".to_string())
        })
        .unwrap_err();
        assert!(err.contains("回滚"));
        assert_eq!(fs::read(&current_exe).unwrap(), b"old-v1");
        assert!(!new_exe.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_pid_arg_parsing() {
        assert_eq!(
            parse_wait_pid_from_args(["airmonitor.exe", "--wait-pid", "12345"]),
            Some(12345)
        );
        assert_eq!(
            parse_wait_pid_from_args(["airmonitor.exe", "--wait-pid=9876"]),
            Some(9876)
        );
        assert_eq!(parse_wait_pid_from_args(["airmonitor.exe"]), None);
        assert_eq!(
            parse_wait_pid_from_args(["airmonitor.exe", "--wait-pid", "abc"]),
            None
        );
    }
}
