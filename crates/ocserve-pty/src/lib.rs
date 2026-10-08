//! ocserve PTY session manager — terminal sessions for the `/pty/*` route
//! group (web-UI integrated terminal, VS Code, SDK clients).
//!
//! Freeze parity (probed live against opencode 1.18.31, 2026-10-08 — see
//! bench/pty/PTY-PLAN.md for every captured shape):
//! - sessions are in-process; info {id,title,command,args,cwd,status,pid,
//!   exit_code?}; the legacy `/pty` surface hides exited sessions (upstream
//!   handlers/pty.ts filters status==="running"), so get/list/attach/remove
//!   here only see running ones; exit retention (cap 25) still publishes
//!   pty.exited/pty.deleted events.
//! - output buffer: 2 MiB retained, absolute UTF-16 cursor (JS
//!   `chunk.length` counts UTF-16 code units — D-PTY-1), replay from an
//!   arbitrary cursor, -1 = tail from current end (core/src/pty.ts attach).
//! - login shells (bash/sh/dash/zsh/ksh/fish) get "-l" appended to args.
//! - default title `Terminal <last4>`; env TERM=xterm-256color,
//!   OPENCODE_TERMINAL=1 (core/src/pty.ts create).
//! - subscribers are bounded (1024 messages, try_send) — a stalled websocket
//!   is dropped, never an unbounded queue (AGENTS §2.3; upstream uses one
//!   unbounded pending Vec per subscriber).
//!
//! Divergences (recorded): Windows PTY unsupported (stub error; upstream
//! uses conpty); `workspace` query ignored (single-directory server).

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub mod shells;
pub mod ticket;

#[cfg(unix)]
mod unix;
#[cfg(not(unix))]
use stub::Proc;
#[cfg(unix)]
use unix::Proc;

/// Retained output cap (core/src/pty.ts BUFFER_LIMIT).
pub const BUFFER_LIMIT: usize = 2 * 1024 * 1024;
/// Exited-session retention cap (core/src/pty.ts EXITED_LIMIT).
pub const EXITED_LIMIT: usize = 25;
/// Per-subscriber message bound; a full channel means a stalled client and
/// the subscriber is dropped (bounded by construction).
pub const SUB_CHANNEL: usize = 1024;

/// Stream message to a websocket attachment.
pub enum Msg {
    Data(String),
    End { exit_code: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    NotFound,
    Exited,
}

/// Snapshot returned by attach: replay text from the requested cursor, the
/// absolute cursor after replay, and the subscription handle.
pub struct Attachment {
    pub replay: String,
    pub cursor: u64,
    pub sub_id: u64,
    pub rx: tokio::sync::mpsc::Receiver<Msg>,
}

struct Buf {
    data: String,
    /// Absolute UTF-16 offset of data[0].
    buffer_cursor: u64,
    /// Absolute UTF-16 offset of the end.
    cursor: u64,
    subs: HashMap<u64, tokio::sync::mpsc::Sender<Msg>>,
}

pub struct Session {
    id: String,
    inner: Mutex<SessionState>,
    buf: Mutex<Buf>,
    next_sub: AtomicU64,
    proc: Proc,
}

struct SessionState {
    title: String,
    command: String,
    args: Vec<String>,
    cwd: String,
    pid: u32,
    status: &'static str,
    exit_code: Option<i32>,
}

impl Session {
    fn info_json(&self) -> Value {
        let s = self.inner.lock().unwrap();
        let mut v = json!({
            "id": self.id,
            "title": s.title,
            "command": s.command,
            "args": s.args,
            "cwd": s.cwd,
            "status": s.status,
            "pid": s.pid,
        });
        if let Some(code) = s.exit_code {
            v["exitCode"] = json!(code);
        }
        v
    }

    fn running(&self) -> bool {
        self.inner.lock().unwrap().status == "running"
    }

    /// Append a chunk from the reader thread: advance cursor, retain ≤2 MiB,
    /// fan out to subscribers (bounded; full channel ⇒ drop subscriber).
    fn push_output(&self, chunk: String) {
        let len = utf16_len(&chunk);
        let mut buf = self.buf.lock().unwrap();
        buf.cursor += len;
        buf.data.push_str(&chunk);
        if utf16_len(&buf.data) > BUFFER_LIMIT as u64 {
            let excess = utf16_len(&buf.data) - BUFFER_LIMIT as u64;
            let cut = utf16_to_byte(&buf.data, excess as usize);
            buf.data.drain(..cut);
            buf.buffer_cursor += excess;
        }
        let mut dead: Vec<u64> = Vec::new();
        for (id, tx) in buf.subs.iter() {
            if tx.try_send(Msg::Data(chunk.clone())).is_err() {
                dead.push(*id);
            }
        }
        for id in dead {
            buf.subs.remove(&id);
        }
    }

    fn end(&self, exit_code: Option<i32>) {
        let mut buf = self.buf.lock().unwrap();
        for tx in buf.subs.values() {
            let _ = tx.try_send(Msg::End { exit_code });
        }
        buf.subs.clear();
    }

    pub fn write(&self, data: &str) {
        if self.running() {
            self.proc.write(data);
        }
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        if self.running() {
            self.proc.resize(rows, cols);
        }
    }
}

/// Event publication callback: (event_type, properties) → bus frame.
pub type EventSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

pub struct PtyManager {
    inner: Mutex<Inner>,
    pub directory: String,
    event_sink: Mutex<Option<EventSink>>,
    tickets: ticket::TicketStore,
}

struct Inner {
    sessions: HashMap<String, Arc<Session>>,
    exit_order: VecDeque<String>,
}

impl PtyManager {
    pub fn new(directory: String) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                exit_order: VecDeque::new(),
            }),
            directory,
            event_sink: Mutex::new(None),
            tickets: ticket::TicketStore::new(),
        })
    }

    /// Wire durable event publication (pty.created/updated/exited/deleted) to
    /// the HTTP layer's bus. Called once at construction.
    pub fn set_event_sink(&self, f: EventSink) {
        *self.event_sink.lock().unwrap() = Some(f);
    }

    fn publish(&self, event_type: &str, properties: Value) {
        if let Some(f) = self.event_sink.lock().unwrap().as_ref() {
            f(event_type, properties);
        }
    }

    pub fn list_running(&self) -> Vec<Value> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<(u64, Value)> = inner
            .sessions
            .values()
            .filter(|s| s.running())
            .map(|s| (start_ms(&s.id), s.info_json()))
            .collect();
        // upstream list is insertion order of its Map (JS Map preserves
        // insertion); ascending id time reproduces that order here.
        out.sort_by_key(|(t, _)| *t);
        out.into_iter().map(|(_, v)| v).collect()
    }

    pub fn get_running(&self, id: &str) -> Result<Value, AttachError> {
        let inner = self.inner.lock().unwrap();
        match inner.sessions.get(id) {
            Some(s) if s.running() => Ok(s.info_json()),
            Some(_) => Err(AttachError::Exited),
            None => Err(AttachError::NotFound),
        }
    }

    pub fn create(self: &Arc<Self>, input: &Value) -> Result<Value, String> {
        let id = format!("pty_{}", ocserve_core::ids::ascending_tail());
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| shells::preferred(&self.directory));
        let mut args: Vec<String> = input
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if shells::login(&command) {
            args.push("-l".to_string());
        }
        let cwd = input
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.directory.clone());
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Terminal {}", &id[id.len().saturating_sub(4)..]));
        let mut env: Vec<(String, String)> = Vec::new();
        if let Some(map) = input.get("env").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(s) = v.as_str() {
                    env.push((k.clone(), s.to_string()));
                }
            }
        }
        env.push(("TERM".into(), "xterm-256color".into()));
        env.push(("OPENCODE_TERMINAL".into(), "1".into()));

        let proc = spawn_process(&command, &args, &cwd, &env)?;
        let pid = proc.pid;

        let session = Arc::new(Session {
            id: id.clone(),
            inner: Mutex::new(SessionState {
                title,
                command,
                args,
                cwd,
                pid,
                status: "running",
                exit_code: None,
            }),
            buf: Mutex::new(Buf {
                data: String::new(),
                buffer_cursor: 0,
                cursor: 0,
                subs: HashMap::new(),
            }),
            next_sub: AtomicU64::new(1),
            proc,
        });

        {
            let mut inner = self.inner.lock().unwrap();
            inner.sessions.insert(id.clone(), session.clone());
        }

        let info = session.info_json();
        self.publish("pty.created", json!({ "info": info.clone() }));

        // Reader + waiter threads (no-op on unsupported platforms).
        let reader_session = session.clone();
        let weak = Arc::downgrade(self);
        session.proc.start(
            reader_session.clone(),
            Arc::new(move |exit_code: Option<i32>| {
                {
                    let mut st = reader_session.inner.lock().unwrap();
                    st.status = "exited";
                    st.exit_code = exit_code;
                }
                let id = reader_session.id.clone();
                reader_session.end(exit_code);
                if let Some(m) = weak.upgrade() {
                    m.publish(
                        "pty.exited",
                        json!({"id": id, "exitCode": exit_code.unwrap_or(0)}),
                    );
                    m.note_exit(&id);
                }
            }),
        );

        Ok(info)
    }

    /// Retention: keep at most EXITED_LIMIT exited sessions (FIFO by exit
    /// time); removal publishes pty.deleted.
    fn note_exit(&self, id: &str) {
        let to_remove: Vec<String> = {
            let mut inner = self.inner.lock().unwrap();
            inner.exit_order.push_back(id.to_string());
            let mut rm = Vec::new();
            while inner.exit_order.len() > EXITED_LIMIT {
                if let Some(old) = inner.exit_order.pop_front() {
                    let still_exited = inner
                        .sessions
                        .get(&old)
                        .map(|s| !s.running())
                        .unwrap_or(false);
                    if still_exited {
                        inner.sessions.remove(&old);
                        rm.push(old);
                    }
                }
            }
            rm
        };
        for old in to_remove {
            self.publish("pty.deleted", json!({ "id": old }));
        }
    }

    pub fn update(&self, id: &str, input: &Value) -> Result<Value, AttachError> {
        let session = {
            let inner = self.inner.lock().unwrap();
            match inner.sessions.get(id) {
                Some(s) if s.running() => s.clone(),
                Some(_) => return Err(AttachError::Exited),
                None => return Err(AttachError::NotFound),
            }
        };
        if let Some(title) = input.get("title").and_then(|v| v.as_str()) {
            session.inner.lock().unwrap().title = title.to_string();
        }
        if let Some(size) = input.get("size") {
            let rows = size.get("rows").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let cols = size.get("cols").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            if rows > 0 && cols > 0 {
                session.resize(rows, cols);
            }
        }
        let info = session.info_json();
        self.publish("pty.updated", json!({ "info": info.clone() }));
        Ok(info)
    }

    pub fn remove(&self, id: &str) -> Result<Value, AttachError> {
        let session = {
            let inner = self.inner.lock().unwrap();
            match inner.sessions.get(id) {
                Some(s) if s.running() => s.clone(),
                Some(_) => return Err(AttachError::Exited),
                None => return Err(AttachError::NotFound),
            }
        };
        {
            let mut inner = self.inner.lock().unwrap();
            inner.sessions.remove(id);
            inner.exit_order.retain(|x| x != id);
        }
        session.end(None);
        session.proc.kill();
        self.publish("pty.deleted", json!({ "id": id }));
        Ok(json!(true))
    }

    pub fn attach(&self, id: &str, cursor: Option<i64>) -> Result<Attachment, AttachError> {
        let session = {
            let inner = self.inner.lock().unwrap();
            match inner.sessions.get(id) {
                Some(s) if s.running() => s.clone(),
                Some(_) => return Err(AttachError::Exited),
                None => return Err(AttachError::NotFound),
            }
        };
        let sub_id = session.next_sub.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::channel(SUB_CHANNEL);
        let mut buf = session.buf.lock().unwrap();
        let end = buf.cursor;
        let start = buf.buffer_cursor;
        let from = match cursor {
            Some(-1) => end,
            Some(c) if c >= 0 => c as u64,
            _ => 0,
        };
        let replay = if buf.data.is_empty() || from >= end {
            String::new()
        } else {
            let offset = from.saturating_sub(start);
            let byte = utf16_to_byte(&buf.data, offset as usize);
            buf.data[byte..].to_string()
        };
        buf.subs.insert(sub_id, tx);
        Ok(Attachment {
            replay,
            cursor: end,
            sub_id,
            rx,
        })
    }

    /// Write terminal input (websocket inbound). No-op when not running.
    pub fn write_session(&self, id: &str, data: &str) {
        let inner = self.inner.lock().unwrap();
        if let Some(s) = inner.sessions.get(id)
            && s.running()
        {
            s.write(data);
        }
    }

    /// Drop a subscription (websocket closed).
    pub fn detach(&self, id: &str, sub_id: u64) {
        let inner = self.inner.lock().unwrap();
        if let Some(s) = inner.sessions.get(id) {
            s.buf.lock().unwrap().subs.remove(&sub_id);
        }
    }

    pub fn issue_ticket(&self, id: &str) -> Value {
        let (ticket, expires_in) = self.tickets.issue(id, &self.directory);
        json!({ "ticket": ticket, "expires_in": expires_in })
    }

    pub fn consume_ticket(&self, id: &str, ticket: &str) -> bool {
        self.tickets.consume(ticket, id, &self.directory)
    }

    pub fn shells(&self) -> Vec<Value> {
        shells::list()
    }
}

fn start_ms(id: &str) -> u64 {
    // ids are pty_<12-hex-time><14-random>; the time half is
    // ms*4096+counter (schema/src/identifier.ts) — parse for stable order.
    let body = id.strip_prefix("pty_").unwrap_or(id);
    let take = body.len().min(12);
    u64::from_str_radix(&body[..take], 16).unwrap_or(0)
}

pub fn utf16_len(s: &str) -> u64 {
    s.chars().map(|c| c.len_utf16() as u64).sum()
}

/// Byte offset of the given UTF-16 offset (clamped to the end).
pub fn utf16_to_byte(s: &str, want: usize) -> usize {
    let mut u16off = 0usize;
    for (byte, ch) in s.char_indices() {
        if u16off >= want {
            return byte;
        }
        u16off += ch.len_utf16();
    }
    s.len()
}

#[cfg(unix)]
fn spawn_process(
    command: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
) -> Result<Proc, String> {
    unix::spawn(command, args, cwd, env)
}

#[cfg(not(unix))]
fn spawn_process(
    _command: &str,
    _args: &[String],
    _cwd: &str,
    _env: &[(String, String)],
) -> Result<Proc, String> {
    Err("PTY sessions are not supported on this platform (upstream uses conpty; ocserve divergence D-PTY-2)".into())
}

#[cfg(not(unix))]
mod stub {
    pub struct Proc {
        pub pid: u32,
    }
    impl Proc {
        pub fn write(&self, _data: &str) {}
        pub fn resize(&self, _rows: u16, _cols: u16) {}
        pub fn kill(&self) {}
        pub fn start(
            &self,
            _session: std::sync::Arc<super::Session>,
            _on_exit: std::sync::Arc<dyn Fn(Option<i32>) + Send + Sync>,
        ) {
        }
    }
}

/// Ticket store: UUID v4, 60 s TTL, single use, scoped (pty + directory),
/// capacity-bounded.
pub use ticket::TicketStore;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_handle_multibyte() {
        let s = "aé😀b"; // 1 + 1 + 2 + 1 = 5 UTF-16 units
        assert_eq!(utf16_len(s), 5);
        assert_eq!(utf16_to_byte(s, 0), 0);
        assert_eq!(utf16_to_byte(s, 1), 1); // after 'a'
        assert_eq!(utf16_to_byte(s, 2), 3); // after 'é' (2 bytes)
        assert_eq!(utf16_to_byte(s, 4), 7); // after emoji (4 bytes)
        assert_eq!(utf16_to_byte(s, 5), s.len());
        assert_eq!(utf16_to_byte(s, 99), s.len());
    }
}
