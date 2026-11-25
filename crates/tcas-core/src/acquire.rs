//! The acquire decision of leases.md section 5, on data already loaded.
//! The caller supplies the facts that need I/O (holder state, cleanliness).

use crate::lease::{self, HolderFacts, LeaseFacts, Limits, Why, Yield};
use crate::relpath;

#[derive(Debug, Clone)]
pub struct Want {
    pub root: String,
    pub key: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct Held {
    pub root: String,
    pub key: String,
    pub is_prefix: bool,
    pub session: String,
    pub facts: LeaseFacts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocker {
    pub want: usize,
    pub lease: usize,
    pub why: Why,
    /// The lease would yield, but its file has uncommitted changes.
    pub dirty: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub granted: bool,
    /// Per want: the requester's own lease that already covers it.
    pub covered: Vec<Option<usize>>,
    /// Others' leases that give way; released when the request is granted.
    pub yielded: Vec<(usize, Why)>,
    /// Others' leases left behind by ended sessions and now clean; released
    /// whatever the outcome.
    pub housekeeping: Vec<usize>,
    pub blockers: Vec<Blocker>,
    /// The earliest time a blocker held only by time may give way.
    pub next_change: Option<i64>,
}

fn intersect(w: &Want, h: &Held) -> bool {
    w.root == h.root && relpath::intersects(&h.key, h.is_prefix, &w.key, w.is_dir)
}

/// Indices of other sessions' leases that intersect any wanted path. The
/// caller checks these for cleanliness before calling [`decide`].
pub fn conflicting(session: &str, wants: &[Want], held: &[Held]) -> Vec<usize> {
    (0..held.len())
        .filter(|&i| held[i].session != session && wants.iter().any(|w| intersect(w, &held[i])))
        .collect()
}

/// The requester's own lease covering `w`, preferring an exact match.
pub fn own_cover(session: &str, w: &Want, held: &[Held]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, h) in held.iter().enumerate() {
        if h.session != session || h.root != w.root {
            continue;
        }
        if !lease::covers(&h.key, h.is_prefix, &w.key, w.is_dir) {
            continue;
        }
        let exact = h.key == w.key;
        match best {
            Some(b) if held[b].key == w.key => {}
            _ if exact => best = Some(i),
            None => best = Some(i),
            Some(b) if held[b].key.len() < h.key.len() => best = Some(i),
            _ => {}
        }
    }
    best
}

pub fn decide(
    session: &str,
    wants: &[Want],
    held: &[Held],
    holder: impl Fn(usize) -> HolderFacts,
    dirty: impl Fn(usize) -> bool,
    now: i64,
    lim: &Limits,
) -> Decision {
    let mut d = Decision { covered: vec![None; wants.len()], ..Default::default() };
    // Per lease, once looked at: (verdict, why, dirty, until).
    type Seen = Option<(Yield, Why, bool, Option<i64>)>;
    let mut seen: Vec<Seen> = vec![None; held.len()];

    for (wi, w) in wants.iter().enumerate() {
        d.covered[wi] = own_cover(session, w, held);
        for (li, h) in held.iter().enumerate() {
            if h.session == session || !intersect(w, h) {
                continue;
            }
            let (y, why, is_dirty, until) = *seen[li].get_or_insert_with(|| {
                let v = lease::verdict(&h.facts, &holder(li), now, lim);
                let is_dirty = v.yields == Yield::IfClean && dirty(li);
                (v.yields, v.why, is_dirty, v.until)
            });
            let gives_way = match y {
                Yield::Never => false,
                Yield::Now => true,
                Yield::IfClean => !is_dirty,
            };
            if gives_way {
                if !d.yielded.iter().any(|(i, _)| *i == li) {
                    d.yielded.push((li, why));
                    if why == Why::Orphaned {
                        d.housekeeping.push(li);
                    }
                }
            } else {
                d.blockers.push(Blocker { want: wi, lease: li, why, dirty: is_dirty });
                if let Some(t) = until {
                    d.next_change = Some(d.next_change.map_or(t, |n: i64| n.min(t)));
                }
            }
        }
    }
    d.granted = d.blockers.is_empty();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::{LeaseState, Source};

    const LIM: Limits = Limits { explicit_idle_ms: 1000, waited_grace_ms: 500 };

    fn held(session: &str, key: &str, dir: bool, source: Source) -> Held {
        Held {
            root: "r".into(),
            key: key.into(),
            is_prefix: dir,
            session: session.into(),
            facts: LeaseFacts {
                is_prefix: dir,
                in_repo: true,
                state: LeaseState::Active,
                source,
                acquired_at: 0,
                touched_at: 0,
                committed_at: None,
            },
        }
    }

    fn want(key: &str, dir: bool) -> Want {
        Want { root: "r".into(), key: key.into(), is_dir: dir }
    }

    const LIVE: HolderFacts = HolderFacts { ended: false, turn_ended: false, busy: false };

    #[test]
    fn dirty_implicit_blocks() {
        let h = vec![held("B", "a.txt", false, Source::Edit)];
        let d = decide("A", &[want("a.txt", false)], &h, |_| LIVE, |_| true, 10, &LIM);
        assert!(!d.granted);
        assert!(d.blockers[0].dirty);
        let d = decide("A", &[want("a.txt", false)], &h, |_| LIVE, |_| false, 10, &LIM);
        assert!(d.granted);
        assert_eq!(d.yielded, vec![(0, Why::Implicit)]);
    }

    #[test]
    fn all_or_nothing_over_dirs() {
        let h = vec![
            held("B", "src/auth/b.ts", false, Source::Edit),
            held("B", "lib", true, Source::Cli),
        ];
        let d = decide("A", &[want("src/auth", true), want("x.ts", false)], &h, |_| LIVE, |_| true, 10, &LIM);
        assert!(!d.granted);
        assert_eq!(d.blockers.len(), 1);
        let d = decide("A", &[want("lib/q.ts", false)], &h, |_| LIVE, |_| false, 10, &LIM);
        assert_eq!(d.blockers[0].why, Why::Held);
        assert_eq!(d.next_change, Some(1000));
    }

    #[test]
    fn own_leases_cover() {
        let h = vec![held("A", "src", true, Source::Cli), held("A", "src/a.ts", false, Source::Edit)];
        let d = decide("A", &[want("src/a.ts", false), want("src/b.ts", false)], &h, |_| LIVE, |_| true, 10, &LIM);
        assert!(d.granted);
        assert_eq!(d.covered, vec![Some(1), Some(0)]);
        assert!(conflicting("A", &[want("src", true)], &h).is_empty());
    }

    #[test]
    fn expired_dir_yields_now() {
        let h = vec![held("B", "src", true, Source::Cli)];
        let d = decide("A", &[want("src/x.ts", false)], &h, |_| LIVE, |_| panic!("no git for dirs"), 1000, &LIM);
        assert!(d.granted);
        assert_eq!(d.yielded, vec![(0, Why::Expired)]);
    }
}
