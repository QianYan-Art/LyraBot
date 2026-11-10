# LyraBot 产品需求（PRD）

记录日期：2026-11-07（2026-11-10 命名与人设解耦统一更新）
状态：已经 grill-me 讨论确认，作为 `.mission/lyrabot.csv` 的执行输入。
关联：`docs/01-architecture.md`（技术总览与文档索引，细节见 `03`–`08`）、`docs/02-decisions.md`（关键决策）、`docs/09-backlog.md`（还没做的事）。

## 1. 目标

用 Rust 从零构建一个常驻的个人 agent（`LyraBot` / `lyrabot`），最终替代原服务器上的 Hermes 网关，接管用户的 QQ 私聊。

- 门面是主对话角色：角色内核聊天能力不能因 agent 能力退化；人设与称呼通过 `assets/persona/` 完全解耦配置，开箱支持导入任意 SillyTavern 角色卡。
- agent 能力不弱于 Hermes 常用面，并新增多代理会话继承（子代理可选模型与思考等级）。
- 极致性能与长时间稳定运行；验收以用户实际使用为准，接受使用中暴露问题再修。
- 代码优雅、可扩展：平台、模型、提供方、外部工具都有清晰的扩展点。

## 2. 使用者与边界

- 唯一用户：单实例单用户设计（QQ 唯一允许账号 `allowed_users`）。不做多用户、群聊、权限分级。
- 信任模型沿用 Hermes 现状（审批关闭）：所有者可信；只做审计日志和极少数硬禁止的高危命令。
- 第一版只接 QQ 官方机器人 API（C2C 私聊）+ 本地命令行。平台层预留 OneBot v11（SnowLuma/NapCat 个人号）接入，不在第一版实现。
- 模型只接 kimi-code（key 来自 `kimi.txt`）。模型目录和接口差异配置化，便于后续跟进官方新模型与接口调整。

## 3. 功能范围

### 3.1 内置（核心，不靠外部挂载）

| 能力 | 说明 |
|---|---|
| 角色内核 | 角色卡（`card.json`）+ 常驻/按需资料 + 关系默认（`persona.toml` 配置）；自然 QQ 回复，无 UI 面板 |
| 拟人层 | 回复节奏（合并连发、分段发送、插话打断）、表情包、表达与黑话学习（后台学习、前台限额注入） |
| 多代理 | 按任务派出、可续接；常驻专员；三种上下文继承；子代理可选模型与思考等级；结果回到主对话角色转述 |
| 终端 | shell 执行，配 RTK 压缩长输出 |
| 文件 | 读、写、补丁、搜索 |
| 长期记忆 | 记忆笔记 + 用户画像 |
| 定时任务 | cron 调度，触发后台任务或提醒 |
| todo | 多步任务清单（子代理用） |
| 消息与媒体投递 | QQ 文本、图片、语音、文件 |
| 邮件 VPS 管理 | 迁移 mail-vps-ops |
| 流量日报 | Komari 公开 API，原生 Rust 实现 |
| 博客同步 | 调用服务器 `/usr/local/bin/blog-sync-kbase.sh`，保留代理失败汇报规则 |
| 技能体系 | SKILL.md 兼容；保留 grill-me，locator 改写为本项目文档定位 `lyrabot-docs` |
| 看图/看视频 | 作为主模型原生多模态输入，模型不支持视频时提供 `watch_video` 代看工具 |
| 语音识别 | 使用 QQ 自带 `asr_refer_text`；拿不到时交给能直接听音频的模型或如实说明；不在服务器常驻 whisper |
| 聊天命令 | 重新设计，见 `docs/07-platform-commands.md` 第 4 节 |

### 3.2 可插拔提供方（QQ 命令可调）

生图、语音合成（GPT-SoVITS 工作站桥接）、网页搜索（腾讯元宝搜索，替换 Tavily）、grok-search。
要求：提供方与模型 ID、端点、默认参数写在配置；key/url 来自独立 secret 文件；在 QQ 里用 `/provider` 命令查看与修改，热生效并持久化。

### 3.3 外部工具

抖音发布（含 Playwright 浏览器）保持为独立服务 `lyrabot-douyin`，通过外部工具接口接入内核。内核同时支持通用 MCP 客户端作为后续外部工具扩展口。

### 3.4 明确不做

clarify 工具、浏览器工具组、paper-translation-to-docx、多用户、群聊、OneBot（仅预留）、Web 管理界面、闲聊式主动搭话。

## 4. 主动消息

只做任务型主动：定时任务、后台任务完成、提醒。优先在被动回复窗口内发送，窗口外才消耗 QQ 主动消息额度；额度不足时延后到用户下次说话一并告知。

## 5. 非功能要求

- 性能目标：常驻内存（RSS）空闲 ≤ 40 MB；单个静态二进制；冷启动 ≤ 1 s；热路径无 Python/Node。
- 稳定性：WS 断线自动重连与会话恢复；单任务崩溃不拖垮进程；systemd 看门狗；SQLite WAL；日志与媒体缓存自动轮转清理。
- 提示词开销：角色固定前缀可命中缓存；拟人动态注入 ≤ 150 token；子代理工具输出不进入主对话角色上下文。
- 安全：secret 只在 0600 文件中，不进日志、不进提示词、不进 Git。

## 6. 迁移与切换

- 开发期间只用本地命令行测试；功能齐后一次性停掉 Hermes、启动 `lyrabot`。
- 迁移：角色卡与按需设定、`SOUL.md` 风格、`MEMORY.md`/`USER.md`、表情包、邮件与 Komari 配置、kimi key、元宝搜索 key/url。不迁移 Hermes 会话历史。
- 收尾清理与命名统一（2026-11-08 首次切换清理，2026-11-10 全仓与服务器命名统一为 `LyraBot`）：
  1. 本地测试凭据文件已删除，生产凭据只保留在服务器 `/etc/lyrabot/secrets/`。
  2. 服务器 Hermes 已完全移除：hermes 用户与 `/home/hermes`、`hermes-gateway` 与清理定时器、`hermes-*` 脚本、sudo 规则、cron 脚本及旧残留；全局 SSH 与 journald 配置保留内容、去掉 hermes 命名。
  3. 工程、二进制、配置、数据库、systemd 服务与运行用户统一命名为 `lyrabot`（程序 `/opt/lyrabot`，配置 `/etc/lyrabot/lyrabot.toml`，数据 `/var/lib/lyrabot/lyrabot.db`，日志 `/var/log/lyrabot`）；抖音外部服务统一命名为 `lyrabot-douyin`（源码位于 `ops/douyin/`，本机入库脚本为 `lyrabot-upload.ps1`）。`blogsync` 组保留（同时是博客站点目录的权限组）。

## 7. 验收

- 每个阶段以任务表中的可验证条件为准；端到端效果由用户实际使用验收，未跑的验收在任务表中如实标注。
- 切换完成标准：用户在 QQ 里日常使用 `lyrabot` 覆盖第 3 节全部常用能力，并确认可以开始收尾清理。
