//! Process identity: PID plus start time, so a reused PID is not mistaken
//! for the same process.

use tcas_core::proto::ProcId;

#[derive(Debug, Clone, Copy)]
pub struct ProcInfo {
    pub ppid: i32,
    /// Milliseconds since the epoch.
    pub started_at: i64,
}

#[cfg(target_os = "macos")]
pub fn info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: proc_pidinfo fills at most `size` bytes of the zeroed struct.
    unsafe {
        let mut bi: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut libc::c_void, size);
        if n != size {
            return None;
        }
        Some(ProcInfo {
            ppid: bi.pbi_ppid as i32,
            started_at: bi.pbi_start_tvsec as i64 * 1000 + bi.pbi_start_tvusec as i64 / 1000,
        })
    }
}

#[cfg(target_os = "linux")]
pub fn info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may contain spaces.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state(3) ppid(4) ... starttime(22); index from field 3.
    let ppid: i32 = fields.get(1)?.parse().ok()?;
    let ticks: i64 = fields.get(19)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as i64;
    let btime: i64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok()))?;
    Some(ProcInfo { ppid, started_at: btime * 1000 + ticks * 1000 / hz })
}

pub fn proc_id(pid: i32) -> Option<ProcId> {
    info(pid).map(|i| ProcId { pid, started_at: i.started_at })
}

/// Whether the process that was `pid` at `started_at` is still running.
pub fn alive(pid: i32, started_at: i64) -> bool {
    match info(pid) {
        Some(i) => (i.started_at - started_at).abs() <= 1,
        None => false,
    }
}

/// The calling process's ancestors, nearest first, stopping before PID 1.
pub fn ancestors() -> Vec<ProcId> {
    let mut out = Vec::new();
    let mut pid = std::process::id() as i32;
    for _ in 0..64 {
        let Some(i) = info(pid) else { break };
        let parent = i.ppid;
        if parent <= 1 {
            break;
        }
        match proc_id(parent) {
            Some(p) => out.push(p),
            None => break,
        }
        pid = parent;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process() {
        let me = std::process::id() as i32;
        let i = info(me).expect("own process info");
        assert!(i.ppid > 0);
        assert!(alive(me, i.started_at));
        assert!(!alive(me, i.started_at - 10_000));
        assert!(!ancestors().is_empty());
    }
}
