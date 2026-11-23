//! The wait graph and deadlock detection (leases.md section 7).

use std::collections::{BTreeMap, BTreeSet};

/// Edges from a waiting session to the sessions holding what it waits for.
pub type Graph = BTreeMap<String, BTreeSet<String>>;

/// A cycle through `start`, as the list of sessions after `start` in order,
/// ending with `start` itself. `None` when `start` is on no cycle.
pub fn cycle_through(g: &Graph, start: &str) -> Option<Vec<String>> {
    fn dfs(g: &Graph, node: &str, start: &str, seen: &mut BTreeSet<String>, path: &mut Vec<String>) -> bool {
        let Some(next) = g.get(node) else { return false };
        for n in next {
            if n == start {
                path.push(n.clone());
                return true;
            }
            if seen.insert(n.clone()) {
                path.push(n.clone());
                if dfs(g, n, start, seen, path) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }
    let mut seen = BTreeSet::new();
    let mut path = Vec::new();
    if dfs(g, start, start, &mut seen, &mut path) { Some(path) } else { None }
}

/// Some session lying on a cycle, if the graph has one.
pub fn any_cycle(g: &Graph) -> Option<Vec<String>> {
    for s in g.keys() {
        if let Some(mut c) = cycle_through(g, s) {
            c.insert(0, s.clone());
            c.pop();
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(edges: &[(&str, &str)]) -> Graph {
        let mut g = Graph::new();
        for (a, b) in edges {
            g.entry(a.to_string()).or_default().insert(b.to_string());
        }
        g
    }

    #[test]
    fn two_sessions() {
        let graph = g(&[("A", "B"), ("B", "A")]);
        assert_eq!(cycle_through(&graph, "B").unwrap(), vec!["A", "B"]);
    }

    #[test]
    fn no_cycle() {
        let graph = g(&[("A", "B"), ("B", "C")]);
        assert!(cycle_through(&graph, "A").is_none());
        assert!(any_cycle(&graph).is_none());
    }

    #[test]
    fn three_sessions() {
        let graph = g(&[("A", "B"), ("B", "C"), ("C", "A"), ("D", "A")]);
        assert_eq!(cycle_through(&graph, "A").unwrap(), vec!["B", "C", "A"]);
        assert!(cycle_through(&graph, "D").is_none());
        let c = any_cycle(&graph).unwrap();
        assert_eq!(c.len(), 3);
    }
}
