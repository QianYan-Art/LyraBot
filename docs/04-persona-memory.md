# 人格层：提示词、节奏、学习与记忆

记录日期：2026-11-07（2026-11-09 自 `01-architecture.md` 拆出整理）
范围：`qya-persona` 与运行时中唐语歌相关的部分（`apps/qya/src/runtime.rs`、`runtime/learning.rs`）。

## 1. 提示词布局（缓存友好）

| 段 | 内容 | 变化频率 |
|---|---|---|
| S1 身份（system） | 角色卡渲染 + 常驻资料 + 单用户恋人关系默认 + 能力与边界 | 几乎不变 → 缓存前缀 |
| S2 记忆快照（system） | `user.md`（关于阿颜）+ `memory.md`（约定与共同的事），各 ≤2000 字 | 每个会话冻结；新会话或压缩时刷新 |
| 历史 | 会话树当前分支（含〔后台结果〕〔定时提醒〕等系统消息） | 追加 |
| 尾注入（附在最后一条 user 消息后） | 当前时间 + ≤3 条说法提示（≤220 字） | 每轮生成，不落库 |

人格资产目录 `assets/persona/`（部署时复制到 `/opt/qya/persona/`）：

```
persona.toml        清单：user_name、常驻资料、运行时段落、按需资料条目
card.json           角色卡（chara_card_v2，身份唯一来源；迁移自 Hermes 融合版）
core/soul.md        内核与驱动        ┐
core/speech.md      说话方式细节      ├ 常驻 S1
core/limits.md      表达与事实边界    ┘
runtime/relationship.md  单用户恋人关系默认（qya 部署专属）
runtime/agency.md        能力与边界：真实操作以工具为准、工具用法、〔〕消息的含义、答应提醒时当场设 cron
notes/{appearance,behavior,relationship,life_events}.md   按需资料（persona_notes 工具读取）
```

- S1 渲染顺序：`# 唐语歌` + 身份优先级说明 → 关系默认 → 核心描述 → 性格 → 聊天准则 → 常驻资料 → 示例对话 → 补充设定（角色书常驻条目）→ 能力与边界；`{{user}}/{{char}}` 替换为 阿颜/唐语歌。约 7,600 字；2026-11-08 服务器实测：极短对话首轮输入约 6.9k token，其中固定前缀约 6.1k，后续轮次缓存命中 80–98%。
- S2 会话创建时生成并冻结在 kv `persona_s2:<session>`，压缩后刷新，保证同一会话前缀字节稳定；记忆改动因此在新会话或压缩后生效。
- 原 Hermes 的 `skill_view(tangyuge-roleplay)` 改为专用轻工具 `persona_notes(topic)`，条目与用途写在工具说明里。Hermes 的 `SOUL.md`（工作风格）不进入唐语歌提示，留给后台执行者；`MEMORY.md` 的运维流程迁到专员记忆种子（`assets/memory_seed/`）。
- 唐语歌只用轻工具：`persona.tools` 默认 image_generate、speak、web_search、cron、skill_view、recall，另有人格专属 persona_notes、delegate、agent_ctl、memory、sticker；注册了 watch_video 时自动带上。重活经 delegate 派出（见 `05-orchestra.md`）。
- 唐语歌不会主动开口：答应“到时候提醒你”这类有时间的事，当场用 cron 设好并告诉阿颜（具体某天的用一次性提醒），时间未定先问清楚。
- 渲染卫生由 `crates/qya-persona/tests/persona.rs` 断言（首块为身份、段落顺序、无 Hermes/UI/占位符残留、重复渲染字节一致）。

## 2. Pacer（回复节奏）

阶段机（`qya_persona::pacer` 纯逻辑 + 运行时计时）：

| 阶段 | 新消息的处置 |
|---|---|
| Idle / Debouncing | 进缓冲，(重新) 计时；去抖 1.8 s，从第一条起最多等 6 s，到点合并为一轮 |
| Generating（尚未调用工具，QQ 端也尚未发出任何内容） | 取消本轮（不落库半成品），与缓冲合并后重新去抖 |
| Tools | 作为 steer 插话注入当前轮；若工具阶段已过才到达，内核在本轮收尾时转为追问处理 |
| Delivering（分段发送中） | 进缓冲，发送完成后开始新一轮去抖 |

- 分段：优先按空行；单段超过 120 字再按句末标点切；含代码块不拆；合并到至多 `max_segments`（默认 4）。段间延迟 = 400 ms + 30 ms/字，上限 2.5 s。流式平台（CLI）不分段；`qya --no-stream` 可在本地模拟 QQ。
- `/stop` 同时停止当前轮并丢弃待合并缓冲；内核队列里未注入的插话在被打断时并回缓冲。人格一轮在独立任务里执行，panic 也会复位阶段。
- 表情：`sticker(emotion, hint?)` 工具；扫描 `<stickers_dir>`（默认 `<data_dir>/stickers`），按“情绪-场景-角色”文件名索引，情绪清单写进工具说明；图片随回复文字之后发送。目录为空时不暴露该工具。
- 参数位于配置 `[persona.pacer]`：`debounce_ms`、`max_wait_ms`、`max_segments`。

## 3. 学习与记忆（learner + consolidate）

原则“写少读多”：聊天路径不为学习和记忆多花一次模型调用；学到的东西只影响说法、用词与记忆文件，不写入身份段、不覆盖角色卡。

### 3.1 后台学习

- 触发：后台每 60 s 检查一次，唐语歌空闲且（新消息 ≥ 30 条，或空闲 ≥ 10 分钟且 ≥ 8 条）时学习；`/learn now` 立即学习。进度按会话记在 kv `learn_ckpt:<session>`，对话稿只取最新约 6000 字。模型默认 `kimi-for-coding`、思考 low。
- 一次调用同时输出两类：
  - 说法：只从阿颜的消息里提取 `jargon`（梗、缩写、谐音、专属叫法，带含义）与 `habit`（口头禅、称呼偏好等），每批至多 8 条；排除普通词汇、一次性事件和隐私。
  - 记忆候选：读双方对话，但只记阿颜陈述的（stated）、阿颜明确认可的约定（agreed）、阿颜纠正过的（corrected）；依据缺失或不在这三种之内的一律丢弃；唐语歌对自己的描述与剧情发挥、一次性任务、带时间的提醒、隐私凭据都不记。候选进入 `memory_candidates` 表。

### 3.2 说法库

- 入库（`expressions`，`(kind,text)` 唯一）：已存在则命中 +1、权重 +0.5（上限 5）；本批未出现的条目权重 −0.05，低于 0.2 删除，总数超过 300 淘汰低权重。
- 检索：每轮本地进行，不调用模型。Aho-Corasick 匹配触发词（权重 2/次）+ 情境相似度（中文单字集合去掉高频虚字，加 3 字以上英文词的 Jaccard，对语序不敏感；无触发词命中时要求 ≥ 0.2）+ 权重×0.1，取 top-3，渲染为 `〔你们之间的说法……〕` 附在时间之后，不超过 `inject_max_chars`（默认 220 字）。
- 实测：一段 6 条消息的对话学到“寄了 / 依托答辩 / 摸会儿鱼 / 贴贴”四条，含义准确；之后消息含“寄了”时提示被注入，回复理解正确。

### 3.3 记忆整理（2026-11-08，借鉴 Sodium 的两阶段闸门与 Anthropic dreaming）

- 时机：候选 ≥ 5 条或最早一条超过一天、且唐语歌空闲时进行（`/learn now` 立即）。
- 第一阶段只去掉空白与句末标点后完全相同的（保留 `C++`/`C#` 这类符号差异）；第二阶段一次低价调用让模型只给出 `add/update/remove` 操作，由 `consolidate::apply` 确定性执行：按原编号改删、再追加；单次最多删 3 条；每个文件守住 2000 字，超限的新增放弃、修改导致超限则该文件整体保持原样并撤掉对应说明；输出里有任何无法解释的操作则整批无效、候选保留。
- 一致性：等待模型时不持锁；模型返回后在共享的记忆写锁（唐语歌 `memory` 工具、`/learn forget`、`/learn undo` 共用）内重读，文件已变就放弃本轮。先写修订（整理前后全文、变更说明，存 `memory_revisions`，保留最近 20 次）再原子写文件（临时文件改名），写失败则恢复文件、撤销修订、放回候选。启动时若发现最近一次修订已记下、但文件停在整理前或只写了一半（每个文件都等于整理前或整理后），补写完整；文件被手动改过则不碰。
- 撤销：`/learn undo` 只在当前文件仍等于那次整理结果时恢复整理前内容，避免覆盖之后的手动修改。
- 唐语歌的 `memory` 工具保留，说明为“只在阿颜明确让你记住、或纠正你记错的事时写”。

### 3.4 管理与配置

- `/learn` 总览；`/learn list 说法`、`/learn list 记忆`（记忆按 `u1`、`m1` 编号）；`/learn forget <编号|u3|m2>`；`/learn now`、`/learn undo`、`/learn on|off`。命令输出见 `07-platform-commands.md` 第 4 节。
- 说法写入 SQLite 表达库，记忆写入 `<memory_dir>`（服务器 `/var/lib/qya/memory/`）下的 `user.md`、`memory.md`，专员另有 `ops.md`、`creator.md`。
- 配置 `[persona.learner]`：`enabled`、`model`、`effort`、`batch_messages`、`idle_minutes`、`min_messages`、`max_items`、`inject_max_chars`。
- 旧会话不进提示词：需要时由 `recall` 工具按需检索（见 `06-tools-plugins.md` 第 1 节）。
