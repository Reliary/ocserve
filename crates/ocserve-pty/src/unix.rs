//! Linux PTY spawn: `posix_openpt` + `fork` + `setsid` + `TIOCSCTTY` + `execve`.
//!
//! Mirrors the node-pty pattern upstream uses. Everything after the fork is in
//! `posix.rs` and shared with macOS, so this file contains only the part that is
//! genuinely Linux-specific: how the pty pair is opened and how the child is
//! put into its own session with the slave as its controlling terminal.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::Session;
use crate::posix;

pub struct Proc {
    pub pid: u32,
    master: OwnedFd,
    /// Cleared by the waiter before it fires on_exit.
    alive: Arc<AtomicBool>,
}

pub fn spawn(
    command: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
) -> Result<Proc, String> {
    crate::posix_reject_nul(command, args, cwd, env)?;

    // Pre-build every C string BEFORE fork. Only async-signal-safe calls are
    // permitted between fork and execve in a multi-threaded process; `format!`
    // and `Vec` growth there can deadlock on allocator contention.
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
        let flags = libc::O_RDWR | libc::O_NOCTTY;
        let master = libc::posix_openpt(flags);
        if master < 0 {
            return Err(format!(
                "posix_openpt failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            libc::close(master);
            return Err(format!(
                "grantpt/unlockpt failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut name = [0 as libc::c_char; 128];
        if libc::ptsname_r(master, name.as_mut_ptr(), name.len()) != 0 {
            libc::close(master);
            return Err("ptsname_r failed".into());
        }

        let fd = libc::fork();
        if fd < 0 {
            libc::close(master);
            return Err(format!("fork failed: {}", std::io::Error::last_os_error()));
        }
        if fd == 0 {
            // Child: new session, controlling tty = slave, stdio = slave.
            let slave = libc::open(name.as_ptr(), flags);
            if slave < 0 {
                libc::_exit(127);
            }
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);

            libc::chdir(cwd_c.as_ptr() as *const libc::c_char);

            // The child's env is passed explicitly (execve); the caller
            // pre-merges process env + overrides.
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
            pid: fd as u32,
            master: OwnedFd::from_raw_fd(master),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }
}

impl Proc {
    pub fn write(&self, data: &str) {
        posix::write_fd(self.master.as_raw_fd(), data.as_bytes());
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        posix::set_winsize(self.master.as_raw_fd(), libc::TIOCSWINSZ, rows, cols);
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
            self.master.as_raw_fd(),
            self.alive.clone(),
            session,
            on_exit,
        );
    }
}

#[cfg(test)]
mod nul_tests {
    use super::spawn;

    #[test]
    fn nul_bytes_rejected_in_command_args_env_and_cwd() {
        let ok = spawn("/bin/true", &[], "/", &[]);
        assert!(ok.is_ok(), "clean spawn works");
        let e = match spawn("/bin/true", &["a\0b".into()], "/", &[]) {
            Err(e) => e,
            Ok(_) => panic!("NUL in args must be rejected"),
        };
        assert!(e.contains("NUL"), "{e}");
        let e = match spawn("/bin/true", &[], "/", &[("K".into(), "v\0X".into())]) {
            Err(e) => e,
            Ok(_) => panic!("NUL in env must be rejected"),
        };
        assert!(e.contains("NUL"), "{e}");
        let e = match spawn("/bin/true", &[], "/a\0b", &[]) {
            Err(e) => e,
            Ok(_) => panic!("NUL in cwd must be rejected"),
        };
        assert!(e.contains("NUL"), "{e}");
    }
}
