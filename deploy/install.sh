#!/usr/bin/env bash
# qya 服务器安装（任务 15）。以 root 运行；可重复执行。
# 运行用户 tangyuge；抖音服务 tangyuge-douyin。
# 第 5、7 步中“从 Hermes 迁移”的部分只在首次安装（2026-11-08）时执行过，Hermes 已移除，重装时凭据与数据保留在 /etc/qya、/var/lib/qya，相关步骤会自动跳过。
# 用法：install.sh <暂存目录>（内含 qya 二进制、persona/ agents/ skills/ tools/ memory_seed/、qya.service、qya.server.toml、secrets/）
set -euo pipefail
STAGE="${1:?需要暂存目录}"
HERMES_HOME=/home/hermes/.hermes
ENV_FILE="$HERMES_HOME/.env"

env_get() { grep -E "^$1=" "$ENV_FILE" | head -1 | cut -d= -f2- | tr -d '"'"'"; }

# 1. 用户与组（与 Hermes 同等的组权限：博客同步脚本、抖音 socket）
if ! id tangyuge >/dev/null 2>&1; then
  useradd --system --create-home --home-dir /var/lib/qya --shell /bin/bash tangyuge
fi
usermod -aG blogsync,tangyuge-douyin tangyuge

# 2. 目录
install -d -o root -g root -m 755 /opt/qya /opt/qya/bin
install -d -o tangyuge -g tangyuge -m 750 /etc/qya            # /provider 需要写回配置
install -d -o tangyuge -g tangyuge -m 700 /etc/qya/secrets
install -d -o tangyuge -g tangyuge -m 750 /var/lib/qya /var/lib/qya/memory /var/lib/qya/stickers /var/lib/qya/media \
  /var/lib/qya/work /var/lib/qya/workspace /var/log/qya

# 3. 程序与资产（每次覆盖）
install -o root -g root -m 755 "$STAGE/qya" /opt/qya/bin/qya.new
mv -f /opt/qya/bin/qya.new /opt/qya/bin/qya
for d in persona agents skills tools; do
  rm -rf "/opt/qya/$d"
  cp -r "$STAGE/$d" "/opt/qya/$d"
done
chown -R root:root /opt/qya
chmod -R a+rX /opt/qya

# 4. RTK 移到系统路径（原在 hermes 家目录）
if [ -x /home/hermes/.local/bin/rtk ] && [ ! -x /usr/local/bin/rtk ]; then
  install -o root -g root -m 755 /home/hermes/.local/bin/rtk /usr/local/bin/rtk
fi

# 5. 凭据：本机上传的 kimi/yuanbao，其余首次安装时从 Hermes 迁移（只在服务器内部复制，不经网络传输）
if [ -d "$HERMES_HOME" ]; then
S=/etc/qya/secrets
put() { printf '%s\n' "$2" > "$S/$1"; chown tangyuge:tangyuge "$S/$1"; chmod 600 "$S/$1"; }
install -o tangyuge -g tangyuge -m 600 "$STAGE/secrets/kimi.key" "$S/kimi.key"
install -o tangyuge -g tangyuge -m 600 "$STAGE/secrets/yuanbao.key" "$S/yuanbao.key"
put image.key "$(env_get OPENAI_IMAGE_API_KEY)"
put grok.url "$(env_get GROK_API_URL)"
put grok.key "$(env_get GROK_API_KEY)"
put qq.secret "$(env_get QQ_CLIENT_SECRET)"
install -o tangyuge -g tangyuge -m 600 "$HERMES_HOME/komari_traffic.toml" "$S/komari.toml"
install -o tangyuge -g tangyuge -m 600 /home/hermes/.ssh/mail_vps_readonly_ed25519 "$S/mail_vps_readonly_ed25519"
install -o tangyuge -g tangyuge -m 600 /home/hermes/.ssh/mail_vps_readwrite_ed25519 "$S/mail_vps_readwrite_ed25519"
sed -e "s#/home/hermes/.ssh/mail_vps_readonly_ed25519#$S/mail_vps_readonly_ed25519#" \
    -e "s#/home/hermes/.ssh/mail_vps_readwrite_ed25519#$S/mail_vps_readwrite_ed25519#" \
    -e "s#^attachments_dir *=.*#attachments_dir = \"/var/lib/qya/media/mail_attachments\"#" \
    -e "s#^links_dir *=.*#links_dir = \"/var/lib/qya/media/mail_verification_links\"#" \
    "$HERMES_HOME/mail_vps.toml" > "$S/mail_vps.toml"
chown tangyuge:tangyuge "$S/mail_vps.toml"; chmod 600 "$S/mail_vps.toml"
# 邮件 helper 的 SSH 需要 known_hosts：沿用 hermes 已信任的主机指纹
if [ -f /home/hermes/.ssh/known_hosts ]; then
  install -d -o tangyuge -g tangyuge -m 700 /var/lib/qya/.ssh
  install -o tangyuge -g tangyuge -m 644 /home/hermes/.ssh/known_hosts /var/lib/qya/.ssh/known_hosts
fi

fi

# 6. 配置（只在不存在时生成，避免覆盖 /provider 写回的改动）
if [ ! -f /etc/qya/qya.toml ]; then
  sed -e "s#__QQ_APP_ID__#$(env_get QQ_APP_ID)#" \
      -e "s#__QQ_OWNER__#$(env_get QQ_ALLOWED_USERS | cut -d, -f1)#" \
      "$STAGE/qya.server.toml" > /etc/qya/qya.toml
  chown tangyuge:tangyuge /etc/qya/qya.toml; chmod 640 /etc/qya/qya.toml
fi

# 7. 数据迁移（只在目标为空时复制）
for f in user.md ops.md creator.md; do
  [ -f "/var/lib/qya/memory/$f" ] || install -o tangyuge -g tangyuge -m 640 "$STAGE/memory_seed/$f" "/var/lib/qya/memory/$f"
done
[ -d "$HERMES_HOME/emojis" ] && [ -z "$(ls -A /var/lib/qya/stickers)" ] && { cp -a "$HERMES_HOME/emojis/." /var/lib/qya/stickers/; chown -R tangyuge:tangyuge /var/lib/qya/stickers; }
if [ -d /home/hermes/workspace/starlibrary_covers ] && [ ! -d /var/lib/qya/workspace/starlibrary_covers ]; then
  cp -a /home/hermes/workspace/starlibrary_covers /var/lib/qya/workspace/
  chown -R tangyuge:tangyuge /var/lib/qya/workspace
fi

# 8. 运维权限：沿用原 Hermes 的权限级别（NOPASSWD: ALL）
echo 'tangyuge ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/tangyuge.tmp
chmod 440 /etc/sudoers.d/tangyuge.tmp
visudo -cf /etc/sudoers.d/tangyuge.tmp >/dev/null && mv -f /etc/sudoers.d/tangyuge.tmp /etc/sudoers.d/tangyuge

# 9. systemd 单元（安装但不启动）
install -o root -g root -m 644 "$STAGE/qya.service" /etc/systemd/system/qya.service
systemctl daemon-reload
echo "安装完成"
