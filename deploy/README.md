# 部署与更新（Debian 13 x86_64）

## 布局

| 路径 | 内容 | 属主 |
|---|---|---|
| `/opt/qya/bin/qya`（`qya.prev` 为上一版） | 程序 | root |
| `/opt/qya/{persona,agents,skills,tools}` | 人格资产、执行者提示、技能、邮件 helper | root（只读） |
| `/etc/qya/qya.toml` | 配置（`/provider` 会写回，故目录属运行用户） | tangyuge 640 |
| `/etc/qya/secrets/` | kimi、元宝、生图、grok、QQ、Komari、邮件 SSH key 与配置 | tangyuge 600 |
| `/var/lib/qya/` | 运行用户家目录；数据库、记忆、表情包、媒体、工作目录、制作目录 `workspace/starlibrary_covers` | tangyuge |
| `/etc/systemd/system/qya.service` | `qya --qq`，User=tangyuge，Restart=always，日志进 journald | root |
| `/etc/sudoers.d/tangyuge` | `tangyuge ALL=(ALL) NOPASSWD: ALL` | root 440 |
| `/usr/local/bin/rtk` | RTK | root |
| `/opt/tangyuge-douyin`、`/var/lib/tangyuge-douyin`、`/run/tangyuge-douyin/login.sock` | 抖音服务（`tangyuge-douyin.service`，运行用户 douyin-runner，源码同步在本仓库 `ops/douyin/`） | douyin-runner |
| `/etc/tangyuge-douyin.env` | `DOUYIN_ALLOWED_UID`（tangyuge 的 UID 997）、`DOUYIN_GATEWAY_UNIT=qya.service`、`DOUYIN_EXPECTED_ACCOUNT` | root 600 |

运行用户 `tangyuge`（uid 997）附加组：`blogsync`（博客同步脚本、`/etc/blog-sync/proxy.key` 与博客站点目录共用的组）、`tangyuge-douyin`（抖音 socket）。

服务器上已不存在 Hermes：hermes 用户与家目录、`hermes-gateway` 及其清理定时器、`hermes-*` 脚本与 sudo 规则已于 2026-11-08 删除；原 `99-hermes-key-only.conf`（禁用 SSH 密码登录）与 `90-hermes-limits.conf`（journald 100 MB / 7 天）内容保留，更名为 `99-key-only.conf`、`90-journal-limits.conf`。**不再能回滚到 Hermes。**

## 首次安装

本机：`cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.36 -p qya`，把二进制、`assets/{persona,agents,tools,memory_seed}`、`skills/`、`deploy/{qya.service,qya.server.toml,install.sh}` 与 `secrets/`（文件名对应 `qya.server.toml` 中的 `*_file`，如 `kimi.key`、`yuanbao.key`、`qq.secret`）放进暂存目录上传，root 执行 `QYA_QQ_APP_ID=<AppID> QYA_QQ_OWNER=<user_openid> install.sh <暂存目录>` 后删除暂存目录。需先部署抖音服务（提供 `tangyuge-douyin` 组）并存在 `blogsync` 组。脚本可重复执行：程序与资产每次覆盖，暂存目录里给出的凭据同名覆盖，配置与记忆只在不存在时生成。邮件 helper 的配置格式见 `assets/tools/mail_vps.example.toml`，SSH 主机指纹放 `/var/lib/qya/.ssh/known_hosts`。

## 统一基线（本地 = GitHub = 服务器）

- 只从**干净且已推送**的提交构建：`git status` 为空、`git push` 完成后再 `cargo zigbuild`。二进制内嵌构建提交（有未提交改动时带 `-dirty`）。
- 部署时把同一提交的 `persona/ agents/ tools/ skills/` 一并同步，并写入 `/opt/qya/VERSION`。
- 核对：本地 `git rev-parse --short=12 HEAD`、GitHub `main`、服务器 `cat /opt/qya/VERSION` 与 `qya --version`（QQ 里 `/status` 首行）应为同一哈希。
- 服务器专属的值（凭据、QQ 账号、提供方地址、抖音账号）只存在于 `/etc/qya` 与 `/etc/tangyuge-douyin.env`，不进仓库。

## 更新程序

```bash
scp target/x86_64-unknown-linux-gnu/release/qya root@<server>:/opt/qya/bin/qya.new
ssh root@<server> 'chmod 755 /opt/qya/bin/qya.new && cp -a /opt/qya/bin/qya /opt/qya/bin/qya.prev && mv -f /opt/qya/bin/qya.new /opt/qya/bin/qya && systemctl restart qya'
```

回退到上一版程序：`mv /opt/qya/bin/qya.prev /opt/qya/bin/qya && systemctl restart qya`。

只改人格资产或技能时重新上传对应目录，然后在 QQ 发 `/reload`（S2 记忆快照在新会话或压缩后生效）。

## 抖音成片入库（本机）

`pwsh tangyuge-upload.ps1`（配置 `tangyuge-publish.json`，字段见 `ops/douyin/tangyuge-publish.example.json`；以 root 登录服务器，调用本仓库 `ops/douyin/stage_release.py prepare` 与服务器 `/opt/tangyuge-douyin/stage_release.py install`，入库到 `/var/lib/tangyuge-douyin/releases/<release_id>/`）。

## 切换与清理记录

- 2026-11-08 01:36（北京时间）切换：抖音服务 gateway 单元名改为可配置并设为 `qya.service`、允许 UID 改为运行用户、博客代理 key 属主改为运行用户，停用 hermes-gateway，启用 qya.service。
- 2026-11-08 08:58：抖音账号改过抖音号后，`DOUYIN_EXPECTED_ACCOUNT` 同步更新后状态为 logged_in。以后再改抖音号时同样要改 `/etc/tangyuge-douyin.env` 并重启 tangyuge-douyin。需要人工操作浏览器时：root 执行 `python3 /opt/tangyuge-douyin/manual_window.py open`，本机 `ssh -N -L 16085:127.0.0.1:6085 root@<server>` 后打开 `http://127.0.0.1:16085/vnc.html`；浏览器空闲 180 秒会关闭（黑屏），需持续发送 status 保活。
- 2026-11-08 03:22 改名：运行用户 qya → tangyuge（UID 不变）；hermes-douyin → tangyuge-douyin（单元、目录、用户组、环境文件、代码中的路径常量；删除 Hermes 安装器 install.py）。一次性改名脚本已执行并从仓库移除，见提交 `690520d` 中的 `deploy/rename-tangyuge.sh`。首次安装时从 Hermes 迁移凭据与数据的步骤同样已从 `install.sh` 移除。
- 2026-11-08：清理残留——邮件 helper 配置环境变量 `HERMES_MAIL_VPS_CONFIG` 改为 `QYA_MAIL_VPS_CONFIG`；删除服务器 `/tmp` 中 Hermes 时期的上传暂存、测试目录与 `/root` 下引用 hermes_cli 的检查脚本；博客目录中两个仍属原 hermes UID 的文件改为 `root:blogsync`（与其余文章一致）；博客同步脚本注释去掉 hermes。
