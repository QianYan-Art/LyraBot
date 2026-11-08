---
name: qya-docs
description: 回答“这套系统怎么设计的、某个功能在哪、为什么这样做、部署在哪”等关于 qy-assistant 自身的问题时读取，定位项目文档与关键文件。
---

# qy-assistant 文档定位

qy-assistant（qya）是阿颜用 Rust 自研的个人 agent，唐语歌是它的门面。回答关于系统自身的问题时，先按下表找到对应文档再回答，不凭印象。

| 问题 | 文档 |
|---|---|
| 目标、功能范围、不做什么、切换与清理计划 | `docs/00-PRD.md` |
| 技术栈、模块划分、内核循环、提示词布局、多代理、工具、QQ、存储、命令、部署 | `docs/01-architecture.md` |
| 为什么选这个方案（关键决策与备选） | `docs/02-decisions.md` |
| 唐语歌身份与资料 | `assets/persona/`（card.json、core/、notes/、runtime/） |
| 后台执行者提示 | `assets/agents/worker.md` |
| 配置示例 | `config/qya.example.toml` |

服务器上项目文件位于部署目录（见架构文档“构建与部署”一节）。文档与代码冲突时以代码和实际运行结果为准，并如实说明差异。
