//! Framing for the client<->server named pipe: 1 byte type + u32 LE length + payload.

// client -> server
pub const C_STDIN: u8 = 1; // payload: raw input bytes for the shell
pub const C_RESIZE: u8 = 2; // payload: cols i16 LE, rows i16 LE; first one marks the conn as attached
pub const C_DETACH_ALL: u8 = 3; // detach every attached client
pub const C_KILL: u8 = 4; // terminate the session's shell
pub const C_INFO: u8 = 5; // request session info
pub const C_SIGNAL: u8 = 6; // payload: [0] = 0 Ctrl+C, 1 Ctrl+Break
pub const C_SWITCH: u8 = 7; // payload: target session name; switch attached clients there

// server -> client
pub const S_OUTPUT: u8 = 1; // payload: VT output bytes from the shell
pub const S_EXIT: u8 = 2; // shell exited; server is going away
pub const S_DETACHED: u8 = 3; // server-initiated detach
pub const S_INFO: u8 = 4; // payload: name \t shell_pid \t clients \t started
pub const S_SWITCH: u8 = 5; // payload: session name the client should reattach to

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

pub fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(ty);
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn resize_payload(cols: i16, rows: i16) -> [u8; 4] {
    let c = cols.to_le_bytes();
    let r = rows.to_le_bytes();
    [c[0], c[1], r[0], r[1]]
}

pub fn parse_resize(p: &[u8]) -> Option<(i16, i16)> {
    if p.len() < 4 {
        return None;
    }
    let cols = i16::from_le_bytes([p[0], p[1]]);
    let rows = i16::from_le_bytes([p[2], p[3]]);
    if cols > 0 && rows > 0 {
        Some((cols, rows))
    } else {
        None
    }
}
