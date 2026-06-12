//! The per-session background server: owns the ConPTY-hosted shell, accepts
//! client connections on a named pipe, broadcasts shell output to attached
//! clients, and keeps running when no one is attached.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
use windows_sys::Win32::System::Threading::{
    ExitProcess, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

use crate::conpty;
use crate::protocol::*;
use crate::winutil::*;

const PIPE_ACCESS_DUPLEX: u32 = 0x3;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_BYTE_WAIT_LOCAL: u32 = 0x8; // PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const INFINITE: u32 = 0xFFFF_FFFF;
const RING_CAP: usize = 256 * 1024;

/// Input-mode requests that must never reach the user's real terminal.
/// If a terminal honors ?9001h (win32-input-mode) it starts sending every
/// key and paste as escape sequences over links (SSH) that chunk at
/// arbitrary byte boundaries; intermediate console hosts flush sequences
/// split across chunks as literal typed text, corrupting input. ?1004h
/// (focus events) is solicited but flux never forwards focus, so don't ask.
const FILTERED_SEQS: [&[u8]; 4] = [
    b"\x1b[?9001h",
    b"\x1b[?9001l",
    b"\x1b[?1004h",
    b"\x1b[?1004l",
];

/// Streaming filter that removes FILTERED_SEQS from a byte stream, holding
/// back a chunk's tail when it could be the start of a filtered sequence.
/// Holding is safe: a chunk ending mid-escape always has more bytes coming.
struct SeqFilter {
    hold: Vec<u8>,
}

impl SeqFilter {
    fn new() -> Self {
        SeqFilter { hold: Vec::new() }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.hold);
        data.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        'outer: while i < data.len() {
            if data[i] == 0x1b {
                let rest = &data[i..];
                for pat in FILTERED_SEQS {
                    if rest.starts_with(pat) {
                        i += pat.len();
                        continue 'outer;
                    }
                }
                if rest.len() < 8 && FILTERED_SEQS.iter().any(|p| p.starts_with(rest)) {
                    self.hold.extend_from_slice(rest);
                    break;
                }
            }
            out.push(data[i]);
            i += 1;
        }
        out
    }
}

struct Inner {
    /// Attached clients: (id, pipe handle). Handle is closed only by its handler thread.
    clients: Vec<(u64, H)>,
    /// Recent raw output, replayed to newly attaching clients.
    ring: VecDeque<u8>,
    last_size: (i16, i16),
}

struct Shared {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
    pty: conpty::Pty,
    shell_in: H,
    shell_proc: H,
    shell_pid: u32,
    name: String,
    started: String,
}

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

fn log(msg: &str) {
    if let Some(f) = LOG.get() {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "[{}] {}", local_time_string(), msg);
        }
    }
}

fn init_log(name: &str) {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let dir = format!("{base}\\flux");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{dir}\\{name}.log"))
    {
        let _ = LOG.set(Mutex::new(f));
    }
}

/// Deliver Ctrl+C / Ctrl+Break to the session. ConPTY's input stream does
/// not reliably convert key events into console signals (verified on this
/// Windows build), so do what conhost would: attach to the shell's console
/// and either raise the real signal (processed-input mode, i.e. a command
/// is running) or queue a real Ctrl+C key record (raw reader at the prompt,
/// e.g. PSReadLine).
fn deliver_signal(shell_pid: u32, which: u8) {
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleMode,
        SetConsoleCtrlHandler, WriteConsoleInputW, INPUT_RECORD,
    };
    static SIG_LOCK: Mutex<()> = Mutex::new(());
    let _g = SIG_LOCK.lock();
    unsafe {
        if AttachConsole(shell_pid) == 0 {
            log(&format!(
                "signal: AttachConsole failed (err {})",
                GetLastError()
            ));
            return;
        }
        // Attaching to a console resets ctrl state; re-arm our protection
        // BEFORE generating the event or it kills this server process.
        SetConsoleCtrlHandler(Some(server_ctrl_handler), 1);
        SetConsoleCtrlHandler(None, 1); // and ignore Ctrl+C outright
        let conin = windows_sys::Win32::Storage::FileSystem::CreateFileW(
            wide("CONIN$").as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            3, // FILE_SHARE_READ | FILE_SHARE_WRITE
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        );
        let mut processed = true; // if in doubt, send the signal
        if conin != INVALID_HANDLE_VALUE {
            let mut mode: u32 = 0;
            if GetConsoleMode(conin, &mut mode) != 0 {
                processed = mode & 0x1 != 0; // ENABLE_PROCESSED_INPUT
            }
        }
        if which == 1 || processed {
            // 0 = CTRL_C_EVENT, 1 = CTRL_BREAK_EVENT; group 0 = everyone on
            // this console. Our own ctrl handler ignores it (see run()).
            GenerateConsoleCtrlEvent(which as u32, 0);
            log("signal: ctrl event generated");
        } else if conin != INVALID_HANDLE_VALUE {
            // Raw-mode reader: hand it the actual keystroke.
            let mut recs: [INPUT_RECORD; 2] = std::mem::zeroed();
            for (i, rec) in recs.iter_mut().enumerate() {
                rec.EventType = 1; // KEY_EVENT
                let k = &mut rec.Event.KeyEvent;
                k.bKeyDown = if i == 0 { 1 } else { 0 };
                k.wRepeatCount = 1;
                k.wVirtualKeyCode = 0x43; // 'C'
                k.wVirtualScanCode = 0x2E;
                k.uChar.UnicodeChar = 0x03;
                k.dwControlKeyState = 0x0008; // left ctrl
            }
            let mut n: u32 = 0;
            WriteConsoleInputW(conin, recs.as_ptr(), 2, &mut n);
            log("signal: ctrl+c key queued (raw mode)");
        }
        if conin != INVALID_HANDLE_VALUE {
            close_handle(conin);
        }
        FreeConsole();
    }
}

/// Keep the server alive when it raises console ctrl events while attached
/// to the session's console.
unsafe extern "system" fn server_ctrl_handler(_ty: u32) -> i32 {
    1
}

/// Remove terminal query sequences from replayed output. Queries (DECRQM,
/// DA, DSR, color probes, XTGETTCAP) were answered live when first emitted;
/// replaying them makes the user's terminal answer AGAIN, and that stale
/// reply arrives as keystrokes typed into the shell (seen as "27;3$y").
fn strip_queries(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() {
            let kind = data[i + 1];
            if kind == b'[' {
                // CSI: runs to its final byte (0x40..=0x7E).
                let mut j = i + 2;
                while j < data.len() && !(0x40..=0x7e).contains(&data[j]) {
                    j += 1;
                }
                if j < data.len() {
                    let body = &data[i + 2..j];
                    let fin = data[j];
                    let is_query = fin == b'c' // DA1 / DA2
                        || (fin == b'n' && matches!(body, b"5" | b"6" | b"?6")) // DSR
                        || (fin == b'p' && body.ends_with(b"$")) // DECRQM
                        || (fin == b'q' && body.starts_with(b">")); // XTVERSION
                    if !is_query {
                        out.extend_from_slice(&data[i..=j]);
                    }
                    i = j + 1;
                    continue;
                }
            } else if kind == b']' || kind == b'P' {
                // OSC / DCS: runs to BEL or ST (ESC \).
                let mut j = i + 2;
                let mut end = None;
                while j < data.len() {
                    if data[j] == 0x07 {
                        end = Some(j + 1);
                        break;
                    }
                    if data[j] == 0x1b && j + 1 < data.len() && data[j + 1] == b'\\' {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                if let Some(e) = end {
                    let body = &data[i + 2..e];
                    let is_query = (kind == b']' && body.windows(2).any(|w| w == b";?"))
                        || (kind == b'P' && body.starts_with(b"+q"));
                    if !is_query {
                        out.extend_from_slice(&data[i..e]);
                    }
                    i = e;
                    continue;
                }
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

/// Overlapped ConnectNamedPipe (mandatory on overlapped pipe handles).
/// Returns true once a client is connected.
fn connect_instance(h: HANDLE) -> bool {
    unsafe {
        use windows_sys::Win32::System::Threading::CreateEventW;
        use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
        let ev = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        if ev.is_null() {
            return false;
        }
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = ev;
        let ok = ConnectNamedPipe(h, &mut ov);
        let result = if ok != 0 {
            true
        } else {
            match GetLastError() {
                ERROR_PIPE_CONNECTED => true,
                e if e == ERROR_IO_PENDING => {
                    let mut n: u32 = 0;
                    GetOverlappedResult(h, &ov, &mut n, 1) != 0
                }
                _ => false,
            }
        };
        close_handle(ev);
        result
    }
}

fn create_instance(path_w: &[u16], sd: *mut std::ffi::c_void, first: bool) -> Option<HANDLE> {
    let sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let h = unsafe {
        CreateNamedPipeW(
            path_w.as_ptr(),
            mode,
            PIPE_BYTE_WAIT_LOCAL,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(h)
    }
}

pub fn run(name: &str, cols: i16, rows: i16, shell_arg: Option<&str>) -> ! {
    // Clear our std handles (they point at NUL). A console-subsystem child
    // spawned onto a ConPTY only binds its std handles to that console when
    // the parent's std handle values are NULL; otherwise it inherits the
    // values, thinks stdin is redirected, sees EOF, and exits immediately.
    unsafe {
        use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, SetStdHandle};
        SetStdHandle(STD_INPUT_HANDLE, null_mut());
        SetStdHandle(STD_OUTPUT_HANDLE, null_mut());
        SetStdHandle(-12i32 as u32, null_mut()); // STD_ERROR_HANDLE
                                                 // Survive the ctrl events we generate while attached to the
                                                 // session's console (deliver_signal).
        SetConsoleCtrlHandler(Some(server_ctrl_handler), 1);
        // Clear any inherited "ignore Ctrl+C" flag — the shell inherits this
        // process state, and with it set it would drop every CTRL_C_EVENT.
        SetConsoleCtrlHandler(None, 0);
    }
    init_log(name);
    let sid = match user_sid_string() {
        Ok(s) => s,
        Err(e) => {
            log(&format!("cannot get user SID: {e}"));
            unsafe { ExitProcess(3) }
        }
    };
    let path = pipe_path(&sid, name);
    let path_w = wide(&path);
    let sd = match user_only_security_descriptor() {
        Ok(p) => p,
        Err(e) => {
            log(&format!("cannot build security descriptor: {e}"));
            unsafe { ExitProcess(3) }
        }
    };

    // Claiming the first pipe instance is what makes session names unique.
    let first = match create_instance(&path_w, sd, true) {
        Some(h) => h,
        None => {
            log(&format!(
                "session '{name}' already exists (err {})",
                unsafe { GetLastError() }
            ));
            unsafe { ExitProcess(2) }
        }
    };

    // The shell inherits this so `fx detach` / `fx kill` inside it know their session.
    std::env::set_var("FLUX_SESSION", name);
    // The client resolves the shell argument (explicit `--` command, then
    // FLUX_SHELL, then the shell that invoked fx); the chain here is the
    // backstop. Windows PowerShell ships with the OS, so the default exists
    // on every machine flux can run on; if it still fails to start, fall
    // back to the system command interpreter (%ComSpec%). A shell the user
    // chose explicitly fails loudly instead of being substituted.
    let chosen = shell_arg
        .map(str::to_string)
        .or_else(|| std::env::var("FLUX_SHELL").ok());
    let explicit = chosen.is_some();
    let shell = chosen.unwrap_or_else(|| {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        format!("{root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe -NoLogo")
    });

    let pty = match conpty::spawn(&shell, cols, rows) {
        Ok(p) => p,
        Err(e) if !explicit => {
            let fallback = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into());
            log(&format!(
                "default shell failed ({shell}): {e}; falling back to {fallback}"
            ));
            match conpty::spawn(&fallback, cols, rows) {
                Ok(p) => p,
                Err(e2) => {
                    log(&format!("fallback shell failed ({fallback}): {e2}"));
                    close_handle(first);
                    unsafe { ExitProcess(3) }
                }
            }
        }
        Err(e) => {
            log(&format!("failed to start shell ({shell}): {e}"));
            close_handle(first);
            unsafe { ExitProcess(3) }
        }
    };
    log(&format!(
        "session '{name}' started: shell pid {}, size {cols}x{rows}",
        pty.pid
    ));

    let shared = Arc::new(Shared {
        inner: Mutex::new(Inner {
            clients: Vec::new(),
            ring: VecDeque::with_capacity(8 * 1024),
            last_size: (cols, rows),
        }),
        next_id: AtomicU64::new(1),
        pty: pty.pty,
        shell_in: pty.input_w,
        shell_proc: pty.process,
        shell_pid: pty.pid,
        name: name.to_string(),
        started: local_time_string(),
    });

    // Pump: shell output -> ring buffer + all attached clients.
    {
        let s = Arc::clone(&shared);
        let out = pty.output_r;
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let mut filter = SeqFilter::new();
            loop {
                let n = match read_some(out.raw(), &mut buf) {
                    Ok(0) => {
                        log("pty output: EOF");
                        break;
                    }
                    Err(e) => {
                        log(&format!("pty output: {e}"));
                        break;
                    }
                    Ok(n) => n,
                };
                let chunk = filter.feed(&buf[..n]);
                if chunk.is_empty() {
                    continue;
                }
                let mut g = s.inner.lock().unwrap();
                if g.ring.len() + chunk.len() > RING_CAP {
                    let excess = (g.ring.len() + chunk.len() - RING_CAP).min(g.ring.len());
                    g.ring.drain(..excess);
                }
                g.ring.extend(chunk.iter().copied());
                g.clients
                    .retain(|(_, h)| write_frame(h.0, S_OUTPUT, &chunk).is_ok());
            }
        });
    }

    // Waiter: when the shell exits, tell everyone and shut down.
    {
        let s = Arc::clone(&shared);
        thread::spawn(move || {
            unsafe { WaitForSingleObject(s.shell_proc.0, INFINITE) };
            let mut code: u32 = 0;
            unsafe { GetExitCodeProcess(s.shell_proc.0, &mut code) };
            log(&format!("shell exited with code {code}"));
            {
                let g = s.inner.lock().unwrap();
                for (_, h) in g.clients.iter() {
                    let _ = write_frame(h.0, S_EXIT, &code.to_le_bytes());
                }
            }
            // Deliberately no ClosePseudoConsole: it can block indefinitely on
            // lingering console clients. ExitProcess reclaims the ConPTY and
            // closes pipe handles gracefully, so buffered frames (including
            // S_EXIT) are still delivered to clients.
            unsafe { ExitProcess(0) };
        });
    }

    // Accept loop.
    let mut instance = first;
    loop {
        if !connect_instance(instance) {
            // Instance is broken; make a fresh one.
            close_handle(instance);
        } else {
            let s = Arc::clone(&shared);
            let h = H(instance);
            thread::spawn(move || handle_conn(s, h));
        }
        instance = match create_instance(&path_w, sd, false) {
            Some(h) => h,
            None => {
                log("failed to create pipe instance; exiting");
                unsafe { TerminateProcess(shared.shell_proc.0, 1) };
                unsafe { ExitProcess(3) }
            }
        };
    }
}

fn handle_conn(s: Arc<Shared>, h: H) {
    let mut attached: Option<u64> = None;
    while let Ok((ty, payload)) = read_frame(h.0) {
        match ty {
            C_STDIN => {
                let _ = write_all(s.shell_in.0, &payload);
            }
            C_RESIZE => {
                if let Some((cols, rows)) = parse_resize(&payload) {
                    let mut g = s.inner.lock().unwrap();
                    if g.last_size != (cols, rows) {
                        g.last_size = (cols, rows);
                        conpty::resize(s.pty, cols, rows);
                    }
                    if attached.is_none() {
                        // First resize marks this connection as an attached
                        // client: replay recent output (minus terminal
                        // queries, which must not be re-asked), then
                        // subscribe.
                        let id = s.next_id.fetch_add(1, Ordering::Relaxed);
                        let (a, b) = g.ring.as_slices();
                        let mut full = Vec::with_capacity(a.len() + b.len());
                        full.extend_from_slice(a);
                        full.extend_from_slice(b);
                        let replay = strip_queries(&full);
                        if !replay.is_empty() {
                            let _ = write_frame(h.0, S_OUTPUT, &replay);
                        }
                        g.clients.push((id, h));
                        attached = Some(id);
                        drop(g);
                        log("client attached");
                    }
                }
            }
            C_DETACH_ALL => {
                let mut g = s.inner.lock().unwrap();
                for (_, ch) in g.clients.drain(..) {
                    let _ = write_frame(ch.0, S_DETACHED, &[]);
                }
                drop(g);
                log("detached all clients");
            }
            C_KILL => {
                log("kill requested");
                unsafe { TerminateProcess(s.shell_proc.0, 1) };
            }
            C_SIGNAL => {
                deliver_signal(s.shell_pid, payload.first().copied().unwrap_or(0));
            }
            C_SWITCH => {
                // `fx <other>` inside this session: hand every attached
                // client over to the named session (no nesting).
                if let Ok(target) = String::from_utf8(payload) {
                    if valid_name(&target) {
                        let g = s.inner.lock().unwrap();
                        for (_, ch) in g.clients.iter() {
                            let _ = write_frame(ch.0, S_SWITCH, target.as_bytes());
                        }
                        drop(g);
                        log(&format!("switch to '{target}' requested"));
                    }
                }
            }
            C_INFO => {
                // Hold the lock while replying: the pump writes S_OUTPUT to
                // this same handle under it, and frames must not interleave.
                let g = s.inner.lock().unwrap();
                let info = format!(
                    "{}\t{}\t{}\t{}",
                    s.name,
                    s.shell_pid,
                    g.clients.len(),
                    s.started
                );
                let _ = write_frame(h.0, S_INFO, info.as_bytes());
                drop(g);
            }
            _ => {}
        }
    }
    if let Some(id) = attached {
        let mut g = s.inner.lock().unwrap();
        g.clients.retain(|(cid, _)| *cid != id);
        drop(g);
        log("client detached");
    }
    // Closed only here, after removal from the broadcast list, so no other
    // thread can write to a stale/reused handle value.
    close_handle(h.0);
}

#[cfg(test)]
mod tests {
    use super::strip_queries;

    #[test]
    fn strips_queries_keeps_normal_output() {
        let input: &[u8] =
            b"hi\x1b[?2027$p\x1b[31mred\x1b[c\x1b[6n\x1b]11;?\x07\x1b]0;title\x07\x1bP+q544e\x1b\\end";
        assert_eq!(
            strip_queries(input),
            b"hi\x1b[31mred\x1b]0;title\x07end".to_vec()
        );
    }

    #[test]
    fn passes_incomplete_sequences_through() {
        assert_eq!(strip_queries(b"abc\x1b[?20"), b"abc\x1b[?20".to_vec());
        assert_eq!(strip_queries(b"abc\x1b"), b"abc\x1b".to_vec());
    }

    #[test]
    fn keeps_non_query_csi_and_osc() {
        let input: &[u8] = b"\x1b[2J\x1b[?25h\x1b[38;5;9mx\x1b]0;a;b\x07";
        assert_eq!(strip_queries(input), input.to_vec());
    }
}
