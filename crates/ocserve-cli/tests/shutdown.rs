//! SIGTERM must reach the drain path.
//!
//! The server waited only on `ctrl_c()`, so `systemctl restart` (SIGTERM) never
//! logged `shutdown: draining`. That was invisible for months because nothing
//! asserted it and the journal simply lacked the line.
//!
//! CONTROL (proven): replace the `tokio::select!` SIGTERM arm with a bare
//! `ctrl_c().await` and this test sees zero drain lines while the process is
//! killed by the default SIGTERM disposition.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> std::path::PathBuf {
    // The integration test binary lives in target/<profile>/deps/…
    let mut p = std::env::current_exe().expect("test exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("ocserve")
}

#[test]
fn sigterm_logs_the_drain_line() {
    let exe = bin();
    assert!(exe.exists(), "server binary missing at {}", exe.display());

    // The port must be free before we start, or a leftover server from an
    // earlier aborted run answers the probe and our SIGTERM goes to the wrong
    // process (measured: a stale 5197 listener made this test fail while the
    // binary under test was fine).
    assert!(
        std::net::TcpStream::connect("127.0.0.1:5197").is_err(),
        "port 5197 already in use; a stale test server is running"
    );

    let home = tempfile::tempdir().expect("home");
    let data = tempfile::tempdir().expect("data");
    // stderr goes to a FILE, not a pipe: with a pipe, a chatty boot fills the
    // 64 KiB buffer and the server blocks in `write(2)` before the runtime ever
    // polls `shutdown_signal()`, so the SIGTERM handler is never installed and
    // the process dies with the default disposition. That produced a false red
    // on the first version of this test (measured: exit 143, zero drain lines,
    // same binary passes when stderr is a file).
    let log_path = data.path().join("server.log");
    let log = std::fs::File::create(&log_path).expect("log file");
    let log2 = log.try_clone().expect("clone log");
    let mut child = Command::new(&exe)
        .arg("serve")
        .arg("--port")
        .arg("5197")
        .env("HOME", home.path())
        .env("OCSERVE_DATA_DIR", data.path())
        .env("OCSERVE_LEGACY_SYNC", "0")
        .env("OCSERVE_SERVER_PASSWORD_FILE", "")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .expect("spawn server");

    // Wait for boot (the serving line) before signalling; a SIGTERM during
    // boot would exit before the shutdown handler is installed.
    let started = Instant::now();
    let mut up = false;
    while started.elapsed() < Duration::from_secs(30) {
        if std::net::TcpStream::connect("127.0.0.1:5197").is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(up, "server never listened on 5197");

    // SIGTERM, exactly what systemd sends. Guard that the pid we signal is the
    // child we spawned (the stale-listener bug above was a harness defect, but
    // an explicit liveness check makes the failure mode obvious next time).
    assert!(
        std::path::Path::new(&format!("/proc/{}", child.id())).exists(),
        "child pid {} vanished before the signal",
        child.id()
    );
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }

    // Bounded wait: if the handler is missing, the process is still alive and
    // an unbounded `wait()` hangs the whole suite (measured: 20 min on the
    // negative-control run). Kill it and fail with the reason instead.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                panic!(
                    "server did not exit within 10s of SIGTERM (no drain path). log tail: {}",
                    log.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let stderr = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        status.success(),
        "server exited non-zero on SIGTERM: {status:?}"
    );
    assert!(
        stderr.contains("shutdown: draining"),
        "no drain line on SIGTERM — the handler is missing (stderr tail: {})",
        stderr.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
    );
}
