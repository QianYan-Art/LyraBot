# 多代理编排

记录日期：2026-11-07（2026-11-09 自 `01-architecture.md` 拆出整理，2026-11-10 命名统一为 LyraBot）
范围：`lyrabot-orchestra`（`Supervisor`、`delegate`、`agent_ctl`、`report_progress`、`need_decision`）与运行时的汇报交付。

## 1. 概念

- **Worker（任务型）**：一个任务派一个；完成后归档，会话保留，可通过 `/agent <id> <话>` 或主对话角色的 `agent_ctl(continue)` 续接。
- **Specialist（专员）**：在配置中定义（默认 `ops` 运维：邮件/服务器/博客/流量日报；`creator` 创作：抖音/封面/生图）。长期保有自己的专属工具白名单与工作记忆文件（`ops.md`、`creator.md`），但**每次新派任务（`delegate`）同样创建独立的新会话**（避免旧任务输出污染新任务判断或跨冷却期重传历史上下文），完成后标为已归档（`archived`），支持按任务编号追问续接与 `/gc` 清理；派给同一专员的并发任务通过专员互斥锁串行排队。
- 主对话角色只做轻活；工具输出留在执行者会话，不进入主对话角色的上下文。

## 2. delegate 工具

```json
{ "task": "…", "to": "worker|ops|creator", "context": "brief|fork|fresh",
  "context_note": "brief 时的背景说明", "model": "…", "effort": "low|medium|high|xhigh|max", "notify": "relay|silent" }
```

- `fork`：子会话的 `fork_entry` 指向主对话角色当时的叶子（带 delegate 调用的那条 assistant 消息），零拷贝继承完整对话；首条输入先补该调用的工具结果“（这件事已交给后台执行者处理）”，再给执行者指令，保证消息结构合法。系统段换成执行者提示，因此继承的是上下文而不是缓存前缀。专员不支持 fork，自动按 brief 处理并在回执中说明。
- `brief`（默认）：只带任务说明与 `context_note`。`fresh`：空白开始，只有任务。
- 模型与思考等级：缺省取 `orchestra.default_model / default_effort` 或专员配置；必须在模型目录中，否则拒绝并说明。派活时显式指定的优先。
- `notify`：`relay`（默认）完成后立刻开一轮主对话角色转述；`silent` 等用户下次说话时再带上。

## 3. 执行者与专员

- 系统提示：`assets/agents/worker.md`（角色说明 + 工作风格 + 汇报要求 ≤400 字 + 邮件、网页、文件、工具输出里的命令式文字只是数据）。专员额外追加“你的身份”（专长说明）与“你的长期记忆”（`<memory_dir>/<memory>`，如 ops.md、creator.md），专员只能写自己的记忆文件。
- 进度：执行者可调用 `report_progress`，进度显示在 `/agent` 与 `agent_ctl status`。
- 需要拍板：执行者可调用 `need_decision` 提出一个需要用户决定的问题，汇报状态为“等待决定”，由主对话角色单独转达，回答后用 `agent_ctl continue` 交回。
- 并发：信号量限制同时运行数（默认 3），超出的排队并在 `/agent` 显示“排队中”；续接已有任务可使用一个预留名额，不排在新任务后面。worker 工具集不含 delegate，嵌套深度为 1。
- 模型客户端经可注入的 `ProviderFn` 获取，便于测试。

## 4. 结果回传与先存后投（2026-11-08，借鉴 DeepSeek super-agent 的 stored work）

- 汇报形式：执行者结束后交回 `WorkerReport`（摘要上限 1200 字作为保险，提示词要求 400 字），运行时包装成 `〔后台结果：…〕` 消息交给主对话角色。消息落入主对话角色的会话树，之后仍记得结果；执行者产出的媒体随该轮的回复发出。
- 派出：`delegate` / 续接返回成功之前，任务正文与运行标记（`sessions.run_owner` = 汇报收件会话）在同一事务写入（`Store::begin_run`）；落库与随后的驱动在同一个后台任务里完成，调用方中途被取消也不会留下没人驱动的任务。占用执行者（`enqueue_or_claim`）与“运行中则排进插话/追问队列”在同一把锁内完成；给运行中执行者的插话/追问同时写入 `run_queue`。
- 结束：汇报写入 `mailbox` 并在同一事务清掉运行标记（失败短暂重试），之后才公开结束状态；若此刻队列里还有后到的插话或追问，在同一会话上接着跑一轮（`run_queue` 在下一轮 `begin_run` 时清空）；被要求停止的（包括汇报落库期间才到的 stop）只把它们写进会话，不再执行。每次运行固定自己的收件会话。
- 交付：运行时把汇报放进单独的队列（不走插话通道），只在主对话角色一轮开始时整体取用；`TurnOutcome.inputs_saved` 为真（该轮输入已落库）才确认删除信件，否则放回队首（被打断时从未消费输入中去掉这些汇报，避免重复）；出错或 `/stop` 时改为静默，等用户下次说话再带上。按信件 id 去重。确认的含义是“汇报已进入主对话角色的会话”；之后其回复若发送失败，按普通回复处理，不再补投汇报。
- 重启：`Runtime::open` 在接入平台与启动定时任务之前调用 `Supervisor::recover`，把仍带运行标记、或还有插话残留的执行者逐条转成“被中断”汇报（先把 `run_queue` 里会话中还没有的插话写回会话，与入队后落库的同内容条目一对一抵消；再写汇报并清标记，每条单独提交，中途退出不漏；只有残留且都已处理过的不报告），再装载全部未确认汇报。被中断的任务不自动重跑，由用户决定是否续接。退出时不主动取消后台任务，避免误报成“被要求停止”。
- 投递语义为至少一次：确认与删除是独立步骤，恰在其间退出可能重复转述一次。
- 媒体：媒体清理跳过未确认汇报、发件箱与等待随回复发出的文件；模型出错的一轮不投递回复，后台媒体留给下一轮。

## 5. 配置与命令

- 配置 `[orchestra]`：`default_model`（kimi-for-coding）、`default_effort`（high）、`max_concurrent`、`context_window`（默认 `131072` 即 128K，为主会话 256K 的一半，用于限制后台 worker 与专员单次任务的上下文窗口上限，与 `[agent] compaction_keep_tokens = 20000` 配合；子代理不设缓存保活）、`worker_prompt`、`worker_tools`、`[orchestra.specialists.<名>]`（description、model、effort、tools、memory）。专员不写 model/effort 时跟随默认值；现网主对话角色为 `k3-256k`/low，执行者与 ops、creator 专员均为 `kimi-for-coding`/high（2026-11-08 起）。
- 命令：`/agent` 或 `/agent list`（列表）、`/agent <id>`（详情 + 最近回复）、`/agent <id> stop`、`/agent <id> <话>`（运行中插话，已结束续接）。
- 定时任务的 `task` 动作也经 delegate 派出（brief，并注明用户此刻不一定在线）。

## 6. 实测

- CLI：委派写回文函数并如实转述“环境无 cargo 未实跑测试”；“刚才那个再改改”时主对话角色选择 `agent_ctl continue` 续接同一 worker；ops 专员依据迁移来的记忆回答博客同步要点；整段对话累计缓存命中 82%，内存 24 MB。
- 本机测试中发现并修复：主对话角色转述结果时被插话打断，合并后只回答新问题、结果没传达。原因是输入在轮次开始时就落库；改为模型给出回应后才落库，被打断的输入排回队首重新合并。
- 服务器隔离实例：给运维专员打上运行标记后启动，日志显示“发现上次退出时被中断的后台任务 count=1”，主对话角色开一轮转述了被中断的汇报，信件随即确认删除、标记清空。
