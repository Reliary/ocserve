//! A SIGTERM arriving immediately after listen must still drain.
//!
//! The original test signalled only after the port accepted. Registration used
//! to happen when `axum::serve` first polled the future, so a signal in the
//! bind->poll window took the default disposition and killed the process
//! (observed once as `ExitStatus(15)` in a full parallel run, 2026-10-23).
//! This test signals on the FIRST successful connect, which is the tightest
//! observation point a client has, and requires the drain line.

#![cfg(unix)]

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("ocserve")
}

#[test]
fn sigterm_at_first_accept_still_drains() {
    let exe = bin();
    assert!(exe.exists(), "server binary missing at {}", exe.display());
    assert!(
        std::net::TcpStream::connect("127.0.0.1:5198").is_err(),
        "port 5198 already in use"
    );
    let home = tempfile::tempdir().expect("home");
    let data = tempfile::tempdir().expect("data");
    let log_path = data.path().join("server.log");
    let log = std::fs::File::create(&log_path).expect("log");
    let log2 = log.try_clone().expect("clone");
    let mut child = Command::new(&exe)
        .arg("serve")
        .arg("--port")
        .arg("5198")
        .env("HOME", home.path())
        .env("OCSERVE_DATA_DIR", data.path())
        .env("OCSERVE_LEGACY_SYNC", "0")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env("RUST_LOG", "info")
        // Debug builds only: widen the bind->serve window so this test can
        // deterministically signal inside it. Release ignores the variable.
        .env("OCSERVE_TEST_BIND_DELAY_MS", "600")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .expect("spawn server");

    let started = Instant::now();
    let mut accepted = false;
    while started.elapsed() < Duration::from_secs(30) {
        if std::net::TcpStream::connect("127.0.0.1:5198").is_ok() {
            accepted = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(accepted, "server never listened on 5198");
    // Signal the instant the socket exists.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("no exit within 10s of an at-accept SIGTERM");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let mut text = String::new();
    std::fs::File::open(&log_path)
        .expect("log")
        .read_to_string(&mut text)
        .expect("read log");
    assert!(
        status.success(),
        "server exited non-zero on an at-accept SIGTERM: {status:?}"
    );
    assert!(
        text.contains("shutdown: draining"),
        "no drain line for an at-accept SIGTERM (log tail: {})",
        text.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
    );
}
