//! The attach side: puts this console into raw mode, forwards keyboard
//! input to the server as plain text + standard VT sequences (exactly what
//! sshd feeds a console, parsed by every conhost since Win10 1809), and
//! pumps server output to the screen. Ctrl+\ detaches.

use std::io;
use std::mem::zeroed;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    GetConsoleCP, GetConsoleMode, GetConsoleOutputCP, ReadConsoleInputW, SetConsoleCP,
    SetConsoleCtrlHandler, SetConsoleMode, SetConsoleOutputCP, INPUT_RECORD, KEY_EVENT_RECORD,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    ExitProcess, GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW,
};

use crate::protocol::*;
use crate::winutil::*;

const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
const ENABLE_WINDOW_INPUT: u32 = 0x0008;
const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
const CP_UTF8: u32 = 65001;
const EV_KEY: u16 = 1;
const EV_WINDOW_BUFFER_SIZE: u16 = 4;

// Detach key constants (Ctrl+\): VK_OEM_5 is backslash on US layouts; the
// FS control char catches every layout and every VT transport.
const VK_OEM_5: u16 = 0xDC;
const SHIFT_PRESSED: u32 = 0x0010;
const CTRL_PRESSED: u32 = 0x0008 | 0x0004; // left | right
const ALT_PRESSED: u32 = 0x0001 | 0x0002; // right | left
const FS_CHAR: u16 = 0x1C; // the control char Ctrl+\ produces

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;

// NB: no CREATE_NEW_PROCESS_GROUP here — it sets the process-level
// "ignore Ctrl+C" flag, which children INHERIT, so the session shell
// would silently drop every CTRL_C_EVENT.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

// Saved console state, restorable exactly once from any thread.
static SAVED: AtomicBool = AtomicBool::new(false);
static RESTORED: AtomicBool = AtomicBool::new(false);
static IN_MODE: AtomicU32 = AtomicU32::new(0);
static OUT_MODE: AtomicU32 = AtomicU32::new(0);
static IN_CP: AtomicU32 = AtomicU32::new(0);
static OUT_CP: AtomicU32 = AtomicU32::new(0);
/// Set when the user detaches locally, so the reader thread exits quietly.
static DETACHING: AtomicBool = AtomicBool::new(false);
/// The pipe of the session this client is currently attached to. Swapped in
/// place when the server tells us to switch sessions; also used by the
/// console ctrl handler thread.
static CURRENT_PIPE: AtomicUsize = AtomicUsize::new(0);
/// Name of the currently attached session (kept in step with CURRENT_PIPE).
static CURRENT_NAME: Mutex<String> = Mutex::new(String::new());
/// Serializes client->server writes (input loop, ctrl handler, switcher) so
/// frames never interleave and nobody writes to a just-closed pipe.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Send a frame to the currently attached session. The pipe is resolved
/// under the write lock so a concurrent session switch can't close it
/// between load and write.
fn send_current(ty: u8, payload: &[u8]) -> io::Result<()> {
    let _g = WRITE_LOCK.lock().unwrap();
    let pipe = CURRENT_PIPE.load(Ordering::SeqCst) as HANDLE;
    if pipe.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "no session pipe",
        ));
    }
    write_frame(pipe, ty, payload)
}

/// Close the current pipe as part of a local detach.
fn detach_close() {
    DETACHING.store(true, Ordering::SeqCst);
    let _g = WRITE_LOCK.lock().unwrap();
    let pipe = CURRENT_PIPE.swap(0, Ordering::SeqCst) as HANDLE;
    if !pipe.is_null() {
        close_handle(pipe);
    }
}

/// Encode one key event as standard VT bytes — exactly what sshd feeds a
/// console host. Every conhost since Windows 10 1809 parses this; the
/// win32-input-mode protocol this replaced is only understood by modern
/// (Windows 11) conhosts and old ones silently drop it, which killed
/// Backspace/arrows/F-keys in sessions hosted on Windows 10.
fn encode_vt_key(out: &mut Vec<u8>, k: &KEY_EVENT_RECORD) {
    if k.bKeyDown == 0 {
        return; // VT has no key-up events
    }
    let uc = unsafe { k.uChar.UnicodeChar };
    let cs = k.dwControlKeyState;
    let shift = cs & SHIFT_PRESSED != 0;
    let alt = cs & ALT_PRESSED != 0;
    let ctrl = cs & CTRL_PRESSED != 0;
    // xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4)
    let m = 1 + shift as u8 + ((alt as u8) << 1) + ((ctrl as u8) << 2);

    fn csi_letter(out: &mut Vec<u8>, fin: u8, m: u8) {
        if m == 1 {
            out.extend_from_slice(&[0x1b, b'[', fin]);
        } else {
            out.extend_from_slice(format!("\x1b[1;{m}").as_bytes());
            out.push(fin);
        }
    }
    fn csi_tilde(out: &mut Vec<u8>, code: u8, m: u8) {
        if m == 1 {
            out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b[{code};{m}~").as_bytes());
        }
    }

    let mut one: Vec<u8> = Vec::with_capacity(8);
    match k.wVirtualKeyCode {
        0x08 => {
            // Backspace: terminals send DEL, and BS for the Ctrl chord.
            if alt {
                one.push(0x1b);
            }
            one.push(if ctrl { 0x08 } else { 0x7f });
        }
        0x09 => {
            if shift {
                one.extend_from_slice(b"\x1b[Z");
            } else {
                one.push(b'\t');
            }
        }
        0x0D => {
            if alt {
                one.push(0x1b);
            }
            one.push(b'\r');
        }
        0x1B => one.push(0x1b),
        0x20 if ctrl => one.push(0x00),        // Ctrl+Space -> NUL
        0x21 => csi_tilde(&mut one, 5, m),     // PgUp
        0x22 => csi_tilde(&mut one, 6, m),     // PgDn
        0x23 => csi_letter(&mut one, b'F', m), // End
        0x24 => csi_letter(&mut one, b'H', m), // Home
        0x25 => csi_letter(&mut one, b'D', m), // Left
        0x26 => csi_letter(&mut one, b'A', m), // Up
        0x27 => csi_letter(&mut one, b'C', m), // Right
        0x28 => csi_letter(&mut one, b'B', m), // Down
        0x2D => csi_tilde(&mut one, 2, m),     // Insert
        0x2E => csi_tilde(&mut one, 3, m),     // Delete
        vk @ 0x70..=0x73 => {
            // F1-F4: SS3 plain, CSI 1;m{P..S} with modifiers
            let fin = [b'P', b'Q', b'R', b'S'][(vk - 0x70) as usize];
            if m == 1 {
                one.extend_from_slice(&[0x1b, b'O', fin]);
            } else {
                csi_letter(&mut one, fin, m);
            }
        }
        vk @ 0x74..=0x7B => {
            // F5-F12
            let code = [15u8, 17, 18, 19, 20, 21, 23, 24][(vk - 0x74) as usize];
            csi_tilde(&mut one, code, m);
        }
        _ => {
            if uc == 0 {
                return; // bare modifier press or a key with no VT form
            }
            // AltGr (right alt + left ctrl) types a regular character.
            let altgr = cs & 0x0001 != 0 && cs & 0x0008 != 0;
            if alt && !altgr && uc < 0x80 {
                one.push(0x1b); // ESC-prefix Alt chords
            }
            if let Some(c) = char::from_u32(uc as u32) {
                let mut b = [0u8; 4];
                one.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
    for _ in 0..k.wRepeatCount.max(1) {
        out.extend_from_slice(&one);
    }
}

/// Forward Ctrl+Break (and a stray Ctrl+C, should one slip through) to the
/// session as a signal instead of dying. Runs on its own thread.
unsafe extern "system" fn ctrl_handler(ty: u32) -> i32 {
    if ty == CTRL_C_EVENT || ty == CTRL_BREAK_EVENT {
        let which = if ty == CTRL_BREAK_EVENT { 1u8 } else { 0u8 };
        let _ = send_current(C_SIGNAL, &[which]);
        return 1;
    }
    0
}

fn enter_raw() -> io::Result<()> {
    unsafe {
        let hin = std_handle(STD_INPUT_HANDLE);
        let hout = std_handle(STD_OUTPUT_HANDLE);
        let (mut im, mut om) = (0u32, 0u32);
        if GetConsoleMode(hin, &mut im) == 0 || GetConsoleMode(hout, &mut om) == 0 {
            return Err(io::Error::other(
                "fx must run in a console (interactive terminal)",
            ));
        }
        IN_MODE.store(im, Ordering::SeqCst);
        OUT_MODE.store(om, Ordering::SeqCst);
        IN_CP.store(GetConsoleCP(), Ordering::SeqCst);
        OUT_CP.store(GetConsoleOutputCP(), Ordering::SeqCst);
        SAVED.store(true, Ordering::SeqCst);

        // Raw input: no line buffering/echo/Ctrl+C processing, no VT
        // translation (we read raw INPUT_RECORDs and do our own encoding);
        // resize events on; quick-edit off.
        if SetConsoleMode(hin, ENABLE_WINDOW_INPUT | ENABLE_EXTENDED_FLAGS) == 0 {
            return Err(last_err());
        }
        if SetConsoleMode(
            hout,
            ENABLE_PROCESSED_OUTPUT
                | ENABLE_WRAP_AT_EOL_OUTPUT
                | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                | DISABLE_NEWLINE_AUTO_RETURN,
        ) == 0
        {
            return Err(last_err());
        }
        SetConsoleCP(CP_UTF8);
        SetConsoleOutputCP(CP_UTF8);
        Ok(())
    }
}

fn restore_console() {
    if !SAVED.load(Ordering::SeqCst) || RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let hout = std_handle(STD_OUTPUT_HANDLE);
        // Undo any lingering colors/hidden cursor from the session.
        let _ = write_all(hout, b"\x1b[0m\x1b[?25h");
        SetConsoleMode(std_handle(STD_INPUT_HANDLE), IN_MODE.load(Ordering::SeqCst));
        SetConsoleMode(hout, OUT_MODE.load(Ordering::SeqCst));
        SetConsoleCP(IN_CP.load(Ordering::SeqCst));
        SetConsoleOutputCP(OUT_CP.load(Ordering::SeqCst));
    }
}

fn say(msg: &str) {
    let _ = write_all(std_handle(STD_OUTPUT_HANDLE), msg.as_bytes());
}

/// Find the parent process id of this process.
fn parent_pid() -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let me = GetCurrentProcessId();
        let mut found = None;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == me {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        close_handle(snap);
        found
    }
}

fn process_image_path(pid: u32) -> Option<String> {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        close_handle(h);
        if ok == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Best-effort detection of the shell that launched this fx process, so new
/// sessions default to the environment they were created from: run fx from
/// cmd and the session is cmd; from pwsh and it's pwsh. Unknown parents
/// (Explorer, sshd, scripts) return None and the server's built-in default
/// chain applies.
fn detect_parent_shell() -> Option<String> {
    let path = process_image_path(parent_pid()?)?;
    let file = std::path::Path::new(&path)
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    match file.as_str() {
        "powershell.exe" | "pwsh.exe" => Some(format!("\"{path}\" -NoLogo")),
        "cmd.exe" | "bash.exe" | "nu.exe" => Some(format!("\"{path}\"")),
        _ => None,
    }
}

/// Spawn the detached per-session server process.
///
/// The session shell is resolved here: explicit choice (`fx name -- ...`)
/// beats FLUX_SHELL beats the shell that invoked fx; if none resolve, the
/// server's built-in default (PowerShell, then %ComSpec%) applies.
///
/// Raw CreateProcessW with bInheritHandles=FALSE: the server must not hold
/// duplicates of our console/stdout pipes, or anything capturing this
/// process's output (e.g. `$x = fx name -d`) would wait forever for EOF.
pub fn spawn_server(name: &str, shell: Option<&str>) -> io::Result<()> {
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let exe = std::env::current_exe()?;
    let exe = exe.to_string_lossy().into_owned();
    let (cols, rows) = console_size();
    let exe_w = wide(&exe);
    let shell = shell
        .map(str::to_string)
        .or_else(|| std::env::var("FLUX_SHELL").ok())
        .or_else(detect_parent_shell);
    let mut cmdline = format!("\"{exe}\" __server {name} {cols} {rows}");
    if let Some(sh) = &shell {
        cmdline.push_str(&format!(" \"{}\"", sh.replace('"', "\\\"")));
    }

    let base = DETACHED_PROCESS;
    let mut last = io::Error::other("spawn failed");
    // Breakaway first: escapes kill-on-close job objects (some sshd setups).
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let mut cmd_w = wide(&cmdline);
        let mut si: STARTUPINFOW = unsafe { zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
        let ok = unsafe {
            CreateProcessW(
                exe_w.as_ptr(),
                cmd_w.as_mut_ptr(),
                null(),
                null(),
                0, // inherit no handles
                flags,
                null(),
                null(),
                &si,
                &mut pi,
            )
        };
        if ok != 0 {
            close_handle(pi.hProcess);
            close_handle(pi.hThread);
            return Ok(());
        }
        last = last_err();
    }
    Err(last)
}

fn connect(name: &str, create: bool, shell: Option<&str>) -> io::Result<HANDLE> {
    let sid = user_sid_string()?;
    let path = pipe_path(&sid, name);
    match open_pipe(&path) {
        Ok(h) => Ok(h),
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) && create => {
            ensure_session(name, shell)?;
            open_pipe(&path)
        }
        Err(e) => Err(e),
    }
}

pub fn attach(name: &str, create: bool, shell: Option<&str>) -> i32 {
    if let Ok(current) = std::env::var("FLUX_SESSION") {
        // Inside a session: don't nest — ask our session's server to swap
        // the attached client(s) over to the target.
        return request_switch(&current, name, create, shell);
    }
    let pipe = match connect(name, create, shell) {
        Ok(h) => h,
        Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
            eprintln!("fx: no session '{name}' (use `fx {name}` to create it, or `fx ls`)");
            return 1;
        }
        Err(e) => {
            eprintln!("fx: cannot attach to '{name}': {e}");
            return 1;
        }
    };

    println!("[flux] attached to '{name}' — Ctrl+\\ detaches, 'exit' ends the session");
    if let Err(e) = enter_raw() {
        eprintln!("fx: {e}");
        close_handle(pipe);
        return 1;
    }

    CURRENT_PIPE.store(pipe as usize, Ordering::SeqCst);
    *CURRENT_NAME.lock().unwrap() = name.to_string();
    // Ctrl+Break must be forwarded, not obeyed (it is raised as a signal
    // regardless of console mode).
    unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), 1) };

    // Announce our size; the first resize also subscribes us to output.
    let (cols, rows) = console_size();
    if send_current(C_RESIZE, &resize_payload(cols, rows)).is_err() {
        restore_console();
        eprintln!("fx: connection failed");
        return 1;
    }

    spawn_reader(H(pipe));

    // Input loop: keyboard -> server.
    let code = input_loop((cols, rows));
    restore_console();
    if code == 0 {
        let last = CURRENT_NAME.lock().unwrap().clone();
        println!("\n[flux] detached from '{last}' — reattach with: fx {last}");
    }
    code
}

fn spawn_reader(pipe: H) {
    thread::spawn(move || reader_loop(pipe));
}

/// Server -> screen pump for one session connection. Ends either with the
/// whole process (session over / link lost) or by handing off to a new
/// reader after a session switch.
fn reader_loop(rpipe: H) {
    let hout = std_handle(STD_OUTPUT_HANDLE);
    loop {
        match read_frame(rpipe.raw()) {
            Ok((S_OUTPUT, p)) => {
                let _ = write_all(hout, &p);
            }
            Ok((S_EXIT, _)) => {
                restore_console();
                say("\r\n[flux] session ended.\r\n");
                unsafe { ExitProcess(0) };
            }
            Ok((S_DETACHED, _)) => {
                restore_console();
                say("\r\n[flux] detached.\r\n");
                unsafe { ExitProcess(0) };
            }
            Ok((S_SWITCH, p)) => {
                if let Ok(target) = String::from_utf8(p) {
                    if valid_name(&target) && do_switch(rpipe, &target) {
                        return; // a new reader owns the new pipe now
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {
                if DETACHING.load(Ordering::SeqCst) {
                    return; // local detach in progress; main thread reports
                }
                restore_console();
                say("\r\n[flux] connection lost.\r\n");
                unsafe { ExitProcess(1) };
            }
        }
    }
}

/// Swap this client from `old` to the session `target` in place: same
/// console, same raw mode — the new session's replay repaints the screen.
fn do_switch(old: H, target: &str) -> bool {
    let sid = match user_sid_string() {
        Ok(s) => s,
        Err(_) => return false,
    };
    let new = match open_pipe(&pipe_path(&sid, target)) {
        Ok(h) => h,
        Err(_) => {
            say(&format!(
                "\r\n[flux] switch failed: no session '{target}'\r\n"
            ));
            return false;
        }
    };
    let (cols, rows) = console_size();
    if write_frame(new, C_RESIZE, &resize_payload(cols, rows)).is_err() {
        close_handle(new);
        say("\r\n[flux] switch failed\r\n");
        return false;
    }
    {
        // Swap under the write lock so in-flight keystrokes either land on
        // the old session or the new one, never on a closed handle.
        let _g = WRITE_LOCK.lock().unwrap();
        CURRENT_PIPE.store(new as usize, Ordering::SeqCst);
        close_handle(old.raw());
    }
    *CURRENT_NAME.lock().unwrap() = target.to_string();
    say(&format!("\r\n[flux] \u{2192} session '{target}'\r\n"));
    spawn_reader(H(new));
    true
}

/// Ensure a session exists, spawning its server if needed and waiting for
/// its pipe to come up.
fn ensure_session(name: &str, shell: Option<&str>) -> io::Result<()> {
    let sid = user_sid_string()?;
    let path = pipe_path(&sid, name);
    if let Ok(h) = open_pipe(&path) {
        close_handle(h);
        return Ok(());
    }
    spawn_server(name, shell)?;
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(h) = open_pipe(&path) {
            close_handle(h);
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "server did not start (see %LOCALAPPDATA%\\flux\\ logs)",
    ))
}

/// `fx <target>` typed inside a session: create the target if allowed, then
/// ask our own server to move the attached client(s) over.
fn request_switch(current: &str, target: &str, create: bool, shell: Option<&str>) -> i32 {
    if current == target {
        println!("[flux] already in session '{target}'");
        return 0;
    }
    let sid = match user_sid_string() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fx: {e}");
            return 1;
        }
    };
    if create {
        if let Err(e) = ensure_session(target, shell) {
            eprintln!("fx: cannot start session '{target}': {e}");
            return 1;
        }
    } else if open_pipe(&pipe_path(&sid, target))
        .map(close_handle)
        .is_err()
    {
        eprintln!("fx: no session '{target}' (use `fx {target}` to create it)");
        return 1;
    }
    let h = match open_pipe(&pipe_path(&sid, current)) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("fx: cannot reach current session '{current}': {e}");
            return 1;
        }
    };
    let ok = write_frame(h, C_SWITCH, target.as_bytes()).is_ok();
    close_handle(h);
    if !ok {
        eprintln!("fx: switch request failed");
        return 1;
    }
    println!("[flux] switching to '{target}'");
    0
}

/// Ctrl+C must become a real console signal in the session (ConPTY's input
/// stream does not convert key events to signals on current Windows), so it
/// is intercepted and sent as C_SIGNAL instead of a keystroke. Matches both
/// proper chords (Windows Terminal) and the bare 0x03 records that plain VT
/// terminals produce. Alt combos (e.g. AltGr chars) are left alone.
fn is_ctrl_c(k: &KEY_EVENT_RECORD) -> bool {
    let uc = unsafe { k.uChar.UnicodeChar };
    let cs = k.dwControlKeyState;
    let alt = cs & 0x0003 != 0; // right | left alt
    !alt && (uc == 0x03 || (k.wVirtualKeyCode == 0x43 && cs & CTRL_PRESSED != 0 && uc <= 0x03))
}

/// Forward unmodified printable characters (and bare Enter/Tab) as raw
/// UTF-8. Pastes are floods of plain chars; raw text cannot be corrupted by
/// chunk boundaries the way escape sequences can. Returns true when the
/// record was consumed (key-ups of such chars are dropped: the session's
/// console host synthesizes its own when parsing text).
fn forward_plain(out: &mut Vec<u8>, k: &KEY_EVENT_RECORD, pending_high: &mut Option<u16>) -> bool {
    let uc = unsafe { k.uChar.UnicodeChar };
    let cs = k.dwControlKeyState;
    if cs & (CTRL_PRESSED | ALT_PRESSED) != 0 {
        return false; // modified keys go through the VT encoder
    }
    let repeat = k.wRepeatCount.max(1) as usize;

    // Surrogate pairs (emoji etc.) arrive as two records.
    if (0xD800..=0xDBFF).contains(&uc) {
        if k.bKeyDown != 0 {
            *pending_high = Some(uc);
        }
        return true;
    }
    if (0xDC00..=0xDFFF).contains(&uc) {
        if k.bKeyDown != 0 {
            if let Some(hi) = pending_high.take() {
                let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (uc as u32 - 0xDC00));
                if let Some(c) = char::from_u32(cp) {
                    push_utf8(out, c, repeat);
                }
            }
        }
        return true;
    }

    let plain_printable = uc >= 0x20 && uc != 0x7F;
    let bare_key = (uc == 0x0D || uc == 0x09) && cs & SHIFT_PRESSED == 0; // Enter/Tab
    if plain_printable || bare_key {
        if k.bKeyDown != 0 {
            if let Some(c) = char::from_u32(uc as u32) {
                push_utf8(out, c, repeat);
            }
        }
        return true;
    }
    false
}

fn push_utf8(out: &mut Vec<u8>, c: char, repeat: usize) {
    let mut buf = [0u8; 4];
    let s = c.encode_utf8(&mut buf);
    for _ in 0..repeat {
        out.extend_from_slice(s.as_bytes());
    }
}

/// Detach key: Ctrl+\ (Shift optional). Plain VT transports (ssh from any
/// OS/terminal) encode Ctrl+\ and Ctrl+Shift+\ identically as the bare FS
/// control char with no modifier info, so the char alone must trigger; the
/// modifier+VK form additionally catches it on full-fidelity local input.
fn is_detach_chord(k: &KEY_EVENT_RECORD) -> bool {
    if k.bKeyDown == 0 {
        return false;
    }
    let uc = unsafe { k.uChar.UnicodeChar };
    if uc == FS_CHAR {
        return true;
    }
    let cs = k.dwControlKeyState;
    cs & CTRL_PRESSED != 0 && cs & SHIFT_PRESSED != 0 && k.wVirtualKeyCode == VK_OEM_5
}

fn input_loop(initial: (i16, i16)) -> i32 {
    let hin = std_handle(STD_INPUT_HANDLE);
    let mut last_size = initial;
    let mut pending_high: Option<u16> = None;
    let mut records: [INPUT_RECORD; 128] = unsafe { zeroed() };
    loop {
        let mut n: u32 = 0;
        if unsafe { ReadConsoleInputW(hin, records.as_mut_ptr(), 128, &mut n) } == 0 {
            return 1;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let mut resized = false;
        let mut detach = false;
        for rec in records.iter().take(n as usize) {
            match rec.EventType {
                EV_KEY => {
                    let k = unsafe { rec.Event.KeyEvent };
                    // U+FEFF is the win32-input-mode handshake marker some
                    // console hosts inject — not a keystroke. Forwarding it
                    // would type an invisible char that breaks commands.
                    if unsafe { k.uChar.UnicodeChar } == 0xFEFF {
                        continue;
                    }
                    if is_detach_chord(&k) {
                        detach = true;
                        break; // swallow the chord and everything after it
                    }
                    if is_ctrl_c(&k) {
                        if k.bKeyDown != 0 {
                            if !bytes.is_empty() {
                                let _ = send_current(C_STDIN, &bytes);
                                bytes.clear();
                            }
                            let _ = send_current(C_SIGNAL, &[0]);
                        }
                        continue; // swallow both edges of the chord
                    }
                    if forward_plain(&mut bytes, &k, &mut pending_high) {
                        continue;
                    }
                    encode_vt_key(&mut bytes, &k);
                }
                EV_WINDOW_BUFFER_SIZE => resized = true,
                _ => {} // focus/menu events; mouse not enabled
            }
        }
        if resized {
            let size = console_size();
            if size != last_size {
                last_size = size;
                let _ = send_current(C_RESIZE, &resize_payload(size.0, size.1));
            }
        }
        if !bytes.is_empty() && send_current(C_STDIN, &bytes).is_err() {
            // Server went away; reader thread reports the details.
            thread::sleep(Duration::from_millis(200));
            return 1;
        }
        if detach {
            detach_close();
            return 0;
        }
    }
}
