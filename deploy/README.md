# 部署与更新（Debian 13 x86_64）

## 布局

| 路径 | 内容 | 属主 |
|---|---|---|
| `/opt/lyrabot/bin/lyrabot`（`lyrabot.prev` 为上一版） | 程序 | root |
| `/opt/lyrabot/{persona,agents,skills,tools,docs}` | 人格资产、执行者提示、技能、邮件 helper、项目文档（供 lyrabot-docs 技能读取） | root（只读） |
| `/etc/lyrabot/lyrabot.toml` | 配置（`/provider` 会写回，故目录属运行用户） | lyrabot 640 |
| `/etc/lyrabot/secrets/` | kimi、元宝、生图、grok、QQ、Komari、邮件 SSH key 与配置 | lyrabot 600 |
| `/var/lib/lyrabot/` | 运行用户家目录；数据库（`lyrabot.db`）、记忆、表情包、媒体、工作目录、制作目录 `workspace/starlibrary_covers` | lyrabot |
| `/etc/systemd/system/lyrabot.service` | `lyrabot --qq`，User=lyrabot，Restart=always，日志进 journald | root |
| `/etc/sudoers.d/lyrabot` | `lyrabot ALL=(ALL) NOPASSWD: ALL` | root 440 |
| `/usr/local/bin/rtk` | RTK 1.2.0（官方 x86_64 musl 包；升级方法见下文“升级 RTK”，升级时临时保留的 `rtk.prev` 验证后可删） | root |
| `/opt/lyrabot-douyin`、`/var/lib/lyrabot-douyin`、`/run/lyrabot-douyin/login.sock` | 抖音服务（`lyrabot-douyin.service`，运行用户 douyin-runner，源码同步在本仓库 `ops/douyin/`） | douyin-runner |
| `/etc/lyrabot-douyin.env` | `DOUYIN_ALLOWED_UID`（lyrabot 的 UID 997）、`DOUYIN_GATEWAY_UNIT=lyrabot.service`、`DOUYIN_EXPECTED_ACCOUNT` | root 600 |

运行用户 `lyrabot`（uid 997）附加组：`blogsync`（博客同步脚本、`/etc/blog-sync/proxy.key` 与博客站点目录共用的组）、`lyrabot-douyin`（抖音 socket）。

服务器上已不存在 Hermes：hermes 用户与家目录、`hermes-gateway` 及其清理定时器、`hermes-*` 脚本与 sudo 规则已于 2026-11-08 删除；原 `99-hermes-key-only.conf`（禁用 SSH 密码登录）与 `90-hermes-limits.conf`（journald 100 MB / 7 天）内容保留，更名为 `99-key-only.conf`、`90-journal-limits.conf`。**不再能回滚到 Hermes。**

## 首次安装

本机：`cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.36 -p lyrabot`，把二进制、`assets/{persona,agents,tools,memory_seed}`、`skills/`、`docs/`（组装方式见下文“统一基线”）、`deploy/{lyrabot.service,lyrabot.server.toml,install.sh}` 与 `secrets/`（文件名对应 `lyrabot.server.toml` 中的 `*_file`，如 `kimi.key`、`yuanbao.key`、`qq.secret`）放进暂存目录上传，root 执行 `LYRABOT_QQ_APP_ID=<AppID> LYRABOT_QQ_OWNER=<user_openid> install.sh <暂存目录>` 后删除暂存目录。需先部署抖音服务（提供 `lyrabot-douyin` 组）并存在 `blogsync` 组。脚本可重复执行：程序与资产每次覆盖，暂存目录里给出的凭据同名覆盖，配置与记忆只在不存在时生成。邮件 helper 的配置格式见 `assets/tools/mail_vps.example.toml`，SSH 主机指纹放 `/var/lib/lyrabot/.ssh/known_hosts`。

## 统一基线（本地 = GitHub = 服务器）

- 只从**干净且已推送**的提交构建：`git status` 为空、`git push` 完成后再 `cargo zigbuild`。二进制内嵌构建提交（有未提交改动时带 `-dirty`）。
- 部署时把同一提交的 `persona/ agents/ tools/ skills/` 与 `docs/`（`docs/*.md`、`deploy/README.md` 改名 `deploy.md`、`config/lyrabot.example.toml`）一并同步，并写入 `/opt/lyrabot/VERSION`。
- 核对：本地 `git rev-parse --short=12 HEAD`、GitHub `main`、服务器 `cat /opt/lyrabot/VERSION` 与 `lyrabot --version`（QQ 里 `/status` 首行）应为同一哈希。
- 服务器专属的值（凭据、QQ 账号、提供方地址、抖音账号）只存在于 `/etc/lyrabot` 与 `/etc/lyrabot-douyin.env`，不进仓库。

## 更新程序

```bash
scp target/x86_64-unknown-linux-gnu/release/lyrabot root@<server>:/opt/lyrabot/bin/lyrabot.new
ssh root@<server> 'chmod 755 /opt/lyrabot/bin/lyrabot.new && cp -a /opt/lyrabot/bin/lyrabot /opt/lyrabot/bin/lyrabot.prev && mv -f /opt/lyrabot/bin/lyrabot.new /opt/lyrabot/bin/lyrabot && systemctl restart lyrabot'
```

回退到上一版程序：`mv /opt/lyrabot/bin/lyrabot.prev /opt/lyrabot/bin/lyrabot && systemctl restart lyrabot`。

只改人格资产或技能时重新上传对应目录，然后在 QQ 发 `/reload`（S2 记忆快照在新会话或压缩后生效）。

## 升级 RTK

服务器直连 GitHub 很慢，在本机下载 `rtk-x86_64-unknown-linux-musl.tar.gz` 与 `checksums.txt`，核对 SHA256 后上传，服务器上再核对一次；解压后以 `lyrabot` 身份在 `/var/lib/lyrabot`（服务工作目录）下测试 `rtk rewrite "git status"` 返回 3 并输出 `rtk git status`、`rtk rewrite "echo hi"` 返回 1，再保留 `rtk.prev` 原子替换。注意：在 `lyrabot` 无权访问的目录（如 `/root`）下测试，1.x 会对所有命令返回 1。

## 抖音成片入库（本机）

`pwsh lyrabot-upload.ps1`（配置 `lyrabot-publish.json`，字段见 `ops/douyin/lyrabot-publish.example.json`；以 root 登录服务器，调用本仓库 `ops/douyin/stage_release.py prepare` 与服务器 `/opt/lyrabot-douyin/stage_release.py install`，入库到 `/var/lib/lyrabot-douyin/releases/<release_id>/`）。

## 切换与清理记录

- 2026-11-08 01:36（北京时间）切换：抖音服务 gateway 单元名改为可配置、允许 UID 改为运行用户、博客代理 key 属主改为运行用户，停用 hermes-gateway 并启用新服务。
- 2026-11-08 08:58：抖音账号改过抖音号后，`DOUYIN_EXPECTED_ACCOUNT` 同步更新后状态为 logged_in。以后再改抖音号时同样要改 `/etc/lyrabot-douyin.env` 并重启 lyrabot-douyin。需要人工操作浏览器时：root 执行 `python3 /opt/lyrabot-douyin/manual_window.py open`，本机 `ssh -N -L 16085:127.0.0.1:6085 root@<server>` 后打开 `http://127.0.0.1:16085/vnc.html`；浏览器空闲 180 秒会关闭（黑屏），需持续发送 status 保活。
- 2026-11-08：清理残留——删除服务器 `/tmp` 中 Hermes 时期的上传暂存、测试目录与 `/root` 下引用 hermes_cli 的检查脚本；博客目录中两个仍属原 hermes UID 的文件改为 `root:blogsync`（与其余文章一致）；博客同步脚本注释去掉 hermes。
- 2026-11-10：项目内部命名与部署路径全面统一为 `lyrabot`（消除旧代号 `qya` 与特定人设服务名 `tangyuge`）：二进制、crate 前缀、配置、数据库、环境变量、服务器目录（`/opt/lyrabot`、`/etc/lyrabot`、`/var/lib/lyrabot`、`/var/log/lyrabot`）、systemd 单元（`lyrabot.service`）、运行用户（`lyrabot`，UID 997 不变）及抖音外部服务（`lyrabot-douyin`）全部统一。
