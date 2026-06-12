//! Spawn a shell attached to a Windows pseudo console (ConPTY).

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW,
};

use crate::winutil::{close_handle, last_err, wide, H};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;

/// Send+Sync wrapper for the pseudo console handle.
#[derive(Clone, Copy)]
pub struct Pty(pub HPCON);
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

pub struct ConPty {
    pub pty: Pty,
    /// We write keystrokes here; ConPTY feeds them to the shell.
    pub input_w: H,
    /// We read the shell's rendered VT output here.
    pub output_r: H,
    pub process: H,
    pub pid: u32,
}

pub fn spawn(cmdline: &str, cols: i16, rows: i16) -> io::Result<ConPty> {
    unsafe {
        let mut in_r: HANDLE = null_mut();
        let mut in_w: HANDLE = null_mut();
        let mut out_r: HANDLE = null_mut();
        let mut out_w: HANDLE = null_mut();
        if CreatePipe(&mut in_r, &mut in_w, null(), 0) == 0 {
            return Err(last_err());
        }
        if CreatePipe(&mut out_r, &mut out_w, null(), 0) == 0 {
            return Err(last_err());
        }

        let size = COORD { X: cols, Y: rows };
        let mut hpc: HPCON = 0; // HPCON is isize in windows-sys
        let hr = CreatePseudoConsole(size, in_r, out_w, 0, &mut hpc);
        if hr < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole failed: HRESULT 0x{:08x}",
                hr as u32
            )));
        }
        // ConPTY duplicated these; close our copies of the child-side ends.
        close_handle(in_r);
        close_handle(out_w);

        // Attribute list binding the child to the pseudo console.
        let mut attr_size: usize = 0;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size.max(1)];
        let attr = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attr, 1, 0, &mut attr_size) == 0 {
            return Err(last_err());
        }
        if UpdateProcThreadAttribute(
            attr,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
            hpc as *const std::ffi::c_void, // the HPCON itself is the attribute value
            size_of::<HPCON>(),
            null_mut(),
            null(),
        ) == 0
        {
            return Err(last_err());
        }

        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attr;

        let mut cmd = wide(cmdline);
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            null(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            null(),
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(attr);
        if ok == 0 {
            let e = last_err();
            ClosePseudoConsole(hpc);
            return Err(e);
        }
        close_handle(pi.hThread);

        Ok(ConPty {
            pty: Pty(hpc),
            input_w: H(in_w),
            output_r: H(out_r),
            process: H(pi.hProcess),
            pid: pi.dwProcessId,
        })
    }
}

pub fn resize(pty: Pty, cols: i16, rows: i16) {
    unsafe {
        ResizePseudoConsole(pty.0, COORD { X: cols, Y: rows });
    }
}
