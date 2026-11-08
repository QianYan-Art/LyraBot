#!/usr/bin/env python3
"""Fetch mailbox data and perform constrained mail actions from Hermes.

连接参数不写在这里。本仓库是公开的，主机、端口、SSH 用户和私钥路径都从服务器
本地的 ``mail_vps.toml`` 读取，查找顺序：

1. ``$HERMES_MAIL_VPS_CONFIG``
2. ``$HERMES_HOME/mail_vps.toml``
3. ``~/.hermes/mail_vps.toml``

配置缺失或不完整时返回结构化错误，不回退到任何内置默认地址。
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import shlex
import subprocess
import time
import tomllib
from pathlib import Path

READ_COMMANDS = {"list-mailboxes", "read-mail", "list-attachments", "fetch-attachment"}
WRITE_COMMANDS = {"send-mail", "reply-mail", "forward-mail", "move-mail-to-trash", "delete-mail"}

DEFAULT_RETENTION_HOURS = 24

# 只读命令在网络类失败时重试。读操作是幂等的，重复执行结果一样。
#
# 写操作绝不重试：ssh 超时不代表远端没执行——邮件可能已经发出去、消息可能已经
# 删掉，只是响应没回来。这种情况下重试等于重复发信或重复删除，所以宁可如实报错
# 交给调用方决定。
READ_COMMAND_ATTEMPTS = 2
RETRY_BACKOFF_SECONDS = 2.0
# ssh 自身的连接失败退出码。只有它同时没有任何 stdout 时才算网络问题——远端一旦
# 输出了 JSON 就说明命令真的跑过了，那是业务结果，重试没有意义。
SSH_CONNECT_FAILURE = 255


class ConfigError(Exception):
    """配置缺失或字段不完整。"""


def _config_path() -> Path:
    override = os.environ.get("HERMES_MAIL_VPS_CONFIG", "").strip()
    if override:
        return Path(override).expanduser()
    home = os.environ.get("HERMES_HOME", "").strip()
    base = Path(home).expanduser() if home else Path.home() / ".hermes"
    return base / "mail_vps.toml"


def _load_config() -> dict:
    path = _config_path()
    if not path.exists():
        raise ConfigError(f"missing_config:{path}")
    try:
        with path.open("rb") as handle:
            cfg = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise ConfigError(f"unreadable_config:{type(exc).__name__}") from exc

    host = str(cfg.get("host") or "").strip()
    port = cfg.get("port")
    if not host or not isinstance(port, int):
        raise ConfigError("config_missing_host_or_port")
    for section in ("readonly", "readwrite"):
        entry = cfg.get(section)
        if not isinstance(entry, dict):
            raise ConfigError(f"config_missing_section:{section}")
        if not str(entry.get("user") or "").strip() or not str(entry.get("key") or "").strip():
            raise ConfigError(f"config_missing_user_or_key:{section}")
    return cfg


def _cache_dirs(cfg: dict) -> tuple[Path, Path, int]:
    """附件目录、验证链接目录、保留小时数。缺省沿用 HERMES_HOME 下的既有位置。"""
    home = os.environ.get("HERMES_HOME", "").strip()
    base = Path(home).expanduser() if home else Path.home() / ".hermes"
    cache = cfg.get("cache") if isinstance(cfg.get("cache"), dict) else {}
    attachments = str(cache.get("attachments_dir") or "").strip()
    links = str(cache.get("links_dir") or "").strip()
    retention = cache.get("retention_hours")
    if not isinstance(retention, int) or retention <= 0:
        retention = DEFAULT_RETENTION_HOURS
    return (
        Path(attachments).expanduser() if attachments else base / "cache" / "mail_attachments",
        Path(links).expanduser() if links else base / "cache" / "mail_verification_links",
        retention,
    )


def prune_cache_dir(directory: Path, max_age_hours: int) -> int:
    """删除目录下超过保留期的文件，返回删除数量。

    这两个目录由本脚本自己写入，gateway 的缓存清理不覆盖它们；不在这里清就
    没有任何机制会清。每次调用顺带扫一遍，代价可忽略。
    """
    if not directory.exists():
        return 0
    cutoff = time.time() - (max_age_hours * 3600)
    removed = 0
    try:
        entries = list(directory.iterdir())
    except OSError:
        return 0
    for f in entries:
        try:
            if f.is_file() and f.stat().st_mtime < cutoff:
                f.unlink()
                removed += 1
        except OSError:
            continue
    return removed


def _ssh_credentials(cfg: dict, command: str) -> tuple[str, str]:
    if command in READ_COMMANDS:
        section = cfg["readonly"]
    elif command in WRITE_COMMANDS:
        section = cfg["readwrite"]
    else:
        raise ValueError(f"unsupported_command:{command}")
    return str(section["user"]).strip(), str(section["key"]).strip()


def _run_remote(cfg: dict, command: str, remote_args: list[str]) -> tuple[int, str, str]:
    """执行一次远端命令；读命令在网络类失败时重试，写命令永不重试。"""
    user, key = _ssh_credentials(cfg, command)
    key_path = Path(key)
    if not key_path.exists():
        payload = {"ok": False, "error": "missing_ssh_key", "key_path": str(key_path)}
        return 2, json.dumps(payload, ensure_ascii=False), ""

    remote_command = " ".join(shlex.quote(x) for x in remote_args)
    ssh_cmd = [
        "ssh",
        "-i",
        str(key_path),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ConnectTimeout=10",
        "-p",
        str(cfg["port"]),
        f"{user}@{cfg['host']}",
        remote_command,
    ]

    attempts = READ_COMMAND_ATTEMPTS if command in READ_COMMANDS else 1
    last: tuple[int, str, str] = (4, "", "")
    for attempt in range(1, attempts + 1):
        try:
            proc = subprocess.run(
                ssh_cmd, capture_output=True, text=True, timeout=30, check=False
            )
        except subprocess.TimeoutExpired:
            payload = {"ok": False, "error": "ssh_timeout", "attempts": attempt}
            last = (3, json.dumps(payload, ensure_ascii=False), "")
        else:
            stdout = proc.stdout.strip()
            # 远端有输出就说明命令真的执行过，无论成败都直接返回。
            if proc.returncode != SSH_CONNECT_FAILURE or stdout:
                return proc.returncode, stdout, proc.stderr.strip()
            last = (proc.returncode, stdout, proc.stderr.strip())
        if attempt < attempts:
            time.sleep(RETRY_BACKOFF_SECONDS)
    return last


def _parse_json_output(returncode: int, stdout: str, stderr: str) -> tuple[int, dict]:
    if returncode != 0 and not stdout:
        return 4, {"ok": False, "error": "ssh_failed", "returncode": returncode, "stderr": stderr[:300]}
    try:
        payload = json.loads(stdout or "{}")
    except json.JSONDecodeError:
        return 5, {"ok": False, "error": "invalid_remote_json", "stdout": (stdout or stderr)[:300]}
    if returncode != 0 and payload.get("ok") is not False:
        payload = {"ok": False, "error": "ssh_failed", "returncode": returncode, "stderr": stderr[:300]}
    return 0, payload


def _print_payload(payload: dict) -> int:
    print(json.dumps(payload, ensure_ascii=False, indent=2))
    return 0


def _sanitize_filename(name: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "_", name).strip("._")
    return cleaned or "attachment.bin"


def _save_attachment(payload: dict, attachment_dir: Path) -> dict:
    attachment = payload.get("attachment") or {}
    data_b64 = payload.get("data_b64")
    if not isinstance(data_b64, str):
        raise ValueError("missing_attachment_data")
    raw = base64.b64decode(data_b64.encode("ascii"))
    attachment_dir.mkdir(parents=True, exist_ok=True)
    filename = _sanitize_filename(attachment.get("filename") or "attachment.bin")
    local_path = attachment_dir / filename
    if local_path.exists():
        stem = local_path.stem
        suffix = local_path.suffix
        local_path = attachment_dir / f"{stem}_{attachment.get('index', 0)}{suffix}"
    local_path.write_bytes(raw)
    result = {
        "ok": True,
        "email": payload.get("email"),
        "message_ref": payload.get("message_ref"),
        "attachment": attachment,
        "local_path": str(local_path),
        "media_tag": f"MEDIA:{local_path}",
    }
    return result


def _extract_link_url(item) -> str:
    if isinstance(item, str):
        return item.strip()
    if isinstance(item, dict):
        for key in ("url", "link", "href", "verification_url"):
            value = item.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    return ""


def _extract_link_label(item, index: int) -> str:
    if isinstance(item, dict):
        for key in ("label", "title", "text", "description"):
            value = item.get(key)
            if isinstance(value, str) and value.strip():
                return value.strip()
    return f"verification_link_{index}"


def _build_link_exports(
    owner_email: str, owner_message_ref: str, items: list, link_dir: Path
) -> list[dict]:
    link_dir.mkdir(parents=True, exist_ok=True)
    exports = []
    email_part = _sanitize_filename(owner_email or "mail")
    message_part = _sanitize_filename(owner_message_ref or "message")

    for index, item in enumerate(items, start=1):
        url = _extract_link_url(item)
        if not url:
            continue
        raw_label = _extract_link_label(item, index)
        label = _sanitize_filename(raw_label)
        local_path = link_dir / f"{email_part}_{message_part}_{index}_{label}.txt"
        local_path.write_text(url + "\n", encoding="utf-8")
        exports.append(
            {
                "index": index,
                "label": raw_label,
                "url": url,
                "local_path": str(local_path),
                "media_tag": f"MEDIA:{local_path}",
            }
        )
    return exports


def _export_verification_links(payload: dict, link_dir: Path) -> dict:
    top_level_items = payload.get("verification_links")
    if isinstance(top_level_items, list) and top_level_items:
        exports = _build_link_exports(
            str(payload.get("email") or ""),
            str(payload.get("message_ref") or ""),
            top_level_items,
            link_dir,
        )
        if exports:
            payload["verification_link_exports"] = exports
            payload["preferred_media_tag"] = exports[0]["media_tag"]

    messages = payload.get("messages")
    if isinstance(messages, list):
        first_export = None
        for message in messages:
            if not isinstance(message, dict):
                continue
            items = message.get("verification_links")
            if not isinstance(items, list) or not items:
                continue
            exports = _build_link_exports(
                str(payload.get("email") or ""),
                str(message.get("message_ref") or payload.get("message_ref") or ""),
                items,
                link_dir,
            )
            if exports:
                message["verification_link_exports"] = exports
                if first_export is None:
                    first_export = exports[0]
        if first_export:
            payload["preferred_media_tag"] = first_export["media_tag"]

    if payload.get("preferred_media_tag"):
        payload["exact_link_reply_hint"] = (
            "When returning a long verification URL, prefer the exported txt file "
            "via MEDIA: so the link stays byte-for-byte unchanged."
        )
    return payload


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    p_list = sub.add_parser("list-mailboxes")
    p_list.add_argument("--domain")
    p_list.add_argument("--limit", type=int, default=0)

    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--email")
    common.add_argument("--message-ref")
    common.add_argument("--from-contains")
    common.add_argument("--subject-contains")
    common.add_argument("--body-contains")
    common.add_argument("--require-code", action="store_true")
    common.add_argument("--require-link", action="store_true")
    common.add_argument("--has-attachment", action="store_true")

    p_read = sub.add_parser("read-mail", parents=[common])
    p_read.add_argument("--limit", type=int, default=1)
    p_read.add_argument("--want", default="message_ref,from,subject,date,codes,verification_links,snippet,attachments")

    p_att = sub.add_parser("list-attachments", parents=[common])

    p_fetch = sub.add_parser("fetch-attachment", parents=[common])
    p_fetch.add_argument("--attachment-index", type=int)
    p_fetch.add_argument("--attachment-name")
    p_fetch.add_argument("--max-bytes", type=int, default=10 * 1024 * 1024)

    p_send = sub.add_parser("send-mail")
    p_send.add_argument("--from-email", required=True)
    p_send.add_argument("--to", required=True)
    p_send.add_argument("--cc")
    p_send.add_argument("--subject", required=True)
    p_send.add_argument("--body", required=True)

    p_reply = sub.add_parser("reply-mail", parents=[common])
    p_reply.add_argument("--from-email", required=True)
    p_reply.add_argument("--body", required=True)
    p_reply.add_argument("--reply-all", action="store_true")

    p_forward = sub.add_parser("forward-mail", parents=[common])
    p_forward.add_argument("--from-email", required=True)
    p_forward.add_argument("--to", required=True)
    p_forward.add_argument("--body", default="")

    p_trash = sub.add_parser("move-mail-to-trash", parents=[common])

    p_delete = sub.add_parser("delete-mail", parents=[common])

    args = parser.parse_args()
    if args.command not in {"list-mailboxes", "send-mail"} and not getattr(args, "email", None):
        return _print_payload({"ok": False, "error": "email_required"})

    try:
        cfg = _load_config()
    except ConfigError as exc:
        return _print_payload({"ok": False, "error": "config_error", "detail": str(exc)})

    attachment_dir, link_dir, retention_hours = _cache_dirs(cfg)
    # 两个目录没有其他清理机制，每次调用顺带回收过期文件。
    prune_cache_dir(attachment_dir, retention_hours)
    prune_cache_dir(link_dir, retention_hours)

    remote_command = "export-attachment" if args.command == "fetch-attachment" else args.command
    remote_args = [remote_command]

    if args.command == "list-mailboxes":
        if args.domain:
            remote_args.extend(["--domain", args.domain])
        if args.limit:
            remote_args.extend(["--limit", str(args.limit)])
    else:
        for key in ("email", "message_ref", "from_contains", "subject_contains", "body_contains"):
            value = getattr(args, key, None)
            if value:
                remote_args.extend([f"--{key.replace('_', '-')}", str(value)])
        if getattr(args, "require_code", False):
            remote_args.append("--require-code")
        if getattr(args, "require_link", False):
            remote_args.append("--require-link")
        if getattr(args, "has_attachment", False):
            remote_args.append("--has-attachment")

        if args.command == "read-mail":
            remote_args.extend(["--limit", str(args.limit), "--want", args.want])
        elif args.command == "fetch-attachment":
            if args.attachment_index is not None:
                remote_args.extend(["--attachment-index", str(args.attachment_index)])
            if args.attachment_name:
                remote_args.extend(["--attachment-name", args.attachment_name])
            remote_args.extend(["--max-bytes", str(args.max_bytes)])
        elif args.command == "send-mail":
            remote_args = [
                "send-mail",
                "--from-email",
                args.from_email,
                "--to",
                args.to,
                "--subject",
                args.subject,
                "--body",
                args.body,
            ]
            if args.cc:
                remote_args.extend(["--cc", args.cc])
        elif args.command == "reply-mail":
            remote_args.extend(["--from-email", args.from_email, "--body", args.body])
            if args.reply_all:
                remote_args.append("--reply-all")
        elif args.command == "forward-mail":
            remote_args.extend(["--from-email", args.from_email, "--to", args.to])
            if args.body:
                remote_args.extend(["--body", args.body])

    returncode, stdout, stderr = _run_remote(cfg, args.command, remote_args)
    status, payload = _parse_json_output(returncode, stdout, stderr)
    if status != 0:
        return _print_payload(payload)

    if args.command == "fetch-attachment" and payload.get("ok"):
        try:
            payload = _save_attachment(payload, attachment_dir)
        except Exception as exc:
            return _print_payload({"ok": False, "error": "attachment_save_failed", "detail": str(exc)})
    elif args.command == "read-mail" and payload.get("ok"):
        payload = _export_verification_links(payload, link_dir)
    return _print_payload(payload)


if __name__ == "__main__":
    raise SystemExit(main())
