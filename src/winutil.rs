//! Thin Win32 helpers shared by client and server.

use std::ffi::c_void;
use std::io;
use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_USER};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO,
};
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

use crate::protocol::{frame, MAX_FRAME};

// Constants defined locally (values are ABI-stable; avoids churn in windows-sys exports).
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const OPEN_EXISTING: u32 = 3;
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PIPE_BUSY: u32 = 231;
pub const ERROR_IO_PENDING: u32 = 997;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
pub const ERROR_PIPE_CONNECTED: u32 = 535;
pub const STD_INPUT_HANDLE: u32 = -10i32 as u32;
pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const TOKEN_QUERY: u32 = 0x8;
const TOKEN_USER_CLASS: i32 = 1; // TokenUser

/// Copyable, Send+Sync wrapper for a raw HANDLE so threads can share it.
#[derive(Clone, Copy)]
pub struct H(pub HANDLE);
unsafe impl Send for H {}
unsafe impl Sync for H {}

impl H {
    /// Accessor instead of `.0` so move-closures capture the Send wrapper,
    /// not the raw pointer field (Rust 2021 captures fields disjointly).
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn last_err() -> io::Error {
    io::Error::last_os_error()
}

pub fn close_handle(h: HANDLE) {
    unsafe {
        CloseHandle(h);
    }
}

// ---------------------------------------------------------------- raw I/O --
//
// Named pipe handles MUST use the overlapped functions below: on a
// synchronous handle the kernel serializes all I/O per file object, so a
// thread blocked in ReadFile blocks every concurrent WriteFile on the same
// handle until data happens to arrive — deadlocking duplex traffic.
// The sync functions are for ConPTY/console handles (one direction each).

struct OvOp {
    ov: OVERLAPPED,
}

impl OvOp {
    fn new() -> io::Result<Self> {
        let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
        if ev.is_null() {
            return Err(last_err());
        }
        let mut ov: OVERLAPPED = unsafe { zeroed() };
        ov.hEvent = ev;
        Ok(OvOp { ov })
    }

    /// Wait for the issued operation; `started` is the ReadFile/WriteFile
    /// return value. Returns bytes transferred.
    fn finish(&mut self, h: HANDLE, started: i32) -> io::Result<usize> {
        if started == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
        }
        let mut n: u32 = 0;
        let ok = unsafe { GetOverlappedResult(h, &self.ov, &mut n, 1) };
        if ok == 0 {
            return Err(last_err());
        }
        Ok(n as usize)
    }
}

impl Drop for OvOp {
    fn drop(&mut self) {
        if !self.ov.hEvent.is_null() {
            close_handle(self.ov.hEvent);
        }
    }
}

pub fn pipe_read_some(h: HANDLE, buf: &mut [u8]) -> io::Result<usize> {
    let mut op = OvOp::new()?;
    let started = unsafe {
        ReadFile(
            h,
            buf.as_mut_ptr(),
            buf.len() as u32,
            null_mut(),
            &mut op.ov,
        )
    };
    op.finish(h, started)
}

pub fn pipe_write_all(h: HANDLE, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let mut op = OvOp::new()?;
        let started =
            unsafe { WriteFile(h, buf.as_ptr(), buf.len() as u32, null_mut(), &mut op.ov) };
        let n = op.finish(h, started)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "pipe write returned 0",
            ));
        }
        buf = &buf[n..];
    }
    Ok(())
}

pub fn write_all(h: HANDLE, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let mut n: u32 = 0;
        let ok = unsafe { WriteFile(h, buf.as_ptr(), buf.len() as u32, &mut n, null_mut()) };
        if ok == 0 {
            return Err(last_err());
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
        buf = &buf[n as usize..];
    }
    Ok(())
}

pub fn read_some(h: HANDLE, buf: &mut [u8]) -> io::Result<usize> {
    let mut n: u32 = 0;
    let ok = unsafe { ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut n, null_mut()) };
    if ok == 0 {
        return Err(last_err());
    }
    Ok(n as usize)
}

fn pipe_read_exact(h: HANDLE, buf: &mut [u8]) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let n = pipe_read_some(h, &mut buf[off..])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
        }
        off += n;
    }
    Ok(())
}

/// Frames travel only over named pipes, so these use overlapped I/O.
pub fn write_frame(h: HANDLE, ty: u8, payload: &[u8]) -> io::Result<()> {
    pipe_write_all(h, &frame(ty, payload))
}

pub fn read_frame(h: HANDLE) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    pipe_read_exact(h, &mut hdr)?;
    let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized frame",
        ));
    }
    let mut payload = vec![0u8; len];
    pipe_read_exact(h, &mut payload)?;
    Ok((hdr[0], payload))
}

// ------------------------------------------------------ identity/security --

/// String SID of the current user, e.g. "S-1-5-21-...-1001".
pub fn user_sid_string() -> io::Result<String> {
    unsafe {
        let mut tok: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return Err(last_err());
        }
        let mut len: u32 = 0;
        GetTokenInformation(tok, TOKEN_USER_CLASS, null_mut(), 0, &mut len);
        let mut buf = vec![0u8; len.max(64) as usize];
        let ok = GetTokenInformation(
            tok,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut len,
        );
        close_handle(tok);
        if ok == 0 {
            return Err(last_err());
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut ws: *mut u16 = null_mut();
        if ConvertSidToStringSidW(tu.User.Sid, &mut ws) == 0 {
            return Err(last_err());
        }
        let mut n = 0usize;
        while *ws.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(ws, n));
        LocalFree(ws as *mut c_void);
        Ok(s)
    }
}

/// Security descriptor restricting the pipe to SYSTEM + the current user.
/// Returned pointer is intentionally leaked (lives for the process lifetime).
pub fn user_only_security_descriptor() -> io::Result<*mut c_void> {
    let sid = user_sid_string()?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid})");
    let w = wide(&sddl);
    let mut psd: *mut c_void = null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), 1, &mut psd, null_mut())
    };
    if ok == 0 {
        return Err(last_err());
    }
    Ok(psd)
}

// ----------------------------------------------------------------- console --

pub fn std_handle(which: u32) -> HANDLE {
    unsafe { GetStdHandle(which) }
}

/// Current console viewport size (cols, rows); falls back to 120x30.
pub fn console_size() -> (i16, i16) {
    unsafe {
        let h = std_handle(STD_OUTPUT_HANDLE);
        if h != INVALID_HANDLE_VALUE && !h.is_null() {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = zeroed();
            if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                let cols = info.srWindow.Right - info.srWindow.Left + 1;
                let rows = info.srWindow.Bottom - info.srWindow.Top + 1;
                if cols > 0 && rows > 0 {
                    return (cols, rows);
                }
            }
        }
        (120, 30)
    }
}

// -------------------------------------------------------------------- misc --

pub fn local_time_string() -> String {
    unsafe {
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = zeroed();
        GetLocalTime(&mut st);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
        )
    }
}

pub fn pipe_path(sid: &str, name: &str) -> String {
    format!(r"\\.\pipe\flux.{sid}.{name}")
}

pub fn pipe_prefix(sid: &str) -> String {
    format!("flux.{sid}.")
}

/// Open a client connection to a session pipe.
pub fn open_pipe(path: &str) -> io::Result<HANDLE> {
    let w = wide(path);
    loop {
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(h);
        }
        let e = unsafe { GetLastError() };
        if e == ERROR_PIPE_BUSY {
            // All instances busy: wait for the server to post a new one.
            use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
            if unsafe { WaitNamedPipeW(w.as_ptr(), 3000) } == 0 {
                return Err(last_err());
            }
            continue;
        }
        return Err(last_err());
    }
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Sorted names of this user's live sessions (one pipe per session).
pub fn list_sessions(sid: &str) -> Vec<String> {
    let mut names = Vec::new();
    let prefix = pipe_prefix(sid);
    if let Ok(rd) = std::fs::read_dir(r"\\.\pipe\") {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(rest) = n.strip_prefix(&prefix) {
                if valid_name(rest) {
                    names.push(rest.to_string());
                }
            }
        }
    }
    names.sort();
    names
}
