# Hook probe results

Reproduce: `tests/probe/run.sh` (headless, then interactive), or `python3 tests/probe/run.py` / `python3 tests/probe/interactive.py`.
Needs `claude` and Python 3 (stdlib only). Everything runs with `--model haiku` in temp repos under `/tmp/tcas-probe`; `~/.claude` settings are not modified (claude itself records trust for the temp dir in `~/.claude.json`, and writes its own transcripts/session-env).
Date: 2026-11-23. Claude Code 2.1.327, macOS. Harness model shown in hook inputs: `claude-sonnet-5-5` (the `haiku` alias is remapped in this environment).

Row semantics: PASS/FAIL compare observed against the expectation in column 2; INFO rows are plain measurements. A few rows are findings whose "expected" encodes the finding (e.g. failed Edit emits no hook event).

## Headless (`claude -p`) table

| conclusion | expected | observed | result |
|---|---|---|---|
| hooks fire in headless mode | all of PostToolUse,PostToolUseFailure,PreToolUse,SessionEnd,SessionStart,Stop,UserPromptSubmit | all present | PASS |
| PermissionRequest observed (info) | - | 0 events | INFO |
| PreToolUse JSON deny blocks edit | deny-1.txt absent | deny-1.txt absent | PASS |
| allow lets edit through (control) | ok-1.txt present | present | PASS |
| denied call has no Post event | 0 Post events for deny-1.txt | 0 | PASS |
| plugin bin/ on Bash PATH | probe-anc ran | ['amp', 'bashc', 'direct', 'envu'] | PASS |
| CLAUDE_ENV_FILE only in SessionStart | set in SessionStart, empty elsewhere | SessionStart=True; other events with value: 0 | PASS |
| CLAUDE_ENV_FILE var reaches Bash | TCAS_SESSION=probe-<sid> | probe-0c1ee3ff-60b9-46a0-9d90-2f8165246605 | PASS |
| CLAUDE_CODE_SESSION_ID == session_id | equal | 0c1ee3ff-60b9-46a0-9d90-2f8165246605 | PASS |
| all hooks share one ppid (harness) | 1 distinct ppid | [86778] | PASS |
| SessionEnd fires at end of -p turn | 1 event, reason other | ['other'] | PASS |
| Bash ancestors include harness pid (direct) | harness pid 86778 in chain | [87192, 86778, 86769, 86768, 55365, 55331] | PASS |
| Bash ancestors include harness pid (env -u) | harness pid 86778 in chain | [87206, 86778, 86769, 86768, 55365, 55331] | PASS |
| Bash ancestors include harness pid (bash -c) | harness pid 86778 in chain | [87220, 86778, 86769, 86768, 55365, 55331] | PASS |
| Bash ancestors include harness pid (background '&') | harness pid 86778 in chain | [87234, 86778, 86769, 86768, 55365, 55331] | PASS |
| env -u removes both vars (so ancestors needed) | both None | (None, None) | PASS |
| failing command -> PostToolUseFailure | event with error | Exit code 1 probe-fail: failing on purpose | PASS |
| failing command has no PostToolUse | 0 | 0 | PASS |
| Read input carries file_path | file_path | /private/tmp/tcas-probe/basic/repo/src/a.txt | PASS |
| hook paths are symlink-resolved | /private/tmp/... | /private/tmp/tcas-probe/basic/repo/src/a.txt | PASS |
| resume keeps session id | same session_id, SessionStart source=resume | [('resume', True)] | PASS |
| resume has a new harness pid | different ppid | ([86778], 87337) | PASS |
| TCAS_SESSION present after resume | probe-<sid> | probe-0c1ee3ff-60b9-46a0-9d90-2f8165246605 | PASS |
| SessionEnd per -p turn | 2 total (one per turn) | 2 | PASS |
| background Bash PostToolUse fires early w/ backgroundTaskId | post before cmd end, backgroundTaskId set | backgroundTaskId=bkuig012i, post 5.0s before cmd end | PASS |
| Stop lists background_tasks while running | non-empty on first Stop | [1, 0] | PASS |
| bg completion wakes session (explicit bg) | UserPromptSubmit <task-notification> after Stop | 1 notification(s) after Stop | PASS |
| task-notification content (info) | status/exit code/output path, no output | <task-notification> <task-id>bkuig012i</task-id> <tool-use-id>toolu_01AMFDWiSCAGujgGj7EB1fzR</tool-use-id> <output-file>/private/tmp/claude-501/-private-tmp-tcas-probe-bg-repo/6512be16-f08b-4397-bc87- | INFO |
| additionalContext from wakeup reaches model (explicit bg) | model quotes PROBE-CTX-UserPromptSubmit | PROBE-CTX-SessionStart PROBE-CTX-UserPromptSubmit PROBE-CTX-PostToolUse | PASS |
| bg command env has TCAS_SESSION | probe-<sid> | probe-6512be16-f08b-4397-bc87-9d8249596ca6 | PASS |
| SessionEnd only after all bg commands ended | SessionEnd ts > bg end ts | 2.5 | PASS |
| updatedInput rewrites fg Bash to background | PreToolUse input has no run_in_background; PostToolUse early with backgroundTaskId | pre.run_in_background=None, backgroundTaskId=byfryx5z0, post 5.0s before cmd end | PASS |
| bg completion wakes session (rewritten) | UserPromptSubmit <task-notification> after Stop | 1 notification(s) after Stop | PASS |
| task-notification content (info) | status/exit code/output path, no output | <task-notification> <task-id>byfryx5z0</task-id> <tool-use-id>toolu_01HmisFtjcgm9nwHDRvRhF2N</tool-use-id> <output-file>/private/tmp/claude-501/-private-tmp-tcas-probe-rewrite-repo/c43a56a2-4fc5-4bb7- | INFO |
| additionalContext from wakeup reaches model (rewritten) | model quotes PROBE-CTX-UserPromptSubmit | PROBE-CTX-SessionStart PROBE-CTX-UserPromptSubmit PROBE-CTX-PostToolUse | PASS |
| bg command env has TCAS_SESSION | probe-<sid> | probe-c43a56a2-4fc5-4bb7-bd73-75ebbe451851 | PASS |
| onFailure block: hook exit code 1 | file not created | not created | PASS |
| onFailure block: hook exit code 2 + stderr | file not created | not created | PASS |
| onFailure block: hook invalid JSON on stdout | file not created | not created | PASS |
| onFailure block: hook timeout (5s) | file not created | not created | PASS |
| control: unaffected write | fine.txt created | True | PASS |
| onFailure block: hook program missing | file not created; reason mentions onFailure | not created; reply: The Write tool didn't create the file. A PreToolUse hook blocked the call and returned this error:  ``` PreToolUse:Write | PASS |
| Grep input carries path; result filenames | tool_input.path + tool_response.filenames | ('src', ['src/a.txt']) | PASS |
| failed Edit (old_string not found): hook events | model got the error; record events (finding: none) | error seen=True; tool hook events=NONE | INFO |
| failed Edit (old_string not found) -> PostToolUseFailure | no PreToolUse/Post* at all (validation fails before hooks) | NONE | PASS |
| NotebookEdit path field name | notebook_path | ['cell_id', 'new_source', 'notebook_path'] | PASS |
| bypassPermissions still runs PreToolUse | PreToolUse logged, permission_mode=bypassPermissions | ['bypassPermissions'] | PASS |
| bypassPermissions: PreToolUse deny still blocks | deny-bp.txt absent | absent | PASS |
| hook allow vs user settings deny rule | deny rule wins: command not run | command did not run | PASS |
| SubagentStart / SubagentStop fire | both present | ['SubagentStart', 'SubagentStop'] | PASS |
| subagent tool hooks: same session_id + agent_id/agent_type | session_id equal, agent_id present | (True, 'ab44d8fee51025d4d', 'general-purpose') | PASS |
| subagent Bash has TCAS_SESSION | probe-<sid> | probe-ea8fa46a-ad9a-4da8-bb08-14ef471bc1d0 | PASS |

## Interactive (real TUI through a pty) table

| conclusion | expected | observed | result |
|---|---|---|---|
| (a) hooks fire in interactive mode | SessionStart,UserPromptSubmit,PreToolUse,PostToolUse,Stop | ['PostToolUse', 'PreToolUse', 'SessionStart', 'Stop', 'UserPromptSubmit'] | PASS |
| (a) SessionStart source in interactive | startup | startup | PASS |
| (a) PermissionRequest fired for allow-listed/hook-approved cmd (info) | - | 0 | INFO |
| (a) CLAUDE_ENV_FILE only in SessionStart | set only in SessionStart | ['SessionStart'] | PASS |
| (a) hook ppid is one harness pid | 1 distinct ppid == pty child pid or its descendant | ppids=[81854], spawned pid=81854 | PASS |
| (e) Bash ancestors include harness pid | hook ppid in chain | ([82270, 81854, 81845, 81843, 55365, 55331], [81854]) | PASS |
| (a) TCAS_SESSION / CLAUDE_CODE_SESSION_ID in Bash | probe-<sid> / <sid> | ('probe-b1d62bb2-b386-47a8-b38a-42943e0a88a3', 'b1d62bb2-b386-47a8-b38a-42943e0a88a3') | PASS |
| (a) hook sees CLAUDE_CODE_SESSION_ID too | == session_id | b1d62bb2-b386-47a8-b38a-42943e0a88a3 | PASS |
| (b) updatedInput fg->bg works interactively | PostToolUse early w/ backgroundTaskId, cmd not finished | backgroundTaskId=b23g4hesq, pre->post 0.04s, pre.run_in_background=None | PASS |
| (c) Stop fired before bg cmd ended | Stop ts < cmd end | ended yet=False | PASS |
| (c) idle session woken by bg completion (no keypress) | UserPromptSubmit <task-notification> after Stop | 1 notification(s); delay after cmd end 0.0 s | PASS |
| (c) task-notification prompt (info) | - | <task-notification> <task-id>b23g4hesq</task-id> <tool-use-id>toolu_01S51xjxzV6jv6YbzA4Etk5M</tool-use-id> <output-file>/private/tmp/claude-501/-private-tmp-tca | INFO |
| (c) additionalContext from wakeup reaches model | screen shows PROBE-CTX-UserPromptSubmit in model reply | on screen: True | PASS |
| (c) hook ctx on wakeup UserPromptSubmit event | UserPromptSubmit logged with TCAS_SESSION env intact | True | PASS |
| (f) /clear fires SessionStart source=clear | source clear | ['clear'] | PASS |
| (f) /clear changes session_id | new id (rule in harness.md s2) | old=b1d62bb2-b386-47a8-b38a-42943e0a88a3 new=66fc2ca0-8ad8-4d9a-ae92-b267fde453a5 | PASS |
| (f) /clear fires SessionEnd for old session | SessionEnd reason clear | [('clear', True)] | PASS |
| (f) /clear: same harness ppid | same ppid | (81854, 81854) | PASS |
| (f) after /clear Bash env: TCAS_SESSION / CLAUDE_CODE_SESSION_ID | new session's values (probe-<new>) | ('probe-66fc2ca0-8ad8-4d9a-ae92-b267fde453a5', '66fc2ca0-8ad8-4d9a-ae92-b267fde453a5', 'new_sid=66fc2ca0-8ad8-4d9a-ae92-b267fde453a5') | PASS |
| (d) /exit with bg cmd running shows a confirmation dialog | dialog 'Exit and stop tasks / Move to background and exit / Stay' | True | INFO |
| (d) interactive claude exits after choosing 'Exit and stop tasks' | process exits | exited=True; SessionEnd reason=prompt_input_exit | PASS |
| (d) bg command after 'Exit and stop tasks' | killed (measured) | pid 83549 alive 2s after exit: False | INFO |
| (d) bg command after 'Move to background and exit' | survives (measured) | claude exited=True; pid 84523 alive 2s later: True | INFO |
| (d) detached bg command ran to completion after claude exit | bin-wait-end logged | True | INFO |

## Notes on how the runs were set up

- Headless: `claude -p <prompt> --model haiku --permission-mode default --max-turns N --plugin-dir tests/probe/plugin --output-format json`; the `--resume` row reuses the repo+log of the `basic` run.
- Interactive: `claude --model haiku --permission-mode default --plugin-dir ...` in a pty (50x160); the child's env is stripped of `CLAUDE_CODE_*`/`CLAUDECODE` markers inherited from the outer session. The folder-trust dialog (shown because the project has `.claude/settings.json`) is answered with Down+Enter ("Yes, I trust"). Bash approval: project `.claude/settings.json` `permissions.allow: ["Bash(probe-*)"]`, so no permission prompt ever appeared (PermissionRequest count 0).
- Not measured: marketplace install; `bypassPermissions` in interactive mode (headless only); harness force-kill (`SessionEnd` absence); Codex; Linux; model-behaviour rows (brief handling); git rows. Interactive runs were done once each, not repeated for stability.

## Raw facts for the implementation

### Hook stdin (trimmed examples, all events carry `session_id`, `transcript_path`, `cwd`, `hook_event_name`)

```
SessionStart        {"source":"startup"|"resume"|"clear"|"compact"?, "model":"claude-sonnet-5-5"}      (interactive adds model; source seen: startup, resume, clear)
UserPromptSubmit    {"prompt_id","permission_mode","prompt"}   prompt = "<task-notification>\n<task-id>..</task-id>\n<tool-use-id>..</tool-use-id>\n<output-file>/private/tmp/claude-501/<proj>/<sid>/tasks/<task-id>.output</output-file>\n<status>completed</status>\n<summary>Background command \"..\" completed (exit code 0)</summary>\n</task-notification>" on wakeup
PreToolUse          {"prompt_id","permission_mode","effort":{"level"},"tool_name","tool_input","tool_use_id"}
PostToolUse         + {"tool_response","duration_ms"}
PostToolUseFailure  {..., "tool_name":"Bash","tool_input":{"command":"probe-fail"},"tool_use_id","error":"Exit code 1\nprobe-fail: failing on purpose","is_interrupt":false,"duration_ms":18}   (no tool_response)
Stop                {"stop_hook_active":false,"last_assistant_message":"..","background_tasks":[{"id":"b..","type":"shell","status":"running","description":"..","command":".."}],"session_crons":[]}
SubagentStart       {"agent_id","agent_type","prompt_id"}
SubagentStop        {"agent_id","agent_type","agent_transcript_path","last_assistant_message","background_tasks":[],"stop_hook_active":false}
SessionEnd          {"prompt_id"?,"reason":"other"|"clear"|"prompt_input_exit"}
```

Tool inputs/responses seen:
- `Bash`: `tool_input {command, description?, run_in_background?}`; `tool_response {stdout, stderr, interrupted, isImage, noOutputExpected}`; background adds `backgroundTaskId` (stdout empty).
- `Write`: `{file_path, content}` (absolute, symlink-resolved `/private/tmp/...`); response `{type:"create", filePath, content, structuredPatch, originalFile, userModified}`.
- `Read`: `{file_path}`; response `{type:"text", file:{filePath, content, numLines, startLine, totalLines}}` (notebooks: `{type:"notebook", file:{filePath, cells}}`).
- `Grep`: `{pattern, path}` (`path` is as given by the model, here relative `src`); response `{mode, filenames:["src/a.txt"], numFiles, totalFiles}` (filenames relative to cwd as given).
- `NotebookEdit`: `{notebook_path, cell_id, new_source}`; response includes `notebook_path`, `error:""`.
- `Agent`: `{description, prompt, subagent_type, run_in_background}`.
- `ToolSearch` and `Agent` also trigger Pre/PostToolUse (matcher-less hooks see them).
- Tool hooks inside a subagent carry the parent's `session_id` plus `agent_id`/`agent_type`.
- Failed `Edit` (old_string not found, or file not read): the model gets `<tool_use_error>String to replace not found in file.`; NO PreToolUse, PostToolUse, PostToolUseFailure or PermissionRequest fires (validation fails before hooks). Only `Stop` closes the turn. So a lease taken in an earlier hook is never "closed" by a Post event for such a call (a PreToolUse that precedes validation does not exist either, so nothing is opened).

### Hook stdout formats that worked (all via `hookSpecificOutput`, include `hookEventName`)

- deny: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"..."}}` (also exit code 2 with reason on stderr).
- allow (Bash approve without prompt, edits without prompt): same with `"permissionDecision":"allow"`.
- updatedInput: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"..","updatedInput":{<FULL tool_input incl. command, with "run_in_background":true>}}}`. The rewritten input is visible in PostToolUse `tool_input` (`run_in_background:true`), the original PreToolUse input has none.
- additionalContext: `{"hookSpecificOutput":{"hookEventName":"SessionStart"|"UserPromptSubmit"|"PostToolUse","additionalContext":"..."}}`; the model quoted all three markers back (also on the `<task-notification>` wakeup, headless and interactive).
- SessionStart env: append `export TCAS_SESSION=...` to the file named by `$CLAUDE_ENV_FILE` (e.g. `~/.claude/session-env/<sid>/sessionstart-hook-0.sh`); it is set only in SessionStart.
- `onFailure:"block"` on PreToolUse blocks (file not created) for: exit 1, exit 2, invalid JSON on stdout, timeout (hook `timeout` 5s, hook slept 30s), missing program. (Exit 2 / deny are the "intended" paths; the others are the failure paths.)
- A user `permissions.deny` rule in project settings beats a hook `allow` (command did not run).
- `bypassPermissions` (headless): PreToolUse still fires (`permission_mode:"bypassPermissions"`) and a PreToolUse deny still blocks the Write.

### Interactive-mode facts

- Hooks fire as in headless: SessionStart(startup), UserPromptSubmit, PreToolUse, PostToolUse, Stop; plus SessionEnd on exit (`reason` `prompt_input_exit`, or `clear`). The hook's ppid equals the pid of the `claude` process we spawned in the pty (one ppid for all hooks, also across `/clear`). Bash ancestors: `[bash-tool-pid, <harness pid>, ...]`.
- A project with `.claude/settings.json` triggers the folder-trust dialog; Down+Enter selects "Yes, I trust this folder".
- `updatedInput` fg->bg works: PostToolUse arrives ~0.04s after PreToolUse with `backgroundTaskId`; command keeps running; `Stop.background_tasks` lists it.
- Idle session is woken when the bg command ends: UserPromptSubmit with `<task-notification>` prompt arrives within the same second as the command's end, with no keypress; our additionalContext reached the model and appeared on screen. A SubagentStop (no SubagentStart) was also logged at wakeup in that run; do not rely on it.
- `/clear`: new session_id; order is SessionEnd(reason `clear`, old id) then SessionStart(source `clear`, new id) with the same harness ppid. After `/clear` Bash gets the new `CLAUDE_CODE_SESSION_ID` and `TCAS_SESSION=probe-<new id>` (the SessionStart hook re-ran and wrote the env file). So the harness.md section 2 `/clear` rule is needed.
- Exit with a running background command: `/exit` shows a dialog "Background work is running ... 1. Exit and stop tasks / 2. Move to background and exit / 3. Stay". Option 1: the command is killed (pid gone 2s later). Option 2: claude exits and the command keeps running to completion (it then has no harness ancestor; its env vars remain). Ctrl-C twice is the other exit path (not measured separately). SessionEnd is logged with reason `prompt_input_exit` for the process that exits.
- Interactive inherited-env caveat: when launched from inside another Claude Code session, the child warns "Transcript saving is off — inherited CLAUDE_CODE_CHILD_SESSION marker"; the probe strips `CLAUDE_CODE_*` variables so that does not apply.
