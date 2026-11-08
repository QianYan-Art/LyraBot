---
name: qya-docs
description: 回答“这套系统怎么设计的、某个功能在哪、为什么这样做、部署在哪”等关于 qy-assistant 自身的问题时读取，定位项目文档与关键文件。
---

# qy-assistant 文档定位

qy-assistant（qya）是阿颜用 Rust 自研的个人 agent，唐语歌是它的门面。回答关于系统自身的问题时，先按下表找到对应文档再回答，不凭印象。

| 问题 | 服务器路径 | 仓库路径 |
|---|---|---|
| 目标、功能范围、不做什么、切换与清理计划 | `/opt/qya/docs/00-PRD.md` | `docs/00-PRD.md` |
| 技术栈、模块划分、内核循环、提示词布局、多代理、工具、QQ、存储、命令、部署 | `/opt/qya/docs/01-architecture.md` | `docs/01-architecture.md` |
| 为什么选这个方案（关键决策与备选） | `/opt/qya/docs/02-decisions.md` | `docs/02-decisions.md` |
| 部署布局、更新与回退、切换记录 | `/opt/qya/docs/deploy.md` | `deploy/README.md` |
| 唐语歌身份与资料 | `/opt/qya/persona/`（card.json、core/、notes/、runtime/） | `assets/persona/` |
| 后台执行者提示 | `/opt/qya/agents/worker.md` | `assets/agents/worker.md` |
| 配置字段说明（示例） | `/opt/qya/docs/qya.example.toml` | `config/qya.example.toml` |
| 实际运行配置 | `/etc/qya/qya.toml`（只读查看；`/etc/qya/secrets/` 是凭据，不读取、不复述） | — |
| 部署的提交号 | `/opt/qya/VERSION` | `git rev-parse --short=12 HEAD` |

文档较长时先用 fs_search 按关键词定位章节，再读相关片段。文档与代码或实际运行结果冲突时，以代码和运行结果为准，并如实说明差异。
