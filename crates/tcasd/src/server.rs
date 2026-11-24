//! Startup, the single-instance lock, and the socket loop.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, RwLock};

use tcas_core::proto::{Request, Response, code};
use tcas_sys::home::Home;

use crate::daemon::{Daemon, load_global, log};
use crate::git::{self, Git};
use crate::waits::Waits;
use crate::world::World;
use crate::{handlers, liveness, store, waits, wsjson};

/// Variables a detached tcasd must not inherit (daemon-and-cli.md section 2).
pub fn scrub_env() {
    let mut names: Vec<String> = vec![
        "TCAS_SESSION".into(),
        "CLAUDE_CODE_SESSION_ID".into(),
        "ANTIGRAVITY_CONVERSATION_ID".into(),
        "ANTIGRAVITY_AGENT".into(),
    ];
    names.extend(git::strip_list());
    for n in names {
        // SAFETY: called at startup before any thread is spawned.
        unsafe { std::env::remove_var(n) };
    }
}

fn lock(home: &Home) -> std::io::Result<Option<File>> {
    let f = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(home.pid_file())?;
    // SAFETY: a valid descriptor owned by `f`.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r != 0 {
        return Ok(None);
    }
    f.set_len(0)?;
    (&f).write_all(format!("{}\n", std::process::id()).as_bytes())?;
    Ok(Some(f))
}

fn rotate_log(home: &Home) {
    let p = home.log();
    if std::fs::metadata(&p).map(|m| m.len() > 10 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&p, p.with_extension("log.1"));
    }
}

/// Runs tcasd in the foreground until `daemon.stop`.
pub fn run(home: Home) -> Result<(), String> {
    scrub_env();
    home.ensure().map_err(|e| format!("{}: {e}", home.dir.display()))?;
    rotate_log(&home);
    let Some(lock_file) = lock(&home).map_err(|e| format!("{}: {e}", home.pid_file().display()))? else {
        return Err("another tcasd is already running".into());
    };
    let sock = home.socket();
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    let conn = store::open(&home.db()).map_err(|e| format!("{}: {e}", home.db().display()))?;

    let d: &'static Daemon = Box::leak(Box::new(Daemon {
        global: RwLock::new(load_global(&home)),
        home: home.clone(),
        db: Mutex::new(conn),
        git: Git::new(),
        waits: Mutex::new(Waits::default()),
        kick_flag: Mutex::new(false),
        kick_cv: Condvar::new(),
        stopping: AtomicBool::new(false),
        started_at: tcas_sys::clock::now_ms(),
        pid: std::process::id() as i32,
        socket: sock.clone(),
        windows: Mutex::new(Default::default()),
        last_wsjson: Mutex::new(None),
    }));
    {
        let g = d.global();
        let conn = d.db();
        if let Ok(w) = World::load(&conn) {
            wsjson::write(d, &w, &g);
        }
    }
    log(format!("tcasd {} started, pid {}, home {}", env!("CARGO_PKG_VERSION"), d.pid, home.dir.display()));
    std::thread::spawn(move || waits::run(d));
    std::thread::spawn(move || liveness::run(d));
    liveness::check(d);

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                std::thread::spawn(move || serve(d, stream));
            }
            Err(e) => log(format!("accept: {e}")),
        }
        if d.stopping.load(Ordering::SeqCst) {
            break;
        }
    }
    drop(lock_file);
    Ok(())
}

fn serve(d: &'static Daemon, stream: UnixStream) {
    let mut line = String::new();
    let mut reader = BufReader::new(&stream);
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let resp = match serde_json::from_str::<Request>(&line) {
        Ok(req) => {
            let op = req.op.clone();
            let r = handlers::dispatch(d, req, &stream);
            if op == "daemon.stop" && r.ok {
                write_resp(&stream, &r);
                shutdown(d);
            }
            r
        }
        Err(e) => Response::err(code::BAD_REQUEST, format!("request: {e}")),
    };
    write_resp(&stream, &resp);
}

fn write_resp(mut stream: &UnixStream, r: &Response) {
    let mut s = serde_json::to_string(r).unwrap_or_else(|_| "{\"ok\":false}".into());
    s.push('\n');
    let _ = stream.write_all(s.as_bytes());
    let _ = stream.flush();
}

fn shutdown(d: &'static Daemon) -> ! {
    d.stopping.store(true, Ordering::SeqCst);
    log("stopping on request");
    // Waits end as cancelled: the user stopped tcasd, so their clients must
    // not start it again to re-register.
    {
        let mut w = d.waits();
        for e in w.list.drain(..) {
            let mut r = tcas_core::proto::AcquireResult::new(tcas_core::proto::Outcome::Cancelled);
            r.session = e.session.clone();
            r.refusal = Some("tcasd was stopped by the user".into());
            let _ = e.tx.send(r);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = std::fs::remove_file(&d.socket);
    let conn = d.db();
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    std::process::exit(0);
}
