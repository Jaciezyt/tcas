//! Session liveness and cleanup (daemon-and-cli.md section 2).

use std::collections::HashMap;
use std::time::Duration;

use tcas_core::proto::Request;

use crate::daemon::{Daemon, log};
use crate::engine;
use crate::handlers;
use crate::store;

const ENDED_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

/// Ends sessions whose process is gone. Sessions without a recorded process
/// (set by hand with `TCAS_SESSION`) are skipped.
pub fn check(d: &'static Daemon) {
    let dead: Vec<String> = {
        let conn = d.db();
        let Ok(all) = store::sessions(&conn) else { return };
        all.into_iter()
            .filter(|s| s.state != "ended")
            .filter(|s| match (s.pid, s.pid_started_at) {
                (Some(pid), Some(st)) => !tcas_sys::proc::alive(pid, st),
                _ => false,
            })
            .map(|s| s.id)
            .collect()
    };
    for sid in dead {
        let req = Request { v: tcas_core::proto::VERSION, op: "internal.end".into(), session: None, cwd: "/".into(), ancestors: vec![], args: serde_json::Value::Null };
        let r = handlers::with_tx_inner(d, &req, HashMap::new(), |env| engine::end_session(env, &sid, "process_gone"));
        if let Err(e) = r {
            log(format!("ending {sid}: {}", e.message));
        } else {
            log(format!("session {sid} ended: process gone"));
        }
    }
    let now = tcas_sys::clock::now_ms();
    let retention = d.global().cfg.daemon.event_retention_days as i64 * 86_400_000;
    let conn = d.db();
    let _ = conn.execute(
        "DELETE FROM session WHERE state='ended' AND ended_at < ?1 AND id NOT IN (SELECT session FROM lease)",
        [now - ENDED_RETENTION_MS],
    );
    let _ = conn.execute("DELETE FROM notice WHERE session NOT IN (SELECT id FROM session)", []);
    if retention > 0 {
        let _ = conn.execute("DELETE FROM event WHERE at < ?1", [now - retention]);
    }
}

pub fn run(d: &'static Daemon) {
    loop {
        let secs = d.global().cfg.daemon.liveness_interval_secs.max(1);
        std::thread::sleep(Duration::from_secs(secs));
        if d.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        check(d);
        d.kick();
    }
}
