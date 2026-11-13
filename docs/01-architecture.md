# LyraBot 架构总览

记录日期：2026-11-07（2026-11-09 拆分整理，2026-11-10 命名统一为 LyraBot）
上游：`docs/00-PRD.md`；决策依据：`docs/02-decisions.md`。
本文只讲总体结构、技术栈与代码布局；各模块细节见下方文档索引。文档与代码或运行结果冲突时，以代码和运行结果为准。

## 1. 文档索引

| 文档 | 内容 |
|---|---|
| `00-PRD.md` | 目标、功能范围、不做什么、迁移与验收 |
| `01-architecture.md` | 本文：总体结构、技术栈、Workspace 布局 |
| `02-decisions.md` | 关键决策与备选方案 |
| `03-model-kernel.md` | 模型层（kimi-code 适配、可靠性）与内核（循环、工具接口、会话树与压缩） |
| `04-persona-memory.md` | 主对话角色的提示词布局、回复节奏、说法学习与记忆整理 |
| `05-orchestra.md` | 多代理编排：派活、上下文继承、专员、先存后投与重启恢复 |
| `06-tools-plugins.md` | 内置工具、可插拔提供方、视频代看、抖音外部服务 |
| `07-platform-commands.md` | 平台层（QQ 官方、CLI、OneBot 预留）与聊天命令 |
| `08-storage-operations.md` | 存储、长期运行与稳定性、构建部署、调试约定 |
| `09-backlog.md` | 待做方向、推迟事项与未采纳方案 |
| `../deploy/README.md` | 服务器布局、安装、更新、回退与运维步骤 |

## 2. 总体结构

```
                ┌────────────── lyrabot 进程（tokio，单二进制） ───────────────┐
QQ 官方 WS ──▶ │ platform::qq ─┐                                             │
本地 CLI   ──▶ │ platform::cli ┼─▶ Inbox ─▶ persona::Pacer ─▶ PersonaAgent ──┼─▶ 平台发送 / 发件箱
                │               │   (合并连发/打断)       │ 轻工具         ▲    │
                │ command（/命令直达，不经模型）            │ delegate()     │    │
                │                                          ▼                │    │
                │                         orchestra::Supervisor ──汇报信箱──┘    │
                │                           ├ Worker(任务型, 可续接)             │
                │                           └ Specialist(常驻专员)              │
                │ agent::Kernel（pi 式循环）  llm::Provider(kimi-code)          │
                │ tools::*  plugins::{image,tts,search,grok,video}  douyin      │
                │ store(SQLite WAL)  cron 调度  learner(说法/记忆)            │
                └──────────────────────────────────────────────────────────────┘
```

设计原则：

- 内核与人格分离：`agent::Kernel` 不绑定具体角色，只跑"提示词 + 消息 + 工具 → 事件流"。人格、拟人、编排都是内核之上的层。
- 扩展点是 trait + 注册表：`Platform`、`Provider`、`Tool`、可插拔提供方（按 `kind` 选择实现）。
- 单写者存储：所有 SQLite 读写经一个专用线程串行执行，避免锁竞争。
- 配置热更新：`ArcSwap<Config>`，读零锁；`/provider` 写回 TOML（`toml_edit` 保留注释）并原子替换，`/reload` 重载配置与人格资产。
- 先存后投：后台汇报、插话、QQ 待发消息都先落库再投递，进程任何时刻重启都不丢（见 `05-orchestra.md`）。

## 3. 技术栈

| 领域 | 选型 | 说明 |
|---|---|---|
| 语言/版本 | Rust stable（edition 2024），workspace | 性能、内存可控、长期运行 |
| 异步运行时 | tokio（multi-thread，`worker_threads = 2`） | 4 核小内存服务器，2 线程足够 |
| 分配器 | mimalloc | 降低碎片与常驻内存 |
| HTTP | reqwest（rustls，连接复用） | SSE 流式 + 多提供方；SSE 为自写行解析 |
| WebSocket | tokio-tungstenite（rustls） | QQ 网关 |
| 序列化 | serde / serde_json | 工具 schema 手写 JSON，字节稳定以利缓存 |
| 存储 | rusqlite（bundled，WAL，FTS5）+ 单线程执行 | 单文件、零运维 |
| 配置 | toml + toml_edit（写回）+ arc-swap（热替换） | 热更新、保留注释 |
| 定时 | croner（cron 表达式）+ jiff（北京时间） | 轻量、时区正确 |
| 文本匹配 | aho-corasick（触发词）+ 本地相似度打分 | 不引入分词词典与向量模型 |
| 日志 | tracing → journald | 结构化、低开销；shell 审计另写 JSONL |
| 错误 | thiserror（库）/ anyhow（二进制边界） | |
| 取消 | tokio-util `CancellationToken` | 打断、停止、关停 |
| 构建 | cargo-zigbuild → `x86_64-unknown-linux-gnu` | Windows 交叉编译，不在服务器编译 |
| 测试 | cargo test + wiremock（HTTP 假服务） | 提示词渲染由断言测试覆盖 |

Release profile：`opt-level=3, lto="fat", codegen-units=1, strip=true, panic="unwind"`（保留 unwind，以便单个任务 panic 被 JoinHandle 隔离，不拖垮进程）。

## 4. Workspace 布局

```
LyraBot/
├─ Cargo.toml                     # workspace
├─ crates/
│  ├─ lyrabot-core/               # 共享类型：Message/ContentPart/ToolCall/AgentEvent/Usage、记忆文件格式 memory_file、命令输出排版 card
│  ├─ lyrabot-config/             # TOML + secret 文件装载（types/raw/lib）、ArcSwap 热替换
│  ├─ lyrabot-llm/                # Provider、kimi-code（Chat Completions）、模型目录、思考等级映射、重试、媒体编码
│  ├─ lyrabot-store/              # SQLite schema 与迁移、会话树、信箱、发件箱、表达库、记忆候选与修订、检索索引、定时任务、清理
│  ├─ lyrabot-agent/              # pi 式 Kernel：循环、工具注册表、steer/follow-up 队列、折叠与压缩
│  ├─ lyrabot-orchestra/          # 子代理（types/spec/supervisor/tools）：派出/续接/专员、上下文继承、汇报与重启恢复
│  ├─ lyrabot-persona/            # 提示词布局、身份装配、Pacer、表情包、说法学习、记忆整理
│  ├─ lyrabot-tools/              # 内置工具：shell(+rtk)/fs/memory/todo/cron/mail/komari/blog/skills/recall
│  ├─ lyrabot-plugins/            # 可插拔提供方：image/tts/search/grok，watch_video，/provider 命令
│  ├─ lyrabot-douyin/             # 抖音外部服务客户端（校验、净化、确认发布与验证码拦截）
│  ├─ lyrabot-platform/           # Platform trait；qq（官方）、cli
│  └─ lyrabot-command/            # /命令解析与帮助
├─ apps/lyrabot/                  # main + runtime（主阶段机、commands 命令执行、tasks 周期任务与保活、learning 学习整理）
├─ assets/                        # persona/（角色资产）、agents/（执行者提示）、memory_seed/、tools/（邮件 helper）
├─ skills/                        # SKILL.md 技能
├─ config/lyrabot.example.toml
├─ deploy/                        # systemd unit、服务器配置模板、安装脚本、部署说明
├─ ops/douyin/                    # 抖音服务源码
└─ docs/
```

依赖方向（单向）：`core ← llm ← agent`，`orchestra` 与 `persona` 并列建在 `agent` 之上、互不依赖；`tools/plugins/platform/command/douyin` 只依赖 `core`（及必要的 `config`、`store`、`agent`、`llm`），由 `apps/lyrabot` 装配。禁止反向依赖。
