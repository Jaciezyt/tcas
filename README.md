# TCAS

以任务调度为核心的多 Agent 系统：让多个 coding agent 在同一个项目里并行工作而不相撞。

主页：[tcas.io](https://tcas.io)

## 状态

第一版（只做租约这一层）的里程碑 1 到 6 已实现：`tcas`、`tcasd`，以及 Claude Code 和 AGY（Antigravity CLI）的 plugin。里程碑 7（Codex）还没做。完整设计见 [docs/design.md](docs/design.md)，第一版的实现文档见 [docs/v1/](docs/v1/README.md)。

## 核心想法

- **调度优先于角色**。agent 是同质的 worker，差异只来自上下文、权限和输出要求。
- **冲突在开工前解决**。任务先声明影响面，调度器据此发放文件租约，决定并行还是排序。
- **隔离和验证按需分级**，每次选够用的最便宜一级。
- **状态在任务板上**，不在任何 agent 的上下文里。所有 agent 掉线时系统仍然可查看、可恢复。
- **使用现成的 harness**（Claude Code、Codex、Antigravity CLI）作为 worker，本项目只做控制面和每台机器上的 daemon。

## 部件

第一版只有两个程序，通过 plugin 接入用户自己启动的 harness 会话，不做任务调度：

| 名称 | 是什么 |
|---|---|
| `tcas` | 命令行。用户用它开 workspace、看状态、干预；agent 用它申请和释放租约；harness 的 hook 和 git 钩子也调用它（`tcas hook`、`tcas git-hook`） |
| `tcasd` | 本机 daemon。保存 workspace、会话、租约和事件日志，所有判定都在这里做 |

完整设计里的 `tcas-hook`（第一版并进了 `tcas hook`）和 `tcas-mcp` 留到之后。

## 代码

| 目录 | 内容 |
|---|---|
| `crates/tcas-core` | 纯逻辑，不做 I/O：路径、租约判定、等待图、命令解析和护栏规则、协议类型 |
| `crates/tcas-sys` | 操作系统相关：时钟、进程、文件系统、`~/.tcas` |
| `crates/tcasd` | daemon：存储、处理请求、git 查询、对账、git 钩子 |
| `crates/tcas` | 命令行和两个程序的入口；`tests/` 是用真实套接字和临时 git 仓库的端到端测试 |
| `plugins/claude`、`plugins/agy` | harness 的 plugin |
| `tests/probe` | 探针 plugin 和实测脚本，结果在 `tests/probe/RESULTS.md` |

```
cargo build --release      # target/release/tcas、tcasd
cargo test --workspace     # 单元测试、性质测试和端到端测试
```
