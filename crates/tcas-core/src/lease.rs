//! Lease rules that need no I/O (leases.md sections 2, 4, 5, 6).

use serde::{Deserialize, Serialize};

use crate::relpath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Edit,
    Bash,
    Wait,
    Cli,
    Grant,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Edit => "edit",
            Source::Bash => "bash",
            Source::Wait => "wait",
            Source::Cli => "cli",
            Source::Grant => "grant",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        Some(match s {
            "edit" => Source::Edit,
            "bash" => Source::Bash,
            "wait" => Source::Wait,
            "cli" => Source::Cli,
            "grant" => Source::Grant,
            _ => return None,
        })
    }

    pub fn is_implicit(self) -> bool {
        matches!(self, Source::Edit | Source::Bash)
    }

    pub fn is_explicit(self) -> bool {
        matches!(self, Source::Cli | Source::Grant)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseState {
    Active,
    Orphaned,
}

impl LeaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseState::Active => "active",
            LeaseState::Orphaned => "orphaned",
        }
    }

    pub fn parse(s: &str) -> Option<LeaseState> {
        match s {
            "active" => Some(LeaseState::Active),
            "orphaned" => Some(LeaseState::Orphaned),
            _ => None,
        }
    }
}

/// Whether an existing lease `(l_key, l_dir)` already covers a wanted path.
pub fn covers(l_key: &str, l_dir: bool, w_key: &str, w_dir: bool) -> bool {
    if l_dir {
        relpath::contains(l_key, w_key)
    } else {
        !w_dir && l_key == w_key
    }
}

/// The two time limits of leases.md section 6, in milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub explicit_idle_ms: i64,
    pub waited_grace_ms: i64,
}

/// What the rules need to know about a lease.
#[derive(Debug, Clone, Copy)]
pub struct LeaseFacts {
    pub is_prefix: bool,
    /// The path lies in a git checkout, so "clean" means something.
    pub in_repo: bool,
    pub state: LeaseState,
    pub source: Source,
    pub acquired_at: i64,
    pub touched_at: i64,
    pub committed_at: Option<i64>,
}

/// What the rules need to know about the holder.
#[derive(Debug, Clone, Copy)]
pub struct HolderFacts {
    pub ended: bool,
    /// The holder's turn has ended (`idle`): `Stop` fired and nothing ran since.
    pub turn_ended: bool,
    /// The holder has an edit call on this path or a Bash window in progress.
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Yield {
    /// Stays with the holder.
    Never,
    /// Yields if the path has no uncommitted changes.
    IfClean,
    /// Yields without looking at the working tree (directory leases and
    /// files outside any checkout).
    Now,
}

/// Why a lease does or does not give way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The holder ended with the file dirty.
    Orphaned,
    /// The holder session has ended.
    HolderEnded,
    /// The holder has a call in progress.
    Busy,
    /// An implicit file lease: it follows the file's uncommitted changes.
    Implicit,
    /// A waited-for lease inside its grace period.
    Grace,
    /// A waited-for file lease past its grace period: it follows the file.
    GraceOver,
    /// Not used for longer than the idle limit.
    Expired,
    /// Committed by the holder, whose turn has since ended.
    Committed,
    /// An explicit lease in use.
    Held,
}

impl Why {
    pub fn as_str(self) -> &'static str {
        match self {
            Why::Orphaned => "orphaned",
            Why::HolderEnded => "holder_ended",
            Why::Busy => "busy",
            Why::Implicit => "implicit",
            Why::Grace => "grace",
            Why::GraceOver => "grace_over",
            Why::Expired => "expired",
            Why::Committed => "committed",
            Why::Held => "held",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub yields: Yield,
    pub why: Why,
    /// When a hold that depends only on time ends, so waits can be
    /// re-evaluated then.
    pub until: Option<i64>,
}

/// Whether a lease held by someone else gives way to a new request at `now`
/// (leases.md section 5 step 4).
pub fn verdict(l: &LeaseFacts, h: &HolderFacts, now: i64, lim: &Limits) -> Verdict {
    let free = if l.is_prefix || !l.in_repo { Yield::Now } else { Yield::IfClean };
    let v = |yields, why, until| Verdict { yields, why, until };

    if l.state == LeaseState::Orphaned {
        return v(free, Why::Orphaned, None);
    }
    if h.ended {
        return v(free, Why::HolderEnded, None);
    }
    if h.busy {
        return v(Yield::Never, Why::Busy, None);
    }

    // Implicit file leases in a checkout follow the file's uncommitted changes.
    if l.in_repo && !l.is_prefix && l.source.is_implicit() {
        return v(Yield::IfClean, Why::Implicit, None);
    }

    // Waited-for file leases are kept for a grace period after the grant. A
    // waited-for directory, or a file outside a checkout, is explicit
    // (leases.md sections 4 and 7).
    if l.source == Source::Wait && l.in_repo && !l.is_prefix {
        let until = l.acquired_at + lim.waited_grace_ms;
        if now < until {
            return v(Yield::Never, Why::Grace, Some(until));
        }
        return v(Yield::IfClean, Why::GraceOver, None);
    }

    // Explicit leases, every directory lease, and files outside a checkout.
    let idle_until = l.touched_at + lim.explicit_idle_ms;
    if now >= idle_until {
        return v(free, Why::Expired, None);
    }
    if l.committed_at.is_some() && h.turn_ended {
        return v(free, Why::Committed, None);
    }
    v(Yield::Never, Why::Held, Some(idle_until))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIM: Limits = Limits { explicit_idle_ms: 1_800_000, waited_grace_ms: 600_000 };

    fn lease(source: Source) -> LeaseFacts {
        LeaseFacts {
            is_prefix: false,
            in_repo: true,
            state: LeaseState::Active,
            source,
            acquired_at: 0,
            touched_at: 0,
            committed_at: None,
        }
    }

    const LIVE: HolderFacts = HolderFacts { ended: false, turn_ended: false, busy: false };
    const IDLE: HolderFacts = HolderFacts { ended: false, turn_ended: true, busy: false };

    fn y(l: &LeaseFacts, h: &HolderFacts, now: i64) -> Yield {
        verdict(l, h, now, &LIM).yields
    }

    #[test]
    fn implicit_yields_if_clean_unless_busy() {
        assert_eq!(y(&lease(Source::Edit), &LIVE, 1), Yield::IfClean);
        let busy = HolderFacts { busy: true, ..LIVE };
        assert_eq!(y(&lease(Source::Edit), &busy, 1), Yield::Never);
    }

    #[test]
    fn explicit_kept_until_idle_limit() {
        let l = lease(Source::Cli);
        let v = verdict(&l, &LIVE, 1000, &LIM);
        assert_eq!((v.yields, v.why, v.until), (Yield::Never, Why::Held, Some(1_800_000)));
        let v = verdict(&l, &LIVE, 1_800_000, &LIM);
        assert_eq!((v.yields, v.why), (Yield::IfClean, Why::Expired));
    }

    #[test]
    fn committed_explicit_yields_once_turn_ended() {
        let l = LeaseFacts { committed_at: Some(5), ..lease(Source::Cli) };
        assert_eq!(y(&l, &LIVE, 10), Yield::Never);
        let v = verdict(&l, &IDLE, 10, &LIM);
        assert_eq!((v.yields, v.why), (Yield::IfClean, Why::Committed));
        let d = LeaseFacts { is_prefix: true, ..l };
        assert_eq!(y(&d, &IDLE, 10), Yield::Now);
        let busy = HolderFacts { busy: true, ..IDLE };
        assert_eq!(y(&l, &busy, 10), Yield::Never);
    }

    #[test]
    fn uncommitted_explicit_kept_when_idle() {
        let l = lease(Source::Cli);
        assert_eq!(y(&l, &IDLE, 10), Yield::Never);
    }

    #[test]
    fn waited_grace_period() {
        let l = lease(Source::Wait);
        let v = verdict(&l, &IDLE, 100, &LIM);
        assert_eq!((v.yields, v.why, v.until), (Yield::Never, Why::Grace, Some(600_000)));
        assert_eq!(y(&l, &LIVE, 600_000), Yield::IfClean);
        let d = LeaseFacts { is_prefix: true, ..l };
        let v = verdict(&d, &LIVE, 600_000, &LIM);
        assert_eq!((v.yields, v.until), (Yield::Never, Some(1_800_000)));
    }

    #[test]
    fn orphaned_and_ended() {
        let l = LeaseFacts { state: LeaseState::Orphaned, ..lease(Source::Cli) };
        assert_eq!(y(&l, &LIVE, 1), Yield::IfClean);
        let gone = HolderFacts { ended: true, ..LIVE };
        assert_eq!(y(&lease(Source::Cli), &gone, 1), Yield::IfClean);
        let d = LeaseFacts { is_prefix: true, ..lease(Source::Cli) };
        assert_eq!(y(&d, &gone, 1), Yield::Now);
    }

    #[test]
    fn outside_checkout_follows_explicit_rule() {
        let l = LeaseFacts { in_repo: false, ..lease(Source::Edit) };
        assert_eq!(y(&l, &LIVE, 1), Yield::Never);
        assert_eq!(y(&l, &LIVE, 1_800_000), Yield::Now);
    }

    #[test]
    fn covering() {
        assert!(covers("src", true, "src/a.ts", false));
        assert!(covers("src", true, "src/auth", true));
        assert!(!covers("src/a.ts", false, "src", true));
        assert!(covers("a.txt", false, "a.txt", false));
        assert!(!covers("src/auth", true, "src", true));
    }

    #[test]
    fn waited_directory_is_explicit() {
        // No grace period: a waited-for directory follows the explicit rules
        // (leases.md section 7), so it yields once committed under and idle.
        let d = LeaseFacts { is_prefix: true, committed_at: Some(1), ..lease(Source::Wait) };
        assert_eq!(verdict(&d, &IDLE, 2, &LIM).why, Why::Committed);
        assert_eq!(y(&d, &LIVE, 2), Yield::Never);
        assert_eq!(y(&d, &LIVE, 1_800_000), Yield::Now);
        let out = LeaseFacts { in_repo: false, ..lease(Source::Wait) };
        assert_eq!(verdict(&out, &LIVE, 2, &LIM).why, Why::Held);
        let f = lease(Source::Wait);
        assert_eq!(verdict(&f, &IDLE, 2, &LIM).why, Why::Grace);
    }
}
