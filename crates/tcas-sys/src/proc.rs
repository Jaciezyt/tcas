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

/// Command-line arguments (`argv`) of `pid`, or empty when unreadable.
#[cfg(target_os = "macos")]
pub fn args(pid: i32) -> Vec<String> {
    if pid <= 0 {
        return Vec::new();
    }
    // SAFETY: KERN_PROCARGS2 writes into `buf` up to `len` bytes.
    unsafe {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut len: libc::size_t = 0;
        if libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len < 4 {
            return Vec::new();
        }
        let mut buf = vec![0u8; len];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        ) != 0
            || len < 4
        {
            return Vec::new();
        }
        buf.truncate(len);
        let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if argc <= 0 {
            return Vec::new();
        }
        let mut pos = 4;
        // Skip executable path and NUL alignment bytes before argv[0].
        while pos < buf.len() && buf[pos] != 0 {
            pos += 1;
        }
        while pos < buf.len() && buf[pos] == 0 {
            pos += 1;
        }
        let mut out = Vec::new();
        for _ in 0..argc {
            if pos >= buf.len() {
                break;
            }
            let end = buf[pos..].iter().position(|&b| b == 0).map(|i| pos + i).unwrap_or(buf.len());
            out.push(String::from_utf8_lossy(&buf[pos..end]).into_owned());
            pos = end + 1;
        }
        out
    }
}

/// Command-line arguments (`argv`) of `pid`, or empty when unreadable.
#[cfg(target_os = "linux")]
pub fn args(pid: i32) -> Vec<String> {
    if pid <= 0 {
        return Vec::new();
    }
    let Ok(mut buf) = std::fs::read(format!("/proc/{pid}/cmdline")) else { return Vec::new() };
    if buf.last() == Some(&0) {
        buf.pop();
    }
    if buf.is_empty() {
        return Vec::new();
    }
    buf.split(|&b| b == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
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
        assert!(!args(me).is_empty());
    }
}
