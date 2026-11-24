//! Human-readable output for `tcas stats` (daemon-and-cli.md section 5, "统计").

use tcas_core::human::{self, short_session};
use tcas_core::proto::*;
use tcas_sys::clock;

use crate::out::Out;

fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        "0%".into()
    } else {
        format!("{}%", (part * 100 + whole / 2) / whole)
    }
}

fn fmt_durations(d: &DurationStats) -> String {
    if d.count == 0 {
        return "none".into();
    }
    if d.count == 1 {
        return human::duration(d.max_ms);
    }
    format!(
        "avg {}, median {}, max {} (total {})",
        human::duration(d.avg_ms),
        human::duration(d.median_ms),
        human::duration(d.max_ms),
        human::duration(d.total_ms),
    )
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { format!("{s}  ") } else { format!("{s}{}", " ".repeat(w - n)) }
}

pub fn print(_o: &Out, s: &StatsResult) -> i32 {
    let repo_part = s.repo.as_ref().map(|r| format!("   repo {r}")).unwrap_or_default();
    let window_part = match (s.since, s.first_event_at) {
        (Some(from), _) => format!("{} .. {}", clock::stamp(from), clock::stamp(s.until)),
        (None, Some(first)) => format!("{} .. {}", clock::stamp(first), clock::stamp(s.until)),
        (None, None) => format!("up to {}", clock::stamp(s.until)),
    };
    println!(
        "workspace  \"{}\"{repo_part}   window {window_part}   {}, {}",
        s.workspace,
        human::count(s.events, "event", "events"),
        human::count(s.sessions, "session", "sessions"),
    );

    println!();
    println!("contention & waits:");
    if s.denials.total == 0 {
        println!("  denials             0");
    } else {
        let vias: Vec<String> = s.denials.by_via.iter().map(|(k, v)| format!("{k} {v}")).collect();
        println!(
            "  denials             {} ({}) — waited {}, did not wait {}",
            s.denials.total,
            vias.join(", "),
            s.denials.waited_after,
            s.denials.not_waited_after,
        );
        let w = s.denials.by_path.iter().take(15).map(|p| p.path.chars().count() + 2).max().unwrap_or(12).max(16);
        for p in s.denials.by_path.iter().take(15) {
            println!("    {}{}", pad(&p.path, w), human::count(p.count, "denial", "denials"));
        }
        if s.denials.by_path.len() > 15 {
            println!("    … and {} more paths", s.denials.by_path.len() - 15);
        }
    }

    if s.waits.total == 0 {
        println!("  waits               0");
    } else {
        let mut parts = vec![
            format!("granted {}", s.waits.granted),
            format!("timeout {}", s.waits.timeout),
            format!("deadlock {}", s.waits.deadlock),
        ];
        if s.waits.cancelled > 0 {
            parts.push(format!("cancelled {}", s.waits.cancelled));
        }
        if s.waits.active > 0 {
            parts.push(format!("active {}", s.waits.active));
        }
        println!("  waits               {} ({})", s.waits.total, parts.join(", "));
        if s.waits.granted > 0 {
            println!("    wait duration     {}", fmt_durations(&s.waits.granted_durations));
        }
    }

    if s.orphaned.total == 0 {
        println!("  orphaned leases     0");
    } else {
        println!(
            "  orphaned leases     {} (current {}) — {}",
            s.orphaned.total,
            s.orphaned.current,
            fmt_durations(&s.orphaned.durations),
        );
    }
    if s.stalled.total == 0 {
        println!("  stalled leases      0");
    } else {
        println!(
            "  stalled leases      {} (current {}) — {}",
            s.stalled.total,
            s.stalled.current,
            fmt_durations(&s.stalled.durations),
        );
    }

    println!();
    println!("upfront leases & scope:");
    if s.files.total == 0 {
        println!("  modified files      0");
    } else {
        println!(
            "  modified files      {} — explicit {} ({}), implicit {}, waited {}",
            s.files.total,
            s.files.explicit,
            pct(s.files.explicit, s.files.total),
            s.files.implicit,
            s.files.waited,
        );
        for f in &s.files_by_session {
            let who = match &f.title {
                Some(t) => format!("{} ({})", short_session(&f.session), t.chars().take(24).collect::<String>()),
                None => short_session(&f.session),
            };
            println!(
                "    {}{} ({} explicit, {} implicit, {} waited)",
                pad(&who, 30),
                human::count(f.total, "file", "files"),
                f.explicit,
                f.implicit,
                f.waited,
            );
        }
    }

    if s.directory_leases.is_empty() {
        println!("  directory leases    0");
    } else {
        println!("  directory leases    {}", s.directory_leases.len());
        let wp = s.directory_leases.iter().map(|d| d.path.chars().count() + 2).max().unwrap_or(14).max(16);
        for d in &s.directory_leases {
            let tracked = match d.tracked_files {
                Some(n) => format!("{n} tracked"),
                None => "? tracked".into(),
            };
            let ratio = match d.tracked_files {
                Some(n) if n > 0 => format!(" ({})", pct(d.modified_files, n)),
                _ => String::new(),
            };
            println!(
                "    {}{}{}{}, {} modified{ratio}",
                pad(&d.path, wp),
                pad(&short_session(&d.session), 16),
                pad(&d.source, 7),
                tracked,
                d.modified_files,
            );
        }
    }

    println!(
        "  acquire reminders   {} suggested, {} acquired afterwards",
        s.suggestions.suggested, s.suggestions.acquired_after,
    );
    if s.upfront_acquires.total == 0 {
        println!("  upfront --brief     0 upfront requests");
    } else {
        println!(
            "  upfront --brief     {} / {} ({})",
            s.upfront_acquires.with_brief,
            s.upfront_acquires.total,
            pct(s.upfront_acquires.with_brief, s.upfront_acquires.total),
        );
    }
    println!(
        "  expired yields      {} (explicit idle {}, waited grace {}); holder re-requested {}",
        s.yields.expired.total,
        s.yields.expired.explicit_idle,
        s.yields.expired.waited_grace,
        s.yields.expired.reacquired,
    );
    println!(
        "  post-turn yields    {} committed + turn ended; holder re-requested next turn {}",
        s.yields.committed_idle.total, s.yields.committed_idle.reacquired_next_turn,
    );

    println!();
    println!("bash, guard & git hooks:");
    println!(
        "  bash reconcile      {} overreach, {} unattributed",
        s.bash.overreach, s.bash.unattributed,
    );
    if s.guard_blocked.total == 0 {
        println!("  command guard       0 refused");
    } else {
        let rules: Vec<String> = s.guard_blocked.by_rule.iter().map(|r| format!("{} {}", r.rule, r.count)).collect();
        println!(
            "  command guard       {} refused (dynamic/unparsed: {}) — {}",
            s.guard_blocked.total,
            s.guard_blocked.dynamic,
            rules.join(", "),
        );
    }
    if s.git_blocked.total == 0 {
        println!("  git hooks           0 refused");
    } else {
        let hooks: Vec<String> = s
            .git_blocked
            .by_hook
            .iter()
            .map(|h| {
                if h.missed_by_guard > 0 {
                    format!("{} {} ({} missed by guard)", h.hook, h.count, h.missed_by_guard)
                } else {
                    format!("{} {}", h.hook, h.count)
                }
            })
            .collect();
        println!(
            "  git hooks           {} refused (missed by guard: {}, user: {}) — {}",
            s.git_blocked.total,
            s.git_blocked.missed_by_guard,
            s.git_blocked.by_user,
            hooks.join(", "),
        );
    }
    if s.git_overrides.total == 0 {
        println!("  TCAS_ALLOW=1        0 overrides");
    } else {
        let hooks: Vec<String> = s.git_overrides.by_hook.iter().map(|h| format!("{} {}", h.hook, h.count)).collect();
        println!("  TCAS_ALLOW=1        {} overrides ({})", s.git_overrides.total, hooks.join(", "));
    }
    0
}
