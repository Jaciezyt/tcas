//! Requests and responses between `tcas` and tcasd (daemon-and-cli.md section 4).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcId {
    pub pid: i32,
    pub started_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestors: Vec<ProcId>,
    #[serde(default)]
    pub args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(result: impl Serialize) -> Response {
        Response { ok: true, result: Some(serde_json::to_value(result).unwrap_or(Value::Null)), error: None }
    }

    pub fn err(code: &str, message: impl Into<String>) -> Response {
        Response {
            ok: false,
            result: None,
            error: Some(ErrorBody { code: code.into(), message: message.into(), detail: None }),
        }
    }
}

/// Error codes and the exit codes they map to (daemon-and-cli.md section 5).
pub mod code {
    pub const BAD_REQUEST: &str = "bad_request";
    pub const VERSION_MISMATCH: &str = "version_mismatch";
    pub const NOT_IN_WORKSPACE: &str = "not_in_workspace";
    pub const NOT_FOUND: &str = "not_found";
    pub const REFUSED: &str = "refused";
    /// Only the user may do this, or it is beyond what an agent may do.
    pub const USER_ONLY: &str = "user_only";
    /// The caller is the user, and the command is for sessions.
    pub const SESSION_ONLY: &str = "session_only";
    pub const INTERNAL: &str = "internal";

    pub fn exit_code(code: &str) -> i32 {
        match code {
            USER_ONLY => 6,
            _ => 1,
        }
    }
}

/// A path inside a workspace root, as shown to people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathRef {
    pub root: String,
    /// Relative to the root, "/"-separated; "" for the root itself.
    pub path: String,
    pub is_dir: bool,
    /// `src/a.ts`, `web/src/`, or `app-backend/src/a.ts` when there are several roots.
    pub display: String,
    pub abs: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnmanagedReason {
    Exempt { dir: String, written: String, source: String },
    GitExcluded { source: String, line: u32, pattern: String },
    GitDir,
    OutsideWorkspace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unmanaged {
    pub input: String,
    pub abs: String,
    pub is_dir: bool,
    pub reason: UnmanagedReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseView {
    pub path: PathRef,
    pub session: String,
    pub state: String,
    pub source: String,
    /// The lease's own brief, or else the session's latest one.
    #[serde(default)]
    pub brief: Option<String>,
    /// `brief` came from the session, not from this lease.
    #[serde(default)]
    pub brief_is_session: bool,
    pub acquired_at: i64,
    pub touched_at: i64,
    #[serde(default)]
    pub committed_at: Option<i64>,
    /// Uncommitted changes; `None` for directories and paths outside a checkout.
    #[serde(default)]
    pub dirty: Option<bool>,
    #[serde(default)]
    pub stalled: bool,
    /// The holder session's state.
    #[serde(default)]
    pub holder_state: String,
    #[serde(default)]
    pub holder_idle_ms: i64,
    /// Only for the user.
    #[serde(default)]
    pub holder_title: Option<String>,
    /// For directory leases.
    #[serde(default)]
    pub files: Option<usize>,
    /// Gives way to the next request: `expired`, `committed`, or empty.
    #[serde(default)]
    pub yields: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Granted {
    pub path: PathRef,
    /// Already held; only its use time was updated.
    pub renewed: bool,
    #[serde(default)]
    pub files: Option<usize>,
    /// The file has uncommitted changes that this session did not make.
    #[serde(default)]
    pub foreign_changes: bool,
    /// The last commit touching it changed while the session waited.
    #[serde(default)]
    pub changed_in: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockerView {
    pub want: PathRef,
    pub lease: LeaseView,
    /// `uncommitted`, `held`, `grace`, `busy`, `orphaned`.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleStep {
    pub session: String,
    /// What this session holds that the previous one waits for.
    pub holds: PathRef,
    /// What this session waits for, held by the next one.
    pub waits_for: PathRef,
    /// The next session in the cycle ("" for the requester).
    pub next: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooBig {
    pub path: PathRef,
    pub files: usize,
    pub limit: u64,
    pub children: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Granted,
    Denied,
    Refused,
    Timeout,
    Deadlock,
    Cancelled,
}

impl Outcome {
    pub fn exit_code(self) -> i32 {
        match self {
            Outcome::Granted => 0,
            Outcome::Denied => 3,
            Outcome::Refused | Outcome::Cancelled => 1,
            Outcome::Timeout => 4,
            Outcome::Deadlock => 5,
        }
    }
}

/// Result of `lease.acquire`, `lease.wait` and `lease.grant`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcquireResult {
    pub outcome: Outcome,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub granted: Vec<Granted>,
    #[serde(default)]
    pub unmanaged: Vec<Unmanaged>,
    #[serde(default)]
    pub blockers: Vec<BlockerView>,
    /// All the managed paths asked for, for the wait command.
    #[serde(default)]
    pub wanted: Vec<PathRef>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub too_big: Option<TooBig>,
    #[serde(default)]
    pub cycle: Vec<CycleStep>,
    #[serde(default)]
    pub waited_ms: Option<i64>,
    /// Granted without a brief, and the session has none either.
    #[serde(default)]
    pub no_brief: bool,
    #[serde(default)]
    pub brief: Option<String>,
}

impl AcquireResult {
    pub fn new(outcome: Outcome) -> AcquireResult {
        AcquireResult {
            outcome,
            session: String::new(),
            granted: vec![],
            unmanaged: vec![],
            blockers: vec![],
            wanted: vec![],
            refusal: None,
            too_big: None,
            cycle: vec![],
            waited_ms: None,
            no_brief: false,
            brief: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootView {
    pub name: String,
    pub path: String,
    pub case_insensitive: bool,
    pub added_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoView {
    pub root: String,
    pub dir: String,
    pub display: String,
    pub kind: String,
    pub branch: Option<String>,
    pub abs: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub mode: String,
    pub opened_by: String,
    pub created_at: i64,
    pub roots: Vec<RootView>,
    pub repos: Vec<RepoView>,
    #[serde(default)]
    pub sessions: usize,
    #[serde(default)]
    pub leases: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutsideWorktree {
    pub path: String,
    pub branch: Option<String>,
}

/// Result of `workspace.open` and `workspace.add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenResult {
    pub workspace: WorkspaceView,
    /// Roots added by this request.
    pub added: Vec<String>,
    pub already_open: bool,
    #[serde(default)]
    pub outside_worktrees: Vec<OutsideWorktree>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub harness: String,
    pub state: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    pub started_at: i64,
    pub last_seen_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    pub leases: usize,
    #[serde(default)]
    pub stalled: usize,
    #[serde(default)]
    pub orphaned: usize,
    /// Paths this session is waiting for.
    #[serde(default)]
    pub waiting_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub workspace: WorkspaceView,
    pub viewer: Option<String>,
    pub sessions: Vec<SessionView>,
    pub leases: Vec<LeaseView>,
    /// Modified files without a lease.
    pub unattributed: Vec<PathRef>,
    /// Orphaned leases released because their files were clean.
    #[serde(default)]
    pub released: Vec<PathRef>,
    /// Checkouts outside every workspace that sessions here edited.
    #[serde(default)]
    pub outside: Vec<OutsideEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoEntry {
    pub input: String,
    #[serde(default)]
    pub path: Option<PathRef>,
    #[serde(default)]
    pub unmanaged: Option<Unmanaged>,
    pub leases: Vec<LeaseView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseResult {
    pub released: Vec<PathRef>,
    /// Dirty files under a released directory, still held.
    pub kept: Vec<PathRef>,
    /// Dirty files released on request, with a warning.
    pub released_dirty: Vec<PathRef>,
    pub not_held: Vec<String>,
    #[serde(default)]
    pub unmanaged: Vec<Unmanaged>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventView {
    pub seq: i64,
    pub at: i64,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: i32,
    pub version: String,
    pub started_at: i64,
    pub home: String,
    pub workspaces: usize,
    pub sessions: usize,
    pub waits: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExemptView {
    pub path: String,
    pub written: String,
    pub source: String,
}

/// A checkout outside every workspace that sessions in a workspace edited
/// (leases.md section 3, "改到了 workspace 外面").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutsideEdit {
    pub path: String,
    pub edits: u64,
    pub sessions: Vec<String>,
    pub last_at: i64,
}

// ---------------------------------------------------------------------------
// The `hook` operation (daemon-and-cli.md section 4)

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookTool {
    /// `edit`, `bash`, `read`, `other`.
    pub kind: String,
    pub call_id: String,
    /// The harness's own name for the tool, for messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// edit, read: absolute paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub background: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_id: Option<String>,
    #[serde(default)]
    pub failed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookRequest {
    pub harness: String,
    /// `session_start`, `prompt`, `pre_tool`, `post_tool`, `stop`, `agent_stop`, `session_end`.
    pub event: String,
    /// The harness's own session ID; tcas's is `<harness>:<session>`.
    pub session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub cwd: String,
    /// The harness process.
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub pid_started_at: i64,
    /// session_start: `startup`, `resume`, `clear`, `compact`. session_end: the harness's reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<HookTool>,
    /// stop: background commands still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_running: Option<Vec<String>>,
}

impl HookRequest {
    pub fn session_id(&self) -> String {
        format!("{}:{}", self.harness, self.session)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookResponse {
    /// `deny` or `allow`; `allow` only for tcas commands (harness.md section 5.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// pre_tool: run this call in the background (tcas lease wait).
    #[serde(default)]
    pub background: bool,
    /// The wait command for a denial; the adapter adds how to run it asynchronously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_command: Option<String>,
    /// The wait command covers more than one path.
    #[serde(default)]
    pub wait_many: bool,
    /// The path stays leased until the user acts (orphaned or stalled).
    #[serde(default)]
    pub needs_user: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Text for the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// session_start: variables for the session's environment.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
}
