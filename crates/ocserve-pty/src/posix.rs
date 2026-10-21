//! POSIX-shared PTY plumbing (Linux + macOS).
//!
//! The two platforms differ only in how the pty pair is opened:
//! - Linux: `posix_openpt` + `grantpt` + `unlockpt` + `ptsname_r` + `fork` +
//!   `setsid` + `ioctl(TIOCSCTTY)` + `dup2`.
//! - macOS: one `forkpty()` call, which does all of the above internally.
//!
//! Everything after the fork — the reader thread, the waiter thread, write,
//! kill, and the exit-code mapping — is identical, so it lives here. Keeping one
//! copy is what makes the macOS port small enough to review, and it means the
//! `waitpid`/`read` behaviour cannot drift between the two.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Session;

// These are used by `unix.rs`, `macos.rs`, or both. They stay public and
// compiled on every platform so the shared surface cannot quietly diverge.
pub const F_SETFL: i32 = 4;
// These MUST come from libc. They were hardcoded to the Linux values, and
// `EAGAIN` is 35 on Darwin rather than 11: the reader therefore treated the
// normal "no data yet" case as an unknown error and exited on the first poll,
// so every macOS terminal produced an empty buffer (caught by the runner).
pub const EIO: i32 = libc::EIO;
pub const EAGAIN: i32 = libc::EAGAIN;
pub const EBADF: i32 = libc::EBADF;

/// Pre-open a class leaf's `cgroup.procs` in the parent so the child can move
/// itself with a single async-signal-safe `write` — no allocation and no path
/// lookup between `fork` and `execve` (the rule this file already follows).
/// Linux-only: cgroup v2 has no macOS equivalent, and `leaf()` is None there.
#[cfg(target_os = "linux")]
pub fn open_leaf_procs() -> Option<std::os::fd::OwnedFd> {
    let path = ocserve_store::cgroup::leaf("pty")?.join("cgroup.procs");
    let f = std::fs::OpenOptions::new().write(true).open(path).ok()?;
    Some(f.into())
}

#[cfg(not(target_os = "linux"))]
pub fn open_leaf_procs() -> Option<std::os::fd::OwnedFd> {
    None
}

/// Child half of PTY containment. `procs` is the pre-opened fd from
/// [`open_leaf_procs`]. Linux-only; must stay async-signal-safe (raw `open`
/// and `write`, a stack-formatted pid, no allocation).
#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // async-signal-safe child path: raw getpid/write/open/close
pub fn contain_child(procs: Option<&std::os::fd::OwnedFd>) {
    use std::os::fd::AsRawFd;
    // Format the pid by hand: `format!` allocates, and allocation between
    // fork and execve in a multi-threaded process can deadlock.
    let mut buf = [0u8; 16];
    let mut n = 0usize;
    let mut v = unsafe { libc::getpid() } as u64;
    let mut digits = [0u8; 20];
    let mut d = digits.len();
    loop {
        d -= 1;
        digits[d] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    buf[n..n + (digits.len() - d)].copy_from_slice(&digits[d..]);
    n += digits.len() - d;
    buf[n] = b'\n';
    n += 1;
    if let Some(file) = procs {
        unsafe {
            libc::write(file.as_raw_fd(), buf.as_ptr().cast(), n);
            libc::close(file.as_raw_fd());
        }
    }
    // Raise our own OOM badness: a memcg kill takes the terminal, not the
    // server. `/proc` is Linux-only, which is why this lives here.
    unsafe {
        let adj = c"/proc/self/oom_score_adj";
        let fd = libc::open(adj.as_ptr(), libc::O_WRONLY);
        if fd >= 0 {
            libc::write(fd, c"500".as_ptr().cast(), 3);
            libc::close(fd);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn contain_child(_procs: Option<&std::os::fd::OwnedFd>) {}

/// NUL-terminated bytes for `execve`'s argv/envp.
pub fn cstring(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().collect();
    v.push(0);
    v
}

/// `WIFEXITED` → code; signal-terminated → `None` (upstream node-pty reports no
/// exitCode on signals).
pub fn exit_code(status: i32) -> Option<i32> {
    if status & 0x7f == 0 {
        Some((status >> 8) & 0xff)
    } else {
        None
    }
}

/// Write the whole buffer to `fd`, retrying short writes.
pub fn write_fd(fd: i32, bytes: &[u8]) {
    let mut off = 0usize;
    while off < bytes.len() {
        let n = unsafe {
            libc::write(
                fd,
                bytes[off..].as_ptr() as *const libc::c_void,
                bytes.len() - off,
            )
        };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
}

/// SIGTERM the child's process group (the child is a session leader on both
/// platforms, so `-pid` addresses the tree), falling back to the bare pid.
pub fn kill_group(pid: u32) {
    unsafe {
        if libc::kill(-(pid as i32), libc::SIGTERM) != 0 {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
}

/// Spawn the reader and waiter threads for an already-forked child.
///
/// The reader gets its own `dup` of the master; the caller keeps the original
/// for write/resize.
pub fn start_threads(
    pid: u32,
    master_fd: i32,
    alive: Arc<AtomicBool>,
    session: Arc<Session>,
    on_exit: Arc<dyn Fn(Option<i32>) + Send + Sync>,
) {
    let master = unsafe { libc::dup(master_fd) };
    let alive_reader = alive.clone();
    let pid_i = pid as i32;

    if master >= 0 {
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                struct FdGuard(i32);
                impl Drop for FdGuard {
                    fn drop(&mut self) {
                        unsafe { libc::close(self.0) };
                    }
                }
                let _guard = FdGuard(master);
                let mut buf = [0u8; 65536];
                loop {
                    let n = unsafe {
                        libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                    };
                    if n > 0 {
                        session
                            .push_output(String::from_utf8_lossy(&buf[..n as usize]).into_owned());
                        continue;
                    }
                    let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    match err {
                        0 if n == 0 => break, // EOF
                        EIO | EBADF => break,
                        EAGAIN => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                            if !alive_reader.load(Ordering::SeqCst) {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            })
            .ok();
    }

    std::thread::Builder::new()
        .name("pty-waiter".into())
        .spawn(move || {
            let mut status: i32 = 0;
            let code = loop {
                let r = unsafe { libc::waitpid(pid_i, &mut status, 0) };
                if r == pid_i {
                    break exit_code(status);
                }
                if r < 0 {
                    let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    if err == libc::ECHILD {
                        break None;
                    }
                }
            };
            alive.store(false, Ordering::SeqCst);
            // Let the reader drain final output before subscribers end.
            std::thread::sleep(std::time::Duration::from_millis(20));
            on_exit(code);
        })
        .ok();
}

/// Resize a pty via `TIOCSWINSZ`. The request constant is passed in because
/// libc does not declare it for Darwin (see `macos.rs`).
pub fn set_winsize(fd: i32, request: u64, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, request as _, &ws as *const libc::winsize);
    }
}

/// Read the current window size via `TIOCGWINSZ` (used by tests to prove the
/// request constant is the real one, not a plausible-looking guess).
///
/// Unused on Linux: the constant is a libc binding there, so the round-trip
/// proof is only needed on macOS. Kept compiled on both so a future caller on
/// either platform gets one implementation.
#[allow(dead_code)]
pub fn get_winsize(fd: i32, request: u64) -> Option<(u16, u16)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, request as _, &mut ws as *mut libc::winsize) };
    if rc != 0 {
        return None;
    }
    Some((ws.ws_row, ws.ws_col))
}

/// Convenience for callers that just want the raw fd of an `OwnedFd`.
#[allow(dead_code)] // Linux call sites use `AsRawFd` directly
pub fn raw(fd: &std::os::fd::OwnedFd) -> i32 {
    fd.as_raw_fd()
}
