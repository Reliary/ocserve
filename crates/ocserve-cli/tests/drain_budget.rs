//! The drain deadline must start at the SIGNAL, not at startup.
//!
//! The first version of the bounded drain wrapped the signal wait in a
//! timeout, so the server shut itself down 20 s after every start (observed
//! live 2026-10-23 17:13/17:15/17:16). This test keeps a started server alive
//! for longer than the drain budget with no signal and requires it to still
//! serve.

#![cfg(unix)]

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
fn server_survives_past_the_drain_budget_without_a_signal() {
    let exe = bin();
    assert!(exe.exists(), "server binary missing at {}", exe.display());
    assert!(
        std::net::TcpStream::connect("127.0.0.1:5199").is_err(),
        "port 5199 already in use"
    );
    let home = tempfile::tempdir().expect("home");
    let data = tempfile::tempdir().expect("data");
    let log_path = data.path().join("server.log");
    let log = std::fs::File::create(&log_path).expect("log");
    let log2 = log.try_clone().expect("clone");
    let mut child = Command::new(&exe)
        .arg("serve")
        .arg("--port")
        .arg("5199")
        .env("HOME", home.path())
        .env("OCSERVE_DATA_DIR", data.path())
        .env("OCSERVE_LEGACY_SYNC", "0")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        // Short budget so the test does not take 20 s.
        .env("OCSERVE_DRAIN_SECS", "2")
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .spawn()
        .expect("spawn server");

    let started = Instant::now();
    loop {
        if std::net::TcpStream::connect("127.0.0.1:5199").is_ok() {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "server never listened"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // Well past the 2 s budget: a timer that starts at boot would have fired.
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "server exited on its own past the drain budget (the timer started at boot)"
    );
    let probe = std::net::TcpStream::connect("127.0.0.1:5199");
    assert!(probe.is_ok(), "server stopped listening without a signal");

    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("server did not exit after SIGTERM");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let text = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        text.contains("shutdown: draining"),
        "no drain line (log tail: {})",
        text.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
    );
}
