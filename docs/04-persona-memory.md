# 人格层：提示词、节奏、学习与记忆

记录日期：2026-11-07（2026-11-09 自 `01-architecture.md` 拆出整理，2026-11-10 命名统一为 LyraBot）
范围：`lyrabot-persona` 与运行时中主对话角色相关的部分（`apps/lyrabot/src/runtime.rs`、`runtime/tasks.rs`、`runtime/learning.rs`）。

## 1. 提示词布局（缓存友好）

| 段 | 内容 | 变化频率 |
|---|---|---|
| S1 身份（system） | 角色卡渲染 + 常驻资料 + 关系默认 + 能力与边界 | 几乎不变 → 缓存前缀 |
| S2 记忆快照（system） | `user.md`（关于用户，`#<user_name>`）+ `memory.md`（约定与共同的事），各 ≤2000 字 | 每个会话冻结；新会话或压缩时刷新 |
| 历史 | 会话树当前分支（含〔后台结果〕〔定时提醒〕等系统消息） | 追加 |
| 尾注入（附在本轮最后一条 user 消息后） | 当前时间 + ≤3 条说法提示（≤220 字） | 每轮生成，随该消息落库并原样回放 |

人格资产目录 `assets/persona/`（部署时复制到 `/opt/lyrabot/persona/`）：

```
persona.toml        清单：user_name、常驻资料、运行时段落、按需资料条目
card.json           角色卡（chara_card_v2，身份唯一来源）
core/soul.md        内核与驱动        ┐
core/speech.md      说话方式细节      ├ 常驻 S1（可选）
core/limits.md      表达与事实边界    ┘
runtime/relationship.md  关系默认（LyraBot 部署可选配置）
runtime/agency.md        能力与边界：真实操作以工具为准、工具用法、〔〕消息的含义、答应提醒时当场设 cron
notes/{appearance,behavior,relationship,life_events}.md   按需资料（persona_notes 工具读取，可选）
```

- S1 渲染顺序：`# <角色名>` + 身份优先级说明 → 关系默认 → 核心描述 → 性格 → 聊天准则 → 常驻资料 → 示例对话 → 补充设定（角色书常驻条目）→ 能力与边界；`{{user}}/{{char}}` 自动替换为 `persona.toml` 的 `user_name` 与 `card.json` 的 `name`（仓库默认示例角色卡为“唐语歌”、用户称呼为“阿颜”）。默认全量配置约 7,600 字；2026-11-08 服务器实测：极短对话首轮输入约 6.9k token，其中固定前缀约 6.1k，后续轮次缓存命中 80–98%。
- S2 会话创建时生成并冻结在 kv `persona_s2:<session>`，压缩后刷新，保证同一会话前缀字节稳定；记忆改动因此在新会话或压缩后生效。
- 尾注入（当前时间 + 与本轮输入相关的表达提示）：每轮挂在本轮最后一条 user 输入的 `Message.tail` 上，随消息落库；每次请求由 `lyrabot_agent::wire_messages` 合并进该消息末尾文本段（纯文本保持单字符串 `content`，含图片/视频时追加文本段），因此同轮工具续轮与之后各轮看到的历史前缀都与当初发出的字节一致，跨轮命中缓存；主对话角色也能看到每条历史消息当时的时间。`Message::text()` 不含尾注入，学习器、过往对话检索与压缩摘要看到的仍是原话。被取消后重新排队的输入会先清掉旧尾注入。本轮没有 user 输入时退回为只在请求里临时附加（不落库）。
- 前缀缓存保活（`[persona.keepalive]`）：订阅端点前缀缓存有效期约 20 分钟。主会话在完成模型请求后按 `interval_minutes`（默认 18 分钟）用下一轮正式请求将使用的前缀（同模型、同思考等级、同系统段、同工具列表、已落库的历史，经与正式请求相同的 `wire_messages` 组装）发送 `max_tokens = 1` 的保活请求续期，结果丢弃不落库；单次用户说话后最多连续保活 `max_count` 次（默认 9 次，约 2.7 小时），之后自动停止直到用户再次说话。用户发消息、切换会话/模型、重载配置或 `/stop` 时立即取消进行中的保活请求并通过代次（`keepalive_epoch`）拒绝旧轮次/旧保活写回；设为 0 可关闭。
- 角色按需设定通过专用轻工具 `persona_notes(topic)` 提供，条目与用途由 `persona.toml` 的 `[[notes]]` 动态生成并写在工具说明里。工作风格 `assets/agents/worker.md` 不进入主对话角色提示，留给后台执行者；运维流程位于专员记忆种子（`assets/memory_seed/`）。
- 主对话角色只用轻工具：`persona.tools` 默认 image_generate、speak、web_search、cron、skill_view、recall，另有人格专属 persona_notes、delegate、agent_ctl、memory、sticker；注册了 watch_video 时自动带上。重活经 delegate 派出（见 `05-orchestra.md`）。
- 派活时 task 照用户的原话写，不自己补充没说的范围或指标，拿不准先问。
- 主对话角色不会主动开口：答应“到时候提醒你”这类有时间的事，当场用 cron 设好并告诉用户（具体某天的用一次性提醒），时间未定先问清楚。
- 渲染卫生由 `crates/lyrabot-persona/tests/persona.rs` 断言（首块为身份、段落顺序、无 Hermes/UI/占位符残留、重复渲染字节一致）。

### 1.1 自定义角色卡、称呼与按需资料

代码不硬编码角色名与用户称呼：运行时统一从 `Persona::name()`（取自 `card.json` 的 `name`）和 `Persona::user_name()`（取自 `persona.toml` 的 `user_name`）读取，并在渲染 S1、按需资料（`notes`）、学习与记忆整理提示词时自动将 `{{char}}` 与 `{{user}}` 替换为对应名称。

#### `card.json` 字段映射（SillyTavern `chara_card_v2`）

支持直接放置字段，或包裹在 `{"spec": "chara_card_v2", "data": { ... }}` 内：

| 字段 | 是否必填 | 在 S1 中的位置与处理 |
|---|---|---|
| `name` | **必填**（非空） | S1 标题 `# <name>`、身份声明 `你就是<name>本人`，以及替换所有 `{{char}}` |
| `description` | 选填 | `## 核心描述` |
| `personality` | 选填 | `## 性格` |
| `system_prompt` | 选填 | `## 聊天准则` |
| `mes_example` | 选填 | `## 示例对话`（仅作语气参考） |
| `character_book.entries` | 选填 | 仅提取 `constant == true && enabled == true` 且非空的条目，按列表顺序拼入 `## 补充设定` |
| `first_mes` / `alternate_greetings` 等 | 忽略 | 聊天由用户主动发起，不使用固定开场白 |

#### `persona.toml` 结构与三种配置方式

| 字段 | 是否必填 | 说明 |
|---|---|---|
| `user_name` | **必填**（非空） | 对用户的称呼，替换所有 `{{user}}`，并用于记忆分区标题（`关于<user_name>`）与学习提示词 |
| `card` | 选填（默认 `"card.json"`） | 角色卡路径（相对 `assets/persona/`） |
| `core` | 选填（默认 `[]`） | 常驻在 S1（位于 `## 聊天准则` 之后、`## 示例对话` 之前）的资料列表，每项为 `{ title = "小节标题", file = "core/xxx.md" }`（文件中的一级标题自动剥离，其余标题降一级） |
| `runtime_relationship` | 选填 | 关系默认说明（如单用户关系背景），插入在 `# <name>` 之后、`## 核心描述` 之前 |
| `runtime_agency` | 选填（**建议保留**） | 默认指向 `"runtime/agency.md"`，定义真实操作以工具为准、轻重活分流（`delegate` / `cron`）与 `〔〕` 系统消息协议，内部已使用 `{{user}}` 占位符 |
| `[[notes]]` | 选填（默认 `[]`） | 按需资料列表，每项含 `id`（枚举值）、`file`（Markdown 路径）、`use`（何时查阅的一句话说明）。为空时不注册 `persona_notes` 工具 |

**方式一：仅使用 SillyTavern 角色卡（最简配置，无额外 Skill / 按需资料）**

若只有一张 `card.json`，部署时只需删去默认示例专属的 `core`、`runtime_relationship` 与 `[[notes]]`，将 `persona.toml` 简化为：

```toml
user_name = "主人"
runtime_agency = "runtime/agency.md"
```

**方式二：将原有的 Roleplay Skill（`SKILL.md`）或长设定拆分为 `core` + `[[notes]]`**

若除了角色卡，还有一份长篇人物设定、世界书非常驻条目或 Roleplay Skill（如仓库内置默认示例）：
1. 将**每轮回复都必须遵守**的核心性格、口癖、叙事红线提炼为 1–3 个文件放入 `core/`，填入 `core = [{ title = "...", file = "core/..." }, ...]`。
2. 将**特定话题才需要**的细节（如外貌穿搭、特定情境反应、相处细节、过往经历/世界观设定）按主题拆成 Markdown 文件放入 `notes/`，并在 `persona.toml` 中配置 `[[notes]]`：

```toml
user_name = "你的称呼"

core = [
  { title = "内核与驱动", file = "core/soul.md" },
  { title = "说话方式细节", file = "core/speech.md" },
  { title = "表达与事实边界", file = "core/limits.md" },
]

runtime_relationship = "runtime/relationship.md"
runtime_agency = "runtime/agency.md"

[[notes]]
id = "appearance"
file = "notes/appearance.md"
use = "聊到长相、身材、穿搭、气味、审美偏好或自拍生图时"

[[notes]]
id = "life_events"
file = "notes/life_events.md"
use = "聊到过去的事、家庭、成长经历或世界观背景时"
```

运行时会自动根据 `[[notes]]` 构造 `persona_notes(topic)` 工具的 JSON Schema 枚举与描述，模型聊到相关话题时才会调用读取，日常闲聊或技术问答不占前缀 token。

## 2. Pacer（回复节奏）

阶段机（`lyrabot_persona::pacer` 纯逻辑 + 运行时计时）：

| 阶段 | 新消息的处置 |
|---|---|
| Idle / Debouncing | 进缓冲，(重新) 计时；去抖 1.8 s，从第一条起最多等 6 s，到点合并为一轮 |
| Generating（尚未调用工具，QQ 端也尚未发出任何内容） | 取消本轮（不落库半成品），与缓冲合并后重新去抖 |
| Tools | 作为 steer 插话注入当前轮；若工具阶段已过才到达，内核在本轮收尾时转为追问处理 |
| Delivering（分段发送中） | 进缓冲，发送完成后开始新一轮去抖 |

- 分段：优先按空行；单段超过 120 字再按句末标点切；含代码块不拆；合并到至多 `max_segments`（默认 4）。段间延迟 = 400 ms + 30 ms/字，限定在 1–2.5 s（下限防止 QQ 把间隔过短的相邻消息合并成一个气泡）。流式平台（CLI）不分段；`lyrabot --no-stream` 可在本地模拟 QQ。
- `/stop` 同时停止当前轮并丢弃待合并缓冲；内核队列里未注入的插话在被打断时并回缓冲。人格一轮在独立任务里执行，panic 也会复位阶段。
- 表情：`sticker(emotion, hint?)` 工具；扫描 `<stickers_dir>`（默认 `<data_dir>/stickers`），按“情绪-场景-角色”文件名索引，情绪清单写进工具说明；图片随回复文字之后发送。目录为空时不暴露该工具。
- 参数位于配置 `[persona.pacer]`：`debounce_ms`、`max_wait_ms`、`max_segments`。

## 3. 学习与记忆（learner + consolidate）

原则“写少读多”：聊天路径不为学习和记忆多花一次模型调用；学到的东西只影响说法、用词与记忆文件，不写入身份段、不覆盖角色卡。

### 3.1 后台学习

- 触发：后台每 60 s 检查一次，主对话角色空闲且（新消息 ≥ 30 条，或空闲 ≥ 10 分钟且 ≥ 8 条）时学习；`/learn now` 立即学习。进度按会话记在 kv `learn_ckpt:<session>`，对话稿只取最新约 6000 字。模型默认 `kimi-for-coding`、思考 low。
- 一次调用同时输出两类：
  - 说法：只从用户的消息里提取 `jargon`（梗、缩写、谐音、专属叫法，带含义）与 `habit`（口头禅、称呼偏好等），每批至多 8 条；排除普通词汇、一次性事件和隐私。
  - 记忆候选：读双方对话，但只记用户陈述的（stated）、用户明确认可的约定（agreed）、用户纠正过的（corrected）；依据缺失或不在这三种之内的一律丢弃；主对话角色对自己的描述与剧情发挥、一次性任务、带时间的提醒、隐私凭据都不记。候选进入 `memory_candidates` 表。

### 3.2 说法库

- 入库（`expressions`，`(kind,text)` 唯一）：已存在则命中 +1、权重 +0.5（上限 5）；本批未出现的条目权重 −0.05，低于 0.2 删除，总数超过 300 淘汰低权重。
- 检索：每轮本地进行，不调用模型。Aho-Corasick 匹配触发词（权重 2/次）+ 情境相似度（中文单字集合去掉高频虚字，加 3 字以上英文词的 Jaccard，对语序不敏感；无触发词命中时要求 ≥ 0.2）+ 权重×0.1，取 top-3，渲染为 `〔你们之间的说法……〕` 附在时间之后，不超过 `inject_max_chars`（默认 220 字）。
- 实测：一段 6 条消息的对话学到“寄了 / 依托答辩 / 摸会儿鱼 / 贴贴”四条，含义准确；之后消息含“寄了”时提示被注入，回复理解正确。

### 3.3 记忆整理（2026-11-08，借鉴 Sodium 的两阶段闸门与 Anthropic dreaming）

- 时机：候选 ≥ 5 条或最早一条超过一天、且主对话角色空闲时进行（`/learn now` 立即）。
- 第一阶段只去掉空白与句末标点后完全相同的（保留 `C++`/`C#` 这类符号差异）；第二阶段一次低价调用让模型只给出 `add/update/remove` 操作，由 `consolidate::apply` 确定性执行：按原编号改删、再追加；单次最多删 3 条；每个文件守住 2000 字，超限的新增放弃、修改导致超限则该文件整体保持原样并撤掉对应说明；输出里有任何无法解释的操作则整批无效、候选保留。
- 一致性：等待模型时不持锁；模型返回后在共享的记忆写锁（主对话角色 `memory` 工具、`/learn forget`、`/learn undo` 共用）内重读，文件已变就放弃本轮。先写修订（整理前后全文、变更说明，存 `memory_revisions`，保留最近 20 次）再原子写文件（临时文件改名），写失败则恢复文件、撤销修订、放回候选。启动时若发现最近一次修订已记下、但文件停在整理前或只写了一半（每个文件都等于整理前或整理后），补写完整；文件被手动改过则不碰。
- 撤销：`/learn undo` 只在当前文件仍等于那次整理结果时恢复整理前内容，避免覆盖之后的手动修改。
- 主对话角色的 `memory` 工具保留，说明为“只在用户明确让你记住、或纠正你记错的事时写”。

### 3.4 管理与配置

- `/learn` 总览；`/learn list 说法`、`/learn list 记忆`（记忆按 `u1`、`m1` 编号）；`/learn forget <编号|u3|m2>`；`/learn now`、`/learn undo`、`/learn on|off`。命令输出见 `07-platform-commands.md` 第 4 节。
- 说法写入 SQLite 表达库，记忆写入 `<memory_dir>`（服务器 `/var/lib/lyrabot/memory/`）下的 `user.md`、`memory.md`，专员另有 `ops.md`、`creator.md`。
- 配置 `[persona.learner]`：`enabled`、`model`、`effort`、`batch_messages`、`idle_minutes`、`min_messages`、`max_items`、`inject_max_chars`。
- 旧会话不进提示词：需要时由 `recall` 工具按需检索（见 `06-tools-plugins.md` 第 1 节）。
