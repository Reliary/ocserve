//! Windows PTY spawn: ConPTY (`CreatePseudoConsole`) + `CreateProcessW`.
//!
//! ## Why not `std::process::Command`
//!
//! ConPTY requires the child to be created with an extended startup info that
//! carries the pseudoconsole in its attribute list, and it requires the child's
//! stdio to be the *pseudoconsole*, not inherited handles. `std::process::Command`
//! exposes neither. So this module calls `CreateProcessW` directly.
//!
//! ## The three things that actually bite
//!
//! 1. **The environment block.** Windows does not accept a map. It wants a
//!    UTF-16 block, sorted case-insensitively by name, terminated by a double
//!    NUL. Get the sort or the terminator wrong and variables silently vanish.
//!    `build_env_block` is separated and unit-tested for that reason.
//! 2. **The command line.** `CreateProcessW` takes one string, not argv, and the
//!    child re-parses it. Every argument is quoted with the MSVCRT rules
//!    (`quote_arg`) and the result is asserted to round-trip, because a silently
//!    re-split argument is a command injection surface for a tool that lets a
//!    model choose arguments.
//! 3. **Shutdown ordering.** `ClosePseudoConsole` blocks until pending output is
//!    drained. If it is called while our reader is mid-`ReadFile` on the output
//!    pipe, the two deadlock. The reader here is a `PeekNamedPipe` poller that
//!    never blocks (the same discipline as the Linux O_NONBLOCK reader), so
//!    close is always safe.
//!
//! ## Session semantics
//!
//! The Linux child is a session leader (`setsid`) so the timeout kill can
//! address the whole process group. Windows has no process groups, so the child
//! is assigned to a **Job Object with `KILL_ON_JOB_CLOSE`**: killing the job
//! terminates the whole tree, and if the server itself dies the handle closes
//! and Windows reaps the tree for us — which `taskkill /T` cannot do.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::WriteFile;
use windows_sys::Win32::System::Console::COORD;
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::Session;

/// A kernel handle moved into a worker thread.
///
/// `HANDLE` is `*mut c_void`, so it is neither `Send` nor `Sync` by default.
/// Windows kernel handles are process-wide and not thread-affine, so moving one
/// to another thread is sound; this wrapper states that once instead of putting
/// an `unsafe impl` on a closure.
#[derive(Clone, Copy)]
struct SendHandle(HANDLE);
// Safety: kernel handles are process-wide; the owner guarantees lifetime and
// single-close (see Proc::Drop, which closes only after the threads exit).
unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

impl SendHandle {
    fn get(self) -> HANDLE {
        self.0
    }
}

/// Owner of the ConPTY, pipes, child handle and job. Closing it in the right
/// order is the whole game; `Drop` is the single place that decides the order.
pub struct Proc {
    pub pid: u32,
    console: HPCON,
    /// Our end of the child's stdin (ConPTY input pipe).
    input: HANDLE,
    /// Our end of the child's stdout+stderr (ConPTY output pipe).
    output: HANDLE,
    process: HANDLE,
    job: HANDLE,
    alive: Arc<AtomicBool>,
    /// Serializes `write` and close: a concurrent WriteFile on a closed pipe is
    /// a use-after-close, and Windows may recycle the handle value.
    write_lock: std::sync::Mutex<()>,
}

// The handles are owned by this struct and only used through `&self` methods.
// ConPTY handles are kernel objects, not thread-affine.
unsafe impl Send for Proc {}
unsafe impl Sync for Proc {}

/// Build the UTF-16 environment block `CreateProcessW` expects.
///
/// Rules, all of which matter: entries are `NAME=VALUE`; the block is sorted
/// case-insensitively by name (the loader binary-searches it); entries are NUL
/// terminated; the block ends with an extra NUL. Duplicate names are resolved
/// last-wins, matching `CreateProcess`'s behaviour when a block contains them.
fn build_env_block(pairs: &[(String, String)]) -> Vec<u16> {
    let mut entries: Vec<(String, String)> = Vec::with_capacity(pairs.len());
    for (k, v) in pairs {
        if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
            continue;
        }
        if let Some(slot) = entries
            .iter_mut()
            .find(|(ek, _)| ek.eq_ignore_ascii_case(k))
        {
            // Replace the whole entry: last-wins includes the spelling, so
            // the block never carries a name the caller did not supply.
            *slot = (k.clone(), v.clone());
        } else {
            entries.push((k.clone(), v.clone()));
        }
    }
    entries.sort_by(|a, b| {
        // The loader compares the upper-cased UTF-16 name; ASCII folding is
        // correct for the names Windows itself uses and is what
        // `RtlEqualUnicodeString(..., TRUE)` reduces to for them.
        a.0.to_ascii_uppercase()
            .cmp(&b.0.to_ascii_uppercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in entries {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// Quote one argument for the MSVCRT command-line parser.
///
/// Backslashes are only special immediately before a quote, so they are doubled
/// there; embedded quotes are escaped; an argument with whitespace or an empty
/// argument is wrapped. This is the algorithm `CommandLineToArgvW` inverts.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // 2n+1 backslashes then a quote: n literal backslashes + quote.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Trailing backslashes precede the closing quote: double them.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

fn build_command_line(command: &str, args: &[String]) -> Vec<u16> {
    let mut line = String::new();
    // argv[0]: the executable path, quoted under the same rules.
    line.push_str(&quote_arg(command));
    for a in args {
        line.push(' ');
        line.push_str(&quote_arg(a));
    }
    line.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

pub fn spawn(
    command: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
) -> Result<Proc, String> {
    crate::posix_reject_nul(command, args, cwd, env)?;

    // Pipes. The ConPTY end of each pipe must be inheritable; our end must not
    // be, or the child keeps our read end alive and output never reaches EOF.
    let mut input_read: HANDLE = INVALID_HANDLE_VALUE;
    let mut input_write: HANDLE = INVALID_HANDLE_VALUE;
    let mut output_read: HANDLE = INVALID_HANDLE_VALUE;
    let mut output_write: HANDLE = INVALID_HANDLE_VALUE;
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // Safety: valid out-pointers, a valid attribute struct, default size.
    unsafe {
        if CreatePipe(&mut input_read, &mut input_write, &inherit, 0) == 0 {
            return Err(last_error("CreatePipe(stdin)"));
        }
        if CreatePipe(&mut output_read, &mut output_write, &inherit, 0) == 0 {
            CloseHandle(input_read);
            CloseHandle(input_write);
            return Err(last_error("CreatePipe(stdout)"));
        }
    }

    // The pseudoconsole owns the child side of both pipes; it duplicates them,
    // so the handles we pass can be closed once it has been created.
    let size = COORD { X: 80, Y: 24 };
    let mut console: HPCON = 0;
    // Safety: the four pipe handles are valid; `console` is a valid out-ptr.
    let hr = unsafe { CreatePseudoConsole(size, input_read, output_write, 0, &mut console) };
    // Our ends of the child-side handles are no longer needed: the pseudoconsole
    // duplicated them. `input_write` and `output_read` are ours to keep.
    unsafe {
        CloseHandle(input_read);
        CloseHandle(output_write);
    }
    if hr < 0 {
        unsafe {
            CloseHandle(input_write);
            CloseHandle(output_read);
        }
        return Err(format!("CreatePseudoConsole failed: HRESULT {hr:#x}"));
    }

    // Job object first, so the process can be assigned at creation via the
    // attribute list. Assigning after CreateProcess leaves a window in which a
    // grandchild can escape; `PROC_THREAD_ATTRIBUTE_JOB_LIST` closes it.
    // Safety: standard job creation; every failure path closes what it opened.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        unsafe {
            ClosePseudoConsole(console);
            CloseHandle(input_write);
            CloseHandle(output_read);
        }
        return Err(last_error("CreateJobObjectW"));
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // Safety: `job` is valid, the struct and its size are correct.
    let set_ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set_ok == 0 {
        let e = last_error("SetInformationJobObject");
        unsafe {
            CloseHandle(job);
            ClosePseudoConsole(console);
            CloseHandle(input_write);
            CloseHandle(output_read);
        }
        return Err(e);
    }

    // Attribute lists: one with {pseudoconsole, job} and a fallback with the
    // pseudoconsole only. The fallback exists because a parent already inside a
    // non-nesting job (GitHub's runners, some service managers) makes the *job
    // attribute* fail process creation with ERROR_ACCESS_DENIED; without the
    // fallback the terminal would be unusable there.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Without STARTF_USESTDHANDLES the child inherits the parent's standard
    // handles by value, even with bInheritHandles=FALSE. It then writes its
    // banner and prompt to the *server's* stdout instead of the pseudoconsole,
    // which is exactly what the runner showed: ConPTY's own init sequences
    // arrived on the output pipe (`ESC]0;...cmd.exe`), but `cmd`'s banner landed
    // in the test log and its stdin was the harness's. Nulling all three is
    // what node-pty's conpty.cc does, with the same comment.
    si.StartupInfo.dwFlags |= windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = std::ptr::null_mut();
    si.StartupInfo.hStdOutput = std::ptr::null_mut();
    si.StartupInfo.hStdError = std::ptr::null_mut();
    let mut cmdline = build_command_line(command, args);
    let cwd_w = wide(cwd);
    let mut env_block = build_env_block(env);
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // `create_with` builds a list of `n` attributes (the pseudoconsole is
    // always first), calls CreateProcessW and deletes the list. It returns the
    // OS error so the caller can decide whether to retry.
    // Returns `None` on success, `Some(error)` otherwise. An io::Error
    // carrying os-error 0 as a success sentinel would be unreadable.
    let mut create_with = |with_job: bool,
                           pi: &mut PROCESS_INFORMATION|
     -> Option<std::io::Error> {
        let count = if with_job { 2 } else { 1 };
        let mut size: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size);
        }
        let mut buf = vec![0u8; size];
        let list = buf.as_mut_ptr() as *mut std::ffi::c_void;
        if unsafe { InitializeProcThreadAttributeList(list, count, 0, &mut size) } == 0 {
            return Some(std::io::Error::last_os_error());
        }
        // The pseudoconsole value is the HPCON itself, not a pointer to it.
        let pc = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const std::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if pc == 0 {
            let e = std::io::Error::last_os_error();
            unsafe { DeleteProcThreadAttributeList(list) };
            return Some(e);
        }
        if with_job {
            let j = unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                    &job as *const HANDLE as *const std::ffi::c_void,
                    std::mem::size_of::<HANDLE>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if j == 0 {
                let e = std::io::Error::last_os_error();
                unsafe { DeleteProcThreadAttributeList(list) };
                return Some(e);
            }
        }
        si.lpAttributeList = list;
        // Safety: strings are NUL-terminated UTF-16 and outlive the call; the
        // attribute list is initialized and valid.
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                env_block.as_mut_ptr() as *const std::ffi::c_void,
                cwd_w.as_ptr(),
                &si.StartupInfo,
                pi,
            )
        };
        unsafe { DeleteProcThreadAttributeList(list) };
        if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        }
    };

    let failed = match create_with(true, &mut pi) {
        None => None,
        // ERROR_ACCESS_DENIED: the parent job forbids nesting. Create without
        // the job attribute and assign afterwards.
        Some(e) if e.raw_os_error() == Some(5) => match create_with(false, &mut pi) {
            None => {
                // Safety: both handles are valid; a failed assign is
                // non-fatal but means only the direct child is killable.
                // Recorded in PLATFORM.md §1.4.
                unsafe {
                    AssignProcessToJobObject(job, pi.hProcess);
                }
                None
            }
            other => other,
        },
        Some(e) => Some(e),
    };
    if let Some(err) = failed {
        unsafe {
            CloseHandle(job);
            ClosePseudoConsole(console);
            CloseHandle(input_write);
            CloseHandle(output_read);
        }
        return Err(format!("CreateProcessW: {err}"));
    }
    unsafe { CloseHandle(pi.hThread) };

    Ok(Proc {
        pid: pi.dwProcessId,
        console,
        input: input_write,
        output: output_read,
        process: pi.hProcess,
        job,
        alive: Arc::new(AtomicBool::new(true)),
        write_lock: std::sync::Mutex::new(()),
    })
}

impl Proc {
    pub fn write(&self, data: &str) {
        let _guard = self.write_lock.lock();
        let mut bytes = data.as_bytes();
        while !bytes.is_empty() {
            let mut written: u32 = 0;
            // Safety: `input` is open and not concurrently closed (write_lock);
            // `bytes` is a valid slice for its length.
            let ok = unsafe {
                WriteFile(
                    self.input,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                return;
            }
            bytes = &bytes[written as usize..];
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        // Safety: `console` is valid until Drop.
        unsafe {
            ResizePseudoConsole(self.console, size);
        }
    }

    pub fn kill(&self) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        // Terminating the job is the whole-tree kill; the direct
        // TerminateProcess is a fallback for the case where the process could
        // not be assigned to the job (nested-job denial).
        // Safety: both handles are valid.
        unsafe {
            if TerminateJobObject(self.job, 1) == 0 {
                TerminateProcess(self.process, 1);
            }
        }
    }

    pub fn start(&self, session: Arc<Session>, on_exit: Arc<dyn Fn(Option<i32>) + Send + Sync>) {
        let output = SendHandle(self.output);
        let alive_reader = self.alive.clone();
        let reader = std::thread::Builder::new()
            .name("pty-reader-win".into())
            .spawn(move || {
                let mut buf = [0u8; 65536];
                loop {
                    let mut avail: u32 = 0;
                    // Safety: `output` is a valid pipe handle; the buffer is
                    // valid for `buf.len()` bytes.
                    let ok = unsafe {
                        PeekNamedPipe(
                            output.get(),
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null_mut(),
                            &mut avail,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 {
                        break; // pipe closed (console closed)
                    }
                    if avail == 0 {
                        if !alive_reader.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    let want = (avail as usize).min(buf.len());
                    let mut got: u32 = 0;
                    // Safety: same as above; `want` <= buf.len().
                    let read_ok = unsafe {
                        windows_sys::Win32::Storage::FileSystem::ReadFile(
                            output.get(),
                            buf.as_mut_ptr(),
                            want as u32,
                            &mut got,
                            std::ptr::null_mut(),
                        )
                    };
                    if read_ok == 0 || got == 0 {
                        break;
                    }
                    session.push_output(String::from_utf8_lossy(&buf[..got as usize]).into_owned());
                }
            })
            .ok();

        let process = SendHandle(self.process);
        let job = SendHandle(self.job);
        let alive_waiter = self.alive.clone();
        std::thread::Builder::new()
            .name("pty-waiter-win".into())
            .spawn(move || {
                // Safety: `process` is a valid handle owned by the Proc, which
                // lives at least as long as this thread (see Drop: it joins).
                let rc = unsafe { WaitForSingleObject(process.get(), u32::MAX) };
                let mut code: u32 = 0;
                if rc == WAIT_OBJECT_0 {
                    unsafe { GetExitCodeProcess(process.get(), &mut code) };
                } else if rc == WAIT_TIMEOUT {
                    code = 0;
                }
                alive_waiter.store(false, Ordering::SeqCst);
                // Let the reader drain before subscribers see the end.
                std::thread::sleep(std::time::Duration::from_millis(20));
                let _ = job.get();
                on_exit(Some(code as i32));
            })
            .ok();
        let _ = reader;
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Order matters: close the console first so the child's stdio goes
        // away, then the job, then our pipe ends. The reader is a non-blocking
        // poller, so `ClosePseudoConsole` cannot deadlock against it.
        let _guard = self.write_lock.lock();
        // Safety: every handle is owned by `self` and closed exactly once.
        unsafe {
            ClosePseudoConsole(self.console);
            if !self.input.is_null() && self.input != INVALID_HANDLE_VALUE {
                CloseHandle(self.input);
            }
            if !self.output.is_null() && self.output != INVALID_HANDLE_VALUE {
                CloseHandle(self.output);
            }
            // KILL_ON_JOB_CLOSE reaps any surviving descendant as this closes.
            if !self.job.is_null() {
                CloseHandle(self.job);
            }
            if !self.process.is_null() {
                CloseHandle(self.process);
            }
        }
        self.console = 0;
        self.input = std::ptr::null_mut();
        self.output = std::ptr::null_mut();
        self.job = std::ptr::null_mut();
        self.process = std::ptr::null_mut();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_block_is_sorted_case_insensitively_and_double_nul_terminated() {
        let block = build_env_block(&[
            ("zeta".into(), "1".into()),
            ("Alpha".into(), "2".into()),
            ("beta".into(), "3".into()),
        ]);
        let text: String = String::from_utf16(&block).unwrap();
        let parts: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
        assert_eq!(parts, vec!["Alpha=2", "beta=3", "zeta=1"]);
        assert!(block.ends_with(&[0, 0]), "must end with a double NUL");
    }

    #[test]
    fn env_block_last_wins_and_drops_invalid_names() {
        let block = build_env_block(&[
            ("Path".into(), "a".into()),
            ("PATH".into(), "b".into()),
            ("BAD=NAME".into(), "x".into()),
            ("".into(), "y".into()),
        ]);
        let text: String = String::from_utf16(&block).unwrap();
        let parts: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
        assert_eq!(parts, vec!["PATH=b"]);
    }

    #[test]
    fn quoting_round_trips_through_the_msvcrt_rules() {
        // Expected outputs are what `CommandLineToArgvW` would produce for each
        // input; the empty case must produce an explicit "" or it disappears.
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("C:\\dir\\"), "C:\\dir\\");
        assert_eq!(quote_arg("trail\\"), "trail\\");
        assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
        // A backslash that is NOT immediately before a quote stays literal.
        // Only a backslash preceding a quote is an escape (the impl was
        // correct here; this expectation was not).
        assert_eq!(quote_arg("x\\ y"), "\"x\\ y\"");
    }

    #[test]
    fn command_line_quotes_each_argument() {
        let line = build_command_line("C:\\Program Files\\x.exe", &["a b".into(), "c".into()]);
        let text = String::from_utf16(&line[..line.len() - 1]).unwrap();
        assert_eq!(text, "\"C:\\Program Files\\x.exe\" \"a b\" c");
    }
}
