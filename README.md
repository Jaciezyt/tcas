# TCAS

以任务调度为核心的多 Agent 系统：让多个 coding agent 在同一个项目里并行工作而不相撞。

主页：[tcas.io](https://tcas.io)

## 状态

设计阶段，还没有代码。完整设计见 [docs/design.md](docs/design.md)。

## 核心想法

- **调度优先于角色**。agent 是同质的 worker，差异只来自上下文、权限和输出要求。
- **冲突在开工前解决**。任务先声明影响面，调度器据此发放文件租约，决定并行还是排序。
- **隔离和验证按需分级**，每次选够用的最便宜一级。
- **状态在任务板上**，不在任何 agent 的上下文里。所有 agent 掉线时系统仍然可查看、可恢复。
- **使用现成的 harness**（Claude Code、Codex、Antigravity CLI）作为 worker，本项目只做控制面和每台机器上的 daemon。

## 部件

| 名称 | 是什么 |
|---|---|
| `tcas` | 命令行，用户和 agent 都用它操作任务板 |
| `tcasd` | 每台机器上的 daemon：启动会话、执行租约、收集变更、管理验证 |
| `tcas-hook` | 各家 harness 共用的 hook 程序 |
| `tcas-mcp` | 提供给 agent 的 MCP 服务 |

## 下一步

第一版只做租约这一层：`tcas` 和 `tcasd` 两个程序，通过 plugin 接入用户自己启动的 Claude Code 会话，不做任务调度。实现文档见 [docs/v1/](docs/v1/README.md)。
