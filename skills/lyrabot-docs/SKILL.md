---
name: lyrabot-docs
description: 回答“这套系统怎么设计的、某个功能在哪、为什么这样做、部署在哪”等关于 LyraBot 自身的问题时读取，定位项目文档与关键文件。
---

# LyraBot 文档定位

LyraBot（lyrabot）是用 Rust 构建的常驻个人 agent。回答关于系统自身的问题时，先按下表找到对应文档再回答，不凭印象。

| 问题 | 服务器路径 | 仓库路径 |
|---|---|---|
| 目标、功能范围、不做什么、切换与清理计划 | `/opt/lyrabot/docs/00-PRD.md` | `docs/00-PRD.md` |
| 总体结构、技术栈、代码布局与文档索引 | `/opt/lyrabot/docs/01-architecture.md` | `docs/01-architecture.md` |
| 模型调用（kimi-code、思考等级、视频模态）、内核循环、上下文压缩 | `/opt/lyrabot/docs/03-model-kernel.md` | `docs/03-model-kernel.md` |
| 人格层的提示词布局、回复节奏、说法学习、记忆整理 | `/opt/lyrabot/docs/04-persona-memory.md` | `docs/04-persona-memory.md` |
| 派活、后台任务与专员、汇报投递、重启恢复 | `/opt/lyrabot/docs/05-orchestra.md` | `docs/05-orchestra.md` |
| 内置工具、生图/语音/搜索/grok、看视频、抖音 | `/opt/lyrabot/docs/06-tools-plugins.md` | `docs/06-tools-plugins.md` |
| QQ 收发与发件箱、聊天命令与输出排版 | `/opt/lyrabot/docs/07-platform-commands.md` | `docs/07-platform-commands.md` |
| 数据库表、清理、运行与稳定性、构建部署 | `/opt/lyrabot/docs/08-storage-operations.md` | `docs/08-storage-operations.md` |
| 还没做的事：待做方向、推迟、未采纳、未验证 | `/opt/lyrabot/docs/09-backlog.md` | `docs/09-backlog.md` |
| 为什么选这个方案（关键决策与备选） | `/opt/lyrabot/docs/02-decisions.md` | `docs/02-decisions.md` |
| 部署布局、更新与回退、切换记录 | `/opt/lyrabot/docs/deploy.md` | `deploy/README.md` |
| 角色身份与资料 | `/opt/lyrabot/persona/`（card.json、core/、notes/、runtime/） | `assets/persona/` |
| 后台执行者提示 | `/opt/lyrabot/agents/worker.md` | `assets/agents/worker.md` |
| 配置字段说明（示例） | `/opt/lyrabot/docs/lyrabot.example.toml` | `config/lyrabot.example.toml` |
| 实际运行配置 | `/etc/lyrabot/lyrabot.toml`（只读查看；`/etc/lyrabot/secrets/` 是凭据，不读取、不复述） | — |
| 部署的提交号 | `/opt/lyrabot/VERSION` | `git rev-parse --short=12 HEAD` |

文档较长时先用 fs_search 按关键词定位章节，再读相关片段。文档与代码或实际运行结果冲突时，以代码和运行结果为准，并如实说明差异。
