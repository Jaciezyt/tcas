//! `~/.tcas/workspaces.json`: the roots and exempt directories, for `tcas hook`
//! to decide without connecting whether a path concerns tcasd at all.

use serde::{Deserialize, Serialize};
use tcas_core::exempt::ExemptDir;

use crate::daemon::{Daemon, Global, log};
use crate::world::World;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootEntry {
    pub name: String,
    pub path: String,
    pub case_insensitive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsEntry {
    pub id: String,
    pub name: String,
    pub roots: Vec<RootEntry>,
    pub exempt: Vec<ExemptDir>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsJson {
    pub version: u32,
    pub workspaces: Vec<WsEntry>,
    pub exempt: Vec<ExemptDir>,
}

pub fn build(world: &World, g: &Global) -> WsJson {
    WsJson {
        version: 1,
        workspaces: world
            .wss
            .iter()
            .map(|w| WsEntry {
                id: w.row.id.clone(),
                name: w.row.name.clone(),
                roots: w
                    .roots
                    .iter()
                    .map(|r| RootEntry { name: r.name.clone(), path: r.path.clone(), case_insensitive: r.ci })
                    .collect(),
                exempt: w.exempt.clone(),
            })
            .collect(),
        exempt: g.exempt.clone(),
    }
}

pub fn write(d: &Daemon, world: &World, g: &Global) {
    let path = d.home.workspaces_json();
    let tmp = path.with_extension("json.tmp");
    let text = match serde_json::to_string_pretty(&build(world, g)) {
        Ok(t) => t,
        Err(e) => return log(format!("workspaces.json: {e}")),
    };
    if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
        log(format!("workspaces.json: {e}"));
    }
}
