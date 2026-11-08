#!/usr/bin/env bash
# qya 服务器安装。以 root 运行；可重复执行，不覆盖已有的配置、记忆和数据。
# 运行用户 tangyuge；抖音服务 tangyuge-douyin（需先存在其用户组）。
# 用法：install.sh <暂存目录>（内含 qya 二进制、persona/ agents/ skills/ tools/ docs/ memory_seed/、qya.service、qya.server.toml，可选 secrets/）
# docs/ 内为 00-PRD.md、01-architecture.md、02-decisions.md、deploy.md（即 deploy/README.md）与 qya.example.toml，供 qya-docs 技能在服务器上读取。
# 首次生成配置时可用环境变量 QYA_QQ_APP_ID、QYA_QQ_OWNER 填入 QQ 机器人 AppID 与主人 user_openid。
set -euo pipefail
STAGE="${1:?需要暂存目录}"

# 1. 用户与组（博客同步脚本、抖音 socket）
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
for d in persona agents skills tools docs; do
  rm -rf "/opt/qya/$d"
  cp -r "$STAGE/$d" "/opt/qya/$d"
done
chown -R root:root /opt/qya
chmod -R a+rX /opt/qya

# 4. 凭据：暂存目录 secrets/ 下的文件逐个安装（同名覆盖），文件名见 qya.server.toml 中的 *_file
if [ -d "$STAGE/secrets" ]; then
  for f in "$STAGE"/secrets/*; do
    [ -f "$f" ] && install -o tangyuge -g tangyuge -m 600 "$f" "/etc/qya/secrets/$(basename "$f")"
  done
fi

# 5. 配置（只在不存在时生成，避免覆盖 /provider 写回的改动）
if [ ! -f /etc/qya/qya.toml ]; then
  sed -e "s#__QQ_APP_ID__#${QYA_QQ_APP_ID:-__QQ_APP_ID__}#" \
      -e "s#__QQ_OWNER__#${QYA_QQ_OWNER:-__QQ_OWNER__}#" \
      "$STAGE/qya.server.toml" > /etc/qya/qya.toml
  chown tangyuge:tangyuge /etc/qya/qya.toml; chmod 640 /etc/qya/qya.toml
  grep -q "__QQ_" /etc/qya/qya.toml && echo "提示：/etc/qya/qya.toml 中仍有 QQ 占位符，启动前请填写"
fi

# 6. 记忆种子（只在目标不存在时复制）
for f in user.md ops.md creator.md; do
  [ -f "/var/lib/qya/memory/$f" ] || install -o tangyuge -g tangyuge -m 640 "$STAGE/memory_seed/$f" "/var/lib/qya/memory/$f"
done

# 7. 运维权限（NOPASSWD: ALL，运维专员执行系统命令需要）
echo 'tangyuge ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/tangyuge.tmp
chmod 440 /etc/sudoers.d/tangyuge.tmp
visudo -cf /etc/sudoers.d/tangyuge.tmp >/dev/null && mv -f /etc/sudoers.d/tangyuge.tmp /etc/sudoers.d/tangyuge

# 8. systemd 单元（安装但不启动）
install -o root -g root -m 644 "$STAGE/qya.service" /etc/systemd/system/qya.service
systemctl daemon-reload
echo "安装完成"
