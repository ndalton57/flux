//! flux — persistent shell sessions for Windows (detach/reattach).
//! Binary name: fx.

mod client;
mod conpty;
mod protocol;
mod server;
mod winutil;

use protocol::*;
use winutil::*;

const DEFAULT_SESSION: &str = "main";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);

    let code = match arg(0) {
        None => client::attach(DEFAULT_SESSION, true, None),
        Some("__server") => {
            // Hidden: fx __server <name> <cols> <rows> [shell cmdline]
            let name = arg(1).unwrap_or(DEFAULT_SESSION).to_string();
            let cols: i16 = arg(2).and_then(|s| s.parse().ok()).unwrap_or(120);
            let rows: i16 = arg(3).and_then(|s| s.parse().ok()).unwrap_or(30);
            server::run(&name, cols.max(2), rows.max(2), arg(4));
        }
        Some("__pty") => cmd_pty(&args[1..]),
        Some("attach") | Some("a") => {
            let name = arg(1).unwrap_or(DEFAULT_SESSION);
            check_name(name).unwrap_or_else(|| client::attach(name, false, None))
        }
        Some("ls") | Some("list") => cmd_ls(),
        Some("detach") | Some("d") => cmd_signal(arg(1), C_DETACH_ALL),
        Some("kill") => cmd_signal(arg(1), C_KILL),
        Some("help") | Some("-h") | Some("--help") => {
            print!("{}", HELP);
            0
        }
        Some("-V") | Some("--version") => {
            println!("flux {}", env!("CARGO_PKG_VERSION"));
            0
        }
        // Anything else is a session name: attach to it or create it,
        // exactly like bare `fx` does for 'main'.
        Some(name) => cmd_session(name, &args[1..]),
    };
    std::process::exit(code);
}

fn check_name(name: &str) -> Option<i32> {
    if valid_name(name) {
        None
    } else {
        eprintln!("fx: invalid session name '{name}' (use letters, digits, '-', '_'; max 32)");
        Some(1)
    }
}

/// `fx <name> [-d] [-- <command>]` — attach to the session, creating it if
/// needed (the same attach-or-create that bare `fx` does for 'main').
fn cmd_session(name: &str, rest: &[String]) -> i32 {
    if !valid_name(name) {
        eprintln!("fx: unknown command or invalid session name '{name}'\n");
        print!("{}", HELP);
        return 1;
    }
    let mut detached = false;
    let mut shell: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-d" | "--detached" => detached = true,
            "--" => {
                // Everything after -- is the shell command line for this session.
                let cmd: Vec<&str> = it.by_ref().map(String::as_str).collect();
                if cmd.is_empty() {
                    eprintln!("fx: expected a command after --");
                    return 1;
                }
                shell = Some(cmd.join(" "));
            }
            s => {
                eprintln!("fx: unexpected argument '{s}'");
                return 1;
            }
        }
    }
    if !detached {
        return client::attach(name, true, shell.as_deref());
    }
    // -d: make sure it exists, but don't attach.
    if session_exists(name) {
        println!("[flux] session '{name}' is already running — attach with: fx {name}");
        return 0;
    }
    if let Err(e) = client::spawn_server(name, shell.as_deref()) {
        eprintln!("fx: failed to start session: {e}");
        return 1;
    }
    // Wait until the pipe is up so callers can rely on it.
    let sid = match user_sid_string() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fx: {e}");
            return 1;
        }
    };
    let path = pipe_path(&sid, name);
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(h) = open_pipe(&path) {
            close_handle(h);
            println!("[flux] session '{name}' started (detached) — attach with: fx {name}");
            return 0;
        }
    }
    eprintln!("fx: server did not start (see %LOCALAPPDATA%\\flux\\ logs)");
    1
}

/// Hidden test harness: run a command under a fresh ConPTY, bridging our
/// stdin -> its input and its output -> our stdout. Lets the interactive
/// client be driven byte-by-byte from a script.
fn cmd_pty(rest: &[String]) -> i32 {
    let cols: i16 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(100);
    let rows: i16 = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let cmd: Vec<String> = rest[2.min(rest.len())..]
        .iter()
        .map(|t| {
            if t.contains(' ') {
                format!("\"{t}\"")
            } else {
                t.clone()
            }
        })
        .collect();
    if cmd.is_empty() {
        eprintln!("fx: __pty needs a command");
        return 1;
    }

    // Grab our pipe handles, then NULL the std handles so the ConPTY child
    // binds to its console instead of inheriting our pipe handle values.
    let stdin = std_handle(winutil::STD_INPUT_HANDLE);
    let stdout = std_handle(winutil::STD_OUTPUT_HANDLE);
    unsafe {
        use windows_sys::Win32::System::Console::SetStdHandle;
        SetStdHandle(winutil::STD_INPUT_HANDLE, std::ptr::null_mut());
        SetStdHandle(winutil::STD_OUTPUT_HANDLE, std::ptr::null_mut());
        SetStdHandle(-12i32 as u32, std::ptr::null_mut()); // STD_ERROR_HANDLE
    }

    let pty = match conpty::spawn(&cmd.join(" "), cols.max(2), rows.max(2)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fx: __pty spawn failed: {e}");
            return 1;
        }
    };

    let h_in = winutil::H(stdin);
    let shell_in = pty.input_w;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match read_some(h_in.raw(), &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write_all(shell_in.raw(), &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let h_out = winutil::H(stdout);
    let pty_out = pty.output_r;
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match read_some(pty_out.raw(), &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if write_all(h_out.raw(), &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    unsafe {
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        WaitForSingleObject(pty.process.0, 0xFFFF_FFFF);
    }
    // Give the output pump a moment to flush the child's last bytes.
    std::thread::sleep(std::time::Duration::from_millis(400));
    0
}

fn session_exists(name: &str) -> bool {
    if let Ok(sid) = user_sid_string() {
        if let Ok(h) = open_pipe(&pipe_path(&sid, name)) {
            close_handle(h);
            return true;
        }
    }
    false
}

fn list_session_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(sid) = user_sid_string() {
        let prefix = pipe_prefix(&sid);
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
    }
    names.sort();
    names
}

fn query_info(name: &str) -> Option<(String, String, String)> {
    let sid = user_sid_string().ok()?;
    let h = open_pipe(&pipe_path(&sid, name)).ok()?;
    let r = (|| {
        write_frame(h, C_INFO, &[]).ok()?;
        let (ty, p) = read_frame(h).ok()?;
        if ty != S_INFO {
            return None;
        }
        let s = String::from_utf8_lossy(&p).into_owned();
        let mut parts = s.split('\t');
        let _name = parts.next()?;
        let pid = parts.next()?.to_string();
        let clients = parts.next()?.to_string();
        let started = parts.next()?.to_string();
        Some((pid, clients, started))
    })();
    close_handle(h);
    r
}

fn cmd_ls() -> i32 {
    let names = list_session_names();
    if names.is_empty() {
        println!("no sessions");
        return 0;
    }
    println!("{:<20} {:>8} {:>8}  STARTED", "NAME", "PID", "CLIENTS");
    for n in names {
        match query_info(&n) {
            Some((pid, clients, started)) => {
                println!("{:<20} {:>8} {:>8}  {}", n, pid, clients, started)
            }
            None => println!("{:<20} {:>8} {:>8}", n, "-", "-"),
        }
    }
    0
}

/// Send a one-shot control frame (detach-all / kill) to a session.
fn cmd_signal(name: Option<&str>, ty: u8) -> i32 {
    let env_name = std::env::var("FLUX_SESSION").ok();
    let name = match name.or(env_name.as_deref()) {
        Some(n) => n.to_string(),
        None => {
            eprintln!("fx: not inside a flux session — specify a name (see `fx ls`)");
            return 1;
        }
    };
    if let Some(c) = check_name(&name) {
        return c;
    }
    let sid = match user_sid_string() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fx: {e}");
            return 1;
        }
    };
    let h = match open_pipe(&pipe_path(&sid, &name)) {
        Ok(h) => h,
        Err(_) => {
            eprintln!("fx: no session '{name}'");
            return 1;
        }
    };
    let ok = write_frame(h, ty, &[]).is_ok();
    if ok && ty == C_KILL {
        // Wait for the server to actually go down (pipe EOF).
        let mut buf = [0u8; 64];
        while matches!(pipe_read_some(h, &mut buf), Ok(n) if n > 0) {}
    }
    close_handle(h);
    if !ok {
        eprintln!("fx: could not signal session '{name}'");
        return 1;
    }
    match ty {
        C_KILL => println!("[flux] killed session '{name}'"),
        _ => println!("[flux] detached all clients from '{name}'"),
    }
    0
}

const HELP: &str = "\
flux — persistent shell sessions for Windows

usage:
  fx [name]           attach to session 'name', creating it if needed
                      (default session: 'main')
  fx <name> -d        start a session without attaching
  fx <name> -- <cmd>  run <cmd> in the session instead of the default shell
  fx attach [name]    attach only; fail if it doesn't exist   (alias: a)
  fx ls               list your sessions                      (alias: list)
  fx detach [name]    detach all attached clients             (alias: d)
  fx kill [name]      end a session (terminates its shell)
  fx --version        print the flux version

inside a session, `fx <name>` switches the attached client(s) over to that
session in place (creating it if needed) — no nesting. For detach/kill,
'name' defaults to the current session (FLUX_SESSION).

keys:
  Ctrl+\\              detach from the current session
                      (Ctrl+Shift+\\ also works on full-fidelity input)

environment:
  FLUX_SHELL          default shell command line for new sessions
                      (otherwise sessions run the shell fx was launched
                      from — cmd, powershell, pwsh, bash, nu — falling
                      back to powershell.exe)
";
