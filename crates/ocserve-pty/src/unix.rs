//! Unix PTY spawn: openpty + fork + setsid + TIOCSCTTY + execvp.
//!
//! Mirrors the node-pty pattern upstream uses: open a pty pair, fork, in
//! the child `setsid` + `TIOCSCTTY` + `dup2` the slave onto stdio +
//! `chdir` + `execvp`; in the parent keep the master for read/write/resize.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Session;

const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0o4000;
const EIO: i32 = 5;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;

pub struct Proc {
    pub pid: u32,
    master: OwnedFd,
    /// Cleared by the waiter before it fires on_exit.
    alive: Arc<AtomicBool>,
}

fn cstring(s: &str) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().collect();
    v.push(0);
    v
}

pub fn spawn(
    command: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
) -> Result<Proc, String> {
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
        let mut name = [0i8; 128];
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
            // child: new session, controlling tty = slave, stdio = slave.
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

            let c = cstring(cwd);
            libc::chdir(c.as_ptr() as *const i8);

            let env_strings: Vec<Vec<u8>> = env
                .iter()
                .map(|(k, v)| cstring(&format!("{k}={v}")))
                .collect();
            let mut envv: Vec<*const i8> = env_strings
                .iter()
                .map(|s| s.as_ptr() as *const i8)
                .collect();
            envv.push(std::ptr::null());

            let file = cstring(command);
            let arg_strings: Vec<Vec<u8>> = args.iter().map(|a| cstring(a)).collect();
            let mut argv: Vec<*const i8> = Vec::with_capacity(args.len() + 2);
            argv.push(file.as_ptr() as *const i8);
            for a in &arg_strings {
                argv.push(a.as_ptr() as *const i8);
            }
            argv.push(std::ptr::null());

            // The child's env is passed explicitly (execvpe-style via
            // execve); upstream merges process.env + overrides — the caller
            // pre-merges, so use execve with an explicit envp.
            libc::execve(file.as_ptr() as *const i8, argv.as_ptr(), envv.as_ptr());
            libc::_exit(127);
        }

        // parent: non-blocking master for the reader thread.
        let f = libc::fcntl(master, F_SETFL, O_NONBLOCK);
        if f < 0 {
            // non-fatal: blocking reader still works, just slower to stop
        }
        Ok(Proc {
            pid: fd as u32,
            master: OwnedFd::from_raw_fd(master),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }
}

impl Proc {
    pub fn write(&self, data: &str) {
        let bytes = data.as_bytes();
        let fd = self.master.as_raw_fd();
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

    pub fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &ws as *const libc::winsize,
            );
        }
    }

    pub fn kill(&self) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        unsafe {
            // process group (setsid child): negative pid; fall back to pid.
            if libc::kill(-(self.pid as i32), libc::SIGTERM) != 0 {
                libc::kill(self.pid as i32, libc::SIGTERM);
            }
        }
    }

    /// Reader + waiter threads.
    pub fn start(&self, session: Arc<Session>, on_exit: Arc<dyn Fn(Option<i32>) + Send + Sync>) {
        // Reader gets its own dup of the master; the parent keeps the
        // original for write/resize.
        let master = unsafe { libc::dup(self.master.as_raw_fd()) };
        if master < 0 {
            // Without a reader there is no output path; still wait for exit.
        }
        let alive = self.alive.clone();
        let alive_reader = self.alive.clone();
        let pid = self.pid as i32;

        if master >= 0 {
            std::thread::Builder::new()
                .name("pty-reader".into())
                .spawn(move || {
                    // OwnedFd closes the dup on drop.
                    // keep the raw fd and read via libc (OwnedFd has no
                    // Read impl); closed on drop through a guard.
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
                            session.push_output(
                                String::from_utf8_lossy(&buf[..n as usize]).into_owned(),
                            );
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
                    let r = unsafe { libc::waitpid(pid, &mut status, 0) };
                    if r == pid {
                        break exit_code(status);
                    }
                    if r < 0 {
                        // ECHILD (already reaped) or EINTR — stop looping on
                        // ECHILD.
                        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                        if err == libc::ECHILD {
                            break None;
                        }
                    }
                };
                alive.store(false, Ordering::SeqCst);
                // let the reader drain final output before subscribers end
                std::thread::sleep(std::time::Duration::from_millis(20));
                on_exit(code);
            })
            .ok();
    }
}

/// WIFEXITED → code; signal-terminated → None (upstream node-pty reports
/// no exitCode on signals).
fn exit_code(status: i32) -> Option<i32> {
    if status & 0x7f == 0 {
        Some((status >> 8) & 0xff)
    } else {
        None
    }
}
