//! macOS PTY spawn: `forkpty()` + `execve`.
//!
//! Darwin's libc does not declare `ptsname_r`, `TIOCSCTTY` or `TIOCSWINSZ`, and
//! the hand-rolled Linux sequence (`posix_openpt` + `grantpt` + `unlockpt` +
//! `ptsname_r` + `fork` + `setsid` + `ioctl(TIOCSCTTY)` + `dup2`) therefore does
//! not compile here. `forkpty(3)` performs exactly that sequence internally
//! (open the pair, `login_tty` in the child: `setsid` + `TIOCSCTTY` +
//! `dup2(0/1/2)`), so this module is a thin wrapper rather than a second
//! implementation.
//!
//! Two consequences worth stating because they are easy to get wrong:
//! - `forkpty` closes the slave side in the parent itself. We must not close it
//!   again (there is no handle to close — the API gives us only the master).
//! - `TIOCSWINSZ`/`TIOCGWINSZ` are ABI constants (`0x80087467` / `0x40087468`
//!   from `<sys/ttycom.h>`), not libc bindings. They are declared here with
//!   their source, and a round-trip test on the macOS runner proves the value
//!   rather than trusting the citation (PLATFORM.md A3).

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::Session;
use crate::posix;

/// Darwin `TIOCSWINSZ` — `<sys/ttycom.h>`, `_IOW('t', 103, struct winsize)`.
const TIOCSWINSZ: u64 = 0x8008_7467;
/// Darwin `TIOCGWINSZ` — `<sys/ttycom.h>`, `_IOR('t', 104, struct winsize)`.
/// Used by the round-trip test below; compiled only there so `-D warnings` stays
/// meaningful for genuinely dead code.
#[cfg(test)]
const TIOCGWINSZ: u64 = 0x4008_7468;

/// Same shape as the Linux `Proc` so `lib.rs` is identical for both.
pub struct Proc {
    pub pid: u32,
    master: OwnedFd,
    alive: Arc<AtomicBool>,
}

pub fn spawn(
    command: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
) -> Result<Proc, String> {
    crate::posix_reject_nul(command, args, cwd, env)?;

    // Pre-build every C string BEFORE fork: only async-signal-safe calls are
    // permitted between fork and execve in a multi-threaded process.
    let cwd_c = posix::cstring(cwd);
    let env_strings: Vec<Vec<u8>> = env
        .iter()
        .map(|(k, v)| posix::cstring(&format!("{k}={v}")))
        .collect();
    let mut envv: Vec<*const libc::c_char> = env_strings
        .iter()
        .map(|s| s.as_ptr() as *const libc::c_char)
        .collect();
    envv.push(std::ptr::null());
    let file = posix::cstring(command);
    let arg_strings: Vec<Vec<u8>> = args.iter().map(|a| posix::cstring(a)).collect();
    let mut argv: Vec<*const libc::c_char> = Vec::with_capacity(args.len() + 2);
    argv.push(file.as_ptr() as *const libc::c_char);
    for a in &arg_strings {
        argv.push(a.as_ptr() as *const libc::c_char);
    }
    argv.push(std::ptr::null());

    unsafe {
        let mut master: libc::c_int = -1;
        // termp/winp NULL: default termios; the window size is set by resize().
        // termp/winp are `*mut` in the signature; `null()` is `*const` and does
        // not coerce. Default termios; the window size is set by resize().
        let pid = libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if pid < 0 {
            return Err(format!(
                "forkpty failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            // Child. `forkpty` has already done setsid + TIOCSCTTY + dup2.
            libc::chdir(cwd_c.as_ptr() as *const libc::c_char);
            libc::execve(
                file.as_ptr() as *const libc::c_char,
                argv.as_ptr(),
                envv.as_ptr(),
            );
            libc::_exit(127);
        }
        // Parent: non-blocking master for the reader thread.
        let _ = libc::fcntl(master, posix::F_SETFL, libc::O_NONBLOCK);
        Ok(Proc {
            pid: pid as u32,
            master: OwnedFd::from_raw_fd(master),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }
}

impl Proc {
    pub fn write(&self, data: &str) {
        posix::write_fd(posix::raw(&self.master), data.as_bytes());
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        posix::set_winsize(posix::raw(&self.master), TIOCSWINSZ, rows, cols);
    }

    pub fn kill(&self) {
        if !self.alive.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        posix::kill_group(self.pid);
    }

    pub fn start(&self, session: Arc<Session>, on_exit: Arc<dyn Fn(Option<i32>) + Send + Sync>) {
        posix::start_threads(
            self.pid,
            posix::raw(&self.master),
            self.alive.clone(),
            session,
            on_exit,
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// The resize constant must be the real one. If `TIOCSWINSZ` were wrong the
    /// `ioctl` would fail and this reads back zeros — so the test fails rather
    /// than silently accepting a plausible-looking number.
    #[test]
    fn winsize_round_trips() {
        let p = match spawn("/bin/sh", &[], "/", &[]) {
            Ok(p) => p,
            Err(e) => panic!("spawn failed: {e}"),
        };
        p.resize(40, 120);
        let got = posix::get_winsize(posix::raw(&p.master), TIOCGWINSZ);
        assert_eq!(
            got,
            Some((40, 120)),
            "TIOCSWINSZ/TIOCGWINSZ constants wrong"
        );
        p.kill();
    }

    /// NUL rejection is shared with Linux but exercised here too: it runs before
    /// `forkpty`, so a bad command never creates a child.
    #[test]
    fn nul_rejected() {
        let e = match spawn("/bin/true", &["a\0b".into()], "/", &[]) {
            Err(e) => e,
            Ok(_) => panic!("NUL in args must be rejected"),
        };
        assert!(e.contains("NUL"), "{e}");
    }
}
