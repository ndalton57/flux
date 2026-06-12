# flux

Persistent shell sessions for Windows

## Usage

```
fx [name]           attach to session 'name', creating it if needed
                    (default session: 'main')
fx <name> -d        start a session without attaching
fx <name> -- <cmd>  run <cmd> in the session instead of the default shell
fx attach [name]    attach only; fail if it doesn't exist   (alias: a)
fx ls               list your sessions                      (alias: list)
fx detach [name]    detach all attached clients             (alias: d)
fx kill [name]      end a session (terminates its shell)
fx --version        print the flux version
```

- **`Ctrl+\` detaches** from the current session (`Ctrl+Shift+\` works too).
  Typing `exit` in the shell ends the session for real. Plain VT transports
  like SSH can't carry the Shift distinction for that key, which is why both
  count.
- Inside a session, `fx detach` and `fx kill` know their own session via the
  `FLUX_SESSION` environment variable, so the name is optional.
- Any first argument that isn't a known command is treated as a session name:
  `fx hello` attaches to (or creates) `hello`.
- Running `fx other` **inside** a session doesn't nest: your attached
  client(s) switch to `other` in place — same window, one attachment,
  creating the session if needed. `fx` alone hops back to `main`.
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