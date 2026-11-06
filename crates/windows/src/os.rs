//! Small Windows helpers for the `sard` app crate: console handling, log
//! redirection, local addresses.

#![allow(non_snake_case, non_camel_case_types, clippy::upper_case_acronyms)]

use crate::sys;
use std::ffi::c_void;
use std::net::Ipv4Addr;

/// True when this process is the only one attached to its console: it was
/// started from Explorer, the Start menu or the Run key (Windows opened a
/// console window just for it), not typed into a terminal.
pub fn console_is_ours() -> bool {
    let mut ids = [0u32; 4];
    // SAFETY: valid buffer of the stated length.
    let n = unsafe { sys::GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
    n <= 1
}

/// Close the console window Windows opened for us (tray app started from the desktop).
pub fn detach_console() {
    // SAFETY: plain calls; harmless without a console. Clearing the standard
    // handles makes later prints no-ops instead of writes to a dead console.
    unsafe {
        sys::FreeConsole();
        sys::SetStdHandle(sys::STD_OUTPUT_HANDLE, std::ptr::null_mut());
        sys::SetStdHandle(sys::STD_ERROR_HANDLE, std::ptr::null_mut());
    }
}

/// Whether this process has a console window (to decide how children are spawned).
pub fn has_console() -> bool {
    // SAFETY: plain query.
    !unsafe { sys::GetConsoleWindow() }.is_null()
}

/// Whether stderr is an interactive console (not a file, pipe or nothing).
pub fn stderr_is_console() -> bool {
    let mut mode = 0u32;
    // SAFETY: plain queries with a valid out pointer.
    unsafe {
        let h = sys::GetStdHandle(sys::STD_ERROR_HANDLE);
        !h.is_null() && h as isize != -1 && sys::GetConsoleMode(h, &mut mode) != 0
    }
}

/// Send stderr (and with it sard's log) to `file` for the rest of the process.
pub fn redirect_stderr(file: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;
    let h = file.into_raw_handle();
    // SAFETY: we hand over (leak) an owned, valid file handle.
    unsafe { sys::SetStdHandle(sys::STD_ERROR_HANDLE, h) };
}

/// `CREATE_NO_WINDOW`: run a console child (curl, tar, a new sard) without
/// flashing a console window when the parent has none.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `DETACHED_PROCESS`: start a console child with no console at all.
pub const DETACHED_PROCESS: u32 = 0x0000_0008;

#[repr(C)]
struct SOCKET_ADDRESS {
    lpSockaddr: *const SOCKADDR_IN,
    iSockaddrLength: i32,
}

#[repr(C)]
struct SOCKADDR_IN {
    sin_family: u16,
    sin_port: u16,
    sin_addr: [u8; 4],
    sin_zero: [u8; 8],
}

/// Leading fields of `IP_ADAPTER_UNICAST_ADDRESS_LH`.
#[repr(C)]
struct IP_ADAPTER_UNICAST_ADDRESS_LH {
    Length: u32,
    Flags: u32,
    Next: *const IP_ADAPTER_UNICAST_ADDRESS_LH,
    Address: SOCKET_ADDRESS,
}

/// Leading fields of `IP_ADAPTER_ADDRESSES_LH`.
#[repr(C)]
struct IP_ADAPTER_ADDRESSES_LH {
    Length: u32,
    IfIndex: u32,
    Next: *const IP_ADAPTER_ADDRESSES_LH,
    AdapterName: *const u8,
    FirstUnicastAddress: *const IP_ADAPTER_UNICAST_ADDRESS_LH,
    FirstAnycastAddress: *const c_void,
    FirstMulticastAddress: *const c_void,
    FirstDnsServerAddress: *const c_void,
    DnsSuffix: *const u16,
    Description: *const u16,
    FriendlyName: *const u16,
    PhysicalAddress: [u8; 8],
    PhysicalAddressLength: u32,
    Flags: u32,
    Mtu: u32,
    IfType: u32,
    OperStatus: u32,
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(offset_of!(IP_ADAPTER_ADDRESSES_LH, Next) == 8);
    assert!(offset_of!(IP_ADAPTER_ADDRESSES_LH, FirstUnicastAddress) == 24);
    assert!(offset_of!(IP_ADAPTER_ADDRESSES_LH, IfType) == 100);
    assert!(offset_of!(IP_ADAPTER_ADDRESSES_LH, OperStatus) == 104);
    assert!(offset_of!(IP_ADAPTER_UNICAST_ADDRESS_LH, Next) == 8);
    assert!(offset_of!(IP_ADAPTER_UNICAST_ADDRESS_LH, Address) == 16);
    assert!(size_of::<SOCKADDR_IN>() == 16);
};

const AF_INET: u32 = 2;
const GAA_FLAG_SKIP_ANYCAST: u32 = 0x2;
const GAA_FLAG_SKIP_MULTICAST: u32 = 0x4;
const GAA_FLAG_SKIP_DNS_SERVER: u32 = 0x8;
const ERROR_BUFFER_OVERFLOW: u32 = 111;
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_OPER_STATUS_UP: u32 = 1;

#[link(name = "iphlpapi")]
unsafe extern "system" {
    fn GetAdaptersAddresses(family: u32, flags: u32, reserved: *mut c_void, buf: *mut c_void, size: *mut u32) -> u32;
}

/// IPv4 addresses of the network adapters that are up (no loopback).
pub fn local_ipv4() -> Vec<Ipv4Addr> {
    let mut size = 16u32 << 10;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..4 {
        buf = vec![0u64; (size as usize).div_ceil(8)];
        // SAFETY: buffer of `size` bytes, 8-byte aligned.
        let r = unsafe {
            GetAdaptersAddresses(
                AF_INET,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        match r {
            0 => break,
            ERROR_BUFFER_OVERFLOW => continue,
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut a = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    // SAFETY: walking the linked lists GetAdaptersAddresses wrote into `buf`.
    unsafe {
        while !a.is_null() {
            if (*a).OperStatus == IF_OPER_STATUS_UP && (*a).IfType != IF_TYPE_SOFTWARE_LOOPBACK {
                let mut u = (*a).FirstUnicastAddress;
                while !u.is_null() {
                    let sa = (*u).Address.lpSockaddr;
                    if !sa.is_null() && (*sa).sin_family as u32 == AF_INET {
                        out.push(Ipv4Addr::from((*sa).sin_addr));
                    }
                    u = (*u).Next;
                }
            }
            a = (*a).Next;
        }
    }
    out
}
