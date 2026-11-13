# 模型层与内核

记录日期：2026-11-07（2026-11-09 自 `01-architecture.md` 拆出整理，2026-11-10 命名统一为 LyraBot）
范围：`lyrabot-llm`（模型调用）与 `lyrabot-agent`（pi 式内核）。

## 1. 模型层（lyrabot-llm）

### 1.1 抽象

`ChatRequest` 只含规范化字段（system 段、messages、tools、model、effort、max_tokens、cache_key），由传输实现负责编码/解码；当前只有 OpenAI Chat Completions 传输。新增官方接口 = 新增一个传输实现，不动内核。流式事件为文本增量、思考增量、工具调用增量、用量与结束原因。

### 1.2 kimi-code 适配要点（2026-11-07 实测，11-08 复核模态）

- 端点：`https://api.kimi.com/coding/v1`，Chat Completions 传输；不带特殊 User-Agent 也返回 200，UA 仍可在配置中覆盖。
- 当前 key 可用模型（`/models`）：`kimi-for-coding`（显示名 K2.8 Preview，1M）、`kimi-for-coding-highspeed`（256K）、`k3`（1M）、`k3-256k`（256K）、`k3.1-pro`（1M）、`k3.1-pro-highspeed`（1M，2 倍用量）。全部 `supports_thinking_type=only`（思考恒开）。
- 思考等级：参数名 `reasoning_effort`，只接受 `low/high/max`。配置 `effort_map`：low→low、medium→high、high→high、xhigh→max、max→max。不存在"关闭思考"。
- 思考回放：带工具调用的 assistant 消息回放时携带模型返回的真实 `reasoning_content`；缺失时补单个空格，不伪造内容。
- 缓存与连接复用：命中数在 `usage.prompt_tokens_details.cached_tokens`。`lyrabot_llm::build` 按 `ProviderConfig` 缓存复用 `Provider` 实例及其底层 HTTP/TLS 连接池（`/reload` 配置变更后自动重建）；内核按会话携带稳定的 `prompt_cache_key`（`lyrabot-s-<session>`），每次模型请求完成时记录 `prompt_tokens`、`cached_tokens`、命中率与距该会话上次请求的间隔 `idle_s`。不把五小时额度窗口当缓存时间。
- 自检：`lyrabot --check` 只核对配置里的模型 ID 是否出现在 `/models` 列表中；`/models` 返回的 `context_length`、模态与 effort 可选值是人工核对后写进配置的，自检不逐项比对。
- 模态：`k3-256k` 输入只有文字与图片（`supports_video_in=false`）；`kimi-for-coding`、`kimi-for-coding-highspeed`、`k3`、`k3.1-pro` 系列支持视频。视频以 `{"type":"video_url","video_url":{"url":"data:video/mp4;base64,…"}}` 发送，服务器实测通过（2 秒测试视频 566 输入 token）。
- 媒体编码按目标模型在模型目录中的 `vision` / `video` 能力处理：本地 `file://` 图片（≤10 MB）与视频（≤20 MB）在请求时才转为 data URI；模型不支持视频、或视频超限时不发画面，改为一句提示“需要了解内容时用 watch_video 查看 <路径>”（见 `06-tools-plugins.md` 第 2 节）。

### 1.3 可靠性

- 重试：仅对连接错误、429、5xx 做指数退避（尊重 `Retry-After`，单次等待不超过 30 s）；流已开始输出后不重试，交给上层决定。
- 超时：连接超时可配；错误正文读取有期限并响应取消；SSE 未换行残余上限 8 MB，工具调用索引上限 128。
- 错误分类：`RateLimited / Auth / BadRequest / ContextOverflow / Transient / Unknown`，上层据此决定压缩或报告。额度用尽时主对话角色如实说明。

## 2. 内核（lyrabot-agent，pi 式）

### 2.1 循环

```
run_turn(session, input):
  loop:
    req = context.build(session)        # 系统段 + 会话树当前分支 + 动态尾注入
    stream provider → 发 AgentEvent（TextDelta/ToolStart/ToolEnd/...）
    if 无工具调用: break
    并发执行工具（按 Tool::concurrency 声明：只读并行，写串行）
    push tool results
    检查 steering 队列：有插话则注入后继续
  检查 follow-up 队列：有排队消息则开始下一轮
```

- 两个队列（与 pi 语义一致）：`steer`（工具执行间隙插入，立即影响当前轮）与 `follow_up`（当前轮结束后再处理）。
- 取消：每轮一个子 `CancellationToken`；`/stop` 或打断触发取消，给工具 2 秒自行收尾，未完成的工具结果写为已取消，保持消息结构合法。
- 输入延迟落库：本轮输入及轮中到达的插话/追问，只在模型给出回应时与回应一起写入会话树；若在此之前被取消（包括压缩被打断），原样经 `TurnOutcome::unconsumed` 交回。主对话角色据此把被打断的输入排回缓冲最前面与新消息合并（`/stop` 则丢弃）；执行者直接落库以保留任务记录。出错时输入照常落库，`TurnOutcome.inputs_saved` 标明输入是否已落库。
- 工具循环防护：同一批调用签名连续重复即停止；单轮工具调用预算（配置 `agent.max_tool_calls`）在执行前检查，超出的调用补“未执行”结果并报告。
- 错误：模型错误以 `StopReason::Error` 连同错误交回（输入已落库）；只有存储等内部错误以 `Err` 返回。

### 2.2 工具接口

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;               // name、description、JSON schema
    fn concurrency(&self) -> Concurrency { Concurrency::Exclusive }
    async fn call(&self, ctx: ToolCtx, args: serde_json::Value) -> ToolOutput;
}
```

`ToolOutput` 带内容、是否出错与媒体引用：媒体走投递通道，不靠在文本里写标签。工具集按会话角色装配，工具按名字排序输出，schema 字节稳定。

### 2.3 会话树与压缩

- 会话以条目树存储（SQLite `entries`：`id, session_id, parent_id, kind, payload, tokens, created_at`），会话记录当前叶子 `leaf_id`。分叉 = 新会话指向父会话某条目，零拷贝。
- 触发：上下文估算达到模型窗口的 90%（`agent.compaction_ratio`）时压缩，保留最近约 20k token 原文（`agent.compaction_keep_tokens`）；`/compact` 立即执行。token 本地按字符近似估算，每轮用服务端 usage 校准。
- 先折叠（2026-11-08，借鉴 Sodium 记忆系统）：切分点之前的工具结果在上下文中替换为一行占位（追加一条 `fold` 条目，原文不动）；折叠后低于窗口 60% 就到此为止，不调用模型。
- 再摘要：仍不够时做“枚举式”摘要——先清点日期与截止、数字与金额、人名与账号、已确认决定、用户纠正过的地方，再输出 `{summary, capsule}`；胶囊含目标、要求与约束、已确认决定、验证状态、下一步，由 `render_compaction` 按固定标签确定性渲染（每项 ≤8 条、每条 ≤150 字、胶囊 ≤1200 字），写入 `compaction` 条目；构建上下文时通过 `Store::context_branch` 从叶子回溯到最近的 compaction 条目的 `first_kept` 锚点即停，避免长寿命会话每次都读取并反序列化早已被压缩覆盖的远古条目。不是 JSON 时当作纯文本摘要；JSON 残缺时按无摘要处理，不把残骸写进摘要。
- 压缩自身失败只告警，不终止本轮；主对话角色压缩后刷新记忆快照（手动 `/compact` 在内容较少无需压缩历史时也会刷新当前会话的记忆快照）。
