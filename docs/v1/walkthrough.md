# 一次完整的并行过程

三个会话加上用户，在同一个 workspace 里并行工作 50 分钟。每一步列出 tcas 在哪里介入、做了什么、租约表怎么变。

这是照着本目录其余几份文档的规则在纸上走的一遍，走的时候发现两条规则对不上，已经在 [harness.md](harness.md) 里改掉（越界的改动由谁处理、用户提交遗留租约的文件是否放行）。tcas 还没有实现，下面的输出和提示文字都是按规则写出来的示意，提交号是编的。过程里依赖的、还没实测过的行为列在第 6 节。

下文的「hook」指 harness（Claude Code）的 hook，「git 钩子」指装在仓库里的 git hook。

## 1. 场景

`~/Code/app` 是一个 workspace，根目录本身不是仓库，里面有 `api/` 和 `web/` 两个仓库，模式是 `auto`。用户开了三个 Claude Code 会话，都从 `~/Code/app` 启动，自己也在这份检出里改东西。

下面出现两种对会话的描述：**标题**是用户发给它的第一条消息，只给用户看（`tcas status` 和用户看到的 git 钩子输出）；**brief** 是会话申请租约时自己写的一句话，给别的会话看。

| 谁 | 任务 | 要改的文件 |
|---|---|---|
| A `claude:a1f3…` | 会话校验从 cookie 换成 token | `api/src/auth/` 下几个文件、`web/src/api/client.ts`，后来还有 `api/src/routes/export.ts` |
| B `claude:b7c2…` | 加 CSV 导出 | `web/src/export/` 下两个文件、`api/src/routes/export.ts`，后来还有 `web/src/api/client.ts` 和 `web/src/utils/date.ts` |
| C `claude:c9d4…` | 修日期格式的 bug | `web/src/utils/date.ts` 和它的测试快照 |
| 用户 | 改 README 里的错字，处理卡住的地方 | `web/README.md` |

有三个文件两方都要改：`client.ts`（A、B）、`export.ts`（A、B）、`date.ts`（B、C）。

## 2. 时间线

| 时间 | A | B | C | 用户 |
|---|---|---|---|---|
| 09:00 | | | | 打开 workspace |
| 09:01 | 启动 | 启动 | | |
| 09:02 | 先申请 `api/src/auth/` 和 `client.ts` | | | |
| 09:03 | | 直接改了 3 个文件，被提醒后申请两个目录 | | |
| 09:04 | | | 启动，改 `date.ts`，跑测试 | |
| 09:08 | 改文件；一条 `sed` 越界改了 B 的 `export.ts` | 收到通知，决定保留 | | |
| 09:12 | | | 终端被关，留下遗留租约 | |
| 09:15 | | 改 `client.ts` 被拒，在后台等 | | |
| 09:20 | 改 `export.ts` 被拒，想等，被判死锁 | | | |
| 09:22 | 提交，放掉 `client.ts`；在后台等 `export.ts`，结束这一轮 | 等到 `client.ts` | | |
| 09:25 | | | | `git commit -a` 被拦，改成只提交自己的文件 |
| 09:30 | | 改 `date.ts` 被拒（遗留租约），在后台等，告诉用户 | | |
| 09:33 | | 被唤醒，等到 `date.ts` | | 看状态，把 C 留下的改动提交掉 |
| 09:40 | 被唤醒，等到 `export.ts` | 提交，放掉目录 | | |
| 09:45 | 改完，提交，释放 | | | |
| 09:50 | | | | 关闭 workspace |

## 3. 过程

每步后面的租约表只列变化：`+` 是新增，`-` 是释放，`~` 是状态变了。

### 步骤 1（09:00）：用户打开 workspace

```
$ cd ~/Code/app && tcas workspace open
opened ~/Code/app (mode auto), 2 repositories:
  api/    repo   main
  web/    repo   main
```

| 介入点 | tcas 做什么 |
|---|---|
| `tcas` 命令行 | 连不上 tcasd，先把它启动起来 |
| tcasd | 登记 `~/Code/app`；发现 `api/`、`web/` 两个仓库，记下各自的分支；更新 `~/.tcas/workspaces.json` |
| tcasd | 试一次本机的 git 是否支持配置式钩子，然后在两个仓库的 `.git/config` 里各加一行引用，装上 git 钩子 |

Claude Code 的 plugin 之前已经装好（`tcas plugin install claude`）。想让会话不经询问就能提交，要事先在 Claude Code 的权限设置里放行 `tcas commit`。

### 步骤 2（09:01）：A 和 B 启动

| 介入点 | tcas 做什么 |
|---|---|
| hook `SessionStart` | 把 `TCAS_SESSION` 写进会话的环境；向 tcasd 登记会话，带上 harness 进程的 PID 和启动时间；注入一段说明：这个目录由 TCAS 管理、有 `api/` 和 `web/` 两个仓库、三条用法、别的会话现在持有什么（此时没有） |
| hook `UserPromptSubmit` | 用户发出第一条消息时，取前 80 个字符作会话的标题。标题只出现在给用户看的地方（`tcas status`、用户自己的 git 操作被拒时），别的会话看不到 |

### 步骤 3（09:02）：A 先申请再动手

A 的任务要改一个目录下的好几个文件，按说明先申请：

```
$ tcas lease acquire api/src/auth/ web/src/api/client.ts --brief "Switch session validation from cookies to tokens; touches api/src/auth and the web API client"
granted: api/src/auth/ (directory, 6 tracked files), web/src/api/client.ts
```

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`（Bash） | 认出整条命令是 `tcas lease acquire`：直接批准，不开对账窗口 |
| `tcas lease acquire`，tcasd | 身份取自 `TCAS_SESSION`。在一个事务里判定两条路径：`api/src/auth/` 下有 6 个被跟踪的文件，没超过上限；两条路径上都没有别人的租约。全部授予，记下 brief，并给 workspace 里其他活着的会话（此时只有 B）留一条开工通知 |
| hook `PostToolUse`（B 的下一次工具调用） | 把通知交给 B：A 开始在 `api/src/auth/` 和 `web/src/api/client.ts` 上工作，以及它的 brief |

```
+ A  api/src/auth/            目录  cli
+ A  web/src/api/client.ts    文件  cli
  A 的 brief: "Switch session validation from cookies to tokens; touches api/src/auth and the web API client"
```

### 步骤 4（09:03）：B 直接开改，改到第三个文件时被提醒

B 没有申请，改了 `web/src/export/button.tsx`，新建了 `web/src/export/csv.ts`，又改了 `api/src/routes/export.ts`。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`（Edit、Write），每次 | 向 tcasd 申请这个文件。没人持有，`auto` 模式下自动授予并放行；记下一次进行中的编辑调用 |
| hook `PostToolUse`（Edit、Write），每次 | 关闭这次调用。第三次之后，B 名下有 3 条自动取得的文件租约，都不在它事先申请的范围里：注入一次提醒 |

```
tcas: you have now changed 3 files here without requesting leases first
(web/src/export/button.tsx, web/src/export/csv.ts, api/src/routes/export.ts).
For a change of this size, request the area before going further, so that a conflict
shows up now rather than halfway through:
  tcas lease acquire web/src/export/ api/src/routes/ --brief "<one line: what you are changing and why>"
```

B 照着运行了这条命令，brief 写的是 `Add CSV export: new web/src/export module and an export route in the API`。过程和步骤 3 一样，两个目录都授予，A 收到开工通知。B 之前自动取得的三条文件租约没有自己的 brief，别人看到的是会话的这一条。

```
+ B  web/src/export/button.tsx    文件  edit
+ B  web/src/export/csv.ts        文件  edit
+ B  api/src/routes/export.ts     文件  edit
+ B  web/src/export/              目录  cli
+ B  api/src/routes/              目录  cli
  B 的 brief: "Add CSV export: new web/src/export module and an export route in the API"
```

### 步骤 5（09:04）：C 启动，改一个文件，跑测试

C 改了 `web/src/utils/date.ts`，然后运行 `cd web && pnpm test date -u`，测试更新了一个快照文件。

| 介入点 | tcas 做什么 |
|---|---|
| hook `SessionStart` | 同步骤 2。这次注入的说明里列出了 A 和 B 各自的 brief 和持有的目录、文件 |
| hook `PreToolUse`（Edit） | `date.ts` 没人持有，自动授予 |
| hook `PreToolUse`（Bash） | 命令里没有 git，命令护栏放行。给两个仓库各拍一次 git 状态的快照，打开对账窗口 |
| hook `PostToolUse`（Bash） | 再拍一次快照来比：多了一个改动 `web/src/utils/__snapshots__/date.test.ts.snap`。它没人持有，这期间也没有别的会话在跑 Bash，算 C 改的，授予租约 |

```
+ C  web/src/utils/date.ts                           文件  edit
+ C  web/src/utils/__snapshots__/date.test.ts.snap   文件  bash
```

### 步骤 6（09:08）：A 改文件，一条 sed 改到了 B 的文件

A 用编辑工具改了 `api/src/auth/session.ts` 和 `web/src/api/client.ts`，然后用一条命令批量改名：

```
cd api && grep -rl readCookie src | xargs sed -i '' 's/readCookie/readToken/g'
```

范围写成了 `src` 而不是 `src/auth`，所以除了 `auth/` 下的 `token.ts`、`middleware.ts`，还改到了 B 持有的 `api/src/routes/export.ts`。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`（Edit） | `session.ts` 在 A 自己的目录租约之下：放行，另记一条文件租约。`client.ts` 是 A 自己申请的：放行 |
| hook `PreToolUse`（Bash） | 命令里没有 git，放行。拍快照，开窗口，记下命令原文 |
| hook `PostToolUse`（Bash） | 比出三个变化的文件。`token.ts`、`middleware.ts` 在 A 的目录租约之下：各记一条文件租约。`export.ts` 是 B 持有的，B 这期间没有在改它：判为越界。告诉 A 它改了 B 的文件、不要再动；给 B 留一条通知 |
| hook `PostToolUse`（B 的下一次工具调用） | 把通知交给 B |

这次写入 tcas 拦不住，也不去还原。怎么处理交给持有者 B：A 改不了这个文件，要撤销只能再越界一次。A 收到的是：

```
tcas: your command changed api/src/routes/export.ts, which is leased by another session
(claude:b7c2…). Their brief: "Add CSV export: new web/src/export module and an export route in the API"
Do not modify or restore it; that session has been told and will decide whether to keep the change.
```

B 收到的是：

```
tcas: api/src/routes/export.ts, which you hold, was changed by a command run by another
session (claude:a1f3…). Their brief: "Switch session validation from cookies to tokens;
touches api/src/auth and the web API client"
  cd api && grep -rl readCookie src | xargs sed -i '' 's/readCookie/readToken/g'
Check the file (git diff) and keep or undo that change.
```

B 看了 diff，对照 A 的 brief：这个函数确实改名了，保留。

```
+ A  api/src/auth/session.ts      文件  edit
+ A  api/src/auth/token.ts        文件  bash
+ A  api/src/auth/middleware.ts   文件  bash
```

### 步骤 7（09:12）：C 的终端被关掉

C 改完后在等用户确认，用户没有回答就把那个终端关了，改动没有提交。

| 介入点 | tcas 做什么 |
|---|---|
| hook `SessionEnd`；没触发的话，是 tcasd 每 15 秒一次的进程检查 | 按会话结束处理。C 的两个文件都有未提交的改动，两条租约变成遗留租约，继续挡着别人 |

```
~ C  web/src/utils/date.ts                           遗留
~ C  web/src/utils/__snapshots__/date.test.ts.snap   遗留
```

### 步骤 8（09:15）：B 要改 A 的文件，被拒，在后台等

B 需要在 `web/src/api/client.ts` 里加一个导出接口的调用。它先读了这个文件，然后去改。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PostToolUse`（Read） | 这个文件被 A 持有：提示 B 它正在被改，带上 A 的 brief，说明现在看到的可能是改到一半的内容。只提示一次 |
| hook `PreToolUse`（Edit） | A 的租约不让出，因为文件有未提交的改动。拒绝，理由里带着等待的命令 |
| hook `PreToolUse`（Bash） | B 运行了等待的命令，没有设成后台：批准，同时把这次调用改成后台运行，工具调用立刻返回 |
| `tcas lease wait`，tcasd | 检查会不会死锁：B 等 A，A 没在等谁，不会。登记等待，连接保持着。给 A 留一条通知：有人在等 `client.ts`，带着 B 写的 brief |
| hook `PostToolUse`（A 的下一次工具调用） | 把这条通知交给 A |

```
tcas: web/src/api/client.ts is leased by another session (claude:a1f3…, since 09:02).
  Their brief: "Switch session validation from cookies to tokens; touches api/src/auth and the web API client"
It has uncommitted work in progress. Do not modify it by any other means, including Bash.
If their brief already covers the change you were about to make, do not wait: tell the user.
To wait for it, run this as a background command (Bash tool, run_in_background):
  tcas lease wait web/src/api/client.ts --brief "<one line: what you need it for>"
Then keep working on something that does not depend on this file, or end your turn. You will
be told when the lease is yours. Otherwise tell the user.
```

A 在换校验方式，B 要加的是一个新接口的调用，不是同一件事，所以 B 去等：`tcas lease wait web/src/api/client.ts --brief "Add the export endpoint call to the API client"`，然后接着写 `csv.ts`。租约表没有变化，多了一个等待：B 等 `web/src/api/client.ts`。A 这时已经知道有人在等这个文件、等它做什么。

### 步骤 9（09:20）：A 要改 B 的文件，被拒，想等，被判死锁

A 发现 `api/src/routes/export.ts` 里除了改名还要多传一个参数，去改它。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`（Edit） | 这个文件是 B 的：有未提交的改动，还在 B 的目录租约之下。拒绝，给出等待的命令 |
| `tcas lease wait`，tcasd | A 要等 B，而 B 正在等 A 持有的 `client.ts`，登记了就成环。不登记，命令立即以退出码 5 结束 |
| 下一个触发的 hook | 把结果注入给 A。后台命令的输出不会自己进入上下文 |

```
deadlock: not waiting.
  claude:b7c2… holds api/src/routes/export.ts and is waiting for web/src/api/client.ts, which you hold.
Commit or release web/src/api/client.ts first, or tell the user.
```

### 步骤 10（09:22）：A 提交，放出 B 在等的文件

A 手上的改动已经可以提交。它先试了 `cd api && git add -A && git commit -m "…"`，被拒后改用 `tcas commit`。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`（Bash） | 命令护栏顺着 `cd api` 认出这是 `api/` 里的 `git add -A`，而 `api/` 里有 B 的租约：拒绝，提示改用 `tcas commit` |
| hook `PreToolUse`（Bash） | `tcas commit` 不自动批准，是否放行由 harness 自己的权限规则决定 |
| `tcas commit`，tcasd | 取 A 改过的文件：`api/` 里 3 个，`web/` 里 1 个。不包括 `export.ts`，它不在 A 名下 |
| `tcas commit` | 两个仓库各做一次只含这些路径的提交，用同一条说明 |
| git 钩子 `pre-commit`，两个仓库各一次 | 这次提交的路径都在 A 的租约之下：放行 |
| git 钩子 `post-commit` | 告诉 tcasd 提交了什么。A 自动取得的 3 条文件租约释放；事先申请的两条保留 |

```
$ tcas commit -m "auth: switch session check from cookie to token"
api   8b10c4e   3 files   api/src/auth/session.ts, api/src/auth/token.ts, api/src/auth/middleware.ts
web   3f2a91c   1 file    web/src/api/client.ts
released: 3 file leases.  still held: api/src/auth/ (directory), web/src/api/client.ts
```

`auth/` 里还有测试要补。A 在步骤 8 就被告知 B 在等 `client.ts`，现在这个文件已经提交，它只放掉这一个，然后去等 `export.ts`：

| 介入点 | tcas 做什么 |
|---|---|
| `tcas lease release web/src/api/client.ts`，tcasd | 释放，并在同一个事务里把它授予正在等的 B |
| B 的 `tcas lease wait` | 以退出码 0 结束 |
| hook `PostToolUse`（B 当前的工具调用结束时） | 把授予的通知交给 B：等了 7 分钟；文件在等待期间被提交过（`3f2a91c`），改之前重读 |
| `tcas lease wait api/src/routes/export.ts`，tcasd | 这次不成环，B 已经不在等了。登记 |
| hook `Stop` | A 没有别的事可做，结束这一轮，状态变成 `idle`。它正在等待，不算停滞 |

```
- A  api/src/auth/session.ts      已提交
- A  api/src/auth/token.ts        已提交
- A  api/src/auth/middleware.ts   已提交
- A  web/src/api/client.ts        主动释放
+ B  web/src/api/client.ts        文件  wait
```

A 还持有 `api/src/auth/`，并且在等 `api/src/routes/export.ts`。B 在 A 开始等的时候收到通知，知道 A 在等这个文件。A 要是没有运行那条 `release`，B 也只多等到 A 这一轮结束：`client.ts` 是 A 事先申请、已经提交的，A 的 `Stop` 一到就让给 B，A 下一轮开始时被告知（[leases.md](leases.md) 第 6 节「提交过的显式租约」）。

### 步骤 11（09:25）：用户自己提交，被 git 钩子拦下

用户改了 `web/README.md` 里的一个错字，顺手运行 `git -C web commit -am "docs: typo"`。

| 介入点 | tcas 做什么 |
|---|---|
| git 钩子 `pre-commit` | 没有会话身份，按用户处理。`-a` 带上了 `web/` 里所有改过的文件，其中两个是 B 正在改的：拒绝，列出是哪些 |
| git 钩子 `pre-commit` | 用户改成 `git -C web commit -m "docs: typo" -- README.md`：只有一个没人持有的文件，放行 |
| git 钩子 `post-commit` | 报告这次提交。没有租约受影响 |

```
tcas: commit refused. It includes files that other sessions are still working on:
  src/export/button.tsx    leased by claude:b7c2… "加 CSV 导出"
  src/api/client.ts        leased by claude:b7c2… "加 CSV 导出"
Commit only your own files:   tcas commit -m "…"    or    git commit -m "…" -- <paths>
To commit them anyway:        TCAS_ALLOW=1 git commit …      (git commit --no-verify also skips this)
```

`-a` 还带上了 C 留下的 `date.ts`。它是遗留租约，对用户不拦，但这次提交因为 B 的两个文件整个被拒。B 新建的 `csv.ts` 还没被跟踪，`-a` 不会带上它。

### 步骤 12（09:30）：B 要改 C 留下的文件，被拒，等，告诉用户

B 拿到 `client.ts` 后重读了它，加上了导出接口的调用。接着，导出的文件名里要带日期，B 去改 `web/src/utils/date.ts`。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`（Edit） | 这个文件上是 C 的遗留租约。当场查了一次，文件仍有未提交的改动：拒绝。理由里说明原会话已经结束，要用户处理之后才会空出来 |
| `tcas lease wait`，tcasd | B 在后台等它，登记 |
| hook `Stop` | B 把情况告诉用户，结束这一轮，状态变成 `idle` |

```
tcas: web/src/utils/date.ts is held by a session that has ended (claude:c9d4…, ended 09:12).
They had not written a brief. The file still has uncommitted changes. It stays leased until the user commits or discards those
changes, transfers the lease, or force-releases it. Tell the user.
To be told when it is free, run this as a background command:
  tcas lease wait web/src/utils/date.ts --brief "<one line: what you need it for>"
```

C 一次都没有申请过，所以没有 brief。B 看不到 C 的标题，那是用户发给 C 的消息；用户在下一步的 `tcas status` 里看得到。

### 步骤 13（09:33）：用户看状态，把 C 留下的改动提交掉

```
$ tcas status
workspace  ~/Code/app   mode auto
  api/    main
  web/    main

SESSION          STATE   LAST   TITLE                 LEASES
claude:a1f3…     idle    11m    会话校验换成 token      1
claude:b7c2…     idle    3m     加 CSV 导出             6
claude:c9d4…     ended   21m    修日期格式              2 (orphaned)

claude:a1f3…  会话校验换成 token
  api/src/auth/                  cli    09:02   dir, 6 files, used 11m ago
  brief: "Switch session validation from cookies to tokens; touches api/src/auth and the web API client"
  waiting 11m for api/src/routes/export.ts    (held by claude:b7c2…)

claude:b7c2…  加 CSV 导出
  web/src/export/button.tsx      edit   09:03   modified
  web/src/export/csv.ts          edit   09:03   new
  api/src/routes/export.ts       edit   09:03   modified
  web/src/api/client.ts          wait   09:22   modified
  web/src/export/                cli    09:03   dir, 4 files, used 5m ago
  api/src/routes/                cli    09:03   dir, 5 files, used 24m ago
  brief: "Add CSV export: new web/src/export module and an export route in the API"
  waiting 3m for web/src/utils/date.ts    (held by claude:c9d4…, ended)

claude:c9d4…  修日期格式   (ended 09:12; uncommitted changes remain)
  web/src/utils/date.ts                            edit   09:04
  web/src/utils/__snapshots__/date.test.ts.snap    bash   09:05
  → commit or discard the changes, or: tcas lease transfer <path> --to <session>
```

用户看了 C 留下的 diff，改得没问题，自己提交：

```
$ git -C web commit -m "fix: date format" -- src/utils/date.ts src/utils/__snapshots__/date.test.ts.snap
```

| 介入点 | tcas 做什么 |
|---|---|
| `tcas status`，tcasd | 现查每条租约的文件是否干净；列出会话、租约、谁在等什么、遗留租约和处理的办法 |
| git 钩子 `pre-commit` | 提交的是遗留租约的文件，发起的是用户：放行，打印一行说明这是 C 留下的 |
| git 钩子 `post-commit` | 文件变干净，C 的两条遗留租约释放；`date.ts` 当场授予正在等的 B |
| B 的 `tcas lease wait` 结束 | harness 把空闲的 B 唤醒 |
| hook `UserPromptSubmit`（唤醒时） | 把授予的通知交给 B：等了 3 分钟；文件刚被提交过，改之前重读 |

```
- C  web/src/utils/date.ts                           已提交（用户）
- C  web/src/utils/__snapshots__/date.test.ts.snap   已提交（用户）
+ B  web/src/utils/date.ts                           文件  wait
```

用户还有两个别的办法：`tcas lease transfer web/src/utils/date.ts --to b7c2`，让 B 接着 C 改到一半的内容做；或者 `claude --resume` 把 C 续接回来，它的遗留租约自动接回。

### 步骤 14（09:34 到 09:40）：B 改完，提交，放掉目录，A 被唤醒

B 重读并改了 `date.ts`，跑了测试，然后提交。

| 介入点 | tcas 做什么 |
|---|---|
| hook `PreToolUse`、`PostToolUse`（Edit、Bash） | 和前面一样。`date.ts` 现在是 B 等来的，放行；跑测试时开窗口、对账，没有意外的改动 |
| `tcas commit` 和 git 钩子 | `api/` 提交 1 个文件，`web/` 提交 4 个。B 名下 5 条文件租约全部释放，两条目录租约保留 |

```
$ tcas commit -m "export: CSV export"
api   5d77a02   1 file    api/src/routes/export.ts
web   c41e9b3   4 files   web/src/export/button.tsx, web/src/export/csv.ts, web/src/api/client.ts, web/src/utils/date.ts
released: 5 file leases.  still held: web/src/export/ (directory), api/src/routes/ (directory)
```

这时 A 仍然拿不到 `export.ts`：文件租约没了，但它还在 B 的目录租约 `api/src/routes/` 之下。B 知道 A 在等，按提示放掉目录：

| 介入点 | tcas 做什么 |
|---|---|
| `tcas lease release --all`，tcasd | 放掉两个目录。`export.ts` 上已经没有别人的租约，当场授予等了 18 分钟的 A |
| A 的 `tcas lease wait` 结束 | harness 把空闲的 A 唤醒 |
| hook `UserPromptSubmit`（唤醒时） | 把通知交给 A：文件在等待期间被提交过（`5d77a02`），改之前重读 |

```
- B  5 条文件租约                  已提交
- B  web/src/export/              主动释放
- B  api/src/routes/              主动释放
+ A  api/src/routes/export.ts     文件  wait
```

### 步骤 15（09:41 到 09:50）：A 收尾，全部结束

A 重读 `export.ts`，改完，跑测试，提交，释放。用户退出两个会话，关闭 workspace。

| 介入点 | tcas 做什么 |
|---|---|
| hook 和 git 钩子 | 同前。A 提交后 `export.ts` 上等来的租约释放 |
| `tcas lease release --all`，tcasd | 放掉 `api/src/auth/` |
| hook `SessionEnd`（A、B） | 两个会话名下都没有租约，直接标为结束 |
| `tcas workspace close` | 去掉两个仓库 `.git/config` 里的那一行引用，workspace 取消登记。tcasd 留在后台 |

这 50 分钟在 `tcas stats` 里对应的数字：

| 数字 | 这次 |
|---|---|
| 编辑被拒 | 3 次：`client.ts`（B）、`export.ts`（A）、`date.ts`（B） |
| 等待 | 4 次。3 次等到，分别等了 7、3、18 分钟；1 次被判死锁 |
| 改之前已经在事先申请的范围里的文件 | A：5 个里 4 个，另 1 个是等来的。B：5 个里 0 个，3 个靠自动取得，2 个是等来的 |
| 提醒先申请 | 1 次，之后申请了 |
| 带 brief 的事先申请 | 2 次里 2 次。C 没有申请过，所以没有 brief |
| Bash 越界 | 1 次 |
| 命令护栏拒绝 | 1 次（`git add -A`） |
| git 钩子拒绝 | 1 次（用户的 `git commit -a`） |
| 遗留租约 | 2 条，存在了 21 分钟 |

## 4. tcas 的介入点

| 介入点 | 什么时候 | 做什么 | 步骤 |
|---|---|---|---|
| hook `SessionStart` | 会话启动、续接、压缩之后 | 写会话身份，登记，注入说明和别人持有的路径 | 2、5 |
| hook `UserPromptSubmit` | 用户发消息；后台命令结束唤醒会话 | 记标题；投递通知 | 2、13、14 |
| hook `PreToolUse`（编辑工具） | 每次改文件之前 | 申请租约：自动授予并放行，或者拒绝，给出持有者的 brief 和等待的命令 | 4、5、6、8、9、12 |
| hook `PostToolUse`（编辑工具） | 每次改完 | 关闭调用；提醒先申请；投递通知 | 4 |
| hook `PostToolUse`（读文件） | 读完 | 读取提示；更新自己租约的使用时间 | 8 |
| hook `PreToolUse`（Bash） | 每条命令之前 | `tcas` 命令：批准，等待改成后台。其余：命令护栏，拍快照开窗口 | 3、5、6、8、10 |
| hook `PostToolUse`（Bash） | 命令结束 | 再拍快照对账：记租约，发现越界；投递通知 | 5、6 |
| hook `Stop` | 一轮结束 | 关闭还开着的调用；会话变空闲 | 10、12 |
| hook `SessionEnd` | 会话退出 | 干净的租约释放，其余变成遗留租约 | 7、15 |
| `tcas lease acquire` | agent 做较大的改动之前 | 全有或全无地授予文件和目录；记下 brief，通知别的会话 | 3、4 |
| `tcas lease wait` | agent 被拒之后，在后台 | 检查死锁，登记，通知持有者有人在等；路径空出来时当场授予 | 8、9、10、12 |
| `tcas lease release` | agent 用完之后 | 释放，并把路径授予正在等的会话 | 10、14、15 |
| `tcas commit` | agent 提交 | 每个仓库只提交本会话改过的文件 | 10、14、15 |
| git 钩子 `pre-commit` | 任何人在这些仓库里提交 | 提交里有别的会话正在改的文件就拒绝 | 10、11、13、14 |
| git 钩子 `post-commit` | 提交之后 | 释放已提交文件上的租约，包括遗留租约 | 10、13、14 |
| tcasd 自己 | 一直在后台 | 每 15 秒查一次会话的进程；租约表一变就重新判定所有等待 | 7、10、13、14 |
| 用户的命令 | 用户需要时 | `workspace open`、`status`、`workspace close`；另有 `lease transfer`、`lease release --force` | 1、13、15 |

## 5. 主线没走到的情况

| 情况 | tcas 做什么 | 见 |
|---|---|---|
| tcasd 起不来 | workspace 里的编辑和 Bash 调用全部被拒，理由里写明原因；git 钩子放行 | [harness.md](harness.md) 第 7 节 |
| `strict` 模式 | 没有事先申请的编辑直接被拒 | [leases.md](leases.md) 第 5 节 |
| 申请了没用，超过 30 分钟 | 别的会话来申请时让出，原持有者收到通知 | [leases.md](leases.md) 第 6 节 |
| 会话开着但 30 分钟没有活动，文件没提交 | 标为停滞，提示用户处理 | [leases.md](leases.md) 第 6 节 |
| 等待超时 | 30 分钟没等到，命令以退出码 4 结束，会话被告知 | [leases.md](leases.md) 第 7 节 |
| 有人变基、stash、切分支 | 命令护栏事前拦；变基和 stash 另有 git 钩子拦；切分支只能事后通知 | [harness.md](harness.md) 第 5 节 |
| 会话续接 | 它的遗留租约自动接回 | [leases.md](leases.md) 第 6 节 |
| 压缩上下文之后 | `SessionStart` 重新注入说明和自己持有的路径 | [harness.md](harness.md) 第 6 节 |
| 子 agent | 算在父会话名下，互相不隔离 | [leases.md](leases.md) 第 9 节 |
| 项目的目录不在一起，主目录外面还有后端、网站目录 | 用 `tcas workspace add` 放进同一个 workspace；没加的，会话改到时提示一次 | [leases.md](leases.md) 第 3 节 |
| AGY、Codex 会话 | 还没有接入 | [harness.md](harness.md) 第 10、11 节 |

## 6. 这次过程里能看到的问题

**提交之后忘了放事先申请的租约。** 步骤 10 和 14 里，A 和 B 提交后都按输出里的提示运行了 `tcas lease release`，等着的一方当场拿到。agent 没有这样做时，按「提交过的显式租约」的规则（[leases.md](leases.md) 第 6 节），持有者这一轮还在进行就保留，这一轮结束、有人在等就让出：

- 步骤 10 里，A 在 09:22 提交后去等 `export.ts`、结束这一轮。`Stop` 一到，`client.ts` 让给 B，和运行了 `release` 只差几秒。A 下一轮（09:40 被唤醒时）先被告知 `client.ts` 已经给了 B。
- 步骤 14 里，B 提交后要是接着做别的、没有结束这一轮，A 要等到 B 这一轮结束。这段时间 B 还在工作，可能还要改这个目录，所以保留是有意的。

这条规则是 2026-11-23 定的。之前的版本里，持有者不放就要多等 30 分钟。

**提交之间，主线可能暂时不自洽。** 步骤 10 之后，`api/` 的 `HEAD` 里已经没有 `readCookie`，而 `export.ts` 里对应的改名还在 B 手里没提交，直到步骤 14。第一版不管提交的先后和验证（design.md 第 7 节）。

**工作目录是共用的。** B 在步骤 8 之后跑的测试，用的是 A 改到一半的 `auth/`。读取提示告诉了 B 这些文件正在被改，但测试结果仍然可能受影响（design.md 第 6 节）。

**依赖还没实测的行为**

| 步骤 | 依赖什么 | 现状 |
|---|---|---|
| 8 | hook 把前台的 `tcas lease wait` 改成后台运行 | 无界面模式下实测可行，交互模式没测 |
| 13、14 | 后台命令结束时，空闲的会话被唤醒 | 同上。交互模式下不唤醒的话，结果要到用户下一次在那个会话里输入才送到 |
| 7 | 关掉终端时触发 `SessionEnd` | 没测。不触发也有兜底：tcasd 在 15 秒内发现进程不在 |
| 全部 | tcas 本身 | 还没有实现。命令护栏的解析、对账的归属判断都只有规则，没有跑过 |
