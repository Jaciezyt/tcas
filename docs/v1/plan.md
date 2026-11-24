# 实施计划

## 1. 技术栈

| | 选择 | 说明 |
|---|---|---|
| 语言 | Rust | `tcas hook` 每次工具调用都要起一个进程，要求启动在毫秒级 |
| tcasd 的并发 | 线程，每个连接一个；不用异步运行时 | 请求少（每次工具调用一两个），每个请求都很短，状态在 SQLite 里 |
| 存储 | SQLite（`rusqlite`，内嵌编译） | 单文件，事务保证租约判定的原子性 |
| git | 调用 `git` 命令行，不链接库 | 行为和用户自己用的 git 完全一致；本机一次调用约 7 到 10 毫秒 |
| 命令行解析 | `clap` | |
| 序列化 | `serde`、`serde_json`、`toml` | |
| 平台 | macOS 先做，Linux 跟上；不支持 Windows | 用到 Unix 套接字、进程启动时间 |

`tcas hook` 这条路径上不调用 git：路径规范化、读 `workspaces.json`、连套接字都在进程内完成，git 查询由 tcasd 做。

## 2. 代码结构

```
Cargo.toml                    工作区
crates/
  tcas-core/                  纯逻辑，不做 I/O
    path.rs                   路径规范化、豁免的目录、相交判定、路径所属的仓库
    lease.rs                  申请和释放的判定
    wait.rs                   等待、等待图和死锁检查
    shell.rs                  命令的静态解析：切词、cd、子 shell、包装层、bash -c
    guard.rs                  命令护栏的规则表
    githook.rs                git 钩子的判定
    reconcile.rs              两次快照的比较和归属
    proto.rs                  请求和应答的类型
  tcasd/
    main.rs                   启动、锁、套接字
    store.rs                  SQLite
    handlers/                 每个操作一个文件
    git.rs                    git 查询（按仓库，并行）
    repos.rs                  发现 workspace 里的仓库
    githooks.rs               在各仓库的 git 配置里加上、去掉那一行引用；探测 git 是否支持配置式钩子
    liveness.rs               会话存活检查
  tcas/
    main.rs                   命令
    client.rs                 连接、自动启动 tcasd
    hook/
      mod.rs                  公共流程、超时、失败即拒绝
      claude.rs               Claude Code 的输入输出格式
      agy.rs                  AGY 的输入输出格式
      codex.rs
    commit.rs
    githook.rs                tcas git-hook 的入口：读钩子的参数和环境，问 tcasd
plugins/
  claude/                     见 harness.md 第 1 节
  agy/                        见 harness.md 第 10 节
  codex/
tests/
  e2e/                        用真实的套接字和临时 git 仓库
  probe/                      探针 plugin 和实测脚本
```

`tcas-core` 不做 I/O，所以租约规则、路径、护栏、对账都能用普通的单元测试覆盖。git 状态以数据的形式传进去。

## 3. 里程碑

每个里程碑结束时都有一个能用的东西。

### 里程碑 1：租约核心

**内容**

- `tcas-core` 的路径（含豁免的目录和被 git 排除的路径）、租约判定（文件和目录）、期限、等待和死锁检查。
- workspace 有一个或几个根，每个根里自动发现仓库，git 查询在路径所属的仓库里做，从第一天就按这个结构写。
- tcasd：启动、锁、套接字、数据库、会话存活检查。
- 命令：`workspace open/add/remove/close/list`、`status`、`lease acquire/wait/release/list/who/grant/transfer`、`session list/end`、`exempt list/add/remove`、`daemon`、`log`。
- 身份用环境变量 `TCAS_SESSION` 手动指定，还没有 hook。这样建立的会话没有登记过进程，存活检查跳过它们，用 `tcas session end` 结束。

**完成的标志**：两个终端各设一个 `TCAS_SESSION`，按 [leases.md](leases.md) 的规则互相申请、拒绝、释放，包括按目录申请；一个终端等另一个持有的路径，对方释放后当场拿到；两个终端互相等时，后一个被拒；杀掉 tcasd 再启动，租约还在；打开一个装着两个仓库的目录，两个仓库都被发现。

### 里程碑 2：Claude Code 的编辑拦截

**内容**

- 把探针整理进 `tests/probe/`，做成可以重复跑的脚本，输出一份和 [harness.md](harness.md) 第 8 节对应的结果表。
- `tcas hook claude`：`SessionStart`、`UserPromptSubmit`、编辑工具的 `PreToolUse` 和 `PostToolUse`、`Read|Grep` 的 `PostToolUse`（只更新租约的使用时间，读取提示在里程碑 4）、`Stop`、`SubagentStop`、`SessionEnd`。
- plugin 目录、`bin/tcas` 转发脚本、`tcas plugin path`。
- 失败即拒绝的各条路径（[harness.md](harness.md) 第 7 节）。
- 拒绝理由里的等待命令；Bash 的 `PreToolUse` 里对 `tcas` 命令的处理（批准、不开窗口、把等待改成后台运行）。动手前先在交互模式下实测后台等待的唤醒和改写（[harness.md](harness.md) 第 9 节）。
- 通知队列和投递，先只用于等待的结果和租约让出；其余各类通知在里程碑 4。
- 改到 workspace 外面的检出：记下，每个（会话，检出）提示一次（[leases.md](leases.md) 第 3 节）。
- 会话结束的处理、遗留租约、续接后接回。

**完成的标志**：第 4 节用例 1 到 8、21 到 26、28 到 34、36 到 39、41、42、48、69、70、72、78、80 到 82、86、88、90、92 到 94、97 到 100、102、103 通过。此时已经可以日常使用，Bash 这一路还没有保护。

### 里程碑 3：Bash

**内容**

- 命令护栏：命令的解析和规则表，每条规则配一组应该拦和不应该拦的例子。
- git 钩子：安装和卸载，`pre-commit`、`post-commit`、`pre-rebase`、`reference-transaction`、三个事后通知的钩子。
- 快照、对账、归属、后台命令的窗口。快照覆盖 workspace 里的所有仓库。
- `tcas commit`，改动分属几个仓库时每个仓库提交一次。
- 分支变化的检测。

**完成的标志**：第 4 节用例 9 到 15、27、35、43 到 47、49 到 62、71、73 到 76、79、83 到 85、87、89、96、101、104 到 106 通过。

### 里程碑 4：提示、通知、安装

**内容**

- 读取提示，其余各类通知（[harness.md](harness.md) 第 6 节的表）。
- `SessionStart` 的状态注入，压缩后的重新注入。
- `skills/tcas/SKILL.md`。
- 先申请再动手：会话开始时的建议，改到第三个文件时的提醒。
- `tcas plugin install claude`、`tcas doctor`。
- `.tcas.toml`、`unmanaged`、`strict` 模式。
- 做完 [harness.md](harness.md) 第 9 节里 Claude Code 的未测项。

**完成的标志**：第 4 节用例 16 到 20、40、63 到 68、77、91、95 通过；在一个真实项目里同时开两三个会话用一周。

### 里程碑 5：统计

**内容**：`tcas stats`（[daemon-and-cli.md](daemon-and-cli.md) 第 5 节）。

**完成的标志**：拿到一周真实使用的数字，据此回答第 5 节的三个问题。

### 里程碑 6：AGY

**内容**：按 [harness.md](harness.md) 第 10 节先跑探针，再写 `hook/agy.rs`、plugin（`plugins/agy/`）以及 `tcas plugin install agy`、`tcas doctor` 的 AGY 检查。

**完成的标志**：一个 Claude Code 会话和一个 AGY 会话在同一个检出里，用例 1 到 15 中适用的部分通过。

### 里程碑 7：Codex

**内容**：按 [harness.md](harness.md) 第 11 节先跑探针，再写 `hook/codex.rs` 和 plugin。

**完成的标志**：一个 Claude Code（或 AGY）会话和一个 Codex 会话在同一个检出里，用例 1 到 15 中适用的部分通过。

里程碑 5、6、7 互不依赖，可以换顺序。

## 4. 验收用例

端到端测试用无界面模式（`claude -p … --plugin-dir`）跑固定的提示词，然后检查文件、`tcas status --json` 和事件日志。和模型怎么回答无关的断言才写进测试。

**编辑拦截**

| # | 做法 | 预期 |
|---|---|---|
| 1 | 会话 A 用编辑工具改 `a.txt` | 成功；A 持有 `a.txt`（`edit`） |
| 2 | A 持有 `a.txt` 且有未提交改动，会话 B 用编辑工具改它 | 被拒；文件内容不变；B 收到的理由里有 A 的会话 ID 和 brief 那一行，没有 A 的标题 |
| 3 | A 事先 `lease acquire src/`，B 改 `src/x.ts` | 被拒 |
| 4 | A 申请 `a.txt` 和 `b.txt`，`b.txt` 被 B 持有 | 退出码 3；A 一条也没拿到 |
| 5 | A 改了 `a.txt` 后结束，没有提交；B 改 `a.txt` | 被拒，理由说明是遗留租约 |
| 6 | 接上例，A 续接 | A 的租约回到 `active`；A 能继续改 |
| 7 | 接用例 5，用户 `git checkout a.txt` 后 B 再改 | 成功 |
| 8 | 停掉 tcasd 并让它无法启动，A 用编辑工具改文件 | 被拒，理由说明 tcasd 不可用；workspace 之外的文件照常能改 |

**Bash**

| # | 做法 | 预期 |
|---|---|---|
| 9 | A 用 Bash 写 `c.txt`（没人持有，没有别的会话在跑 Bash） | A 持有 `c.txt`（`bash`） |
| 10 | B 持有 `a.txt`，A 用 Bash 写 `a.txt` | 文件被改了（拦不住）；A 被告知不要再动它；B 的通知里有 A 那条命令的原文；事件 `bash.overreach` |
| 11 | B 持有租约时，A 运行 `git add -A` | 被拒 |
| 12 | 只有 A 一个会话时，A 运行 `git add -A` | 放行 |
| 13 | A 改了 `a.txt`，B 改了 `b.txt` 并 `git add b.txt`，A 运行 `tcas commit -m x` | 提交里只有 `a.txt`；`b.txt` 仍在暂存区；A 的 `a.txt` 租约已释放 |
| 14 | A 用后台 Bash 在 4 秒后写 `bg.txt` | 写入发生后的下一次对账里 A 持有 `bg.txt` |
| 15 | A 的 Bash 调用期间切换了分支 | 事件 `branch.changed`；B 收到通知 |

**提示、通知、配置**

| # | 做法 | 预期 |
|---|---|---|
| 16 | A 持有 `a.txt`，B 读它 | B 收到一次读取提示；再读不重复提示 |
| 17 | 接用例 2，A 提交 `a.txt` | B 在下一次工具调用后收到释放通知；B 再改成功 |
| 18 | 用户 `lease grant B src/y.ts` | B 下一次 hook 收到通知；A 改 `src/y.ts` 被拒 |
| 19 | `strict` 模式下 A 直接改一个没人持有的文件 | 被拒，提示先申请；申请后成功 |
| 20 | `.tcas.toml` 里 `unmanaged = ["pnpm-lock.yaml"]`，A、B 都改它 | 都成功，没有租约 |

**等待**

| # | 做法 | 预期 |
|---|---|---|
| 21 | A 持有 `a.txt` 且有未提交改动，B 用编辑工具改它 | 拒绝理由里有一条可以直接运行的 `tcas lease wait` 命令，并说明要作为后台命令运行 |
| 22 | 接上例，B 用后台 Bash 运行 `tcas lease wait a.txt`，接着做别的；A 随后提交 `a.txt`（里程碑 2 里用 `git commit -- a.txt`，靠 5 秒一次的检查发现；里程碑 3 之后用 `tcas commit`，当场发现） | B 启动等待的工具调用立刻返回；A 提交后 B 持有 `a.txt`（`wait`）；B 在当前工具调用结束后收到授予的通知；B 再改成功 |
| 23 | B 在后台运行 `tcas lease wait a.txt --timeout 3`，A 不释放 | 命令退出码 4；B 没有拿到任何租约，收到超时的通知；tcasd 里没有残留的等待 |
| 24 | A 持有 `a.txt` 并在等 `b.txt`；B 持有 `b.txt`，再等 `a.txt` | B 的命令立即返回，退出码 5，输出里说明了环；A 的等待不受影响 |
| 25 | B 在后台运行 `tcas lease wait a.txt` 后结束这一轮，A 提交 | B 被唤醒；hook 注入了授予的通知；B 持有 `a.txt` |
| 26 | A 结束后留下遗留租约 `a.txt`，B 等它，用户 `git checkout a.txt` | B 在 10 秒内拿到 |
| 27 | B 在等 `a.txt` 期间，C 用 Bash 写一个没人持有的文件 | C 取得它的租约（等待没有让归属变成不确定） |
| 28 | B 不设后台，直接运行 `tcas lease wait a.txt` | hook 把它改成后台运行：工具调用立刻返回，结果里有后台任务的编号 |
| 29 | B 等到 `a.txt` 后不去改它；A 在授予后 10 分钟内改 `a.txt`，10 分钟后再改一次 | 第一次被拒；第二次成功，B 的租约让出 |

**期限**（测试里把两个时长配成几秒）

| # | 做法 | 预期 |
|---|---|---|
| 30 | A `lease acquire src/` 后不碰它；期限内 B 改 `src/x.ts`，过了期限再改一次 | 第一次被拒；第二次成功，A 的租约让出，A 的通知队列里有一条 |
| 31 | A `lease acquire src/` 后每隔一段时间读 `src/` 下的文件；过了原来的期限 B 改 `src/x.ts` | 被拒（读算用到） |
| 32 | A `lease acquire a.txt` 并改了它，不提交；过了期限 B 改 `a.txt` | 被拒，不论过了多久 |
| 33 | 接上例，A 一直没有活动，超过停滞的时长 | B 收到的拒绝理由里说明持有者多久没有活动、要用户处理；`tcas status` 把这条租约标为停滞；A 再有活动后标记消失 |

**目录租约和先申请**

| # | 做法 | 预期 |
|---|---|---|
| 34 | A `lease acquire src/auth/`；B 用编辑工具改 `src/auth/x.ts`、新建 `src/auth/new.ts`、改 `src/util/y.ts` | 前两个被拒，理由里说明整个目录被占和 A 的 brief；第三个成功 |
| 35 | A 持有 `src/auth/`，用编辑工具改了 `src/auth/a.ts`；目录下另有一个不是 A 改的、有改动的文件；A 运行 `tcas commit -m x` | 提交前 A 名下有目录租约和 `a.ts` 的文件租约；提交里只有 `a.ts`；提交后文件租约释放，目录租约还在 |
| 36 | B 持有 `src/auth/b.ts` 且有未提交改动；A `lease acquire src/auth/` | 退出码 3；输出里列出 `src/auth/b.ts` 和持有者；A 什么都没拿到 |
| 37 | A 持有 `src/auth/` 并改了 `a.ts` 不提交，之后超过期限没再用到这个目录；B 先改 `src/auth/c.ts`，再改 `a.ts` | 第一个成功，A 的目录租约让出，A 收到通知；第二个被拒，`a.ts` 仍归 A |
| 38 | A 持有 `src/auth/` 并改了 `a.ts` 不提交，然后会话结束 | 目录租约释放；`a.ts` 变成遗留租约；B 能改 `src/auth/c.ts` |
| 39 | 把目录大小的限制配成 2；A `lease acquire src/auth/`（3 个文件）；用户 `lease grant A src/auth/` | 第一个被拒绝，输出里有文件数；第二个成功 |
| 40 | A 不申请，用编辑工具接连改 `src/auth/` 下的三个文件，再改第四个 | 第三个之后收到一次提醒，里面有 `tcas lease acquire src/auth/`；第四个之后不再提醒 |

**多个仓库**（workspace 是一个目录，里面有 `web/` 和 `api/` 两个仓库，根目录本身不是仓库）

| # | 做法 | 预期 |
|---|---|---|
| 41 | 打开这个目录；会话 A 从根目录启动，用编辑工具改 `web/a.txt` 和 `api/b.txt` | 两个仓库都被发现；A 持有两条租约，路径带着仓库所在的目录；B 改其中任何一个被拒 |
| 42 | A `lease acquire web/src/ api/src/user.ts`，其中 `api/src/user.ts` 被 B 持有 | 退出码 3；A 在两个仓库里都没拿到 |
| 43 | A 在 `web/` 和 `api/` 各改了一个文件，运行 `tcas commit -m x` | 两个仓库各有一个提交，说明相同，各自只含 A 改的文件；两条文件租约都释放 |
| 44 | 接上例的做法，但 `api/` 的提交钩子失败 | `web/` 已提交，它的租约释放；`api/` 没有提交，租约不变；退出码 1，输出里说明了两边的结果 |
| 45 | A 从根目录用 Bash 写 `api/c.txt`（没人持有） | 对账发现，A 持有 `api/c.txt`（`bash`） |
| 46 | `api/` 里有一个子模块 `api/vendor/proto/`；A 改子模块里的一个文件 | 租约的路径是 `api/vendor/proto/…`；干净与否在子模块里查；`api/` 的快照里不把这个文件算作 `api/` 的改动 |
| 47 | B 在 `api/` 持有租约、在 `web/` 没有人持有租约；A 运行 `git -C api add -A` 和 `git -C web add -A` | 第一条被拒，第二条放行 |
| 48 | 已经有 workspace `~/Code/app/web`，再打开 `~/Code/app` | 被拒绝，说明和哪个 workspace 重叠 |

**命令护栏**（workspace 的根是装着 `api/`、`web/` 的目录；B 在 `api/` 里持有租约；A 从这个目录运行）

| # | A 运行 | 预期 |
|---|---|---|
| 49 | `cd api && git stash` | 被拒，理由里说明作用到 `api/` |
| 50 | `(cd web && git status); git -C api add -A` | 被拒（括号里的 `cd` 不影响后面，`-C` 指向 `api/`） |
| 51 | `bash -c "cd api && git commit -am x"`；`env FOO=1 git -C api stash`；`/usr/bin/git -C api reset --hard` | 三条都被拒 |
| 52 | `git -C api commit -m "fix; cleanup" -- src/mine.ts`（文件是 A 自己的） | 放行（引号里的分号不是分隔符） |
| 53 | `git -C api $SUB`；`ls \| xargs git -C api add` | 被拒，提示把命令写成明确的形式 |
| 54 | `api/` 里配了别名 `ca = commit -a`，A 运行 `git -C api ca -m x` | 被拒 |

**git 钩子**

| # | 做法 | 预期 |
|---|---|---|
| 55 | 打开 workspace；仓库原来有自己的 `pre-commit` 钩子 | 仓库的 `.git/config` 里多了一行引用；提交时两个钩子都运行；仓库自己的钩子文件没被动过 |
| 56 | B 持有 `b.txt` 且有改动；A 运行一个脚本，脚本里 `git add -A && git commit -m x` | 命令护栏认不出，放行；提交被 `pre-commit` 拒绝，输出里列出 `b.txt` 和持有者 |
| 57 | 同上，由用户在终端里运行 `git commit -am x`；再运行 `TCAS_ALLOW=1 git commit -am x` | 第一次被拒；第二次成功，事件 `git.override` |
| 58 | A 改了 `a.txt`，用 `git commit -m x -- a.txt` 提交（不用 `tcas commit`） | 提交成功；A 在 `a.txt` 上的租约释放；等着 `a.txt` 的会话当场拿到 |
| 59 | A 结束后留下遗留租约 `a.txt`；用户提交了 `a.txt` | 租约当场释放，不用等轮询 |
| 60 | B 持有租约时，一个脚本里运行 `git stash`，另一个运行 `git rebase main` | 都被 git 拒绝；工作目录没有变化；`git stash pop` 不受影响 |
| 61 | 关闭 workspace | 各仓库里的那一行引用去掉；仓库原有的其他引用和钩子不变 |
| 62 | A 结束后留下遗留租约 `a.txt`；会话 B 运行 `git commit -m x -- a.txt`，然后用户运行同一条命令 | B 的被 `pre-commit` 拒绝；用户的成功，输出里说明这是哪个会话留下的 |

**brief**

| # | 做法 | 预期 |
|---|---|---|
| 63 | A `lease acquire src/auth/ --brief "X"`；B 用编辑工具改 `src/auth/a.ts`；B 读 `src/auth/b.ts`；B 运行 `tcas lease who src/auth/a.ts` | 三处都显示 A 的 brief `X`，都不显示 A 的标题 |
| 64 | 接上例，A 之后靠自动取得改了 `src/other.ts`；B 去改它 | 拒绝理由里显示的仍是 `X`（会话的 brief） |
| 65 | A 从没写过 brief，靠自动取得持有 `a.txt`；B 去改它 | 拒绝理由里是「没有写 brief」那一行，没有 A 的标题或用户消息 |
| 66 | A `lease acquire src/auth/ --brief "X"` 时 B、C 都在运行；A 再运行一次同样的命令 | B、C 各收到一次通知，带范围和 `X`；第二次不再通知 |
| 67 | A 持有 `a.txt`；B 运行 `tcas lease wait a.txt --brief "Y"` | A 的下一次 hook 收到「有人在等」的通知，带 `Y`；只通知一次 |
| 68 | brief 里有换行和超过 200 个字符的内容 | 只存第一行的前 200 个字符；显示时在引号里 |

**worktree**（仓库 `app` 有一个链接的 worktree）

| # | 做法 | 预期 |
|---|---|---|
| 69 | worktree 在 workspace 里（`app/.claude/worktrees/x`）；A、B 都从这个 worktree 启动，A 改 `src/a.ts`，B 改同一个文件 | A 的租约路径带着 worktree 的目录；B 被拒 |
| 70 | 接上例，C 从主检出启动，改主检出的 `src/a.ts` | 成功：和 worktree 里的不是同一个文件 |
| 71 | `.claude/` 被 git 排除；A 在 `app/.claude/worktrees/x` 里用 Bash 写一个文件 | 这个 worktree 仍被发现；对账给 A 记上租约 |
| 72 | worktree 在 workspace 外面（`../app-x`）；打开主检出；再 `tcas workspace add ../app-x` | 第一次的输出里列出了这个 worktree 和加进来的命令，这时在里面改文件不受约束；加进来之后同一个 worktree 里的两个会话互斥 |
| 73 | 主检出和外面的 worktree 各是一个 workspace；关闭其中一个，在另一个里提交别的会话持有的文件 | 提交仍被 `pre-commit` 拒绝，钩子没有被去掉；两个都关闭后那一行引用才去掉 |
| 74 | 只有 worktree 是 workspace；用户在主检出里 `git commit -a` | 钩子运行但立即放行，不连 tcasd |
| 75 | B 在 worktree 里持有租约，主检出里没人持有；A 在主检出里 `git rebase`，再在 worktree 里 `git rebase` | 第一次放行，第二次被拒 |
| 76 | A 在 worktree 里持有租约；用户用 `git worktree remove --force` 删掉它 | 下一次发现仓库时这个 worktree 下的租约全部释放，事件 `repo.gone` |

**agent 管理 workspace**

| # | 做法 | 预期 |
|---|---|---|
| 77 | 还没有 workspace；会话 A 在仓库 `app` 里运行 `tcas workspace open`；同一个目录里早就在运行的会话 B 去改 A 持有的文件 | 打开成功，`tcas workspace list` 显示是 A 打开的，git 钩子装上了；B 被拒，并在这次或下一次 hook 里收到用法说明 |
| 78 | A 运行 `tcas workspace open ~`；在一个含 9 个仓库的目录里运行 `tcas workspace open`；运行 `tcas workspace open --strict` | 三次都被拒，退出码 6，提示告诉用户 |
| 79 | A 打开了 workspace，B 在里面持有租约；A 运行 `tcas workspace close`；B 提交、释放并退出后 A 再运行一次 | 第一次被拒；第二次成功，git 钩子去掉 |
| 80 | workspace 是用户打开的；A 运行 `tcas workspace close`、`tcas workspace set mode auto`、`tcas lease release --force x`、`tcas daemon stop` | 都被拒，退出码 6 |
| 81 | A 运行 `env -u TCAS_SESSION -u CLAUDE_CODE_SESSION_ID tcas lease release --force x` | 仍然被认作 A（祖先进程里有它的会话进程），被拒 |
| 82 | workspace 的根是一个 worktree；用户用 `git worktree remove --force` 删掉它 | 下一次请求时这个 workspace 自动关闭，事件 `workspace.closed`，原因是目录不在了 |
| 83 | A 在受管理的仓库里运行 `git worktree add ../app-x`，然后 `tcas workspace add ../app-x` | 对账后 A 收到提示，说明在 workspace 外面建了 worktree；加进来成功 |

**几个不在一起的目录**（workspace 的根是 `app/`，并排有 `app-backend/`、`app-website/` 两个仓库；会话都从 `app/` 启动）

| # | 做法 | 预期 |
|---|---|---|
| 84 | 用户 `tcas workspace add ../app-backend`；A 用编辑工具改 `../app-backend/src/a.ts`，B 改同一个文件 | 后端仓库装上了 git 钩子；A 的租约显示为 `app-backend/src/a.ts`；B 被拒，拒绝理由里等待命令的路径是 `../app-backend/src/a.ts` |
| 85 | 接上例，A 运行 `echo x > ../app-backend/c.txt` | 对账发现，A 持有 `app-backend/c.txt`（`bash`） |
| 86 | A 运行 `tcas lease acquire src/auth/ ../app-backend/src/api/`，后端的那个目录里有 B 的租约 | 退出码 3，两个根里都没拿到 |
| 87 | A 在两个根里各改了一个文件，运行 `tcas commit -m x` | 两个仓库各有一个提交 |
| 88 | `app-website/` 没有加进来；A 用编辑工具改 `../app-website/index.html` 两次；用户运行 `tcas status` | 两次都放行；A 只收到一次提示，带 `tcas workspace add ../app-website`；状态里列出这个目录和次数 |
| 89 | B 在 `app-backend/` 里持有租约；A 运行 `cd ../app-backend && git stash` | 被命令护栏拒绝 |
| 90 | 用户 `tcas workspace add` 一个在已有的根里面的目录；会话 A 运行 `tcas workspace remove ../app-backend`，而它是用户加的 | 第一个被拒绝，说明和哪个根重叠；第二个被拒，退出码 6 |
| 91 | `app/.tcas.toml` 里写了 `roots = ["../app-backend"]`；关闭后重新 `tcas workspace open` | 后端目录自动成为根 |

**豁免的目录**

| # | 做法 | 预期 |
|---|---|---|
| 92 | 停掉 tcasd 并让它无法启动；workspace 里的会话 A 用编辑工具写 `/tmp/x.txt` 和 `$TMPDIR/y.txt` | 都成功，hook 没有连 tcasd |
| 93 | A 用编辑工具改 `/tmp/scratch-repo/a.txt`，那是一个 git 检出 | 放行，没有「加进 workspace」的提示，`tcas status` 里不列这个目录 |
| 94 | 用户和会话分别运行 `tcas workspace open /tmp/scratch-repo`；用 `/private/tmp/scratch-repo` 再试一次 | 四次都被拒绝，说明是哪一条豁免 |
| 95 | `.tcas.toml` 里 `exempt = ["scratch/"]`；A 持有 `src/`；B 改 `scratch/n.txt`；A 运行 `echo x > scratch/m.txt`；A 运行 `tcas lease acquire scratch/` | B 成功，没有租约；对账不给 A 记 `scratch/m.txt`；申请的输出说明这个目录不归 tcas 管 |
| 96 | A 用编辑工具改 `~/.tcas/config.toml` 和 workspace 根下的 `.tcas.toml`；A 运行 `echo x >> ~/.tcas/config.toml` 和 `echo x >> .tcas.toml`；A 运行 `tcas exempt add ~/Code` | 五次都被拒 |
| 102 | 在仓库里用编辑工具改 `.git/info/exclude`、`.git/config` | 放行，没有租约 |
| 103 | 用户 `tcas workspace open web/src`（`web/` 是一个检出） | 被拒绝，输出里给出 `web/` |
| 104 | A 在后台运行一条一直不退出的命令；61 秒后 B 用 Bash 新建一个没人持有的文件；C 去改 A 持有的、干净的隐式租约的文件 | B 取得这个文件的租约；C 成功，A 的租约让出 |
| 105 | A `lease acquire a.txt src/auth/`，改了 `a.txt` 和 `src/auth/x.ts` 并 `tcas commit`，没有释放；B 在后台等 `a.txt`；A 接着读别的文件，然后结束这一轮；用户再给 A 发一条消息 | A 这一轮结束之前 B 没有拿到；`Stop` 之后 B 当场拿到 `a.txt`，C 改 `src/auth/y.ts` 成功；A 下一轮的 `UserPromptSubmit` 注入的内容以让出的通知开头，列出 `a.txt` 和 `src/auth/` |
| 106 | 同上，但 A 没有提交，`a.txt` 有未提交改动；A 结束这一轮 | B 没有拿到，A 不被标为停滞，直到超过停滞的时长 |

**被 git 排除的路径**

| # | 做法 | 预期 |
|---|---|---|
| 97 | 仓库里 `dist/` 在 `.gitignore`，`local/` 在 `.git/info/exclude`，`*.bak` 在全局排除文件；A、B 先后用编辑工具改 `dist/out.js`、`local/n.txt`、`x.bak`，再新建 `dist/new.js` | 全部成功，没有租约，没有拒绝 |
| 98 | `dist/keep.js` 是被强制加进仓库的；A 改了它，B 去改 | B 被拒：被跟踪的文件不算被排除 |
| 99 | `.claude/` 被排除，里面有一个 worktree；A、B 都改 `.claude/worktrees/x/src/a.ts`；再都改 `.claude/settings.local.json` | 第一个文件 B 被拒；第二个文件两边都成功 |
| 100 | A 运行 `tcas lease acquire dist/ src/`；运行 `tcas lease who dist/out.js` | 只授予 `src/`，输出说明 `dist/` 不归 tcas 管；`who` 的输出里有是哪个文件的哪条规则 |
| 101 | A 用 Bash 新建 `src/new.ts`，对账给它记了租约；之后把 `src/new.ts` 加进 `.git/info/exclude`；B 去改它 | B 成功，A 的那条租约被释放 |

**不变量**（[leases.md](leases.md) 第 8 节）用随机操作序列做性质测试：任意顺序的申请、等待、释放、会话结束、续接之后，租约表里没有两条相交且属于不同会话的租约，等待图里没有环，每个改过没提交的文件都在改它的会话名下有一条文件租约。

## 5. 第一版要回答的问题

第一版的数据决定下一步做 design.md 里的哪一部分。

| 问题 | 看什么 | 影响 |
|---|---|---|
| agent 能不能在动手前说准要改哪些文件 | 改过的文件里事先在显式租约之下的比例；目录租约下实际改了多少 | 差得远：任务的影响面声明（design.md 第 4.4 节）不可靠，隔离要靠任务目录而不是事先声明 |
| 对账够不够 | Bash 越界和归属不确定的次数 | 多：先做沙箱或任务目录（design.md 第 6 节），再做别的 |
| 被拒之后怎么样 | 被拒次数、其中多少去等了、等了多久、超时和死锁各多少、遗留租约存在多久 | 超时和死锁多：先做预约和顺序（design.md 第 5.2 节） |

## 6. 风险

| 风险 | 后果 | 应对 |
|---|---|---|
| 被拒的 agent 改用 Bash 写同一个文件 | 互斥被绕过 | 拒绝理由里明确说不要这样做；对账会发现并通知双方；统计里看发生的频率 |
| 命令护栏误拦 | 正常的 git 命令被拒，打断工作 | 只在仓库里有别的会话持有租约时才拦；规则表配正反两组例子做测试；统计里看拒绝的次数和规则 |
| git 钩子挡住用户自己的提交 | 用户提交被拒 | 输出里写明是哪些文件、怎么只提交自己的、怎么放行；`check_user = false` 可以只检查会话；钩子连不上 tcasd 时放行 |
| git 钩子和仓库已有的钩子管理工具冲突 | 提交流程被破坏 | 用配置式钩子，不碰 `.git/hooks/` 和 `core.hooksPath`；实测两边并存。git 不支持配置式钩子时不装 |
| `reference-transaction` 钩子拖慢引用更新多的操作 | 大的变基变慢 | 钩子对无关的引用立即退出、不连 tcasd；里程碑 3 实测 `tcas git-hook` 的开销；可以单独关掉 |
| hook 的开销 | 每次编辑多一次进程启动和一次套接字往返；每次 Bash 多两次 `git status` | 目标：编辑的 hook 在 20 毫秒内，Bash 的两个 hook 各在 50 毫秒内（中等仓库）。里程碑 2、3 各测一次 |
| workspace 里仓库很多 | 每次 Bash 调用都要给每个仓库拍两次快照 | 各仓库并行（本机 12 个仓库约 30 毫秒）；不需要管的仓库用 `.tcas.toml` 的 `exempt` 划出去；里程碑 3 测一次 |
| 大仓库上 `git status` 慢 | Bash 调用变慢 | 建议开启 fsmonitor 和 untracked cache；`[bash] reconcile = false` 可关 |
| agent 清掉环境变量冒充用户 | 可以关闭 workspace、强制释放别人的租约 | `tcas` 另外用祖先进程认会话。实测五种运行方式里四种祖先链里有 harness 进程，剩下一种环境变量还在。两样同时去掉的蓄意绕过不防；还没登记的会话只靠环境变量 |
| 被 git 排除的文件被两个会话同时改 | 互相覆盖，没有任何提示（如 `.env.local`、本地的配置文件） | 这类文件本来就不进仓库，对账也看不到；`tcas lease who` 说明一条路径为什么不归管；要管就让 git 跟踪它 |
| 豁免的目录划得太宽 | 该管的文件没人管，两个会话同时改也不会被发现 | 内置的只有临时目录和工具自己的目录；包着 workspace 根的豁免会被拒绝；只有用户能改；`tcas doctor` 和 `tcas exempt list` 里列出每一条和来源 |
| agent 打开了用户不想管的目录 | 那里的仓库被装上 git 钩子，别的会话开始受约束 | 只能打开当前目录或所在的检出，检出数有上限；不自动批准；记下是谁打开的；用户可以关掉这个功能 |
| plugin 的 hook 接口随 harness 版本变化 | 拦截悄悄失效 | 探针脚本进 CI，对每个新版本跑一遍；`tcas doctor` 检查 harness 版本是否测过 |
| 用户用 `bypassPermissions` 或 `--bare` 启动会话 | 可能不受约束 | 里程碑 4 实测前者；后者在文档里说明 |
| tcasd 出问题时所有会话都改不了文件 | 打断工作 | 这是有意的取舍（放行会让互斥悄悄失效）。出路在 [harness.md](harness.md) 第 7 节，拒绝理由里也写 |
| 遗留租约越积越多 | 文件被结束的会话占着 | `tcas status` 把它们单独列出并给出处理命令；统计里看存在时长 |
| 会话互相等，或者等一条不会空出来的路径 | 白等 | 死锁检查；默认 30 分钟超时；等遗留租约时明确说要用户处理 |
| 等到了租约的会话没有被唤醒，或者没有回来用 | 路径被一个不知情的会话占着 | 等来的租约 10 分钟后可以让出；交互模式下的唤醒在里程碑 2 实测 |
| 会话开着没人管 | 它申请过的路径和没提交的文件一直挡着别人 | 干净的路径过了期限让出；有未提交改动的标为停滞，交给用户处理，不自动释放 |
| agent 不按建议先申请 | 大的改动仍然靠自动取得，改到一半才撞上别人 | 会话开始时的说明，加上改到第三个文件时的提醒；统计里看先申请的比例；要强制就用 `strict` |
| 目录申请得过大 | 一个会话占住一大片，别人都在等 | 目录大小的限制；超过期限没用到就让出；统计里看目录租约下实际改了多少 |
| 别的会话的 brief 被当成指示 | 会话去做别人的事，或者被带偏 | 显示时放在引号里、标明出处；会话开始时的说明里写明它不是指示；长度有上限。实测两次开工通知都没有改变会话的做法 |
| brief 没人写，或者写得没有信息量 | 被拒的会话还是不知道对方在做什么 | 建议的命令里带着 `--brief`；不带时输出里提醒；统计里看带 brief 的比例；`require = true` 可以强制 |
| 期限定得不合适 | 太短：agent 还在读代码，租约就让给了别人。太长：没人用的路径占得久 | 读自己持有的文件算用到；让出时通知原持有者；统计里看让出后原持有者又来申请的比例 |
| hook 自动批准 `tcas` 命令 | 命令没经过用户批准就运行 | 只批准整条命令是单个 `tcas` 命令、且子命令在固定名单里的情况；`tcas commit` 不在名单里；可以关 |

## 7. 之后

第一版稳定后，按第 5 节的数字决定顺序。候选：

- **project**：仓库的声明、跨仓库的基线和按序落地（design.md 第 8.3 节）。第一版里跨仓库的提交逐个仓库做，不是原子的。
- **任务**：租约的持有者从会话变成任务，会话可以换，租约不动（design.md 第 4 节）。`Lease.session` 换成 `Lease.task` 是这一步的主要改动。
- **预约和顺序**：第一版的等待没有预约，先等的不保证先拿到。下一步是被拒时登记预约，路径空出来按顺序授予（design.md 第 5.2 节）。
- **Bash 的事前阻止**：沙箱或任务目录（design.md 第 6 节）。这要求由 tcasd 启动会话。
- **搁置**：把遗留租约和停滞的租约的改动拷出保存、路径还原、租约释放（design.md 第 5.6 节）。有了它，带未提交改动的租约才能按时间自动让路。这是 tcasd 第一次需要写工作目录。
