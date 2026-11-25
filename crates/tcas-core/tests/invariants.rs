//! The invariants of leases.md section 8, as a property test (plan.md
//! section 4): random sequences of acquire, wait, edit, commit, release,
//! session end, resume, turn end and discard, applied to a lease table with
//! the decisions of `tcas-core`. After every step:
//!
//! 1. no two intersecting leases belong to different sessions;
//! 2. a request is granted entirely or not at all;
//! 7. the wait graph has no cycle;
//! 8. a file with uncommitted changes loses its lease only by its holder's
//!    or the user's action, never to another session's request;
//! 9. every file a session changed and has not committed (nor released) has
//!    a file lease of that session.
//!
//! The daemon applies the same decisions with SQLite and git around them
//! (crates/tcasd/src/engine.rs); this checks the rules themselves.

use std::collections::{BTreeMap, BTreeSet};

use tcas_core::acquire::{self, Held, Want};
use tcas_core::lease::{self, HolderFacts, LeaseFacts, LeaseState, Limits, Source};
use tcas_core::relpath;
use tcas_core::wait::{self, Graph};

const LIM: Limits = Limits { explicit_idle_ms: 300, waited_grace_ms: 100 };
const SESSIONS: &[&str] = &["A", "B", "C"];
const FILES: &[&str] = &["a.txt", "src/x.ts", "src/y.ts", "src/auth/p.ts", "src/auth/q.ts", "lib/z.ts"];
const DIRS: &[&str] = &["src", "src/auth", "lib"];

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

#[derive(Clone, Debug)]
struct L {
    key: String,
    dir: bool,
    session: String,
    facts: LeaseFacts,
}

#[derive(Default)]
struct World {
    leases: Vec<L>,
    /// Uncommitted changes: file → the session whose change it is (`None`
    /// once that session released it, or for the user's own changes).
    dirty: BTreeMap<String, Option<String>>,
    ended: BTreeSet<String>,
    idle: BTreeSet<String>,
    /// Waiting session → the paths it waits for, in the order waits began.
    waits: Vec<(String, Vec<(String, bool)>)>,
    now: i64,
}

impl World {
    fn held(&self) -> Vec<Held> {
        self.leases
            .iter()
            .map(|l| Held { root: "r".into(), key: l.key.clone(), is_prefix: l.dir, session: l.session.clone(), facts: l.facts })
            .collect()
    }

    fn holder(&self, i: usize) -> HolderFacts {
        let s = &self.leases[i].session;
        HolderFacts { ended: self.ended.contains(s), turn_ended: self.idle.contains(s), busy: false }
    }

    fn is_dirty(&self, i: usize) -> bool {
        let l = &self.leases[i];
        !l.dir && self.dirty.contains_key(&l.key)
    }

    fn active(&mut self, s: &str) {
        self.idle.remove(s);
        if self.ended.remove(s) {
            for l in self.leases.iter_mut().filter(|l| l.session == s) {
                l.facts.state = LeaseState::Active;
            }
        }
    }

    /// One request; returns whether it was granted. Checks invariants 2 and 8.
    fn acquire(&mut self, s: &str, wants: &[(String, bool)], source: Source) -> bool {
        self.active(s);
        let w: Vec<Want> = wants.iter().map(|(k, d)| Want { root: "r".into(), key: k.clone(), is_dir: *d }).collect();
        let held = self.held();
        let d = acquire::decide(s, &w, &held, |i| self.holder(i), |i| self.is_dirty(i), self.now, &LIM);
        let before = self.leases.len();

        // Invariant 8: nothing dirty gives way to another session.
        for (i, why) in &d.yielded {
            assert!(!self.is_dirty(*i), "a dirty file lease yielded to {s}: {:?} ({why:?})", self.leases[*i]);
        }
        let mut gone: BTreeSet<usize> = d.housekeeping.iter().copied().collect();
        if d.granted {
            gone.extend(d.yielded.iter().map(|(i, _)| *i));
        }
        let mut keep = Vec::new();
        for (i, l) in self.leases.drain(..).enumerate() {
            if !gone.contains(&i) {
                keep.push(l);
            }
        }
        self.leases = keep;
        if !d.granted {
            // Invariant 2: nothing new for the requester.
            assert!(self.leases.len() <= before);
            return false;
        }
        for (k, dir) in wants {
            let exact = self.leases.iter_mut().find(|l| l.session == s && l.key == *k && l.dir == *dir);
            if let Some(l) = exact {
                l.facts.touched_at = self.now;
                if matches!(source, Source::Cli | Source::Grant) {
                    l.facts.committed_at = None;
                    if !l.facts.source.is_explicit() {
                        l.facts.source = source;
                    }
                }
                continue;
            }
            let under_own_dir = self.leases.iter().any(|l| l.session == s && l.dir && lease::covers(&l.key, true, k, *dir));
            if under_own_dir && (*dir || !source.is_implicit()) {
                continue;
            }
            self.leases.push(L {
                key: k.clone(),
                dir: *dir,
                session: s.into(),
                facts: LeaseFacts {
                    is_prefix: *dir,
                    in_repo: true,
                    state: LeaseState::Active,
                    source,
                    acquired_at: self.now,
                    touched_at: self.now,
                    committed_at: None,
                },
            });
        }
        // Invariant 2: everything asked for is now covered by the requester.
        for (k, dir) in wants {
            assert!(
                self.leases.iter().any(|l| l.session == s && lease::covers(&l.key, l.dir, k, *dir)),
                "{s} was granted {wants:?} but does not hold {k}"
            );
        }
        true
    }

    fn graph(&self) -> Graph {
        let mut g = Graph::new();
        for (s, paths) in &self.waits {
            for (k, dir) in paths {
                for l in &self.leases {
                    if l.session != *s && relpath::intersects(&l.key, l.dir, k, *dir) {
                        g.entry(s.clone()).or_default().insert(l.session.clone());
                    }
                }
            }
        }
        g
    }

    /// Re-evaluates waits: grants what can be granted, ends the latest wait
    /// on any cycle (leases.md section 7).
    fn evaluate_waits(&mut self) {
        let waits = std::mem::take(&mut self.waits);
        for (s, paths) in waits {
            if self.ended.contains(&s) {
                continue;
            }
            let w: Vec<Want> = paths.iter().map(|(k, d)| Want { root: "r".into(), key: k.clone(), is_dir: *d }).collect();
            let held = self.held();
            let d = acquire::decide(&s, &w, &held, |i| self.holder(i), |i| self.is_dirty(i), self.now, &LIM);
            if d.granted {
                assert!(self.acquire(&s, &paths, Source::Wait));
            } else {
                self.waits.push((s, paths));
            }
        }
        while let Some(cycle) = wait::any_cycle(&self.graph()) {
            let victim = self.waits.iter().rposition(|(s, _)| cycle.contains(s)).expect("a wait on the cycle");
            self.waits.remove(victim);
        }
    }

    fn check(&self, step: usize, op: &str) {
        // Invariant 1.
        for (i, a) in self.leases.iter().enumerate() {
            for b in &self.leases[i + 1..] {
                assert!(
                    a.session == b.session || !relpath::intersects(&a.key, a.dir, &b.key, b.dir),
                    "step {step} ({op}): {a:?} and {b:?} intersect"
                );
            }
        }
        // Invariant 7.
        assert!(wait::any_cycle(&self.graph()).is_none(), "step {step} ({op}): the wait graph has a cycle");
        // Invariant 9.
        for (f, who) in &self.dirty {
            if let Some(s) = who {
                assert!(
                    self.leases.iter().any(|l| l.session == *s && !l.dir && l.key == *f),
                    "step {step} ({op}): {s} changed {f} but holds no file lease on it: {:?}",
                    self.leases
                );
            }
        }
    }
}

fn run(seed: u64, steps: usize) {
    let mut r = Rng(seed | 1);
    let mut w = World::default();
    for step in 0..steps {
        w.now += r.below(60) as i64;
        let s = r.pick(SESSIONS).to_string();
        let op = match r.below(10) {
            0 | 1 => {
                let n = 1 + r.below(3);
                let wants: Vec<(String, bool)> = (0..n)
                    .map(|_| if r.below(3) == 0 { (r.pick(DIRS).to_string(), true) } else { (r.pick(FILES).to_string(), false) })
                    .collect();
                let source = if r.below(4) == 0 { Source::Grant } else { Source::Cli };
                w.acquire(&s, &wants, source);
                format!("{s} acquire {wants:?}")
            }
            2 | 3 => {
                let f = r.pick(FILES).to_string();
                let source = if r.below(2) == 0 { Source::Edit } else { Source::Bash };
                if w.acquire(&s, &[(f.clone(), false)], source) {
                    w.dirty.insert(f.clone(), Some(s.clone()));
                }
                format!("{s} edit {f}")
            }
            4 => {
                // Commit: the session's changed files are clean afterwards;
                // implicit and waited-for leases go, explicit ones remember it.
                w.active(&s);
                let mine: Vec<String> =
                    w.dirty.iter().filter(|(_, who)| who.as_deref() == Some(s.as_str())).map(|(f, _)| f.clone()).collect();
                for f in &mine {
                    w.dirty.remove(f);
                }
                let now = w.now;
                w.leases.retain_mut(|l| {
                    if l.session != s || !mine.iter().any(|f| relpath::contains(&l.key, f)) {
                        return true;
                    }
                    if !l.dir && (l.facts.source.is_implicit() || l.facts.source == Source::Wait) {
                        return false;
                    }
                    l.facts.committed_at = Some(now);
                    l.facts.touched_at = now;
                    true
                });
                format!("{s} commit")
            }
            5 => {
                // Release one of its leases; a changed file is released only by
                // naming it (daemon-and-cli.md section 5), and is then nobody's.
                w.active(&s);
                let mine: Vec<usize> = (0..w.leases.len()).filter(|&i| w.leases[i].session == s).collect();
                if !mine.is_empty() {
                    let i = mine[r.below(mine.len())];
                    let l = w.leases.remove(i);
                    if !l.dir
                        && let Some(who) = w.dirty.get_mut(&l.key)
                    {
                        *who = None;
                    }
                }
                format!("{s} release")
            }
            6 => {
                // Session end: directories go, clean files go, dirty ones stay
                // as orphans (leases.md section 6).
                w.ended.insert(s.clone());
                w.idle.remove(&s);
                w.waits.retain(|(x, _)| *x != s);
                let dirty = w.dirty.clone();
                w.leases.retain_mut(|l| {
                    if l.session != s {
                        return true;
                    }
                    if !l.dir && dirty.contains_key(&l.key) {
                        l.facts.state = LeaseState::Orphaned;
                        return true;
                    }
                    false
                });
                format!("{s} end")
            }
            7 => {
                w.idle.insert(s.clone());
                format!("{s} stop")
            }
            8 => {
                // The user discards a change: the path is clean again.
                let f = r.pick(FILES).to_string();
                w.dirty.remove(&f);
                format!("user discards {f}")
            }
            _ => {
                // Wait for paths, unless it would close a cycle.
                let wants = vec![(r.pick(FILES).to_string(), false)];
                if !w.ended.contains(&s) && !w.waits.iter().any(|(x, _)| *x == s) {
                    w.active(&s);
                    w.waits.push((s.clone(), wants.clone()));
                    if wait::cycle_through(&w.graph(), &s).is_some() {
                        w.waits.pop();
                    }
                }
                format!("{s} wait {wants:?}")
            }
        };
        w.evaluate_waits();
        w.check(step, &op);
    }
}

#[test]
fn invariants_hold_over_random_operation_sequences() {
    for seed in 1..=300u64 {
        run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 400);
    }
}
