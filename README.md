# flux

Persistent shell sessions for Windows

## Usage

```
fx                  list your sessions; current session is green
fx <name>           attach to session 'name', creating it if needed
fx <name> -d        start a session without attaching
fx <name> -- <cmd>  run <cmd> in the session instead of the default shell
fx attach [name]    attach only; fail if it doesn't exist           (alias: a)
fx list             list sessions (same as bare fx)                 (alias: ls)
fx detach [name]    detach all attached clients                     (alias: d)
fx kill [name]      end a session (terminates its shell)
fx autostart        start 'main' at every boot (one-time setup, requires elevated shell)

fx --version        print the flux version
fx --help           displays help page
```

- **`Alt+\` detaches** from the current session. Typing `exit` in the shell
  ends the session for real.
- **`Alt+.`** while inside a session cycles to the next session
- **`Alt+,`** while inside a session cycles to the previous session
- **`Alt+/`** while inside a session opens up the new session modal
- On macOS the Option key must act as Alt (kitty: `macos_option_as_alt yes`
  in kitty.conf). flux swallows its own keys, so PSReadLine's `Alt+.`
  yank-last-arg doesn't fire inside sessions.
- Inside a session, `fx detach` and `fx kill` know their own session via the
  `FLUX_SESSION` environment variable, so the name is optional.
- Any first argument that isn't a known command is treated as a session name:
  `fx hello` attaches to (or creates) `hello`.
- Running `fx other` **inside** a session doesn't nest: your attached
  client(s) switch to `other` in place — same window, one attachment,
  creating the session if needed. `fx` alone lists your sessions.
- Multiple clients can attach to one session simultaneously; all see the same
  output, and the terminal size follows the most recent resize.

## Install

Building requires Rust (no Visual Studio needed — the repo pins the GNU
toolchain, which bundles its own linker).

```
cargo build --release
```

## Limitations

- Replay-on-attach restores recent output, not exact screen state: if a
  full-screen TUI app is running, press `Ctrl+L` (or resize the window) after
  attaching to force a repaint.
- Mouse input is not forwarded.