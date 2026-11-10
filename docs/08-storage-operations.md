# 存储、运行与部署

记录日期：2026-11-07（2026-11-09 自 `01-architecture.md` 拆出整理，2026-11-10 命名统一为 LyraBot）
范围：`lyrabot-store`、长期运行与稳定性、构建部署、调试协作约定。服务器上的具体操作步骤见 `deploy/README.md`。

## 1. 存储（lyrabot-store）

单个 SQLite 文件（服务器 `/var/lib/lyrabot/lyrabot.db`，WAL，`synchronous=NORMAL`）。读写都经同一个专用线程串行执行（单用户吞吐足够，且避免 `SQLITE_BUSY`）。迁移脚本内嵌，启动时按版本号顺序执行；当前 schema v4。

| 表 | 用途 |
|---|---|
| `sessions` | id、kind(persona/worker/specialist)、name、parent_session、fork_entry、leaf_id、model、effort、status、run_owner（执行者运行标记）、created/updated |
| `entries` | 会话树条目（见 `03-model-kernel.md` 第 2.3 节） |
| `mailbox` | 执行者 → 主对话角色的汇报信箱（未确认的汇报，确认后删除） |
| `run_queue` | 交给运行中执行者、尚未确定写进其会话的插话/追问（v3） |
| `outbox` | 平台发件箱（暂时没有投递通道的出站消息，v3） |
| `expressions` | 说法库 |
| `memory_candidates` | 学习器提取、等待整理的记忆候选（v4） |
| `memory_revisions` | 记忆整理的修订：整理前后全文与变更说明，保留最近 20 次（v4） |
| `recall_fts` | 过往对话检索的 FTS5 trigram 索引（v4） |
| `cron_jobs` | 定时任务（动作 JSON，一次性任务带 `once`） |
| `quota` | QQ 主动消息额度计数 |
| `kv` | 杂项状态（当前人格会话、记忆快照、学习进度、检索检查点、QQ 会话、上次清理时间等） |

- 清理：`/gc [天数]` 与每周一次的自动清理删除已结束且超过 N 天的会话及已归档的后台任务/专员任务会话（默认 30 天），连同其条目、信件、插话、检索索引与 kv；当前会话、排队或运行中的执行者/专员任务、被较新会话引用的历史与还有未确认汇报的会话保留（长期记忆文件独立存放于 `<memory_dir>`，不受会话 GC 影响）。删除较多时执行 `wal_checkpoint(TRUNCATE)` 与 `VACUUM`。
- 媒体：`<media_dir>` 每小时检查一次，删除 24 小时前的文件，跳过未确认汇报、发件箱与发送中引用的文件。
- 服务器文件布局：程序 `/opt/lyrabot/bin/lyrabot`（`lyrabot.prev` 为上一版）、资产与文档 `/opt/lyrabot/{persona,agents,skills,tools,docs}`、部署提交号 `/opt/lyrabot/VERSION`；配置 `/etc/lyrabot/lyrabot.toml`、凭据 `/etc/lyrabot/secrets/*`（属主 `lyrabot`，0600）；数据 `/var/lib/lyrabot/{lyrabot.db,memory,media,stickers,workspace}`；日志进 journald，shell 审计在 `/var/log/lyrabot/audit.jsonl`（达到 5 MB 自动轮转为 `.1`）。

## 2. 长期运行与稳定性

- 资源：tokio `worker_threads=2` + mimalloc；常驻内存约 26–37 MB（Hermes 网关曾约 240 MB）。
- systemd：`Type=simple`、`Restart=always`（`RestartSec=5`）、`MemoryMax=400M`（仅作失控保护）；未实现 sd_notify 看门狗（见 `09-backlog.md`）。
- 观测：`/status` 汇总版本、上下文、缓存命中、内存与运行时长；tracing 记录每轮用量、学习与清理结果。
- 启动顺序：配置与迁移 → 恢复被中断的后台任务 → 补写中断的记忆整理 → 装载未确认汇报 → 接入平台与定时任务。

稳定性修复（2026-11-08，依据 codex 静态审阅逐条核实）：

- 内核：压缩被打断时与生成被打断同样经 `unconsumed` 交回输入，压缩自身失败只告警；工具调用预算在执行前检查；取消时给工具 2 秒自行收尾。
- 运行时：人格一轮放在独立任务里执行，panic 也会复位阶段；未注入的插话在被打断时并回缓冲，`/stop` 时一并丢弃；GC 保留排队或运行中的执行者，每周自动清理还要求没有后台任务。
- 子进程：stdout/stderr 与等待并发读取，避免大输出写满管道卡到超时；future 被丢弃时由守卫终止整个进程组。
- 模型：错误正文读取有期限并响应取消；SSE 残余与工具调用索引设上限。
- QQ：连接与 Hello 期限、心跳 ACK 看门狗、拒收事件清锚点、发件箱补发失败放回；`allowed_users` 必须恰好一个。
- 抖音：响应缺字段不再 panic；发码取消时复位收码状态。

记忆、检索与视频（2026-11-08，codex 两轮复核）修复了 UTF-8 切片、并发覆盖记忆、修订丢失、符号差异被去重、未知依据与操作、GC 后编号复用漏索引、当前会话挤掉历史命中、并发重复建索引、临时文件争用、静默截断等问题；修订先落库带来的中断窗口由启动补写覆盖。

## 3. 构建与部署

- 本地：cargo-zigbuild 交叉编译到 `x86_64-unknown-linux-gnu`（目标机 Debian 13 x86_64，命令见 `deploy/README.md`），只从干净且已推送的提交构建；二进制内嵌构建提交（有未提交改动时带 `-dirty`），`lyrabot --version`、`--check` 与 `/status` 首行显示。
- 部署：产物上传为 `/opt/lyrabot/bin/lyrabot.new`，保留 `lyrabot.prev` 后替换并 `systemctl restart lyrabot`；同一提交的 `persona/ agents/ tools/ skills/` 与 `docs/`（`docs/*.md`、`deploy/README.md` 改名 `deploy.md`、`config/lyrabot.example.toml`）一并同步，并写入 `/opt/lyrabot/VERSION`。只改人格或技能时同步对应目录后在 QQ 发 `/reload`。涉及数据库迁移的版本，停服前先在线备份数据库，稳定后删除备份。
- 运行用户 `lyrabot`（加入 `blogsync`、`lyrabot-douyin` 组）；邮件 SSH key 与 Komari 配置在 `/etc/lyrabot/secrets/`。首次安装用 `deploy/install.sh`，布局、RTK 升级、隔离测试与人工操作抖音浏览器等步骤见 `deploy/README.md`。
- 回滚：只用上一版二进制 `lyrabot.prev`；Hermes 已于 2026-11-08 完全移除，不再能回滚到 Hermes。
- 统一基线：本机 `git rev-parse --short=12 HEAD`、GitHub `main`、服务器 `/opt/lyrabot/VERSION` 与 `/status` 首行应一致。

## 4. 调试与协作约定

- 子代理默认 `claude-haiku-5-5`、思考等级 xhigh，最多 3 个并行，写入范围互不重叠。
- 代码疑难问题可在终端调用本机 `codex` CLI 做只读 debug 分析；结论需回到代码和测试中验证。
- 开发检查与 CI 一致：`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`。
- 不提交、不推送，除非用户当次授权；secret 文件全部被 `.gitignore` 排除。
