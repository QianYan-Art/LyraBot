# 工具、提供方与外部服务

记录日期：2026-11-07（2026-11-09 自 `01-architecture.md` 拆出整理）
范围：`qya-tools`（内置工具）、`qya-plugins`（可插拔提供方与 watch_video）、`qya-douyin`（抖音外部服务）。

## 1. 内置工具（qya-tools）

| 工具 | 要点 |
|---|---|
| `shell` | `<shell> -lc`；先经 RTK 改写；长输出落盘只给首尾；进程组清理；硬禁止 + 审计日志 |
| `fs_read / fs_write / fs_patch / fs_search` | patch 要求 old 唯一（或 replace_all）；搜索优先 ripgrep，退回 grep |
| `memory` | 编辑记忆文件（§ 分隔条目，每文件 2000 字），新会话生效 |
| `todo` | 会话内整表覆盖的步骤清单 |
| `cron` | 定时提醒与定时后台任务，北京时间，支持一次性 |
| `mail` | 邮件服务器操作（沿用 Hermes 已验证的 Python 脚本） |
| `komari_report` | Komari 公开 API 流量日报 |
| `blog_sync` | 执行服务器博客同步脚本 |
| `skill_view` | 按需读取 SKILL.md |
| `recall` | 检索唐语歌与阿颜的过往对话 |

- `shell`：执行前调用 `rtk rewrite <cmd>`（2 s 超时，退出码 0/3 且有输出才采用；RTK 1.2.0 起 1 为不改写、2 为拒绝）。输出超过 `output_limit`（默认 8000 字）时，完整内容存到 `<media_dir>/tool-output/`（随媒体清理回收），返回首 40% 尾 60% 与“共多少行、存在哪里、用 fs_search / fs_read 细看”的说明；stdout/stderr 与等待并发读取，每路最多保留 4 MB，超出时如实标注截断。Unix 以 `setsid` 启动，超时、取消或调用被丢弃时 `kill -KILL -<pgid>` 清理整个进程组。硬禁止：对 `/`、`/etc`、`/usr`、`/var/lib/qya` 的递归删除，`mkfs`、`dd of=/dev/*`、关机重启；其余不审批（单用户设计）。每次调用（含被拒绝的）写入审计 JSONL `/var/log/qya/audit.jsonl`。Windows 开发机需把 shell 指向 Git Bash，否则会解析到 WSL 的 bash.exe。
- `memory`：按角色构造实例——唐语歌可写 `user`/`memory`，专员只可写自己的记忆文件（经编排的 extras 钩子注入）；与记忆整理共用写锁（见 `04-persona-memory.md` 第 3 节）。
- `cron`：5 段表达式按北京时间（croner + jiff）；运行时每 30 s 检查到期任务，停机期间错过的只补一次；`remind` 以〔定时提醒〕消息交给唐语歌，`task` 派给指定执行者/专员并按 relay 交回；恢复暂停的任务以当前时间为基准，不补发。动作带 `once: true` 的一次性任务（“明天 9 点”“11 月 10 日”这类具体某天的事）触发前即删除，不会第二年再响（2026-11-09 验收时发现并修复）。没有单独的聊天命令。
- `skill_view`：扫描 `skills/*/SKILL.md` frontmatter，技能清单写在工具说明里；当前技能 grill-me（迁移）、qya-docs（定位本项目文档，同时列服务器路径与仓库路径，部署时文档同步到 `/opt/qya/docs/`）。mail-vps-ops、grok-search、tangyuge-roleplay 已分别由 mail 工具、grok 插件、persona_notes 取代。
- `mail`：保留 Hermes 已验证的 `mail_vps_fetch.py`（仅标准库，经受限 SSH key；位于 `assets/tools/`），工具把结构化参数映射为命令行、做必填校验、解析 JSON，`media_tag/preferred_media_tag` 指向的附件与链接 txt 自动投递；全部邮箱列表不自动加 `--domain/--limit`；写操作失败提示“不要重试、先 read 核实”。Python 只在邮件操作时启动，不在热路径。
- `komari_report`：Python 版逐条移植为 Rust（同样的校验、错误码、口径：近 24 h UTC，缺失不记零），实测 13 台节点 12 台有效。“流量日报”“流量报告”“服务器流量”“探针流量”都指它；网站访问统计是另一回事（2026-11-09 唐语歌曾把“流量报告”转成“网站流量”，专员去翻了 nginx 日志，已在 ops 介绍、工具说明、ops 记忆与派活规则里写明）。
- `blog_sync`：调用 `/usr/local/bin/blog-sync-kbase.sh`（超时 1300 s），日志含“代理同步失败”时在汇报首行提示一次，不干预脚本。
- `recall`（2026-11-08）：检索唐语歌与阿颜之前的会话与压缩摘要。查询时先在一个事务里把检查点之后的新条目增量写入 FTS5 trigram 索引，再在 SQL 中排除当前会话后取最新 8 条，返回命中处前后约 60 字的片段（总计 ≤1500 字），注明日期与说话人。查询可写多个说法（空格、顿号分隔，最多 6 个），分别匹配后合并去重；3 字以下的词退回子串扫描。query 留空时返回之前最近的 12 条对话（按时间正序）——“上次说去哪儿玩了”这类问题字面关键词对不上原话，靠这一模式找到。不用就没有常驻开销；GC 删除会话时一并删除索引并回调检查点。
- 原设计中的 `send` 工具未实现：执行者产出的媒体随〔后台结果〕由唐语歌那一轮一起发出。
- 工具分配：唐语歌见 `04-persona-memory.md` 第 1 节；worker 与专员的工具清单见配置 `[orchestra]`。

## 2. 可插拔提供方（qya-plugins）

- 配置结构：`[plugins.<类别>] active` + `providers.<名>`（原始 TOML 表，含 `kind`），类别为 image、tts、search、grok；字段示例见 `config/qya.example.toml`。工具每次调用读取当前激活提供方，`/provider` 写回后立即生效。secret 只接受 `*_file`（取文件首行）；`/provider` 拒绝修改 `*_file` 与 `kind`，修改后若新配置校验失败自动恢复原文件。
- 元宝搜索（kind `tencent_wsa`，2026-11-07 实测）：`POST <url>`，`Authorization: Bearer <服务 API Key>`，请求体 `{"Query", "Cnt"(10/20，其他值报 illegal Cnt), "Site"?, "Mode"?}`；返回 `Response.Pages` 为 JSON 字符串数组（title/url/site/date/passage/content/score/favicon/pics），错误在 `Response.Error`。`FromTime` 被拒绝；`Mode:1` 对普通查询可能返回空，默认 0。
- 生图（kind `openai_images`）：无输入图 JSON 调 `/images/generations`，有输入图 multipart 调 `/images/edits`（mask 需配合输入图）；`b64_json` 或 `url` 均支持；HTTP 200 但正文含 `error` 判失败；解析 PNG/JPEG/WebP/GIF 头部取实际尺寸，与请求比例偏差 >5% 时提示。
- 语音（kind `gptsovits_http`）：经服务器本机 19880（SSH 反向隧道到工作站）调用 GPT-SoVITS，沿用 Hermes 桥接的请求体，校验 RIFF/WAVE 与 data 块长度（空/截断即失败）；连不上时明确提示“语音工作站离线或转发未建立”。
- grok（kind `openai_chat`）：`grok_search`（JSON 结果数组）与 `grok_fetch`（网页转 Markdown），提示词移植自 Hermes grok-search。`grok_fetch` 可带 `focus`，只提取相关段落（≤3000 字），默认上限 12000 字。
- 搜索结果整形（`shape.rs`，2026-11-08）：元宝与 grok 搜索统一为“标题（来源 · 日期）/ 链接 / 摘要”的紧凑列表；按链接（协议、`www.`、主机名大小写、末尾斜杠、锚点不计，路径与参数区分大小写）与标题去重；元宝按 `score` 排序、摘要缺失时用正文开头；摘要在句子边界截到 200 字。grok 搜索解析失败时截取原文前 4000 字。
- `watch_video`（2026-11-08）：用 `[tools] video_model`（默认 `kimi-for-coding`，须在唐语歌提供方的模型目录中且支持视频，否则不注册）代看视频并回答问题，唐语歌再用自己的口吻回复。只允许读取媒体目录内的文件；超过 20 MB 先用 ffmpeg 压到 480p、每秒 2 帧、去掉音轨（独立临时文件、原子改名发布，结果缓存在 `<media_dir>/video-cache/`，所有失败出口清理临时文件，180 s 超时）；超过 10 分钟只看前 10 分钟并在结果开头注明，读不出时长时也如实说明；压缩后仍超 20 MB 则看不了。

## 3. 抖音外部服务（qya-douyin）

- 服务：`tangyuge-douyin`（目录 `/opt/tangyuge-douyin`、`/var/lib/tangyuge-douyin`，socket `/run/tangyuge-douyin/login.sock`，组 `tangyuge-douyin`），源码同步在 `ops/douyin/`。不走 MCP shim：服务按 `SO_PEERCRED` 的 **UID** 授权（`DOUYIN_ALLOWED_UID`，即运行用户 tangyuge），`publish`、`verification_request_sms`、`verification_submit` 还要求调用方 PID 等于 `systemctl show <单元> MainPID`，单元名读 `DOUYIN_GATEWAY_UNIT`（默认 qya.service）。qya 主进程直连 socket 即可满足。
- 协议：每次一个连接，一行 JSON 请求、一行 JSON 响应。登录 `{"action":"status"|"login","recover"?}`（响应 ≤3 MB，含 `qr_png_base64` 时落盘 PNG 作为媒体）；内容 `{"action":"content","request":{...}}`（请求 ≤12 MB，响应 ≤1 MB）。只有 status/login、list、job 在连接错误时重试一次，写请求不重发；超时 50 s。工具为 `douyin_login / douyin_content / douyin_verification`，归创作专员。
- 校验与净化：参数白名单与格式（release_id、32 位 plan_id/job_id/image_id、64 位 cover_id、publish_at 必须带时区）；响应根字段、状态集合、恢复指引集合、分页范围，缺字段报错而不 panic；封面预览 JPEG 必须为 320×240 / 240×320、≤32 KB，落盘后作为媒体发给阿颜；递归拒绝含 cookie/base64/path 的键名，以及 cookie 赋值、file://、绝对路径、base64 原文。
- 封面制作目录（`[douyin] saved_covers_dir`，服务器 `/var/lib/qya/workspace/starlibrary_covers`）：只读 `<release_id>/<release_id>_<9x16|16x9|4x3|3x4>.<png|jpg|jpeg|webp>`，拒绝符号链接与非普通文件，读取前后比对大小与修改时间，格式/比例/像素按文件头校验；image_id = sha256(release_id、文件名、内容哈希) 前 32 位，内容变化即失效。
- 发布（阿颜决定）：`publish` 不给模型。阿颜在 QQ 发送整条“确认发布 <plan_id>”时，运行时在命令解析与模型之前拦截，立即回执，后台直接提交发布并跟进任务最多 3 分钟；需要短信验证时自动请求发码；结果以〔抖音发布：…〕消息交给唐语歌转述。
- 短信验证码：仅当存在收码窗口时拦截纯文本六位数字；固定回执直接发给阿颜，验证码只在提交请求里出现，不进入模型、日志、会话记录；结果另以〔抖音短信验证…〕告知唐语歌。发码请求被取消时守卫复位收码状态；发码结果非 ready 时报告失败。
- QQ 图片：入站图片的本地路径登记 30 分钟（最多 20 张），供 `cover_sources` / `cover_import` 使用。
- 人工操作抖音浏览器的步骤见 `deploy/README.md`。
