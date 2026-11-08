# qy-assistant 架构与技术细节

记录日期：2026-11-07
上游：`docs/00-PRD.md`；决策依据：`docs/02-decisions.md`。
标注【待核实】的条目必须在对应任务中用真实请求或源码确认后再落实现，不得凭本文档假设。

## 1. 总体结构

```
               ┌──────────────── qya 进程（tokio，单二进制） ────────────────┐
QQ 官方 WS ──▶ │ platform::qq ─┐                                             │
本地 CLI   ──▶ │ platform::cli ┼─▶ Inbox ─▶ persona::Pacer ─▶ PersonaAgent ──┼─▶ Outbox ─▶ 平台发送
               │               │   (合并连发/打断)       │ 轻工具         ▲    │
               │ command::Router（/命令直达，不经模型）    │ delegate()     │    │
               │                                          ▼                │    │
               │                         orchestra::Supervisor ──结果信箱──┘    │
               │                           ├ Worker(任务型, 可续接)             │
               │                           └ Specialist(常驻专员)              │
               │ agent::Kernel（pi 式循环）  llm::Provider(kimi-code)          │
               │ tools::*  plugins::{image,tts,search,grok}  external::{mcp,douyin}│
               │ store(SQLite WAL)  scheduler(cron)  learner(表达/黑话)         │
               └──────────────────────────────────────────────────────────────┘
```

设计原则：

- 内核与人格分离：`agent::Kernel` 不知道唐语歌，只跑"提示词 + 消息 + 工具 → 事件流"。人格、拟人、编排都是内核之上的层。
- 一切扩展点都是 trait + 注册表：`Platform`、`Provider`、`Tool`、`PluginProvider`、`ExternalToolHost`。
- 单写者存储：所有 SQLite 写入经一个专用线程，避免锁竞争。
- 配置热更新：`ArcSwap<Config>`，读零锁；`/provider`、`/model` 等命令写回 TOML（`toml_edit` 保留注释）并原子替换。

## 2. 技术栈

| 领域 | 选型 | 理由 |
|---|---|---|
| 语言/版本 | Rust stable（edition 2024），workspace | 性能、内存可控、长期运行 |
| 异步运行时 | tokio（multi-thread，`worker_threads = 2`） | 4 核小内存服务器，2 线程足够 |
| 分配器 | mimalloc | 降低碎片与常驻内存 |
| HTTP | reqwest（rustls，http2，连接池复用，gzip） | SSE 流式 + 多提供方 |
| WebSocket | tokio-tungstenite（rustls） | QQ 网关 |
| SSE 解析 | eventsource-stream 或自写行解析器 | 体积小，可控 |
| 序列化 | serde / serde_json；工具 schema 用 schemars | 工具参数强类型 |
| 存储 | rusqlite（bundled，WAL）+ 单写线程 | 单文件、零运维 |
| 配置 | toml + toml_edit（写回）+ arc-swap（热替换） | 热更新、保留注释 |
| 定时 | croner（cron 表达式）+ tokio 定时器 | 轻量 |
| 文本匹配 | aho-corasick（触发词）+ 字符二元组打分 | 不引入大分词词典 |
| MCP | rmcp（官方 Rust SDK，客户端，stdio / streamable HTTP） | 通用外部工具 |
| 日志 | tracing + tracing-subscriber + tracing-appender（按天轮转，保留 7 天） | 结构化、低开销 |
| 错误 | thiserror（库）/ anyhow（二进制边界） | |
| 取消 | tokio-util `CancellationToken` | 打断、停止、关停 |
| systemd | sd-notify（READY / WATCHDOG） | 看门狗 |
| 时间 | jiff | 时区正确 |
| 构建 | cargo-zigbuild → `x86_64-unknown-linux-gnu`（glibc 不高于目标机） | Windows 交叉编译，不在服务器编译 |
| 测试 | cargo test + insta（提示词快照）+ wiremock（HTTP 假服务） | |

Release profile：`opt-level=3, lto="fat", codegen-units=1, strip=true, panic="unwind"`（保留 unwind，以便单个任务 panic 被 JoinHandle 隔离，不拖垮进程）。

## 3. Workspace 布局

```
qy_assistant/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ qya-core/      # 共享类型：Message/ContentPart/ToolCall/AgentEvent/Usage/Error、ids
│  ├─ qya-config/    # TOML + secret 文件装载、ArcSwap 热替换
│  ├─ qya-llm/       # Provider trait、kimi-code 驱动、模型目录、思考等级映射、重试
│  ├─ qya-store/     # SQLite schema、迁移、会话树、记忆、表达库、任务、定时任务
│  ├─ qya-agent/     # pi 式 Kernel：循环、工具注册表、steer/follow-up 队列、压缩
│  ├─ qya-orchestra/ # 子代理：派出/续接/专员、上下文继承、结果信箱
│  ├─ qya-persona/   # 提示词布局、身份装配、Pacer、表情、表达/黑话学习器
│  ├─ qya-tools/     # 内置工具：shell(+rtk)/fs/memory/todo/cron/send/mail/komari/blog/skills
│  ├─ qya-plugins/   # 可插拔提供方：image/tts/search/grok；external：mcp、douyin
│  ├─ qya-platform/  # Platform trait；qq（官方）、cli；onebot 预留
│  └─ qya-command/   # /命令解析与路由
├─ apps/qya/         # main：配置装载、依赖装配、Supervisor、信号与 systemd
├─ assets/persona/   # 角色卡 JSON、roleplay 资料、风格 overlay（从 Hermes 迁移）
├─ skills/           # SKILL.md 技能
├─ config/qya.example.toml
├─ deploy/           # systemd unit、部署脚本
└─ docs/
```

依赖方向（单向）：`core ← llm ← agent ← orchestra ← persona`；`tools/plugins/platform/command` 只依赖 `core`（及必要的 `store`），由 `apps/qya` 装配。禁止反向依赖。

## 4. 模型层（qya-llm）

### 4.1 抽象

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    async fn stream(&self, req: ChatRequest, cancel: CancellationToken)
        -> Result<BoxStream<'static, Result<StreamEvent>>>;
}
pub enum StreamEvent { TextDelta(String), ReasoningDelta(String),
    ToolCallDelta{index, id, name, args_delta}, Usage(Usage), Done(StopReason) }
```

`ChatRequest` 只含规范化字段（system 段、messages、tools、model、effort、max_tokens）；各传输格式（OpenAI Chat Completions、Anthropic Messages）各自一个 `Transport` 实现负责编码/解码。新增官方接口 = 新增一个 Transport，不动内核。

### 4.2 kimi-code 适配要点（2026-11-07 用 `kimi.txt` 实测）

- 端点：`https://api.kimi.com/coding/v1`，Chat Completions 传输。
- User-Agent：Chat Completions 传输不带特殊 UA 也返回 200（Hermes 在 Anthropic 传输上用 `claude-code/0.1.0` 是另一条路径）。UA 仍可在配置中覆盖。
- 当前 key 可用模型（`/models`）：`kimi-for-coding`（显示名 K2.8 Preview，1M）、`kimi-for-coding-highspeed`（256K）、`k3`（1M）、`k3-256k`（256K，不支持视频输入）、`k3.1-pro`（1M）、`k3.1-pro-highspeed`（1M，2 倍用量）。全部 `supports_thinking_type=only`（思考恒开）。
- 思考等级：参数名 `reasoning_effort`，只接受 `low/high/max`。配置 `effort_map`：low→low、medium→high、high→high、xhigh→max、max→max。不存在"关闭思考"。
- 思考回放：带工具调用的 assistant 消息回放时携带模型返回的真实 `reasoning_content`（实测通过）；缺失时补单个空格，不伪造内容。
- 缓存：命中数在 `usage.prompt_tokens_details.cached_tokens`；CLI 实测第二轮命中 87%。不把五小时额度窗口当缓存时间。
- `/models` 返回的 `context_length`、模态、effort 可选值可用于启动自检（`qya --check` 已比对配置与可用列表）。
- 模态（2026-11-08 `/models` 复核）：`k3-256k` 输入只有文字与图片（`supports_video_in=false`）；`kimi-for-coding`、`kimi-for-coding-highspeed`、`k3`、`k3.1-pro` 系列支持视频。视频以 `{"type":"video_url","video_url":{"url":"data:video/mp4;base64,…"}}` 发送，服务器实测通过（2 秒测试视频 566 输入 token）。
- 媒体编码按目标模型的 `vision` / `video` 能力（配置的模型目录）处理：本地 `file://` 图片（≤10 MB）与视频（≤20 MB）在请求时才转为 data URI；模型不支持视频时不发画面，改为一句提示“需要了解内容时用 watch_video 查看 <路径>”，支持视频但文件超限时同样提示走 watch_video。

### 4.3 可靠性

- 重试：仅对连接错误、429、5xx 做指数退避（最多 3 次，尊重 `Retry-After`）；流已开始输出后不重试，交给上层决定。
- 超时：连接 10 s；首字节 60 s；流空闲 90 s。
- 错误分类：`RateLimited / Auth / BadRequest / ContextOverflow / Transient / Unknown`，上层据此决定压缩、降级或报告。

## 5. 内核（qya-agent，pi 式）

### 5.1 循环

```
run_turn(session, input):
  push user message
  loop:
    req = context.build(session)        # 系统段 + 会话树当前分支 + 动态尾注入
    stream provider → 发 AgentEvent（TextDelta/ToolStart/ToolEnd/...）
    if 无工具调用: break
    并发执行工具（只读工具并行，写工具串行，按 Tool::concurrency 声明）
    push tool results
    检查 steering 队列：有插话则注入后继续
  检查 follow-up 队列：有排队消息则开始下一轮
```

- `AgentEvent` 通过 `tokio::broadcast` 发布：平台层、编排层、日志订阅互不耦合。
- 两个队列（与 pi 语义一致）：`steer`（工具执行间隙插入，立即影响当前轮）与 `follow_up`（当前轮结束后再处理）。
- 取消：每轮一个子 `CancellationToken`；`/stop` 或打断触发取消，未完成的工具结果写为 "cancelled"，保持消息结构合法。
- 输入延迟落库：本轮输入及轮中到达的插话/追问，只在模型给出回应时与回应一起写入会话树；若在此之前被取消，原样经 `TurnOutcome::unconsumed` 交回。唐语歌据此把被打断的输入排回缓冲最前面与新消息合并（`/stop` 则丢弃）；执行者则直接落库以保留任务记录。出错时输入照常落库。
- 工具循环防护：同一工具同参数连续重复 3 次、单轮工具调用超过上限（默认 40）即停止并报告。

### 5.2 工具接口

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;               // name、description、JSON schema（schemars 生成）
    fn concurrency(&self) -> Concurrency { Concurrency::Exclusive }
    async fn call(&self, ctx: ToolCtx, args: serde_json::Value) -> ToolOutput;
}
```

`ToolOutput { content: Vec<ContentPart>, is_error: bool, media: Vec<MediaRef>, detail: Option<Value> }`：`media` 走投递通道，不再靠在文本里写 `MEDIA:` 标签。工具集（toolset）是工具名白名单，按会话角色装配。

### 5.3 会话树与压缩

- 会话以条目树存储（SQLite `entries`：`id, session_id, parent_id, kind, payload, tokens, created_at`），会话记录当前叶子 `leaf_id`。分叉 = 新会话指向父会话某条目，零拷贝。
- 压缩：上下文估算达到模型窗口的 90%（可配）时，对较早的前缀生成摘要，写入 `compaction` 条目（记录被覆盖的范围）；构建上下文时从最近的 compaction 往后取。摘要提示词要求保留未完成任务、阿颜偏好和承诺事项。
- 压缩两步走（2026-11-08，借鉴 Sodium 记忆系统）：
  - 先折叠：切分点之前的工具结果在上下文中替换为一行占位（追加一条 `fold` 条目，原文不动）；折叠后低于窗口 60% 就到此为止，不调用模型。
  - 再摘要：仍不够时做“枚举式”摘要——先清点日期与截止、数字与金额、人名与账号、已确认决定、阿颜纠正过的地方，再输出 `{summary, capsule}`；胶囊含目标、要求与约束、已确认决定、验证状态、下一步，由 `render_compaction` 按固定标签确定性渲染（每项 ≤8 条、每条 ≤150 字、胶囊 ≤1200 字）。不是 JSON 时当作纯文本摘要；JSON 残缺时按无摘要处理，不把残骸写进摘要。
  - 模型错误以 `StopReason::Error` 连同错误交回（输入已落库）；只有存储等内部错误以 `Err` 返回。
- token 估算：本地按字符做近似估算，每轮用服务端 usage 校准。

## 6. 人格层（qya-persona）

### 6.1 唐语歌会话的提示词布局（缓存友好）

| 段 | 内容 | 变化频率 |
|---|---|---|
| S1 身份（system） | 角色卡渲染 + 单用户恋人关系默认 + 自然 QQ 回复规则 + 轻工具使用说明 | 几乎不变 → 缓存前缀 |
| S2 记忆快照（system） | `USER.md` 用户画像 + `MEMORY.md` 摘要（各限 2 KB） | 每个会话冻结；新会话或压缩时刷新 |
| 历史 | 会话树当前分支 | 追加 |
| 尾注入（附在最后一条 user 消息后） | 当前时间、≤3 条表达/黑话提示（≤150 token）、待转述的后台结果摘要 | 每轮 |

**实现（任务 06，2026-11-07）**

人格资产目录 `assets/persona/`（部署时复制到服务器）：

```
persona.toml        清单：user_name、常驻资料、运行时段落、按需资料条目
card.json           角色卡（chara_card_v2，身份唯一来源；迁移自 Hermes 融合版）
core/soul.md        内核与驱动        ┐
core/speech.md      说话方式细节      ├ 常驻 S1
core/limits.md      表达与事实边界    ┘
runtime/relationship.md  单用户恋人关系默认（qya 部署专属）
runtime/agency.md        能力与边界：真实操作以工具为准、persona_notes 用法、记忆快照与〔〕尾注的含义
notes/{appearance,behavior,relationship,life_events}.md   按需资料
```

- S1 渲染顺序：`# 唐语歌` + 身份优先级说明 → 关系默认 → 核心描述 → 性格 → 聊天准则 → 常驻资料 → 示例对话 → 补充设定（角色书常驻条目）→ 能力与边界；`{{user}}/{{char}}` 替换为 阿颜/唐语歌。约 7300 字符（实测首轮输入约 4.6k token）。
- 原 Hermes 的 `skill_view(tangyuge-roleplay)` 改为专用轻工具 `persona_notes(topic)`，条目与用途写在工具说明里；实测模型会在首次聊到外貌时主动读取。
- S2 来自 `<memory_dir>/user.md`（关于阿颜）与 `memory.md`（你的备忘），每文件上限 2000 字；会话创建时生成并冻结在 kv `persona_s2:<session>`，压缩后刷新，保证同一会话前缀字节稳定。
- Hermes 的 `SOUL.md`（工作风格）不进入唐语歌提示，留给后台执行者（任务 09）。Hermes `MEMORY.md` 的运维流程迁到专员记忆种子（`assets/memory_seed/`，见其 README）。
- 唐语歌的工具集只含轻工具；当前为 `persona_notes`、`sticker`，后续加入 `delegate`、`agent_ctl`、`memory`、`image_generate`、`speak`、`web_search`。工具按名字排序输出，schema 字节稳定。
- 渲染卫生由 `crates/qya-persona/tests/persona.rs` 断言（首块为身份、段落顺序、无 Hermes/UI/占位符残留、重复渲染字节一致），未引入 insta。

### 6.2 Pacer（回复节奏）

阶段机（`qya_persona::pacer` 纯逻辑 + 运行时计时，任务 07 实现）：

| 阶段 | 新消息的处置 |
|---|---|
| Idle / Debouncing | 进缓冲，(重新) 计时；去抖 1.8 s，从第一条起最多等 6 s，到点合并为一轮 |
| Generating（尚未调用工具，QQ 端也尚未发出任何内容） | 取消本轮（不落库半成品），与缓冲合并后重新去抖 |
| Tools | 作为 steer 插话注入当前轮；若工具阶段已过才到达，内核在本轮收尾时转为追问处理 |
| Delivering（分段发送中） | 进缓冲，发送完成后开始新一轮去抖 |

- 分段：优先按空行；单段超过 120 字再按句末标点切；含代码块不拆；合并到至多 `max_segments`（默认 4，QQ 被动回复额度待任务 13 核实）。段间延迟 = 400 ms + 30 ms/字，上限 2.5 s。流式平台（CLI）不分段；`qya --no-stream` 可在本地模拟 QQ。
- `/stop` 同时停止当前轮并丢弃待合并缓冲。
- 表情：`sticker(emotion, hint?)` 工具；扫描 `<stickers_dir>`（默认 `<data_dir>/stickers`），按“情绪-场景-角色”文件名索引，情绪清单写进工具说明；图片随回复文字之后发送。目录为空时不暴露该工具。
- 参数位于配置 `[persona.pacer]`：`debounce_ms`、`max_wait_ms`、`max_segments`。

### 6.3 学习与记忆（learner + consolidate）

- 触发：每累计 30 条阿颜消息或空闲 10 分钟，后台任务用低价模型（默认 `kimi-for-coding`，effort=low）批量分析新增对话。
- 产出：`expressions(situation, text, kind[habit|jargon], meaning, hits, last_used, weight)`，存 SQLite；按时间衰减，上限 300 条，超出淘汰低权重条目。
- 注入：每轮本地检索——对最近 3 条消息用 aho-corasick 匹配触发词，再用字符二元组 Jaccard 打分，取 top-3，渲染为 ≤150 token 的尾注入。不额外调用模型。
- 隔离：学习结果只影响说法和用词，不写入身份段，不覆盖角色卡；`/learn off` 可整体关闭；`/learn list 说法|记忆`、`/learn forget <编号>` 可人工管理。

**实现（任务 08，2026-11-07）**

- 只从阿颜的消息里提取两类：`jargon`（梗、缩写、谐音、专属叫法，带含义）与 `habit`（口头禅、称呼偏好等），每批至多 8 条；排除普通词汇、一次性事件和隐私。
- 触发：后台每 60 s 检查一次，会话空闲且（新消息 ≥ 30 条，或空闲 ≥ 10 分钟且 ≥ 8 条）时学习；`/learn now` 立即学习。进度按会话记在 kv `learn_ckpt:<session>`，对话稿只取最新约 6000 字。
- 入库（schema v2：`triggers` 列 + `(kind,text)` 唯一索引）：已存在则命中 +1、权重 +0.5（上限 5）；本批未出现的条目权重 −0.05，低于 0.2 删除，总数超过 300 淘汰低权重。
- 检索：Aho-Corasick 匹配触发词（权重 2/次）+ 字符二元组 Jaccard（无触发词命中时要求 ≥ 0.2）+ 权重×0.1，取 top-3，渲染为 `〔你们之间的说法……〕` 附在时间之后，不超过 `inject_max_chars`（默认 220 字）。
- 实测：一段 6 条消息的对话学到“寄了 / 依托答辩 / 摸会儿鱼 / 贴贴”四条，含义准确；之后消息含“寄了”时提示被注入，回复理解正确。

**记忆整理（2026-11-08，借鉴 Sodium 的两阶段闸门与 Anthropic dreaming；原则“写少读多”，聊天路径不为记忆多花调用）**

- 学习器一次调用同时输出 `expressions` 与 `memories`。记忆候选读双方对话，但只记阿颜陈述的（stated）、阿颜明确认可的约定（agreed）、阿颜纠正过的（corrected）；依据缺失或不在这三种之内的一律丢弃；唐语歌对自己的描述与剧情发挥、一次性任务、带时间的提醒、隐私凭据都不记。候选进入 `memory_candidates` 表。
- 整理：候选 ≥ 5 条或最早一条超过一天、且唐语歌空闲时进行（`/learn now` 立即）。第一阶段只去掉空白与句末标点后完全相同的（保留 `C++`/`C#` 这类符号差异），第二阶段一次低价调用让模型只给出 `add/update/remove` 操作，由 `consolidate::apply` 确定性执行：按原编号改删、再追加；单次最多删 3 条；每个文件守住 2000 字，超限的新增放弃、修改导致超限则该文件整体保持原样并撤掉对应说明；输出里有任何无法解释的操作则整批无效、候选保留。
- 一致性：等待模型时不持锁；模型返回后在共享的记忆写锁（唐语歌 `memory` 工具、`/learn forget`、`/learn undo` 共用）内重读，文件已变就放弃本轮。先写修订（整理前后全文、变更说明，存 `memory_revisions`，保留最近 20 次）再原子写文件（临时文件改名），写失败则恢复文件、撤销修订、放回候选。启动时若发现最近一次修订已记下、但文件停在整理前或只写了一半（每个文件都等于整理前或整理后），补写完整；文件被手动改过则不碰。
- 撤销：`/learn undo` 只在当前文件仍等于那次整理结果时恢复整理前内容，避免覆盖之后的手动修改。记忆快照按会话冻结，所有改动在新会话生效。
- 唐语歌的 `memory` 工具保留，说明改为“只在阿颜明确让你记住、或纠正你记错的事时写”。
- 说法检索的情境匹配改为中文单字集合（去掉高频虚字）加 3 字以上英文词的 Jaccard，对语序不敏感（Sodium 文档的结论：bigram 对语序太脆）。
- 未采纳：向量检索与联想网络（需要常驻嵌入模型，记忆只有几千字、本来整份注入）、情绪天气与“做梦”独白、主动开口。
- 配置 `[persona.learner]`：`enabled`、`model`（默认 kimi-for-coding）、`effort`、`batch_messages`、`idle_minutes`、`min_messages`、`max_items`、`inject_max_chars`。

## 7. 多代理编排（qya-orchestra）

### 7.1 概念

- **Worker（任务型）**：一个任务派一个；完成后状态为 `archived`，会话保留，可通过 `/agent <id> <消息>` 或唐语歌的 `agent_ctl(continue)` 续接。
- **Specialist（常驻专员）**：在配置中定义（默认 `ops` 运维：邮件/服务器/博客/流量日报；`creator` 创作：抖音/生图）。长期保有自己的会话，自动压缩，跨天保留；派给专员的任务排进其 follow-up 队列。

### 7.2 delegate 工具

```json
{ "task": "…", "context": "fork|brief|fresh", "to": "worker|ops|creator",
  "model": "k3-256k?", "effort": "low|medium|high|xhigh|max?", "toolset": "ops?", "notify": "relay|silent" }
```

- `fork`：子会话以唐语歌当前叶子为父条目（零拷贝继承完整对话），追加一条"你现在是后台执行者，不扮演角色，只完成任务并汇报"的指令；系统段换成执行者提示。适合"刚才聊的那个"类任务。
- `brief`：只带任务说明 + 唐语歌写的上下文摘要（由 delegate 参数 `context_note` 提供）。默认值。
- `fresh`：空白开始，只有任务。
- 模型与思考等级：缺省取 `orchestra.default_model / default_effort` 或专员配置；必须在模型目录中且当前 key 可用，否则拒绝并说明。
- 并发上限：同时运行的 worker 默认 3；嵌套深度 1（worker 不能再派 worker）。

### 7.3 结果回传

- worker 结束时写入唐语歌会话的结果信箱（结构化：状态、摘要 ≤ 600 字、产物媒体引用、worker id）。
- 唐语歌下一轮在尾注入中看到摘要并以自己的语气转述；完整工具日志只留在 worker 会话，不进入唐语歌上下文。
- 若阿颜当前没有说话：`notify=relay` 时由主动消息策略（第 9 节）决定立即触发一轮唐语歌转述还是延后。
- 进度：长任务可调用 `report_progress`，只在阿颜用 `/agents` 查看或任务超过 5 分钟时由唐语歌提一句。

### 7.4 实现（任务 09，2026-11-07）

- crate `qya-orchestra`：`Supervisor`（派出、续接、停止、列表）+ 工具 `delegate`、`agent_ctl`（唐语歌用）、`report_progress`（执行者用，每个执行者一份实例）。模型客户端经可注入的 `ProviderFn` 获取，便于测试。
- 执行者系统提示：`assets/agents/worker.md`（角色说明 + Hermes SOUL 的工作风格 + 汇报要求 ≤400 字）。专员额外追加“你的身份”（专长说明）与“你的长期记忆”（`<memory_dir>/<memory>`，如 ops.md、creator.md）。
- fork：worker 会话的 `fork_entry` 指向唐语歌当时的叶子（即带 delegate 调用的那条 assistant 消息），首条输入先补该调用的工具结果“（这件事已交给后台执行者处理）”，再给执行者指令，保证消息结构合法。系统段不同，因此 fork 继承的是上下文而不是缓存前缀。专员不支持 fork，自动按 brief 处理并在回执中说明。
- 结果回传：执行者结束后交回 `WorkerReport`（摘要上限 1200 字作为保险，提示词要求 400 字）；运行时把它包装成 `〔后台结果：…〕` 消息交给唐语歌。该消息会落入唐语歌的会话树，她之后仍记得结果；执行者产出的媒体随她那一轮的回复发出。偏离原设计（尾注入摘要）的原因：落库后可被后续轮次引用。可靠投递见下一条。
- 先存后投（2026-11-08，借鉴 DeepSeek super-agent 的 stored work）：
  - 派出：`delegate` / 续接返回成功之前，任务正文与运行标记（`sessions.run_owner` = 汇报收件会话）在同一事务写入（`Store::begin_run`），落库与随后的驱动在同一个后台任务里完成，调用方中途被取消也不会留下没人驱动的任务。占用执行者（`enqueue_or_claim`）与“运行中则排进插话/追问队列”在同一把锁内完成；给运行中执行者的插话/追问同时写入 `run_queue`。
  - 结束：汇报写入 `mailbox` 并在同一事务清掉运行标记（失败短暂重试），之后才公开结束状态；若此刻队列里还有后到的插话或追问，在同一会话上接着跑一轮（循环驱动，`run_queue` 在下一轮 `begin_run` 时清空）；被要求停止的（包括汇报落库期间才到的 stop）只把它们写进会话，不再执行。每次运行固定自己的收件会话。
  - 交付：运行时把汇报放进单独的队列（不走插话通道），只在唐语歌一轮开始时整体取用；`TurnOutcome.inputs_saved` 为真（该轮输入已落库）才确认删除信件，否则放回队首（被打断时从未消费输入中去掉这些汇报，避免重复）；出错或 `/stop` 时改为静默，等阿颜下次说话再带上。按信件 id 去重。模型错误由内核以 `StopReason::Error` 连同错误交回（输入已落库），存储等内部错误才以 `Err` 返回。确认的含义是“汇报已进入唐语歌的会话”；之后她的回复若发送失败，按普通回复处理，不再补投汇报。
  - 重启：`Runtime::open` 在接入平台与启动定时任务之前调用 `Supervisor::recover`，把仍带运行标记、或还有插话残留的执行者逐条转成“被中断”汇报（先把 `run_queue` 里会话中还没有的插话写回会话（与入队后落库的同内容条目一对一抵消），再写汇报并清标记，每条单独提交，中途退出不漏；只有残留且都已处理过的不报告），再装载全部未确认汇报。退出时不主动取消后台任务，避免误报成“被要求停止”。投递语义为至少一次：确认与删除是独立步骤，恰在其间退出可能重复转述一次。
  - 媒体：每日媒体清理跳过未确认汇报、发件箱与等待随回复发出的文件；模型出错的一轮不投递回复，后台媒体留给下一轮。
  - 执行者可调用 `need_decision` 提出一个需要阿颜拍板的问题，汇报状态为“等阿颜决定”，唐语歌单独转达；续接已有任务可使用一个预留并发名额，不排在新任务后面。
- 并发：信号量限制同时运行数（默认 3），超出的排队并在 `/agents` 显示“排队中”；worker 工具集不含 delegate，嵌套深度为 1。
- 配置 `[orchestra]`：`default_model`（kimi-for-coding）、`default_effort`（high）、`max_concurrent`、`worker_prompt`、`worker_tools`、`[orchestra.specialists.<名>]`（description、model、effort、tools、memory）。专员不写 model/effort 时跟随默认值；现网唐语歌为 `k3-256k`/low，执行者与 ops、creator 专员均为 `kimi-for-coding`/high（2026-11-08 起），派活时显式指定的模型与思考等级优先。
- 命令：`/agents`、`/agent <id>`（详情 + 最近回复）、`/agent <id> stop`、`/agent <id> <话>`（运行中插话，已结束续接）。
- 实测（CLI）：委派写回文函数并如实转述“环境无 cargo 未实跑测试”；“刚才那个再改改”时唐语歌选择 `agent_ctl continue` 续接同一 worker；ops 专员依据迁移来的记忆回答博客同步要点并标注待核实项；整段对话累计缓存命中 82%，内存 24 MB。

## 8. 工具与提供方

### 8.1 内置工具（qya-tools）

| 工具 | 要点 |
|---|---|
| `shell` | `bash -lc`，工作目录与超时可配；输出超 4 KB 时经 RTK（服务器 `rtk` 可执行文件）压缩，失败回退截断；进程组整体 kill；审计日志记录命令与退出码 |
| `fs_read/fs_write/fs_patch/fs_search` | patch 用精确替换 + 唯一性校验；搜索调用 `rg`（存在时） |
| `memory` | 编辑 `MEMORY.md`/`USER.md`（新增/替换/删除条目，限额 2 KB），下个会话生效 |
| `todo` | worker 会话内的任务清单 |
| `cron` | 增删改查定时任务；触发时派出 worker 或发提醒 |
| `send` | 向阿颜投递文本/媒体（worker 产物投递用） |
| `mail` | 迁移 mail-vps-ops 的 SSH 脚本调用（全部邮箱列表不自动加 `--domain/--limit`） |
| `komari_report` | 原生实现 Komari 公开 API 流量日报（近 24 h UTC，缺失不记零） |
| `blog_sync` | 执行 `/usr/local/bin/blog-sync-kbase.sh`，超时 1300 s；日志出现"代理同步失败"时汇报一次但不干预 |
| `skill_view` | 按需读取 SKILL.md；提示词里只列技能名与一句描述 |

硬禁止（shell）：对 `/`、`/etc`、`/usr`、`/var/lib/qya` 的递归删除，`mkfs`、`dd of=/dev/*`、关机重启命令。其余不审批，与 Hermes 现状一致。

**实现（任务 10、11，2026-11-07，crate `qya-tools`）**

- `shell`：`<shell> -lc`；执行前调用 `rtk rewrite <cmd>`（2 s 超时，退出码 0/3 且有输出才采用，与 Hermes rtk-rewrite 插件一致）；输出超过 `output_limit`（默认 8000 字）时，完整内容存到 `<media_dir>/tool-output/`（随每日媒体清理回收），返回首尾与“共多少行、存在哪里、用 fs_search / fs_read 细看”的说明，超过每路 4 MB 捕获上限时如实标注；Unix 以 `setsid` 启动，超时/取消时 `kill -KILL -<pgid>` 清理整个进程组；输出超 `output_limit` 保留首 40% 尾 60%；每次调用（含被拒绝的）写入审计 JSONL。Windows 开发机需把 shell 指向 Git Bash，否则会解析到 WSL 的 bash.exe。
- `fs_read / fs_write / fs_patch / fs_search`：patch 要求 old 唯一（或 replace_all）；搜索优先 ripgrep，退回 grep。
- `memory`：§ 分隔条目，每文件 2000 字；按角色构造实例——唐语歌可写 `user`/`memory`，专员只可写自己的记忆文件（经编排的 extras 钩子注入）。
- `todo`：会话内整表覆盖的步骤清单。原设计中的 `send` 工具未实现：执行者产出的媒体随〔后台结果〕由唐语歌那一轮一起发出，不需要执行者直接对阿颜说话。
- `cron`：5 段表达式按北京时间（croner + jiff）；运行时每 30 s 检查到期任务，停机期间错过的只补一次；`remind` 以〔定时提醒〕消息交给唐语歌，`task` 派给指定执行者/专员并按 relay 交回；恢复暂停的任务以当前时间为基准，不补发；动作带 `once: true` 的一次性任务（“明天 9 点”“11 月 10 日”这类具体某天的事）触发前即删除，不会第二年再响。
- `skill_view`：扫描 `skills/*/SKILL.md` frontmatter，技能清单写在工具说明里；当前技能 grill-me（迁移）、qya-docs（新写，替代 38 KB 的 hermes-md-locator；同时列服务器路径与仓库路径，部署时文档同步到 `/opt/qya/docs/`）。mail-vps-ops、grok-search、tangyuge-roleplay 已分别由 mail 工具、grok 插件、persona_notes 取代。
- `komari_report`：Python 版逐条移植为 Rust（同样的校验、错误码、口径），实测 13 台节点 12 台有效。
- `blog_sync`：调用 `/usr/local/bin/blog-sync-kbase.sh`（1300 s），日志含“代理同步失败”时在汇报首行提示一次，不干预脚本。
- `mail`：保留 Hermes 已验证的 `mail_vps_fetch.py`（仅标准库，经受限 SSH key；位于 `assets/tools/`），工具把结构化参数映射为命令行、做必填校验、解析 JSON，`media_tag/preferred_media_tag` 指向的附件与链接 txt 自动投递；写操作失败提示“不要重试、先 read 核实”。Python 只在邮件操作时启动，不在热路径。
- `recall`（2026-11-08）：检索唐语歌与阿颜的过往对话（已结束的会话与压缩摘要），查询时先在一个事务里把检查点之后的新条目增量写入 FTS5 trigram 索引，再在 SQL 中排除当前会话后取最新 8 条，返回命中处前后约 60 字的片段（总计 ≤1500 字），注明日期与说话人。查询可写多个说法（空格、顿号分隔，最多 6 个），分别匹配后合并去重；3 字以下的词退回子串扫描。query 留空时返回之前最近的 12 条对话（按时间正序）——服务器实测中“上次说去哪儿玩了”这类问题字面关键词对不上原话，靠这一模式找到。不用就没有任何常驻开销；GC 删除会话时一并删除索引并回调检查点（条目编号可能被复用）。
- 工具分配：唐语歌 `persona.tools` 默认 image_generate、speak、web_search、cron、skill_view、recall（另有人格专属 persona_notes、delegate、agent_ctl、memory、sticker；注册了 watch_video 时自动带上）；worker 与专员的工具清单见配置 `[orchestra]`。

### 8.2 可插拔提供方（qya-plugins）

**2026-11-08 补充**

- `watch_video`：用 `[tools] video_model`（默认 `kimi-for-coding`，须在唐语歌提供方的模型目录中且支持视频，否则不注册）代看视频并回答问题，唐语歌再用自己的口吻回复。只允许读取媒体目录内的文件；超过 20 MB 先用 ffmpeg 压到 480p、每秒 2 帧、去掉音轨（独立临时文件、原子改名发布，结果缓存在 `<media_dir>/video-cache/`，所有失败出口清理临时文件，180 s 超时）；超过 10 分钟只看前 10 分钟并在结果开头注明。
- 搜索结果整形（`shape.rs`）：元宝与 grok 搜索统一为“标题（来源 · 日期）/ 链接 / 摘要”的紧凑列表；按链接（协议、`www.`、主机名大小写、末尾斜杠、锚点不计，路径与参数区分大小写）与标题去重；元宝按 `score` 排序、摘要缺失时用正文开头；摘要在句子边界截到 200 字。grok 搜索解析 JSON 数组后同样渲染（失败时截取原文前 4000 字）。
- `grok_fetch` 增加 `focus`：只提取相关段落（≤3000 字），默认上限由 30000 字降到 12000 字。

```rust
pub trait ImageProvider { async fn generate(&self, req: ImageReq) -> Result<Vec<MediaRef>>; async fn edit(...); }
pub trait TtsProvider    { async fn speak(&self, text: &str, opts: TtsOpts) -> Result<MediaRef>; }
pub trait SearchProvider { async fn search(&self, q: SearchReq) -> Result<Vec<SearchHit>>; }
```

配置示例：

```toml
[plugins.image]
active = "default"
[plugins.image.providers.default]
kind = "openai_images"
base_url_file = "secrets/image.url"   # 或 base_url = "..."
key_file = "secrets/image.key"
model = "gpt-image-2.5-flare"
quality = "medium"
timeout_s = 180

[plugins.search]
active = "yuanbao"
[plugins.search.providers.yuanbao]
kind = "tencent_wsa"
url_file = "secrets/yuanbao.url"      # 迁移自 yuanbao.txt：https://api.wsa.cloud.tencent.com/SearchPro
key_file = "secrets/yuanbao.key"
mode = 0                              # 0 公开网页 / 1 权威垂直 / 2 混合【待核实参数名】

[plugins.tts.providers.gptsovits]
kind = "gptsovits_http"
base_url = "http://127.0.0.1:19880"   # 经 FRP 到工作站
endpoint = "/tts"
timeout_s = 300

[plugins.grok.providers.default]
kind = "grok_openai_compat"           # grok-search 改为原生实现：web_search / web_fetch
url_file = "secrets/grok.url"
key_file = "secrets/grok.key"
model = "grok-4.20-auto"
```

**实现（任务 12，2026-11-07，crate `qya-plugins`）**

- 配置层只保证结构：`[plugins.<类别>] active` + `providers.<名>`（原始 TOML 表，含 `kind`）；工具每次调用读取当前激活提供方，`/provider` 写回后立即生效。secret 只接受 `*_file`（取文件首行，兼容 yuanbao.txt“首行 key、次行 url”）；`/provider` 拒绝修改 `*_file` 与 `kind`，修改后若新配置校验失败自动恢复原文件。
- 元宝搜索（kind `tencent_wsa`，2026-11-07 实测）：`POST <url>`，`Authorization: Bearer <服务 API Key>`，请求体 `{"Query", "Cnt"(10/20，其他值报 illegal Cnt), "Site"?, "Mode"?}`；返回 `Response.Pages` 为 JSON 字符串数组（title/url/site/date/passage/content/score/favicon/pics），错误在 `Response.Error`。`FromTime` 被拒绝；`Mode:1` 对普通查询可能返回空，默认 0。
- 生图（kind `openai_images`）：无输入图 JSON 调 `/images/generations`，有输入图 multipart 调 `/images/edits`（mask 需配合输入图）；`b64_json` 或 `url` 均支持；HTTP 200 但正文含 `error` 判失败；解析 PNG/JPEG/WebP/GIF 头部取实际尺寸，与请求比例偏差 >5% 时提示。
- 语音（kind `gptsovits_http`）：沿用 Hermes 桥接的请求体，校验 RIFF/WAVE 与 data 块长度（空/截断即失败）；连不上时明确提示“语音工作站离线或转发未建立”。
- grok（kind `openai_chat`）：`grok_search`（JSON 结果数组）与 `grok_fetch`（网页转 Markdown），提示词移植自 Hermes grok-search；web_map 与 Tavily 兜底未移植。

### 8.3 外部工具（qya-plugins::external）

- 通用：`ExternalToolHost` trait；实现一 `McpHost`（rmcp 客户端，stdio 或 streamable HTTP），把 MCP 工具映射为内核 `Tool`，名字加 `mcp_<server>_` 前缀。
- 抖音：现有 `hermes-douyin` 服务通过受 UID 校验的 Unix socket 提供 RPC，并且校验对端 PID 等于 systemd 记录的网关 MainPID（见 Hermes 仓库 `docs/tangyuge-hermes/10-douyin-publishing.md`）。因此抖音不走 MCP shim（shim 子进程 PID 过不了校验），而是实现 `DouyinHost`：qya 主进程直接连接该 socket，复用现有 RPC 协议；服务端把允许的调用方从 `hermes-gateway.service` 改为 `qya.service`。RPC 报文格式【待核实】，迁移时从服务器插件 `plugins/douyin-login` 源码读取。
- 抖音三个工具 `douyin_login / douyin_content / douyin_verification` 的语义、重试边界（写请求不重发）与短信验证码旁路消费保持不变。

**实现（任务 14，2026-11-07，crate `qya-douyin`）—— 以服务器插件源码为准修正上文**

- 授权的实际规则：hermes-douyin 按 `SO_PEERCRED` 的 **UID** 授权（`DOUYIN_ALLOWED_UID`），只有 `publish`、`verification_request_sms`、`verification_submit` 额外要求调用方 PID 等于 `systemctl show <单元> MainPID`，单元名在 `/opt/hermes-douyin/service.py` 中写死为 `hermes-gateway.service`。qya 主进程直连 socket 即可满足 PID 条件。
- 协议：每次一个连接，一行 JSON 请求、一行 JSON 响应。登录 `{"action":"status"|"login","recover"?}`（响应 ≤3 MB，含 `qr_png_base64` 时落盘 PNG 作为媒体）；内容 `{"action":"content","request":{...}}`（请求 ≤12 MB，响应 ≤1 MB）。只有 status/login、list、job 在连接错误时重试一次，写请求不重发；超时 50 s。
- 校验与净化逐条移植：参数白名单与格式（release_id、32 位 plan_id/job_id/image_id、64 位 cover_id、publish_at 必须带时区）；响应根字段、状态集合、恢复指引集合、分页范围；封面预览 JPEG 必须为 320×240 / 240×320、≤32 KB，落盘后作为媒体发给阿颜；递归拒绝含 cookie/base64/path 的键名，以及 cookie 赋值、file://、绝对路径、base64 原文。
- 固定制作目录（`[douyin] saved_covers_dir`，取代 hermes 家目录下的 `workspace/starlibrary_covers`）：只读 `<release_id>/<release_id>_<9x16|16x9|4x3|3x4>.<png|jpg|jpeg|webp>`，拒绝符号链接与非普通文件，读取前后比对大小与修改时间，格式/比例/像素按文件头校验；image_id = sha256(release_id、文件名、内容哈希) 前 32 位，内容变化即失效。（插件用 PIL 完整解码校验，这里只解析文件头。）
- **发布流程变更（阿颜决定）**：`publish` 从模型工具中移除。阿颜在 QQ 发送整条“确认发布 <plan_id>”时，运行时在命令解析与模型之前拦截，立即回执，后台直接提交发布并跟进任务最多 3 分钟；需要短信验证时自动请求发码；结果以〔抖音发布：…〕消息交给唐语歌转述。
- 短信验证码：仅当存在收码窗口时拦截纯文本六位数字（插件会拦截所有六位数字，这里收窄，避免误吞普通数字）；固定回执直接发给阿颜，验证码只在提交请求里出现，不进入模型、日志、会话记录；结果另以〔抖音短信验证…〕告知唐语歌。
- QQ 图片：入站图片的本地路径登记 30 分钟（最多 20 张），供 `cover_sources` / `cover_import` 使用。
- 工具归属：创作专员（`creator`）。通用 MCP 客户端按阿颜决定推迟，`ExternalToolHost` 只保留为设计扩展口。
- 验证边界：协议与校验有单测；真实 socket 只能在 Linux 服务器上验证（任务 15/16）。
- 现状（2026-11-08）：服务已更名 `tangyuge-douyin`（`/opt/tangyuge-douyin`、`/var/lib/tangyuge-douyin`、socket `/run/tangyuge-douyin/login.sock`、组 `tangyuge-douyin`），`service.py` 的 gateway 单元名改为读取 `DOUYIN_GATEWAY_UNIT`（默认 qya.service），允许 UID 为运行用户 tangyuge；以运行用户实测 status、list 正常。源码同步在 `ops/douyin/`。

## 9. 平台层（qya-platform）

```rust
#[async_trait]
pub trait Platform: Send + Sync {
    fn id(&self) -> PlatformId;
    async fn run(&self, inbox: mpsc::Sender<Inbound>, cancel: CancellationToken) -> Result<()>;
    async fn send(&self, out: Outbound) -> SendResult;          // 带 error_kind
    fn capabilities(&self) -> PlatformCaps;                     // 主动消息、分段额度、媒体类型
}
```

### 9.1 QQ 官方（第一版）

- 鉴权：AppID/Secret 换 access_token，到期前提前刷新。
- 网关：Identify / Resume、心跳、断线指数退避重连（1 s → 60 s）；`_ws` 已关闭时立即报错进入重连（Hermes 曾有空转 100% CPU 的坑）。
- 入站：C2C 文本、图片、视频、文件、语音（优先 `asr_refer_text`，其次 `voice_wav_url` 交给支持音频的模型，否则如实说明）；只接受阿颜的 openid。
- 出站：文本（`msg_seq` 递增）、富媒体先上传后发送；被动回复锚定最近入站 `msg_id`；维护 `event_id` / `is_wakeup` 与主动消息许可事件（`C2C_MSG_RECEIVE/REJECT`、`FRIEND_ADD/DEL`）。
- 主动消息策略：任务型主动消息先检查被动窗口；窗口外检查许可与月度额度（计数持久化）；不足则延后到下次入站时附带。
- `SendResult.error_kind`：`too_long / bad_format / forbidden / not_found / rate_limited / transient / unknown`。

**实现（任务 13，2026-11-07，`qya-platform::qq`，对照 Hermes adapter 与官方协议）**

- 鉴权：`POST https://bots.qq.com/app/getAppAccessToken {appId, clientSecret}`，到期前 60 s 刷新；网关 4004 时作废重取。API 头 `Authorization: QQBot <token>`，基址 `https://api.sgroup.qq.com`。
- 网关：`GET /gateway` 取 wss 地址；Hello(op10) 后按间隔 80% 发心跳 `{"op":1,"d":<最新 seq>}`；有 session_id 与 seq 时 Resume(op6)，否则 Identify(op2，intents = 1<<25 | 1<<30 | 1<<12 | 1<<26，与 Hermes 现网一致）；op7 重连并 Resume；op9 不可恢复时清会话重新 Identify。致命关闭码 4001/4002/4010/4011/4012/4013/4014/4914/4915 停止重连并让进程退出（由 systemd 处理）；4006/4007 清会话；4008 等 60 s；其余按 1/2/5/10/30/60 s 退避，连接稳定超过 60 s 后退避归零；连接关闭时立即返回进入重连，不空转。
- 入站：只处理 `C2C_MESSAGE_CREATE`，只接受 `allowed_users`，按 msg_id 去重（最近 200 条）。附件在后台下载（不阻塞心跳）：图片落盘并以 `file://` 引用，请求时才由传输层转为 data URI（数据库不存 base64，缓存清理后显示“已清理”）；语音取 `asr_refer_text`；视频与文件落盘并在文字中写明路径。
- 出站路由（`reply::ReplyState`）：以阿颜最近一条消息为锚点被动回复（窗口 60 分钟、最多 5 次、msg_seq 1..5 递增）→ 主动消息许可事件的 event_id → 主动消息（每月额度，计数持久化在 kv `qq_proactive:YYYY-MM`，北京时间）→ 都不可用时写入 SQLite 发件箱（`outbox` 表，文字与每个媒体各占一条，上限 50 条），阿颜下次说话后在后台补发：文字入箱前按 1500 字切块，相邻文字合并为不超过 1500 字的批次，逐批确认；合并后被判超长则逐条重发，单条仍被拒则保留；文件已不存在则跳过，上传失败或再次没有通道则保留到下次；补发串行执行，不阻塞网关收包。主动消息被拒（Forbidden）同样转入发件箱。出站逻辑在 `qq/courier.rs`。
- 富媒体：本地文件 base64 内联上传 `POST /v2/users/{openid}/files`（≤9 MB，类型按扩展名 1 图片 / 2 视频 / 3 语音 / 4 文件），再以 msg_type 7 发送 `file_info`。超过 9 MB 的文件暂不支持（Hermes 的分片上传未移植，记为待办）。QQ 是否接受 WAV 作为语音未经实测：语音上传被拒时自动改按文件发送。
- 运行：`qya --qq`；SIGTERM/Ctrl+C 优雅退出。
- 验证边界：协议由本地 WebSocket + wiremock 契约测试覆盖（Identify 载荷、READY、心跳 seq、过滤与去重、op7 后 Resume、被动回复 msg_id/msg_seq、媒体上传、致命关闭码、主动额度计数）；真实 QQ 只能在一次性切换时验收。

### 9.2 CLI

本地开发与测试用：stdin 逐行输入，事件流式打印，支持全部 `/命令`；媒体以本地路径展示。

### 9.3 OneBot（预留）

只定义 `PlatformCaps` 中的 poke、表情回应、自由主动消息等能力位，Pacer 和主动策略据此分支；第一版不实现适配器。

## 10. 存储（qya-store）

单个 SQLite 文件 `/var/lib/qya/qya.db`（WAL，`synchronous=NORMAL`）。实现上读写都经同一个专用线程串行执行（单用户吞吐足够，且彻底避免 `SQLITE_BUSY`）；若以后出现读延迟问题再加只读连接。

| 表 | 用途 |
|---|---|
| `sessions` | id、kind(persona/worker/specialist)、parent_session、fork_entry、model、effort、status、leaf_id、run_owner（执行者运行标记）、created/updated |
| `entries` | 会话树条目（见 5.3） |
| `mailbox` | 执行者 → 唐语歌的汇报信箱（未确认的汇报，确认后删除） |
| `memory_candidates` | 学习器提取、等待整理的记忆候选（v4） |
| `memory_revisions` | 记忆整理的修订：整理前后全文与变更说明，保留最近 20 次，供 `/learn` 与撤销（v4） |
| `recall_fts` | 过往对话检索的 FTS5 trigram 索引（v4） |
| `outbox` | 平台发件箱（暂时没有投递通道的出站消息） |
| `run_queue` | 交给运行中执行者、尚未确定写进其会话的插话/追问 |
| `expressions` | 表达/黑话库 |
| `cron_jobs` | 定时任务 |
| `quota` | QQ 主动消息额度计数 |
| `kv` | 杂项状态（QQ session_id/seq、版本号） |

迁移脚本内嵌（`include_str!`），启动时按版本号顺序执行。每周一次 `PRAGMA optimize` 与 WAL checkpoint；归档超过 90 天的 worker 会话可由 `/gc` 清理。

文件布局（服务器）：`/opt/qya/bin/qya`、`/etc/qya/qya.toml`、`/etc/qya/secrets/*`（0600，属主 qya）、`/var/lib/qya/{qya.db,media,skills,persona}`、日志 `/var/log/qya/`。媒体缓存 24 h 清理（沿用 Hermes 规则）。

## 11. 聊天命令（重新设计）

原则：命令不经过模型、立即响应；每个命令只做一件事；危险操作需要加 `!` 确认后缀。命令与命令的回复都不写入唐语歌的会话，也不进入表达学习；只有已知命令才拦截，`/笑` 这类以 / 开头的普通说话照常交给唐语歌（2026-11-08 起）。

| 命令 | 作用 |
|---|---|
| `/help [命令]` | 命令列表（一行一个加一句说明）；带命令名看详细用法 |
| `/status` | 版本、会话、模型与思考等级、上下文占用、用量与缓存命中、后台任务、内存、运行时长 |
| `/new` | 唐语歌开启新会话（记忆、关系不变，只是技术会话重开） |
| `/sessions` | 最近会话、各类会话数量、记录条数与数据库大小 |
| `/gc [天数]` | 清理已结束且超过 N 天（默认 30）没有活动的会话；`/gc 0` 清理全部已结束会话 |
| `/stop` | 停止唐语歌当前这一轮（不影响后台 worker） |
| `/compact` | 立即压缩当前会话（先折叠，不够再摘要） |
| `/model [名称]` | 查看/切换唐语歌模型（校验当前 key 可用） |
| `/effort [等级]` | 查看/切换唐语歌思考等级 |
| `/learn` | 学习与记忆总览；`list 说法` / `list 记忆`（必须写明，不带对象视为参数错误）、`forget 编号\|u3\|m2`、`now`、`undo`、`on\|off` |
| `/agents` | 列出 worker 与专员：编号、状态、模型、耗时、进度 |
| `/agent <id> [消息]` | 查看某个 worker 详情；带消息则续接/插话 |
| `/agent <id> stop` | 停止某个 worker |
| `/provider` | 列出生图/语音/搜索/grok 当前提供方与模型 |
| `/provider <类别> <字段> <值>` | 例：`/provider image model gpt-image-2.5-flare`；写回配置并热生效 |
| `/reload` | 重新加载配置、secret 文件与人格资产 |

定时任务由唐语歌的 `cron` 工具管理（没有单独的聊天命令）；原设计中的 `/restart!` 未实现，进程重启由部署流程执行，重启时后台任务以“被中断”汇报交回。

输出排版（2026-11-08）：QQ 单聊只显示纯文本、手机一行约 20 字，命令回复统一由 `qya_core::card` 排版：标题带一个图标（📊 状态、📂 会话、🌱 学习等），分节用「▍」，条目用「·」，操作提示每条一行各带「💡」，上下文占用显示十格进度条（全角 ■□）；手机 QQ 可能吞掉行首空格、部分安卓字体缺少少见符号，所以不靠缩进对齐，当前项用文字“（当前）”标出；一句话回复成功前缀 ✅、失败或无法执行前缀 ⚠️。token 显示为 `12.3k` / `1.0M`，不用 Markdown 与表格。

## 12. 性能与长期运行

- 内存：`worker_threads=2`；HTTP 连接池上限 8；SSE 增量直接拼到 `String`，不保留原始字节；会话条目按需从 SQLite 读取，内存只缓存当前活跃分支；媒体流式写盘，不整读进内存。
- 启动：配置与迁移同步执行，平台连接异步；目标冷启动 ≤ 1 s。
- 监督树：`apps/qya` 的 Supervisor 管理平台、调度器、学习器、worker 任务；子任务异常退出按指数退避重启并记录；连续失败告警到阿颜（一天最多一次）。
- systemd：`Type=notify`、`WatchdogSec=60`、`Restart=always`、`MemoryMax=300M`、`NoNewPrivileges=yes`；主循环每 20 s 喂狗。
- 观测：`/status` 汇总；tracing 记录每轮 token、缓存命中、耗时、工具耗时。
- 基准：任务 15 在服务器上测量空闲 RSS、单轮延迟开销（不含模型时间），写入运维文档。

**稳定性修复（2026-11-08，依据 codex 静态审阅逐条核实）**：
- 内核：压缩被打断时与生成被打断同样经 `unconsumed` 交回输入，压缩自身失败只告警、不终止本轮；工具调用预算在执行前检查，超出的调用补“未执行”结果；取消时给工具 2 秒自行收尾，不直接丢弃 future。
- 运行时：人格一轮放在独立任务里执行，panic 也会回到 `finish_turn` 复位阶段；内核队列里未注入的插话在被打断时并回缓冲，`/stop` 时一并丢弃；GC 保留排队或运行中的执行者，每周自动清理还要求没有后台任务。
- 子进程：stdout/stderr 与等待并发读取（每路最多保留 4 MB，其余读走丢弃），避免大输出写满管道卡到超时；future 被丢弃时由守卫终止整个进程组。
- 模型：错误正文读取有期限并响应取消；SSE 未换行残余上限 8 MB，工具调用索引上限 128。
- QQ：连接 20 秒、Hello 30 秒期限；心跳连续两次未收到 ACK 即重连；拒收事件同时清除旧事件锚点；延迟队列上限 50 条，补发失败放回队列；`allowed_users` 必须恰好一个（单用户设计）。
- 抖音：响应缺字段不再 panic；发码请求被取消时守卫复位收码状态；发码结果非 ready 时报告失败而不是“已请求发码”。
- 未采纳或推迟：模型 shell 与抖音 socket 的权限隔离（运行用户本就有免密 sudo，隔离需要另设执行用户，属架构调整）；SQLite 读写分离与维护调度、按压缩点裁剪历史读取、附件下载并发与大小限制、主动额度原子计数（当前发送基本串行）。QQ 补发已于同日改为发件箱 + 后台串行补发（见 9.1）。

## 13. 构建与部署

- 本地：`cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.36`（目标机 Debian 13，glibc 新于 2.36），产物 scp 到 `/opt/qya/bin/qya.new`，原子替换后 `systemctl restart qya`。
- 运行用户 `tangyuge`（2026-11-08 由 qya 更名，阿颜要求），加入 `blogsync`、`tangyuge-douyin` 组；邮件 SSH key 与 Komari 配置在 `/etc/qya/secrets/`。抖音服务为 `tangyuge-douyin`，源码在 `ops/douyin/`。布局与更新步骤见 `deploy/README.md`。
- 回滚：保留上一版二进制 `qya.prev`。Hermes 已于 2026-11-08 完全移除，不再能回滚到 Hermes。

**实现（任务 15、16，2026-11-08）**：本机 scoop 安装 zig、cargo install cargo-zigbuild，产物 10.6 MB；`deploy/install.sh` 完成用户、目录、凭据迁移（除 kimi/元宝外均在服务器内部从 Hermes 复制）、数据迁移与 systemd 单元（Hermes 移除后迁移步骤已删去，现在的脚本只安装暂存目录给出的凭据）。与原设计的差异：日志直接进 journald（不另写 /var/log 轮转文件，审计日志仍在 `/var/log/qya/audit.jsonl`）；单元为 `Type=simple`，未实现 sd_notify 看门狗（以 Restart=always 兜底，记为待办）；`MemoryMax=400M` 仅作失控保护。实测：`--check` 通过，19 个工具注册；切换后 QQ 网关 READY，常驻内存 28 MB；qya 用户可直连抖音 socket（status、list 正常）；运维专员在服务器上列出 12 个邮箱并生成流量日报。

## 14. 调试与协作约定

- 子代理默认 `claude-haiku-5-5`、思考等级 xhigh，最多 3 个并行，写入范围互不重叠。
- 代码疑难问题可在终端调用本机 `codex` CLI 做只读 debug 分析；结论需回到代码和测试中验证。
- 不提交、不推送，除非阿颜当次授权；secret 文件全部被 `.gitignore` 排除。
