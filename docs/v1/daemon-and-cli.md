# tcasd 和 tcas

## 1. 文件

全部在 `~/.tcas/` 下（`TCAS_HOME` 可改），目录权限 `0700`。

| 路径 | 内容 |
|---|---|
| `tcasd.sock` | Unix 套接字，权限 `0600` |
| `tcasd.pid` | tcasd 的 PID，同时是防止重复启动的锁文件 |
| `tcas.db` | SQLite 数据库 |
| `workspaces.json` | 各个 workspace 的根目录列表，加上解析好的豁免目录列表（[leases.md](leases.md) 第 3 节）。tcasd 维护，`tcas hook` 直接读它来决定要不要连 tcasd |
| `config.toml` | 全局配置，见第 6 节 |
| `tcasd.log` | 日志，按大小轮转 |
| `bin/tcas`、`bin/tcasd` | 安装的程序 |
| `plugins/` | `tcas plugin install` 生成的 plugin 目录 |
| `git-hooks.gitconfig` | git 钩子的定义。各仓库的 `.git/config` 用一行 `include.path` 引用它（[harness.md](harness.md) 第 5.6 节） |

## 2. tcasd

- **每个用户一个进程**，管这台机器上的所有 workspace。
- **启动**：`tcas` 的任何命令连不上套接字时自动启动它（先拿 `tcasd.pid` 的文件锁，拿不到说明别人正在启动，等套接字出现）。也可以 `tcas daemon start` 手动启动。
- **和启动它的进程脱开**：自动启动的 tcasd 多半是某个会话的 hook 或 git 钩子拉起来的。它要 `setsid` 进新的会话、当前目录换到 `/`、标准输入输出接到日志，环境里去掉 `TCAS_SESSION`、`CLAUDE_CODE_SESSION_ID` 和 git 的本地变量。否则那个 harness 退出或清理进程组时会把它一起带走，继承来的变量也会让它的 git 查询查错（见下表「git 查询」）。
- **不自动退出**。没有 workspace 时它只是空等，占用很小。`tcas daemon stop` 停止。
- **开机自启不做**，第一次用到时会自动启动。

tcasd 做的事：

| | |
|---|---|
| 处理请求 | 每个连接一个请求、一个应答，见第 4 节。`lease.wait` 的连接保持到有结果为止 |
| 等待 | 维护等待和等待图；有等待时每 5 秒查一次被等待的路径是否干净；被等待的租约到期限时重新判定（[leases.md](leases.md) 第 6、7 节） |
| 租约判定 | 全部在一个写事务里完成，SQLite 保证串行 |
| git 查询 | `status`、`check-ignore`、`diff --name-only`、`rev-parse`。只读，都带 `--no-optional-locks`。在路径所属的仓库里运行，涉及几个仓库时并行。tcasd 启动时从自己的环境里去掉 git 的本地变量（`git rev-parse --local-env-vars` 列出的 `GIT_DIR`、`GIT_INDEX_FILE`、`GIT_WORK_TREE` 等），每个 git 子进程也去掉一遍：tcasd 可能是被 git 钩子里的 `tcas` 自动启动的，实测提交钩子里的 `GIT_INDEX_FILE` 指向一个临时索引，继承下来之后所有查询都会查错 |
| git 钩子的那一行 | 打开、关闭 workspace，加、去掉根，以及发现新仓库时，用 `git config` 在仓库配置里加上或去掉 tcas 的那一行引用（[harness.md](harness.md) 第 5.6 节）。这是 tcasd 唯一会写的仓库文件 |
| 发现仓库 | 打开 workspace 时和每次打开 Bash 对账窗口时，找出 workspace 里的仓库，包括用 `git worktree list` 找出的链接 worktree（[leases.md](leases.md) 第 3 节）。目录不在了的，它下面的租约释放 |
| 会话存活检查 | 每 15 秒检查一次 `active` 和 `idle` 会话的进程（PID 加启动时间）。进程不在了就按会话结束处理 |
| 清理 | 结束超过 7 天的会话、超过 30 天的事件（可配置） |

tcasd 不做的事：不写 workspace 里的任何文件（除了上面那一行引用），不启动 harness，不联网，不常驻监听文件变化。

**重启后**：workspace、会话、租约、进行中的调用、通知队列、事件都在数据库里。Bash 窗口打开时拍的快照只在内存里，重启后丢失，那几次调用关闭时不对账。等待也只在内存里，`tcas lease wait` 会自动重连并重新登记。

## 3. 存储

SQLite，WAL 模式。表：

| 表 | 主要的列 | 说明 |
|---|---|---|
| `workspace` | `id`、`name`、`mode`、`opened_by`、`created_at` | `opened_by` 是用户或某个会话 |
| `root` | `workspace`、`name`、`path`、`case_insensitive`、`added_by`、`added_at` | workspace 的根目录，一个或多个（[leases.md](leases.md) 第 3 节）。`path` 是规范化后的绝对路径 |
| `repo` | `workspace`、`root`、`dir`、`kind`、`common_dir`、`last_branch`、`seen_at` | workspace 里发现的检出。`dir` 相对它所在的根。`common_dir` 是它所属仓库的 git 目录，主检出和它的 worktree 相同，用来决定 git 钩子装在哪、何时去掉 |
| `session` | `id`、`harness`、`state`、`pid`、`pid_started_at`、`cwd`、`title`、`brief`、`started_at`、`last_seen_at`、`ended_at` | `title` 给用户看，`brief` 给别的会话看 |
| `lease` | `workspace`、`root`、`path`、`path_key`、`is_prefix`、`session`、`state`、`source`、`brief`、`acquired_at`、`touched_at`、`committed_at` | 主键（`workspace`，`root`，`path_key`）。`path` 相对根；`path_key` 是比较用的形式，在不区分大小写的根里折叠了大小写，其余情况和 `path` 相同。其余带路径的表同样另记 `root` |
| `open_call` | `session`、`agent`、`call_id`、`kind`、`paths`、`background_id`、`opened_at` | 进行中的编辑调用和 Bash 窗口。让出隐式租约之前要查它（[leases.md](leases.md) 第 5 节），所以要持久化 |
| `notice` | `id`、`session`、`kind`、`body`、`created_at`、`delivered_at` | 待投递的通知 |
| `interest` | `session`、`workspace`、`path`、`kind`（`denied` 或 `read`） | 路径释放时该通知谁 |
| `unattributed` | `workspace`、`path`、`seen_at` | 对账时归属不确定的改动 |
| `event` | `seq`、`at`、`workspace`、`session`、`agent`、`type`、`payload` | 只追加 |
| `meta` | `key`、`value` | 数据库版本 |

实现里另有几张辅助表，都是派生出来的状态，丢了不影响租约判定：`session_ws`（会话已经收到过哪些 workspace 的用法说明）、`read_hint`（读取提示已经给过哪些）、`wait_notified`（「有人在等」已经通知过哪些）、`outside_edit`（改到 workspace 外面的记录，`tcas status` 用）、`commit_seen`（已经处理过的提交，钩子和 `tcas commit` 各报一次时只算一次）、`session_alias`（AGY 子 agent 的会话 ID 指向父会话，[harness.md](harness.md) 第 10 节）。`lease` 表另有 `repo`（所属的检出）、`edited_at`、`stalled_at`，`session` 表另有 `turn` 等统计用的列。

相交判定不在 SQL 里做：一个 workspace 的租约数量是几十到几百条，读出来在内存里比。

**事件**是审计记录，也是以后做统计的原料。每次租约表变化和对应的事件在同一个事务里。

| 类型 | 何时 |
|---|---|
| `workspace.root_added`、`workspace.root_removed` | 带目录和是谁做的 |
| `workspace.outside_edit` | 会话用编辑工具改了 workspace 外面某个检出里的文件，带那个检出的目录 |
| `workspace.opened`、`workspace.closed` | 带是谁做的（用户或哪个会话）；`closed` 带原因：用户关闭、会话关闭、目录不在了 |
| `repo.found`、`repo.gone` | 发现了新的仓库或 worktree，或原来的目录不在了。`gone` 时它下面的租约全部释放 |
| `session.started`、`session.resumed`、`session.ended` | `ended` 带原因：hook、进程不在、被新会话接替 |
| `lease.granted` | 带 `source`、`brief` |
| `lease.denied` | 带持有者、申请来自编辑还是命令行 |
| `lease.released` | 带原因：提交、主动、让出（文件没改成）、过了期限后让出、提交过且这一轮结束后让出、会话结束、用户强制 |
| `lease.orphaned`、`lease.readopted`、`lease.transferred` | |
| `wait.started`、`wait.granted`、`wait.timeout`、`wait.deadlock`、`wait.cancelled` | 带路径和等了多久；`started` 带 brief；`deadlock` 带环里的会话和路径 |
| `lease.suggested` | 提醒了先申请（[harness.md](harness.md) 第 6 节），带建议的命令 |
| `bash.blocked` | 命令护栏拒绝，带命中的规则 |
| `git.blocked` | git 钩子拒绝，带钩子名、发起者、涉及的路径 |
| `git.override` | 带 `TCAS_ALLOW=1` 放行了一次本来会被拒绝的操作 |
| `git.rewrite` | 钩子报告的切分支、合并、变基 |
| `bash.overreach` | Bash 改了别人持有的路径 |
| `bash.unattributed` | 归属不确定 |
| `commit` | 任何提交，由 `post-commit` 钩子报告（没装钩子时由 `tcas commit` 或对账报告），带提交号、路径、发起者 |
| `branch.changed` | 带仓库 |
| `hook.error` | `tcas hook` 报告的异常 |
| `lease.unstalled` | 停滞的标记消失：持有者又有活动、提交、转交或释放 |
| `wait.resumed` | tcasd 重启后 `tcas lease wait` 重新登记 |
| `workspace.mode`、`exempt.added`、`exempt.removed`、`git_hooks.installed`、`git_hooks.removed` | 用户改了设置 |

## 4. 协议

Unix 套接字上，一个连接发一行 JSON 请求，收一行 JSON 应答，然后关闭。`lease.wait` 的应答要等到有结果才发，连接一直开着，客户端断开就取消等待。

```jsonc
// 请求
{ "v": 1, "op": "lease.acquire", "session": "claude:7958…", "cwd": "/…/repo", "args": { … } }
// 应答
{ "ok": true, "result": { … } }
{ "ok": false, "error": { "code": "lease_denied", "message": "…", "detail": { … } } }
```

- `session` 取自环境变量，可以为空。请求里另带 `ancestors`：发起命令的进程的各级祖先（PID 和启动时间）。tcasd 先看 `session`；为空时在 `ancestors` 里找登记过的会话进程；都没有才按用户处理（[harness.md](harness.md) 第 2 节）。
- `v` 不匹配时 tcasd 返回 `version_mismatch`，`tcas` 提示升级。
- 套接字权限 `0600`，只有同一个系统用户能连。不做别的鉴权（[plan.md](plan.md) 第 6 节）。

**命令行用的操作**和第 5 节的命令一一对应：`workspace.open`、`workspace.add`、`workspace.remove`、`workspace.close`、`workspace.list`、`status`、`lease.acquire`、`lease.wait`、`lease.release`、`lease.list`、`lease.who`、`lease.grant`、`lease.transfer`、`session.list`、`session.end`、`exempt.list`、`exempt.add`、`exempt.remove`、`commit.prepare`、`commit.done`、`git_hooks.install`、`git_hooks.remove`、`git_hooks.status`、`log`、`stats`、`doctor`、`whoami`、`daemon.status`、`daemon.stop`。

**git 钩子用的操作**是 `git.hook`：钩子名、仓库、发起者（会话或用户）、涉及的路径或引用；应答是放行或拒绝加理由。

**hook 用的操作**只有一个，`hook`。请求是归一化之后的事件：

```ts
interface HookRequest {
  harness: 'claude' | 'agy' | 'codex'
  event: 'session_start' | 'prompt' | 'pre_tool' | 'post_tool' | 'stop' | 'agent_stop' | 'session_end'
  session: string               // 原生会话 ID
  agent?: string                // 子 agent
  cwd: string
  pid: number                   // harness 进程
  pidStartedAt: Time
  source?: string               // session_start：startup、resume、clear、compact
  prompt?: string               // prompt：只取前 80 个字符作标题
  tool?: {
    kind: 'edit' | 'bash' | 'read' | 'other'
    callId: string
    paths?: string[]            // edit、read：绝对路径
    command?: string            // bash
    background?: boolean        // bash
    backgroundId?: string       // post_tool，后台命令
    failed?: boolean            // post_tool
  }
  backgroundRunning?: string[]  // stop：还在运行的后台命令
}

interface HookResponse {
  decision?: 'deny' | 'allow'   // allow 只用于 tcas 命令（harness.md 第 5.5 节）
  background?: boolean          // pre_tool：把这次调用改成后台运行，用于 tcas lease wait
  waitCommand?: string          // 被拒时可用的等待命令；怎样异步运行的那句话由适配层按 harness 补上
  reason?: string
  context?: string              // 注入给模型的文字
  env?: Record<string, string>  // session_start：要写进会话环境的变量
}
```

`tcas hook <harness>` 负责两头的翻译：把 harness 的输入转成 `HookRequest`，把 `HookResponse` 转成 harness 要的输出格式。tcasd 里没有任何和具体 harness 有关的代码。

**超时**：`tcas hook` 的总时限（含启动 tcasd 的等待）比 hook 配置里的超时短 1 秒，见 [harness.md](harness.md) 第 3 节。

## 5. 命令

约定：

- 身份的认法见 [harness.md](harness.md) 第 2 节。有会话身份时是 agent，没有时是用户。两者各能用哪些命令见下面的「谁能做什么」。
- workspace 从当前目录推断：当前目录在哪个 workspace 的某个根里。`--workspace <名字、id 或路径>` 可指定。
- 所有命令支持 `--json`。
- 路径参数相对当前目录，也可以写绝对路径。输出里的路径：workspace 只有一个根时相对这个根，多仓库时带着仓库所在的目录（`web/src/app.ts`）；有几个根时前面带根的名字（`app-backend/src/user.ts`）。输出里给出的可以直接运行的命令，路径写成从当前目录能用的形式。

**退出码**

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 出错（参数、tcasd 不可用、不在 workspace 里等） |
| 3 | 租约被拒（申请没有通过，或 `tcas commit` 里有不属于自己的路径） |
| 4 | 等待超时，没有拿到任何租约 |
| 5 | 等待会形成死锁，被拒绝或被结束 |
| 6 | 这条命令只有用户能用，或者超出了 agent 可以做的范围 |

### 谁能做什么

| 命令 | 用户 | agent |
|---|---|---|
| 查看：`status`、`workspace list`、`session list`、`lease list`、`lease who`、`exempt list`、`log`、`stats`、`doctor`、`whoami` | 能 | 能 |
| `lease acquire`、`lease wait`、`lease release`、`commit` | 不适用，用户不受租约约束 | 能 |
| `workspace open`、`workspace add` | 能 | 能，有限制，见下 |
| `workspace close`、`workspace remove` | 能 | 只能撤销自己打开或加进来的，而且里面已经没有任何租约、没有别的活着的会话 |
| `workspace set`、`git-hooks install`、`git-hooks remove`、`exempt add`、`exempt remove` | 能 | 不能 |
| `lease grant`、`lease transfer`、`lease release --force`、`session end` | 能 | 不能 |
| `daemon stop`、`plugin install` | 能 | 不能 |

agent 运行了不能用的命令时，退出码 6，输出里说明这是用户的操作，让它告诉用户。

**agent 打开 workspace**

- `workspace open` 可以打开的目录只有两种：
  - 它当前所在的检出（仓库的根，或一个 worktree）。
  - 它的当前目录本身，如果这个目录不在任何检出里（例如装着几个仓库的目录）。
- `workspace add` 可以加进当前 workspace 的只有检出的根目录（仓库或 worktree），一次一个。典型的是它正要去改的并排目录，或者它自己建在外面的 worktree。
- 不能打开用户的主目录和文件系统的根，也不能打开里面发现的检出超过 8 个的目录（`open_max_checkouts`）。一条命令不该给一大片仓库装上 git 钩子。
- 豁免目录里的不能打开，用户也不能（[leases.md](leases.md) 第 3 节）。
- 不能带 `--strict` 和 `--no-git-hooks`，用用户配置的默认值。
- 不自动批准。是否放行由 harness 自己的权限规则决定，默认的权限模式下用户会被问到。
- 记下是哪个会话打开的，`tcas workspace list` 和 `tcas status` 里显示。
- `~/.tcas/config.toml` 里 `[agents] workspace_open = false` 可以关掉，这时只有用户能打开。

**agent 关闭 workspace**

关闭会让里面所有的会话失去保护，并丢掉全部租约。被别人的租约挡住的 agent，不应该能靠关掉 workspace 绕过去。所以 agent 只能撤销自己做过的事：

- 只能关它自己打开的。
- 里面已经没有任何租约，包括它自己的（有未提交改动的文件要先提交）。
- 里面没有别的活着的会话。

`workspace remove` 同理：只能去掉它自己加进来的根，而且这个根下面已经没有任何租约。

其余情况告诉用户。

### workspace

| 命令 | 作用 |
|---|---|
| `tcas workspace open [路径…] [--strict] [--no-git-hooks]` | 登记一个 workspace。每个目录是它的一个根，可以是一个仓库的根，也可以是装着几个仓库的目录；之后还可以用 `add` 再加。不给路径时：当前目录在某个检出里就取那个检出的根（`git rev-parse --show-toplevel`；在链接的 worktree 里是 worktree 自己的目录，不是主检出），否则取当前目录 |
| `tcas workspace add <目录…>` | 把目录加进当前 workspace，成为它的根：发现里面的检出，装上 git 钩子。用来放进不在主目录下面的后端、网站目录，或者建在外面的 worktree（[leases.md](leases.md) 第 3 节） |
| `tcas workspace remove <目录>` | 把一个根从 workspace 里去掉，去掉里面仓库的 git 钩子。下面还有租约时要加 `--force`，只有用户能用 |
| `tcas workspace close [路径]` | 取消登记，去掉各仓库里的 git 钩子（同一个仓库还有检出在别的 workspace 里时保留）。还有租约时要加 `--force`，租约全部丢弃；`--force` 只有用户能用 |
| `tcas workspace list` | 列出 workspace、它的各个根、模式、是谁打开的、仓库数、会话数、租约数 |
| `tcas workspace set mode <auto\|strict>` | 改模式，立即生效。只有用户能用 |

`open` 时做的事：规范化根目录；探测卷是否区分大小写；发现里面的仓库并记下各自的当前分支；给每个仓库装上 git 钩子（[harness.md](harness.md) 第 5.6 节）；更新 `workspaces.json`；检查 harness 的 plugin 是否已安装，没装就提示 `tcas plugin install`。

```
$ cd ~/Code/app && tcas workspace open
opened ~/Code/app (mode auto), 3 repositories:
  web/                 repo        main
  api/                 repo        main
  api/vendor/proto/    submodule   (detached at 3f2a91c)
```

- **项目的目录不在一起时**，把它们加进同一个 workspace，只需要做一次：

  ```
  $ cd ~/Code/app && tcas workspace open
  opened workspace "app" (mode auto), 1 checkout:
    .    repo   main

  $ tcas workspace add ../app-backend ../app-website
  added to workspace "app":
    app-backend    ~/Code/app-backend    repo   main
    app-website    ~/Code/app-website    repo   main
  workspace "app" now has 3 roots. Paths are shown as app/…, app-backend/…, app-website/…
  ```

  也可以写在主目录的 `.tcas.toml` 里（第 6 节的 `roots`），打开时自动加进来。

- **仓库在 workspace 外面还有 worktree 时**，`open` 把它们列出来并给出加进来的命令，不替你加。那里的会话在加进来之前不受管理：

  ```
  $ cd ~/Code/app && tcas workspace open
  opened ~/Code/app (mode auto), 2 checkouts:
    .                             repo       main
    .claude/worktrees/export/     worktree   worktree-export
  note: this repository has a worktree outside this workspace; sessions there are not managed:
    ~/Code/app-feature   feature/login    →  tcas workspace add ~/Code/app-feature
  ```

- **豁免的目录不能打开**，也不能加成根：

  ```
  $ tcas workspace open /tmp/scratch
  refused: /tmp/scratch is under /tmp, which is exempt (built-in: system temp directory).
  TCAS does not manage exempt directories. To manage it anyway, set  [exempt] builtin = false
  in ~/.tcas/config.toml.
  ```

- **根是检出的根目录，或者不在任何检出里的目录。** 要打开或加进来的目录在某个检出里面、又不是它的根（如 `web/src`）时拒绝，给出那个检出的根。否则从里面的文件往上找不到 `.git`（[leases.md](leases.md) 第 3 节第 6 步），它们都会被当成不属于任何仓库的文件，git 钩子也装不上。
- **根不能互相包含**，不管是不是同一个 workspace 的。要打开或加进来的目录在一个已有的根里面，或者包着一个已有的根，都会被拒绝，并说明是哪一个。要把几个单仓库的 workspace 换成它们的上级目录，先关掉它们。
- 目录里一个仓库都没有也可以打开。这时没有 git 可查，所有租约按「不属于任何仓库的文件」处理（[leases.md](leases.md) 第 3 节）。
- workspace 的目录不在了（例如它是一个被删掉的 worktree）：tcasd 在下一次请求时发现，自动关闭它，里面的租约全部释放。
- 每个仓库各开一个 workspace 的用法照旧可用。区别只在 `tcas status` 是分开看还是合在一起看，以及 Bash 对账的范围：会话在一个 workspace 里用 Bash 改了另一个 workspace 的文件，不会被对账发现。所以同一个项目的几个目录应该放进同一个 workspace。

已经在运行的会话，在 workspace 打开之后的下一次 hook 起受约束，不需要重启。它们启动时没有收到过用法说明，tcas 在它们之后第一个能注入内容的 hook 里补上（[harness.md](harness.md) 第 6 节）。不管是用户还是别的会话打开的，都一样。

### 状态

| 命令 | 作用 |
|---|---|
| `tcas status` | 当前 workspace：模式、分支、会话、每个会话持有的目录和文件、遗留租约、未归属的改动 |
| `tcas session list` | 会话：ID、harness、状态、标题、最后活动时间、租约数 |
| `tcas lease list [--all]` | agent：自己持有的。用户：全部 |
| `tcas lease who <路径…>` | 每条路径由谁持有（会话、brief、从何时起），或没人持有。路径不归 tcas 管时说明原因：在哪个豁免目录里、被 git 的哪条规则排除，或者符合哪条 `unmanaged`。会话运行时看不到别的会话的标题 |
| `tcas log [-n N] [--follow] [--session S] [--path P]` | 事件日志 |
| `tcas stats` | 见下 |

`tcas status` 的例子：

```
workspace  ~/Code/app   mode auto   repo .  branch main

SESSION          STATE   LAST    TITLE                    LEASES
claude:7958…     active  now     fix login redirect       3
claude:1cce…     idle    52m     add export button        2 (1 stalled)
claude:cc8c…     ended   1h      refactor date utils      1 (orphaned)

claude:7958…  fix login redirect
  src/auth/session.ts          edit   14:02   modified
  src/auth/redirect.ts         edit   14:05   modified
  src/auth/                    cli    14:01   dir, 14 files, used 3m ago
  brief: "Fix the login redirect loop in src/auth"

claude:1cce…  add export button   (no activity for 52m)
  src/export/button.tsx        edit   13:20   modified   STALLED
  src/export/                  cli    13:15   dir, 9 files, unused 52m: yields on request
  → src/export/button.tsx has uncommitted changes and will stay leased until handled:
    go back to that session, or: tcas lease transfer src/export/button.tsx --to <session>

claude:cc8c…  refactor date utils   (ended; uncommitted changes remain)
  src/utils/date.ts            edit   13:10
  → commit or discard the changes, or: tcas lease transfer src/utils/date.ts --to <session>

unattributed changes (modified, no lease):
  README.md
```

一个根里有几个仓库时，开头列出各个仓库和分支，下面的路径都从这个根写起：

```
workspace  ~/Code/app   mode auto
  web/                 main
  api/                 feature/export   (branch changed 2m ago)
  api/vendor/proto/    submodule, detached at 3f2a91c

claude:7958…  fix login redirect
  web/src/auth/session.ts      edit   14:02   modified
  api/src/user.ts              edit   14:06   modified
  api/src/auth/                cli    14:01   dir, 14 files, used 3m ago
  brief: "Fix the login redirect loop in src/auth"
```

有几个根的 workspace 里，开头列出各个根，路径前面带根的名字。会话改过的、不在 workspace 里的检出列在最后：

```
workspace "app"   mode auto
  app            ~/Code/app            main
  app-backend    ~/Code/app-backend    main

claude:7958…  fix login redirect
  app/src/auth/session.ts        edit   14:02   modified
  app-backend/src/user.ts        edit   14:06   modified

edited outside this workspace (not managed):
  ~/Code/app-website    3 edits by 1 session    →  tcas workspace add ~/Code/app-website
```

会话名后面是它的标题（用户发给它的第一条消息），只在用户运行 `tcas status` 时显示；会话自己运行时这里显示的是 brief。没写过 brief 的会话没有 `brief:` 那一行。

状态命令会现查每条租约的路径是否干净。遗留租约里干净的当场释放；其余的只显示，过了期限的租约要等别的会话来申请才让出。

### 租约

| 命令 | 谁用 | 作用 |
|---|---|---|
| `tcas lease acquire <路径…> [--brief "…"]` | agent | 申请文件或目录，可以混着写。brief 是一句话，说明要做什么，别的会话看得到（[leases.md](leases.md) 第 5 节）。全有或全无。失败时退出码 3，列出每条被占的路径和持有者，并给出等待的命令。对已经持有的路径再运行一次就是续期。做较大的改动前先用它（[leases.md](leases.md) 第 5 节） |
| `tcas lease wait <路径…> [--timeout <秒>] [--brief "…"]` | agent | 和 `acquire` 相同，但拿不到时等待，直到全部授予（退出码 0）、超时（4）或死锁（5）。默认最多等 30 分钟。要作为后台命令运行，不占会话的主线程（[harness.md](harness.md) 第 6 节）。规则见 [leases.md](leases.md) 第 7 节 |
| `tcas lease release [<路径…>] [--all]` | agent | 释放自己持有的。释放目录时，它下面改过没提交的文件仍然留在自己名下。`--all` 释放全部目录租约和干净的文件租约。有未提交改动的文件要写出路径才释放，并且会警告 |
| `tcas lease grant <会话> <路径…> [--brief "…"]` | 用户 | 给某个会话发租约。规则和 `acquire` 相同，对方在下一次 hook 时收到通知 |
| `tcas lease transfer <路径…> --to <会话>` | 用户 | 把租约转给另一个会话，不论原持有者是否还活着。原持有者收到通知 |
| `tcas lease release --force <路径…>` | 用户 | 强制释放。原持有者收到通知 |
| `tcas session end <会话>` | 用户 | 按会话结束处理：干净的租约释放，其余变成遗留租约 |

按目录申请时的输出：

```
$ tcas lease acquire src/auth/ --brief "Rename getUser to fetchUser in src/auth and update all callers"
granted: src/auth/ (directory, 14 tracked files)

$ tcas lease acquire src/auth/
denied: nothing was acquired.
  src/auth/token.ts     leased by claude:1cce… since 13:20 (uncommitted changes)
      their brief: "Add CSV export to the reports page"
If their brief already covers the change you were about to make, do not wait: tell the user.
To wait until src/auth/ is free, run this as a background command:
  tcas lease wait src/auth/ --brief "<one line: what you need it for>"
Or request only the parts you need.

$ tcas lease acquire src/utils/
granted: src/utils/ (directory, 21 tracked files)
note: no brief. Other sessions cannot see what you are doing here. Add one with:
  tcas lease acquire src/utils/ --brief "<one line: what you are changing and why>"

$ tcas lease acquire src/
refused: src/ covers 1,240 tracked files (limit 200). Request narrower directories, or list files.
  src/auth/ 14   src/api/ 38   src/ui/ 902   src/utils/ 21   …
```

申请失败和等待结束时的输出：

```
$ tcas lease acquire src/auth/session.ts src/auth/redirect.ts
denied: nothing was acquired.
  src/auth/session.ts   leased by claude:7958… since 14:02 (uncommitted changes)
      their brief: "Fix the login redirect loop in src/auth"
To wait until both are free, run this as a background command:
  tcas lease wait src/auth/session.ts src/auth/redirect.ts --brief "<one line: what you need them for>"

$ tcas lease wait src/auth/session.ts src/auth/redirect.ts
granted after 41s: src/auth/session.ts, src/auth/redirect.ts
  src/auth/session.ts changed while you waited (commit 3f2a91c). Re-read it before editing.

$ tcas lease wait src/auth/session.ts --timeout 30
timed out after 30s: nothing was acquired.
  src/auth/session.ts   still leased by claude:7958… (uncommitted changes)
      their brief: "Fix the login redirect loop in src/auth"

$ tcas lease wait src/auth/session.ts
deadlock: not waiting.
  claude:7958… holds src/auth/session.ts and is waiting for src/api/user.ts, which you hold.
Commit or release src/api/user.ts first, or tell the user.
```

- 申请的路径不归 tcas 管时不记租约，命令照常成功，输出里说明：

  ```
  $ tcas lease acquire dist/ src/auth/
  granted: src/auth/ (directory, 14 tracked files)
  not managed, no lease needed:
    dist/    excluded by git (.gitignore:3: dist/)

  $ tcas lease who .env.local /tmp/x.txt
  .env.local    not managed: excluded by git (.git/info/exclude:7: .env.local)
  /tmp/x.txt    not managed: exempt directory /tmp (built-in: system temp directory)
  ```

- 路径以 `/` 结尾，或者是一个已存在的目录时，按目录处理。其余的按文件处理，文件可以还不存在（用来申请将要新建的文件）。要申请一个还不存在的目录，末尾加 `/`。
- 目录下被跟踪的文件超过 `max_dir_files`（默认 200）时拒绝，退出码 1。输出里给出文件数和下一级各子目录的文件数，方便换成更小的范围。
- agent 不能用 `grant`、`transfer`、`--force` 和 `session end`，见上面的「谁能做什么」。
- `<会话>` 可以写 ID 的前缀，只要不重复。

### 提交

| 命令 | 作用 |
|---|---|
| `tcas commit -m "<说明>" [<路径…>]` | 只提交本会话改过的文件。不给路径时提交它改过、还没提交的全部文件；给了目录，就是这个目录下它改过的文件 |

步骤：

1. 问 tcasd 本会话改过哪些文件（它名下的文件租约）、其中哪些有改动（`commit.prepare`）。目录参数展开成这个目录下它改过的文件。文件参数必须在本会话的租约之下，有一个不是就以退出码 3 结束，不提交。
2. 把文件按所属的仓库分组。每个仓库里 `git add -- <路径…>`，然后 `git commit --only -m … -- <路径…>`。索引锁被占用时等 200 毫秒重试，最多 5 次。
3. 提交由 git 的 `post-commit` 钩子报告给 tcasd；仓库没装 git 钩子时由 `tcas commit` 自己报告（`commit.done`）。同一个提交号只处理一次。这些路径上的隐式租约释放，等待它们的会话收到通知。目录租约和其他显式租约保留。
4. 输出提交号、提交了哪些路径、释放了哪些租约、还持有哪些。还持有目录租约时，提示做完了就用 `tcas lease release` 释放。

不提交目录租约下所有有改动的文件，是因为其中可能有用户自己改的，或者不该进仓库的临时文件。要提交一个没记到自己名下的文件，把它的路径写在参数里。

提交钩子（pre-commit 等）照常运行。它们失败时提交失败，租约不变。

**改动分属几个仓库时**，每个仓库提交一次，用同一条说明：

```
$ tcas commit -m "rename getUser to fetchUser"
web   3f2a91c   2 files   web/src/auth/session.ts, web/src/auth/redirect.ts
api   8b10c4e   1 file    api/src/user.ts
released: 3 file leases.  still held: api/src/auth/ (directory)
```

- 顺序按仓库路径的字典序。要自己定先后，就分几次运行，每次给一个仓库的路径（`tcas commit -m … api/`）。
- **不是原子的。** 某个仓库提交失败（多半是提交钩子）时停下：前面已经提交的仓库保持已提交，这个和后面的仓库不提交，退出码 1，输出里逐个仓库说明结果。已提交的文件租约释放，没提交的不变。修好后再运行一次，只会提交剩下的。
- 子模块里的改动提交在子模块里。外层仓库记录的子模块提交号不会自动更新；要更新它，把子模块的路径写在参数里（`tcas commit -m … api/vendor/proto`）。
- 不属于任何仓库的文件不提交。

用户不需要用 `tcas commit`。装了 git 钩子时，用户的提交里如果有会话还在改的文件，会被 `pre-commit` 拒绝并说明是哪些；确实要提交就加 `TCAS_ALLOW=1`。agent 用普通的 `git commit -- <路径>` 提交自己的文件也可以，租约同样会释放。

### 其他

| 命令 | 作用 |
|---|---|
| `tcas exempt list` | 列出豁免的目录和每一条的来源（内置、全局配置、哪个 workspace 的配置）。被 git 排除的路径不在这里列，按路径用 `tcas lease who` 查 |
| `tcas exempt add <目录>`、`tcas exempt remove <目录>` | 往全局的豁免列表里加、去掉一条。只有用户能用。加的目录包着某个 workspace 的根时拒绝 |
| `tcas hook <harness>` | 供 hook 调用，从标准输入读 harness 的事件。不给人用 |
| `tcas git-hook <钩子名> [参数…]` | 供 git 钩子调用。所在的检出不在任何 workspace 里时立即放行。不给人用 |
| `tcas git-hooks install\|remove\|status [仓库]` | 单独给一个仓库装上、去掉 git 钩子，或查看状态 |
| `tcas plugin install <claude\|agy\|codex>` | 生成并安装 plugin |
| `tcas plugin path <harness>` | 输出 plugin 目录，开发时配合 `--plugin-dir` 用 |
| `tcas daemon start\|stop\|status` | `stop` 只有用户能用，tcasd 没在运行时也一样 |
| `tcas whoami` | tcasd 把这条命令认作谁：哪个会话，或用户（第 2 节的认法，含祖先进程）。排查身份问题用 |
| `tcas doctor` | 检查：豁免的目录有哪些、有没有和 workspace 的根冲突；tcasd 是否在运行、版本是否一致、plugin 是否安装、当前目录是否在 workspace 里、git 是否支持配置式钩子、各仓库的 git 钩子是否装着 |

### 统计

`tcas stats` 从事件日志算，按 workspace 和时间段，多仓库时可以用 `--repo` 只看一个仓库：

| 数字 | 用途 |
|---|---|
| 申请被拒的次数，按路径排序 | 哪些文件争用最多（atc 设计文档第 5.3 节的热点文件） |
| 等待的次数、等了多久、超时和死锁各多少 | 等待够不够用，要不要做预约和顺序（atc 设计文档第 5.2 节） |
| 每个会话改过的文件里：改之前已经在它的显式租约之下的有多少，靠自动取得的有多少 | agent 有没有先申请再动手，事先说得准不准（atc 设计文档第 11 节阶段 0） |
| 每条目录租约：目录下有多少文件，实际改了多少 | 申请的范围是不是过大 |
| 提醒了先申请的次数，其中之后确实申请了的次数 | 提醒有没有用 |
| 事先申请里带 brief 的比例；被拒之后去等、没等的各有多少 | brief 有没有人写，被拒的会话有没有据此做不同的决定 |
| Bash 越界和归属不确定的次数 | 对账够不够用，要不要上沙箱 |
| 遗留租约和停滞的租约的数量、存在时长 | 留下脏文件不管有多常见 |
| 过了期限后让出的次数；其中原持有者之后又来申请同一条路径的次数 | 30 分钟和 10 分钟这两个期限是否合适 |
| 提交过、这一轮结束后让出的次数；其中原持有者下一轮又来申请同一条路径的次数 | 「这一轮结束」这个条件是否太早 |
| 命令护栏拒绝的次数，按规则；其中因为看不出要做什么而拒绝的次数 | 规则是否太严，解析够不够 |
| git 钩子拒绝的次数，按钩子；其中命令护栏没拦到的次数 | 钩子补上了多少护栏漏掉的 |
| 用 `TCAS_ALLOW=1` 放行的次数 | 用户被检查挡住有多频繁 |

## 6. 配置

**全局**：`~/.tcas/config.toml`

```toml
[daemon]
liveness_interval_secs = 15
event_retention_days = 30

[defaults]
mode = "auto"

[exempt]
builtin = true                # 系统的临时目录、~/.tcas、harness 自己的目录（leases.md 第 3 节）
paths = []                    # 另外豁免的目录，绝对路径或以 ~ 开头，如 ["~/scratch"]
# 被 git 排除的路径总是不管，没有开关。要管某个被排除的文件，让 git 跟踪它。

[agents]
workspace_open = true         # 会话可以打开 workspace、往里加检出（有限制，见第 5 节）
open_max_checkouts = 8        # 会话打开的目录里最多可以有多少个检出
```

**每个 workspace**：第一个根（打开 workspace 时的那个目录）下的 `.tcas.toml`。没有这个文件时全部用默认值。workspace 本身是一个仓库时，它可以提交进仓库；是装着几个仓库的目录时，它不属于任何仓库，各仓库里可以另放一份（见下）。

```toml
mode = "auto"                 # auto | strict

# 这个 workspace 另外包含的目录，相对本文件所在的目录。打开 workspace 时一起加进来。
roots = ["../app-backend", "../app-website"]

hint_outside = true           # 会话改到 workspace 外面的检出时提示一次（leases.md 第 3 节）

# 这个根里面不管的目录，相对本文件所在的目录。整个目录不发租约、不拦截、不对账。
exempt = ["scratch/"]

# 不发租约、不拦截的路径（git 的 pathspec 写法）。适合几乎每个改动都会碰、
# 由工具生成的文件。两个会话同时改它们时冲突由 git 解决。
unmanaged = ["pnpm-lock.yaml", "**/*.snap"]

[repos]
scan_depth = 4                # 从 workspace 根往下找仓库的最大深度。不想管的仓库用上面的 exempt 划出去

[bash]
reconcile = true              # 关掉后 Bash 改了什么不对账
git_guard = true              # 关掉后命令护栏不拦 git 命令
approve_tcas = true           # hook 自动批准 agent 可用的 tcas 命令（harness.md 第 5.5 节）
background_window_secs = 60   # 后台命令开着多久之内算正在进行的调用（harness.md 第 5.3 节）

[git_hooks]
enabled = true                # 打开 workspace 时给各仓库装 git 钩子（harness.md 第 5.6 节）
guard_stash = true            # 挂 reference-transaction 来拦 git stash；它在每次引用更新时触发
check_user = true             # 用户自己的提交、变基、stash 也检查；false 时只检查会话发起的

[lease]
explicit_idle_secs = 1800     # 事先申请的租约：路径干净且这么久没用到，就可以让出（leases.md 第 6 节）
waited_grace_secs = 600       # 等来的租约：授予后保留这么久
stalled_after_secs = 1800     # 有未提交改动、持有会话这么久没有活动，标为停滞
max_dir_files = 200           # 一条目录租约最多覆盖多少个被跟踪的文件（leases.md 第 4 节）

[brief]
require = false               # true 时 tcas lease acquire 不带 --brief 就拒绝（leases.md 第 5 节）
broadcast = true              # 有会话带着 brief 事先申请时，通知这个 workspace 里的其他会话
max_chars = 200
hint_after_files = 3          # 没有事先申请就改到第几个文件时提醒一次，0 为不提醒

[wait]
default_timeout_secs = 1800   # tcas lease wait 不带 --timeout 时
max_timeout_secs = 14400
```

别的根、以及根里某个仓库的根目录下，也可以各有一份 `.tcas.toml`，可以提交进那个仓库。它只能写 `unmanaged` 和 `exempt`，路径相对它所在的目录，和 workspace 级的合并。`mode`、`roots` 和其余设置只认 workspace 级的。

`.tcas.toml` 变了，tcasd 在下一次请求时重新读。`tcas workspace set mode` 改的是数据库里的值，优先于文件。

`.tcas.toml` 只有用户能改：会话用编辑工具或认得出的 Bash 命令改它会被拒绝（[harness.md](harness.md) 第 4 节）。里面的 `exempt`、`unmanaged`、`[bash]`、`[git_hooks]` 都能关掉保护，被挡住的会话不该能自己改。
