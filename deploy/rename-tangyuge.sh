#!/usr/bin/env bash
# 一次性改名（2026-11-08）：qya 用户 → tangyuge；hermes-douyin 服务 → tangyuge-douyin。以 root 运行。
set -euo pipefail

systemctl stop qya.service
systemctl stop hermes-douyin.service

# 1. 运行用户 qya → tangyuge（UID 不变，文件属主与抖音授权 UID 均不受影响）
if id qya >/dev/null 2>&1; then
  usermod -l tangyuge qya
  groupmod -n tangyuge qya
fi
rm -f /etc/sudoers.d/qya
echo 'tangyuge ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/tangyuge.tmp
chmod 440 /etc/sudoers.d/tangyuge.tmp
visudo -cf /etc/sudoers.d/tangyuge.tmp >/dev/null && mv -f /etc/sudoers.d/tangyuge.tmp /etc/sudoers.d/tangyuge

# 2. 抖音服务 hermes-douyin → tangyuge-douyin
[ -d /opt/hermes-douyin ] && mv /opt/hermes-douyin /opt/tangyuge-douyin
[ -d /var/lib/hermes-douyin ] && mv /var/lib/hermes-douyin /var/lib/tangyuge-douyin
getent group hermes-douyin >/dev/null && groupmod -n tangyuge-douyin hermes-douyin
usermod -d /var/lib/tangyuge-douyin douyin-runner
[ -f /etc/hermes-douyin.env ] && mv /etc/hermes-douyin.env /etc/tangyuge-douyin.env

cd /opt/tangyuge-douyin
rm -f install.py   # Hermes 安装器：引用 hermes 用户与 Hermes 仓库，已无用
sed -i 's#^GATEWAY_UNIT = os.environ.get("DOUYIN_GATEWAY_UNIT", "hermes-gateway.service")$#GATEWAY_UNIT = os.environ.get("DOUYIN_GATEWAY_UNIT", "qya.service")#' service.py
sed -i 's#/run/hermes-douyin/#/run/tangyuge-douyin/#g' service.py
sed -i 's#"/var/lib/hermes-douyin"#"/var/lib/tangyuge-douyin"#g; s#getgrnam("hermes-douyin")#getgrnam("tangyuge-douyin")#g; s#douyin-runner 或 hermes-douyin#douyin-runner 或 tangyuge-douyin#g' stage_release.py
sed -i 's#__hermesEditorProof#__tangyugeEditorProof#g; s#__hermesUploadProof#__tangyugeUploadProof#g; s#__hermesVerificationProof#__tangyugeVerificationProof#g' content.py
sed -i 's#/opt/hermes-douyin/venv#/opt/tangyuge-douyin/venv#' venv/pyvenv.cfg
for f in service.py stage_release.py content.py; do venv/bin/python -m py_compile "$f"; done
find . -name __pycache__ -path "./[!v]*" -prune -exec rm -rf {} + 2>/dev/null || true

sed -e 's#Hermes Douyin creator login service#Tangyuge Douyin creator service#' \
    -e 's#hermes-douyin#tangyuge-douyin#g' \
    /etc/systemd/system/hermes-douyin.service > /etc/systemd/system/tangyuge-douyin.service
systemctl disable hermes-douyin.service 2>/dev/null || true
rm -f /etc/systemd/system/hermes-douyin.service

# 3. qya：运行用户与抖音 socket 路径
sed -i 's/^User=qya$/User=tangyuge/; s/^Group=qya$/Group=tangyuge/; s/hermes-douyin.service/tangyuge-douyin.service/' /etc/systemd/system/qya.service
sed -i 's#/run/hermes-douyin/login.sock#/run/tangyuge-douyin/login.sock#' /etc/qya/qya.toml

systemctl daemon-reload
systemctl enable --now tangyuge-douyin.service
sleep 3
systemctl start qya.service
echo "改名完成"
