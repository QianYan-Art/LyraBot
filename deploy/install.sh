#!/usr/bin/env bash
# LyraBot 服务器安装。以 root 运行；可重复执行，不覆盖已有的配置、记忆和数据。
# 运行用户 lyrabot；抖音服务 lyrabot-douyin（需先存在其用户组）。
# 用法：install.sh <暂存目录>（内含 lyrabot 二进制、persona/ agents/ skills/ tools/ docs/ memory_seed/、lyrabot.service、lyrabot.server.toml，可选 secrets/）
# docs/ 内为仓库 docs/*.md（00–09）、deploy.md（即 deploy/README.md）与 lyrabot.example.toml，供 lyrabot-docs 技能在服务器上读取。
# 首次生成配置时可用环境变量 LYRABOT_QQ_APP_ID、LYRABOT_QQ_OWNER 填入 QQ 机器人 AppID 与主人 user_openid。
set -euo pipefail
STAGE="${1:?需要暂存目录}"

# 1. 用户与组（博客同步脚本、抖音 socket）
if ! id lyrabot >/dev/null 2>&1; then
  useradd --system --create-home --home-dir /var/lib/lyrabot --shell /bin/bash lyrabot
fi
usermod -aG blogsync,lyrabot-douyin lyrabot

# 2. 目录
install -d -o root -g root -m 755 /opt/lyrabot /opt/lyrabot/bin
install -d -o lyrabot -g lyrabot -m 750 /etc/lyrabot            # /provider 需要写回配置
install -d -o lyrabot -g lyrabot -m 700 /etc/lyrabot/secrets
install -d -o lyrabot -g lyrabot -m 750 /var/lib/lyrabot /var/lib/lyrabot/memory /var/lib/lyrabot/stickers /var/lib/lyrabot/media \
  /var/lib/lyrabot/work /var/lib/lyrabot/workspace /var/log/lyrabot

# 3. 程序与资产（每次覆盖）
install -o root -g root -m 755 "$STAGE/lyrabot" /opt/lyrabot/bin/lyrabot.new
mv -f /opt/lyrabot/bin/lyrabot.new /opt/lyrabot/bin/lyrabot
for d in persona agents skills tools docs; do
  rm -rf "/opt/lyrabot/$d"
  cp -r "$STAGE/$d" "/opt/lyrabot/$d"
done
chown -R root:root /opt/lyrabot
chmod -R a+rX /opt/lyrabot

# 4. 凭据：暂存目录 secrets/ 下的文件逐个安装（同名覆盖），文件名见 lyrabot.server.toml 中的 *_file
if [ -d "$STAGE/secrets" ]; then
  for f in "$STAGE"/secrets/*; do
    [ -f "$f" ] && install -o lyrabot -g lyrabot -m 600 "$f" "/etc/lyrabot/secrets/$(basename "$f")"
  done
fi

# 5. 配置（只在不存在时生成，避免覆盖 /provider 写回的改动）
if [ ! -f /etc/lyrabot/lyrabot.toml ]; then
  sed -e "s#__QQ_APP_ID__#${LYRABOT_QQ_APP_ID:-__QQ_APP_ID__}#" \
      -e "s#__QQ_OWNER__#${LYRABOT_QQ_OWNER:-__QQ_OWNER__}#" \
      "$STAGE/lyrabot.server.toml" > /etc/lyrabot/lyrabot.toml
  chown lyrabot:lyrabot /etc/lyrabot/lyrabot.toml; chmod 640 /etc/lyrabot/lyrabot.toml
  grep -q "__QQ_" /etc/lyrabot/lyrabot.toml && echo "提示：/etc/lyrabot/lyrabot.toml 中仍有 QQ 占位符，启动前请填写"
fi

# 6. 记忆种子（只在目标不存在时复制）
for f in user.md ops.md creator.md; do
  [ -f "/var/lib/lyrabot/memory/$f" ] || install -o lyrabot -g lyrabot -m 640 "$STAGE/memory_seed/$f" "/var/lib/lyrabot/memory/$f"
done

# 7. 运维权限（NOPASSWD: ALL，运维专员执行系统命令需要）
echo 'lyrabot ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/lyrabot.tmp
chmod 440 /etc/sudoers.d/lyrabot.tmp
visudo -cf /etc/sudoers.d/lyrabot.tmp >/dev/null && mv -f /etc/sudoers.d/lyrabot.tmp /etc/sudoers.d/lyrabot

# 8. systemd 单元（安装但不启动）
install -o root -g root -m 644 "$STAGE/lyrabot.service" /etc/systemd/system/lyrabot.service
systemctl daemon-reload
echo "安装完成"
