# LyraBot

一个用 Rust 写的常驻个人 agent：门面是角色“唐语歌（Lyra）”，在 QQ 私聊里陪你聊天；需要动手的活交给后台执行者和常驻专员完成，再由她用自己的话转述。项目内部代号 **qya**（二进制、crate 前缀、配置目录都用这个名字）。

> A single-binary personal agent in Rust. A persona front (“Lyra”) chats on QQ while background workers and long-lived specialists do the actual work; a pi-style kernel, multi-agent session inheritance, prompt-cache-friendly persona layout, ~30 MB RSS.

## 特点

- **人格与执行分离**：唐语歌只用轻工具（生图、语音、搜索、定时提醒、记忆、表情包），重活经 `delegate` 派给后台；工具输出不污染角色上下文。
- **多代理会话继承**：`fork`（零拷贝继承当前对话）/ `brief` / `fresh` 三种上下文；子代理可选模型与思考等级；任务做完归档、可续接；常驻专员长期保有自己的会话与记忆。
- **pi 式内核**：事件流式循环、工具并行/串行声明、插话（steer）与追问（follow-up）队列、取消时消息结构合法、工具循环防护、90% 自动压缩、输入延迟落库（被打断的输入不会被当作“已处理”）。
- **缓存友好的提示词**：角色卡渲染的身份段 + 会话内冻结的记忆快照组成稳定前缀，实测后续轮次缓存命中 80–98%；时间与表达提示放在尾注入，不落库。
- **拟人节奏**：连发合并、生成中插话取消重来、分段发送、按情绪发表情包；后台学习你的口头禅与黑话，回复时本地检索至多 3 条提示，不额外调用模型。
- **可插拔提供方**：生图、语音、搜索、grok 的提供方与模型在聊天里用 `/provider` 修改，写回配置（保留注释）并热生效。
- **长期运行**：单个静态链接友好的二进制，常驻约 30 MB；QQ 网关断线自动 Resume；媒体 24 小时清理；历史会话每周自动清理。

## 架构

```
QQ 官方网关 ─▶ 平台层 ─▶ 外部服务前置拦截 ─▶ /命令（不经模型）
                                  └▶ 回复节奏 ─▶ 唐语歌（人格层 + 轻工具）
                                                   │ delegate          ▲ 〔后台结果〕
                                                   ▼                   │
                                   编排层：worker / 常驻专员（可选模型与思考等级）
内核（pi 式循环） · 模型（OpenAI 兼容 Chat Completions） · 工具 / 插件 · SQLite 会话树
```

| crate | 职责 |
|---|---|
| `qya-core` / `qya-config` | 共享类型；TOML + secret 文件配置，ArcSwap 热替换 |
| `qya-llm` | 流式 Chat Completions、思考等级映射、缓存命中、错误分类与重试 |
| `qya-store` | SQLite（WAL，单写线程）：会话树、表达库、定时任务、清理 |
| `qya-agent` | 内核循环、工具抽象、上下文构建与压缩 |
| `qya-persona` | 身份渲染、记忆快照、资料工具、表情包、回复节奏、表达学习 |
| `qya-orchestra` | 派活、上下文继承、续接、专员、结果回传 |
| `qya-tools` | shell（RTK 改写、硬禁止、审计）、文件、记忆、todo、cron、技能、运维工具 |
| `qya-plugins` | 生图、语音、网页搜索、grok |
| `qya-douyin` | 抖音外部服务客户端（校验与净化） |
| `qya-platform` | QQ 官方机器人单聊、命令行 |
| `apps/qya` | 装配、运行时、命令 |

详细设计见 [`docs/01-architecture.md`](docs/01-architecture.md)，需求与决策见 [`docs/00-PRD.md`](docs/00-PRD.md)、[`docs/02-decisions.md`](docs/02-decisions.md)。

## 快速开始（本地命令行）

```bash
cargo build --release -p qya
cp config/qya.example.toml qya.toml        # 按需修改；secret 放在 *_file 指向的文件里
mkdir -p secrets && echo "<your-key>" > secrets/kimi.key
./target/release/qya --config qya.toml --check      # 自检：配置、数据库、模型连通性
./target/release/qya --config qya.toml --cli        # 流式命令行
./target/release/qya --config qya.toml --no-stream  # 模拟 QQ 的分段发送
```

正式运行：`qya --config /etc/qya/qya.toml --qq`（需要配置 `[platforms.qq]`）。服务器部署、更新与目录布局见 [`deploy/README.md`](deploy/README.md)。

## 聊天命令

| 命令 | 作用 |
|---|---|
| `/status` | 版本、模型、上下文占用、缓存命中、内存 |
| `/new` · `/stop` · `/compact` | 新会话 · 停止当前回复 · 压缩会话 |
| `/sessions` · `/gc [天数]` | 查看会话与数据库 · 清理历史会话（默认 30 天，另有每周自动清理） |
| `/model [名]` · `/effort [等级]` | 切换唐语歌的模型与思考等级 |
| `/agents` · `/agent <编号> [stop/话]` | 查看、停止、续接后台任务 |
| `/provider [类别 字段 值]` | 查看或修改生图、语音、搜索、grok 提供方 |
| `/learn [list/forget/on/off/now]` | 表达与黑话学习 |
| `/reload` · `/help` | 重载配置与人格资产 · 帮助 |

## 安全说明

- 设计为**单用户**：QQ 只接受配置中的 `allowed_users`；shell 工具除少数硬禁止外不审批，请只在你信任的环境运行。
- secret 只从 `*_file` 读取，不会出现在日志、提示词或 Debug 输出中；仓库不包含任何凭据。
- 抖音“公开发布”只能由用户在 QQ 发送整条确认命令后由程序提交，模型没有发布动作；短信验证码由程序直接接收，不进入模型与会话记录。

## 许可

[MIT](LICENSE)。`assets/persona/` 中的角色设定为作者原创内容，随项目以相同许可发布。
