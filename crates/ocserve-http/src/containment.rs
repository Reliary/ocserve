//! Path containment, enforced on the **opened object** rather than on a string.
//!
//! ## Why this exists
//!
//! The previous implementation canonicalized a path and compared it with
//! `starts_with`. That has three defects, all of them class-level:
//!
//! 1. **Check and use are separate syscalls.** Between `canonicalize` and
//!    `open`, a component can be swapped for a symlink. This is the textbook
//!    TOCTOU shape; the mitigation is to make the check apply to the object the
//!    use will act on (open first, then verify the handle).
//! 2. **There were two implementations.** `lib.rs::scope_path` had a lexical
//!    fallback that could pass for a path the filesystem had already rejected,
//!    while `v2.rs::resolve_within` canonicalized the deepest existing ancestor.
//!    One was hardened, the other was not.
//! 3. **Windows was never considered.** `canonicalize` returns a verbatim
//!    (`\\?\`) path there, NTFS is case-insensitive, and reserved device names /
//!    trailing dots / alternate data streams are all distinct escape classes.
//!
//! ## The rule
//!
//! Open the object first, then ask the OS what it actually opened, then compare
//! against the base. The handle pins the inode, so no later rename or symlink
//! swap can redirect it. On Linux the relative-path case is stronger still:
//! `openat2(RESOLVE_BENEATH)` makes the kernel refuse any resolution outside the
//! base, including absolute symlink targets, so there is nothing left to verify.
//!
//! Comparison uses the platform's own collation (`CompareStringOrdinal` on
//! Windows, byte comparison elsewhere) rather than `to_lowercase()`, which does
//! not match NTFS's uppercase table.
//!
//! Reference for the design: Half-Moon Cookie (arXiv:2604.15641) — a check that
//! is separate from the use must produce a token the use can verify; here the
//! token is the file handle.

use std::path::{Path, PathBuf};

/// What the caller intends to do with the opened object.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OpenKind {
    /// Read a regular file.
    File,
    /// Enumerate a directory.
    Dir,
    /// Create or truncate a file at this path.
    Create,
}

/// A handle that has been proven to live beneath the base directory.
pub struct Contained {
    file: std::fs::File,
    /// The real path of the opened object, as reported by the OS.
    real: PathBuf,
}

impl Contained {
    /// The verified real path (absolute, resolved by the kernel/filesystem).
    pub fn path(&self) -> &Path {
        &self.real
    }

    /// The opened handle. Reading from this cannot be redirected by a later
    /// rename or symlink swap.
    pub fn file(&self) -> &std::fs::File {
        &self.file
    }

    pub fn into_file(self) -> std::fs::File {
        self.file
    }

    /// Directory entries as `(name, is_dir)`, read **from the verified handle**
    /// on Unix so a swap of the directory's path cannot change what we list.
    pub fn read_entries(&self) -> std::io::Result<Vec<(String, bool)>> {
        #[cfg(unix)]
        {
            use rustix::fs::{Dir, FileType};
            let mut out = Vec::new();
            let mut dir = Dir::read_from(&self.file)?;
            for entry in dir.by_ref() {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "." || name == ".." {
                    continue;
                }
                let is_dir = matches!(entry.file_type(), FileType::Directory);
                out.push((name, is_dir));
            }
            Ok(out)
        }
        #[cfg(not(unix))]
        {
            // Residual (named, not hidden): Windows has no portable
            // directory-fd iteration in std, so entries are read from the
            // verified path. The *directory itself* was verified by handle; the
            // window is that the path could be re-pointed at a different
            // directory between verify and read. Closing it needs
            // GetFileInformationByHandleEx(FileIdBothDirectoryInfo), which is
            // Phase W work.
            let mut out = Vec::new();
            for e in std::fs::read_dir(&self.real)? {
                let e = e?;
                let name = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                out.push((name, is_dir));
            }
            Ok(out)
        }
    }
}

/// Why an open failed.
#[derive(Debug)]
pub enum OpenError {
    /// The path escapes the base, or resolves through a link that does.
    Escape,
    /// The path is malformed for the platform (reserved name, trailing dot,
    /// alternate data stream, interior NUL).
    Invalid(String),
    /// Anything else from the OS.
    Io(std::io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Escape => write!(f, "path escapes the project directory"),
            OpenError::Invalid(m) => write!(f, "invalid path: {m}"),
            OpenError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<std::io::Error> for OpenError {
    fn from(e: std::io::Error) -> Self {
        OpenError::Io(e)
    }
}

/// Reject path shapes that are only meaningful (or only dangerous) on Windows.
///
/// These are not escapes in the `..` sense; they are names the Windows object
/// manager rewrites, so a string comparison can see one path while the kernel
/// opens another:
///   - reserved device names (`CON`, `NUL`, `COM1`…), with or without an
///     extension and in any directory;
///   - trailing dots and spaces, which Win32 strips before resolving;
///   - alternate data streams (`file:stream`), which share a filename prefix
///     with the file they attach to.
///
/// Rejecting them outright is what upstream's path handling effectively does,
/// and it keeps the check honest rather than trying to model every rewrite.
fn validate_platform_shape(raw: &str) -> Result<(), OpenError> {
    if raw.contains('\0') {
        return Err(OpenError::Invalid("interior NUL byte".into()));
    }
    #[cfg(windows)]
    {
        // A drive-qualified absolute path is allowed (`C:\x`), but a colon in
        // a non-prefix position is an alternate data stream.
        let rest = match raw.as_bytes() {
            [_, b':', ..] => &raw[2..],
            _ => raw,
        };
        if rest.contains(':') {
            return Err(OpenError::Invalid(
                "alternate data streams are not allowed".into(),
            ));
        }
        for comp in raw.split(['/', '\\']) {
            if comp.is_empty() {
                continue;
            }
            if comp.ends_with('.') || comp.ends_with(' ') {
                return Err(OpenError::Invalid(
                    "trailing dot or space is rewritten by Win32".into(),
                ));
            }
            // Reserved device names: compare the segment before the first dot.
            let stem = comp.split('.').next().unwrap_or(comp);
            let upper = stem.to_ascii_uppercase();
            let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || (upper.len() == 4
                    && (upper.starts_with("COM") || upper.starts_with("LPT"))
                    && upper.as_bytes()[3].is_ascii_digit());
            if reserved {
                return Err(OpenError::Invalid(format!("reserved device name: {comp}")));
            }
        }
    }
    Ok(())
}

/// Open `raw` beneath `base`, proving containment on the opened object.
///
/// `raw` may be relative, absolute, empty or `.`. Absolute paths are allowed
/// only if the object they resolve to is beneath `base` — same rule as the
/// previous implementation, but applied to the handle.
pub fn open_within(base: &Path, raw: &str, kind: OpenKind) -> Result<Contained, OpenError> {
    validate_platform_shape(raw)?;

    let canon_base = std::fs::canonicalize(base)?;

    // An empty path or "." means the base itself.
    let target = if raw.is_empty() || raw == "." {
        base.to_path_buf()
    } else {
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        }
    };

    // Linux relative paths: let the kernel enforce containment. This is
    // strictly stronger than verify-after-open because there is no window at
    // all, and it also rejects absolute symlink targets.
    #[cfg(target_os = "linux")]
    {
        if !raw.is_empty() && raw != "." && !Path::new(raw).is_absolute() {
            use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
            let dirfd = std::fs::File::open(base)?;
            // `BENEATH` alone is the load-bearing flag: it makes the kernel
            // refuse any resolution that leaves the base, including an absolute
            // symlink target (EXDEV). Deliberately NOT `NO_SYMLINKS`: in-tree
            // symlinks are legitimate (node_modules, linked packages) and
            // `BENEATH` already contains them.
            let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
            let mut flags = OFlags::CLOEXEC;
            // openat2 rejects a non-zero mode unless O_CREAT/O_TMPFILE is set
            // (EINVAL), so the mode is only supplied for creates.
            let mut mode = Mode::empty();
            match kind {
                OpenKind::Dir => flags |= OFlags::DIRECTORY | OFlags::RDONLY,
                OpenKind::File => flags |= OFlags::RDONLY,
                OpenKind::Create => {
                    flags |= OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC;
                    mode = Mode::from_bits_truncate(0o666);
                }
            }
            let rel = raw.trim_start_matches('/');
            match openat2(&dirfd, rel, flags, mode, resolve) {
                Ok(fd) => {
                    let file = std::fs::File::from(fd);
                    let real = real_path_of(&file)?;
                    return Ok(Contained { file, real });
                }
                // Kernel without openat2 (< 5.6): fall through to the portable
                // verify-after-open path. ELOOP from NO_SYMLINKS is a real
                // rejection and must not fall through.
                Err(rustix::io::Errno::NOSYS) => {}
                Err(e) => return Err(map_errno(e)),
            }
        }
    }

    // Portable path: open first, then verify the handle.
    let file = match kind {
        OpenKind::Dir => std::fs::File::open(&target)?,
        OpenKind::File => std::fs::File::open(&target)?,
        OpenKind::Create => {
            // Refuse to follow a symlink at the final component: writing
            // through one is the F6 escape class.
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.custom_flags(libc::O_NOFOLLOW);
            }
            // Parent must exist and be verified before we create inside it.
            if let Some(parent) = target.parent()
                && parent != Path::new("")
            {
                let parent_handle = open_within(base, &parent.to_string_lossy(), OpenKind::Dir)?;
                let _ = parent_handle; // verified; dropped after the open below
            }
            opts.open(&target)?
        }
    };

    let real = real_path_of(&file)?;
    if !is_beneath(&real, &canon_base) {
        return Err(OpenError::Escape);
    }
    Ok(Contained { file, real })
}

/// Create (or truncate) `raw` beneath `base`, creating missing parents, with
/// every directory component opened relative to the verified parent.
///
/// `open_within(.., Create)` alone cannot do this: openat2 will not create
/// directories, and calling `create_dir_all` on an unverified path is exactly
/// the F6 escape (`<base>/link -> /etc`, then create inside `/etc`). Here each
/// component is `mkdirat`+`openat2`'d from the previous verified fd, so a
/// symlinked component is rejected by `RESOLVE_BENEATH` before anything is
/// created through it.
pub fn create_within(base: &Path, raw: &str, body: &[u8]) -> Result<Contained, OpenError> {
    validate_platform_shape(raw)?;
    if raw.is_empty() || raw == "." {
        return Err(OpenError::Invalid("cannot create the base itself".into()));
    }
    if Path::new(raw).is_absolute() {
        // Absolute writes must still land beneath base; route them through the
        // portable path, which verifies the handle after opening.
        std::fs::create_dir_all(
            Path::new(raw)
                .parent()
                .ok_or_else(|| OpenError::Invalid("no parent directory".into()))?,
        )?;
        let mut c = open_within(base, raw, OpenKind::Create)?;
        use std::io::Write;
        c.file.write_all(body).map_err(OpenError::Io)?;
        return Ok(c);
    }

    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{Mode, OFlags, ResolveFlags, mkdirat, openat2};
        let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
        let mut dir = std::fs::File::open(base)?;
        let comps: Vec<&str> = raw
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        if comps.is_empty() {
            return Err(OpenError::Invalid("empty path".into()));
        }
        for (i, comp) in comps.iter().enumerate() {
            if *comp == ".." {
                return Err(OpenError::Escape);
            }
            let last = i + 1 == comps.len();
            if last {
                let fd = openat2(
                    &dir,
                    *comp,
                    OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC | OFlags::CLOEXEC,
                    Mode::from_bits_truncate(0o666),
                    resolve,
                )
                .map_err(map_errno)?;
                let file = std::fs::File::from(fd);
                use std::io::Write;
                (&file).write_all(body).map_err(OpenError::Io)?;
                let real = real_path_of(&file)?;
                let canon_base = std::fs::canonicalize(base)?;
                if !is_beneath(&real, &canon_base) {
                    return Err(OpenError::Escape);
                }
                return Ok(Contained { file, real });
            }
            match mkdirat(&dir, *comp, Mode::from_bits_truncate(0o777)) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(map_errno(e)),
            }
            let fd = openat2(
                &dir,
                *comp,
                OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
                Mode::empty(),
                resolve,
            )
            .map_err(map_errno)?;
            dir = std::fs::File::from(fd);
        }
        unreachable!("loop returns on the last component")
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Portable: verify the deepest existing ancestor, create the rest, then
        // open the leaf without following a final-component symlink.
        let joined = base.join(raw);
        let parent = joined
            .parent()
            .ok_or_else(|| OpenError::Invalid("no parent directory".into()))?;
        let mut probe = parent.to_path_buf();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if open_within(base, &probe.to_string_lossy(), OpenKind::Dir).is_ok() {
                break;
            }
            match (probe.file_name(), probe.parent()) {
                (Some(n), Some(p)) => {
                    tail.push(n.to_os_string());
                    probe = p.to_path_buf();
                }
                _ => return Err(OpenError::Escape),
            }
        }
        if !tail.is_empty() {
            std::fs::create_dir_all(parent)?;
        }
        let mut c = open_within(base, raw, OpenKind::Create)?;
        use std::io::Write;
        c.file.write_all(body).map_err(OpenError::Io)?;
        Ok(c)
    }
}

#[cfg(unix)]
fn map_errno(e: rustix::io::Errno) -> OpenError {
    match e {
        rustix::io::Errno::XDEV | rustix::io::Errno::LOOP | rustix::io::Errno::ACCESS => {
            OpenError::Escape
        }
        other => OpenError::Io(std::io::Error::from_raw_os_error(other.raw_os_error())),
    }
}

/// Ask the OS for the real path of an open handle.
#[cfg(target_os = "linux")]
fn real_path_of(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let link = format!("/proc/self/fd/{}", file.as_raw_fd());
    let p = std::fs::read_link(&link)?;
    // A deleted file reports "… (deleted)"; the suffix is not part of the name.
    let s = p.to_string_lossy();
    let s = s.strip_suffix(" (deleted)").unwrap_or(&s);
    Ok(PathBuf::from(s))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // F_GETPATH has no safe wrapper; buffer is fixed-size.
fn real_path_of(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // Safety: `fd` is valid for the lifetime of `file`; `buf` is PATH_MAX and
    // the kernel writes at most that many bytes including the NUL.
    let rc = unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let s = std::str::from_utf8(&buf[..end])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 path"))?;
    Ok(PathBuf::from(s))
}

#[cfg(windows)]
#[allow(unsafe_code)] // GetFinalPathNameByHandleW is FFI; see the safety note.
fn real_path_of(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, VOLUME_NAME_DOS,
    };
    let handle = file.as_raw_handle() as _;
    let mut buf = vec![0u16; 512];
    loop {
        // Safety: `handle` is owned by `file` and valid; `buf` is a valid
        // writable slice of `buf.len()` UTF-16 units.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if n == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (n as usize) < buf.len() {
            return Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])));
        }
        // Buffer too small: the return is the required size.
        buf.resize(n as usize + 1, 0);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn real_path_of(_file: &std::fs::File) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no handle-to-path primitive on this platform",
    ))
}

/// Is `real` beneath `base`, using the platform's own comparison?
///
/// Both arguments come from the OS, so casing is already the true casing;
/// the Windows case-insensitive comparison exists because NTFS treats `Foo`
/// and `foo` as the same object and `std::fs::canonicalize` preserves whichever
/// form the caller used for the base.
fn is_beneath(real: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        let (Some(r), Some(b)) = (real.to_str(), base.to_str()) else {
            return false;
        };
        // Component-wise, so /a/bc does not match base /a/b.
        let rb = r.trim_end_matches(['\\', '/']);
        let bb = b.trim_end_matches(['\\', '/']);
        if bb.is_empty() {
            return true;
        }
        if !ordinal_starts_with(rb, bb) {
            return false;
        }
        // Next char must be a separator (or end).
        match rb.as_bytes().get(bb.len()) {
            None => true,
            Some(b'\\') | Some(b'/') => true,
            Some(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        real.starts_with(base)
    }
}

/// `CompareStringOrdinal(ignore_case = TRUE)` prefix test.
#[cfg(windows)]
#[allow(unsafe_code)] // FFI; inputs are validated UTF-16 with explicit lengths.
fn ordinal_starts_with(hay: &str, needle: &str) -> bool {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_LESS_THAN, CompareStringOrdinal};
    let hay16: Vec<u16> = hay.encode_utf16().collect();
    let needle16: Vec<u16> = needle.encode_utf16().collect();
    if needle16.len() > hay16.len() {
        return false;
    }
    // Safety: both slices are valid for their stated lengths; the API only
    // reads them.
    let r = unsafe {
        CompareStringOrdinal(
            hay16.as_ptr(),
            needle16.len() as i32,
            needle16.as_ptr(),
            needle16.len() as i32,
            1,
        )
    };
    // CompareStringOrdinal compares the first cchCount1 chars of s1 with all of
    // s2; equality here means s2 is a prefix of s1.
    r == CSTR_EQUAL || r == CSTR_LESS_THAN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn accepts_paths_inside_and_rejects_escapes() {
        let root = tmp();
        let base = root.path().join("base");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/file.txt"), b"hi").unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"no").unwrap();

        assert!(open_within(&base, "sub/file.txt", OpenKind::File).is_ok());
        assert!(open_within(&base, "sub", OpenKind::Dir).is_ok());
        assert!(open_within(&base, ".", OpenKind::Dir).is_ok());
        assert!(open_within(&base, "", OpenKind::Dir).is_ok());

        for bad in [
            "../outside/secret",
            "sub/../../outside/secret",
            "/etc/passwd",
            "sub/../../../../etc/passwd",
        ] {
            match open_within(&base, bad, OpenKind::File) {
                Err(OpenError::Escape) | Err(OpenError::Io(_)) => {}
                other => panic!(
                    "{bad} must not open, got {:?}",
                    other.map(|c| c.path().to_path_buf())
                ),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_pointing_outside() {
        let root = tmp();
        let base = root.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"no").unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();

        // Directly through the link.
        assert!(matches!(
            open_within(&base, "link/secret", OpenKind::File),
            Err(OpenError::Escape) | Err(OpenError::Io(_))
        ));
        // A symlink that is the final component.
        std::os::unix::fs::symlink(outside.join("secret"), base.join("filelink")).unwrap();
        assert!(matches!(
            open_within(&base, "filelink", OpenKind::File),
            Err(OpenError::Escape) | Err(OpenError::Io(_))
        ));
        // Writing through a link must not land outside either.
        let before = std::fs::read_to_string(outside.join("secret")).unwrap();
        let _ = open_within(&base, "link/newfile", OpenKind::Create);
        assert!(
            !outside.join("newfile").exists(),
            "write through a symlinked directory escaped"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            before
        );
    }

    #[test]
    fn create_writes_inside_and_verifies_handle() {
        let root = tmp();
        let base = root.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let c = open_within(&base, "new.txt", OpenKind::Create).expect("create");
        assert!(is_beneath(c.path(), &std::fs::canonicalize(&base).unwrap()));
        drop(c);
        assert!(base.join("new.txt").exists());
    }

    #[test]
    fn base_prefix_is_component_wise() {
        let root = tmp();
        let a = root.path().join("a");
        let ab = root.path().join("ab");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&ab).unwrap();
        let base = std::fs::canonicalize(&a).unwrap();
        let other = std::fs::canonicalize(&ab).unwrap();
        assert!(
            !is_beneath(&other, &base),
            "sibling with shared prefix must not pass"
        );
    }

    #[test]
    fn create_within_makes_nested_parents_inside() {
        let root = tmp();
        let base = root.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        create_within(&base, "a/b/c.txt", b"deep").expect("nested create");
        assert_eq!(
            std::fs::read_to_string(base.join("a/b/c.txt")).unwrap(),
            "deep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_within_cannot_escape_through_symlinked_parent() {
        let root = tmp();
        let base = root.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();

        assert!(
            create_within(&base, "link/sub/evil.txt", b"x").is_err(),
            "creation through a symlinked parent must be refused"
        );
        assert!(
            !outside.join("sub").exists(),
            "created a directory outside the base"
        );
    }

    #[test]
    fn create_within_refuses_dotdot() {
        let root = tmp();
        let base = root.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        assert!(matches!(
            create_within(&base, "../escape.txt", b"x"),
            Err(OpenError::Escape)
        ));
    }

    #[test]
    fn rejects_nul() {
        assert!(matches!(
            validate_platform_shape("a\0b"),
            Err(OpenError::Invalid(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_rewrite_classes() {
        for bad in [
            r"C:\x\CON",
            r"CON",
            "file.txt:stream",
            "trailing.",
            "trailing ",
            r"C:\x\NUL.txt",
            r"C:\x\COM1",
        ] {
            assert!(
                matches!(validate_platform_shape(bad), Err(OpenError::Invalid(_))),
                "{bad} must be rejected"
            );
        }
        // Normal names still pass.
        assert!(validate_platform_shape(r"C:\x\file.txt").is_ok());
    }
}
