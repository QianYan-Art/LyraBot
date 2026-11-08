from __future__ import annotations

import argparse
import ipaddress
import json
import os
import re
import shlex
import stat
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import releases
import stage_release


DEFAULT_SERVER_SOURCE = "/opt/tangyuge-douyin"
REMOTE_UPLOAD_PREFIX = "/tmp/tangyuge-douyin-upload."
_REMOTE_UPLOAD_PATTERN = re.compile(r"^/tmp/tangyuge-douyin-upload\.[A-Za-z0-9]{10,64}$", re.ASCII)
_USER_PATTERN = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]{0,31}$", re.ASCII)
_HOST_LABEL_PATTERN = re.compile(r"^[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?$", re.ASCII)
_REMOTE_COMPONENT_PATTERN = re.compile(r"^[A-Za-z0-9._-]+$", re.ASCII)

_REMOTE_INSPECT_SCRIPT = "\n".join(
    (
        "import json, os, sys",
        "from pathlib import Path",
        "sys.path.insert(0, sys.argv[1])",
        "from releases import ReleaseStore, manifests_have_same_content, validate_manifest",
        "source_root, manifest_path, release_id = sys.argv[1:]",
        "try:",
        "    expected = validate_manifest(json.loads(Path(manifest_path).read_text(encoding='utf-8')))",
        "    store = ReleaseStore('/var/lib/tangyuge-douyin')",
        "    package_path = store.releases_dir / release_id",
        "    if not os.path.lexists(package_path):",
        "        print(json.dumps({'state': 'missing'}))",
        "    else:",
        "        loaded = store.load(release_id)",
        "        installed = {key: value for key, value in loaded.items() if key != 'video_path'}",
        "        if not manifests_have_same_content(installed, expected):",
        "            print(json.dumps({'state': 'conflict'}))",
        "        else:",
        "            print(json.dumps({'state': 'same', 'hash': loaded['video_sha256'], 'bytes': loaded['video_bytes']}))",
        "except Exception:",
        "    raise SystemExit(3)",
    )
)

_REMOTE_HASH_SCRIPT = "\n".join(
    (
        "import hashlib, json, os, sys",
        "path = sys.argv[1]",
        "digest = hashlib.sha256()",
        "with open(path, 'rb') as stream:",
        "    for chunk in iter(lambda: stream.read(1024 * 1024), b''):",
        "        digest.update(chunk)",
        "print(json.dumps({'hash': digest.hexdigest(), 'bytes': os.path.getsize(path)}))",
    )
)


class SyncError(Exception):
    pass


@dataclass(frozen=True)
class SyncConfig:
    host: str
    user: str
    port: int
    identity_file: Path
    projects_root: Path
    server_source: str = DEFAULT_SERVER_SOURCE

    @property
    def destination(self) -> str:
        try:
            address = ipaddress.ip_address(self.host)
        except ValueError:
            host = self.host
        else:
            host = f"[{address.compressed}]" if address.version == 6 else address.compressed
        return f"{self.user}@{host}"


def _validate_host(value: object) -> str:
    if not isinstance(value, str) or not value or value != value.strip():
        raise SyncError("配置中的 host 无效")
    try:
        return ipaddress.ip_address(value).compressed
    except ValueError:
        if len(value) > 253 or any(not _HOST_LABEL_PATTERN.fullmatch(label) for label in value.split(".")):
            raise SyncError("配置中的 host 无效") from None
        return value


def _validate_remote_path(value: object) -> str:
    if not isinstance(value, str) or not value.startswith("/") or value == "/":
        raise SyncError("配置中的 server_source 必须是受限的绝对路径")
    parts = value.split("/")[1:]
    if any(
        part in {"", ".", ".."} or not _REMOTE_COMPONENT_PATTERN.fullmatch(part)
        for part in parts
    ):
        raise SyncError("配置中的 server_source 必须是受限的绝对路径")
    return value


def load_config(config_path: str | os.PathLike[str]) -> SyncConfig:
    try:
        document = json.loads(Path(config_path).read_text(encoding="utf-8"))
    except (OSError, UnicodeError):
        raise SyncError("无法读取配置文件") from None
    except json.JSONDecodeError:
        raise SyncError("配置文件不是有效 JSON") from None
    if not isinstance(document, dict):
        raise SyncError("配置文件必须是 JSON 对象")

    required = {"host", "user", "port", "identity_file", "projects_root"}
    allowed = required | {"server_source", "ops_dir", "python", "cleanup_tool"}
    if required - set(document) or set(document) - allowed:
        raise SyncError("配置文件字段不符合要求")

    host = _validate_host(document["host"])
    user = document["user"]
    if not isinstance(user, str) or not _USER_PATTERN.fullmatch(user):
        raise SyncError("配置中的 user 无效")
    port = document["port"]
    if type(port) is not int or not 1 <= port <= 65535:
        raise SyncError("配置中的 port 必须是 1 到 65535 之间的整数")

    try:
        identity_file = Path(document["identity_file"]).expanduser().resolve(strict=True)
        projects_root = Path(document["projects_root"]).expanduser().resolve(strict=True)
    except (OSError, TypeError, ValueError):
        raise SyncError("配置中的本机路径无效") from None
    if not identity_file.is_file():
        raise SyncError("identity_file 必须指向现有文件")
    if not projects_root.is_dir():
        raise SyncError("projects_root 必须指向现有目录")

    server_source = _validate_remote_path(document.get("server_source", DEFAULT_SERVER_SOURCE))
    return SyncConfig(host, user, port, identity_file, projects_root, server_source)


def _file_state(path: Path) -> tuple[int, int, int, int]:
    info = path.stat()
    if not stat.S_ISREG(info.st_mode):
        raise SyncError("视频必须是普通文件")
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns)


def _validate_video_path(value: str | os.PathLike[str], projects_root: Path) -> tuple[Path, tuple[int, int, int, int]]:
    requested = Path(value).expanduser()
    if requested.suffix.casefold() != ".mp4":
        raise SyncError("--video 必须指向单个 MP4 文件")
    if requested.is_symlink():
        raise SyncError("--video 不能是符号链接")
    try:
        video_path = requested.resolve(strict=True)
        video_path.relative_to(projects_root)
    except (OSError, ValueError):
        raise SyncError("--video 必须位于 projects_root 内") from None
    if video_path.suffix.casefold() != ".mp4":
        raise SyncError("--video 必须指向单个 MP4 文件")
    state = _file_state(video_path)
    if not 1 <= state[2] <= releases.MAX_VIDEO_BYTES:
        raise SyncError("视频大小必须在 1 字节到 2 GiB 之间")
    return video_path, state


def _assert_video_unchanged(
    video_path: Path,
    initial_state: tuple[int, int, int, int],
    expected_hash: str,
    expected_bytes: int,
) -> None:
    try:
        before = _file_state(video_path)
        digest, size = releases._hash_video(video_path, releases.MAX_VIDEO_BYTES)
        after = _file_state(video_path)
    except (OSError, releases.ReleaseError):
        raise SyncError("视频读取失败或在同步期间发生变化") from None
    if before != initial_state or after != initial_state or digest != expected_hash or size != expected_bytes:
        raise SyncError("视频在同步期间发生变化")


def _run(arguments: Sequence[str], *, timeout: int) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            list(arguments),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            stdin=subprocess.DEVNULL,
            check=False,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        raise SyncError("SSH/SCP 操作超时") from None
    except OSError:
        raise SyncError("无法启动 SSH/SCP 程序") from None
    if result.returncode != 0:
        raise SyncError(f"SSH/SCP 操作失败，退出码 {result.returncode}")
    return result


def _ssh(config: SyncConfig, remote_command: str, *, timeout: int = 60) -> subprocess.CompletedProcess[str]:
    arguments = [
        "ssh",
        "-p",
        str(config.port),
        "-i",
        str(config.identity_file),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "IdentitiesOnly=yes",
        config.destination,
        remote_command,
    ]
    return _run(arguments, timeout=timeout)


def _scp(config: SyncConfig, local_path: Path, remote_path: str, *, timeout: int = 7200) -> None:
    arguments = [
        "scp",
        "-P",
        str(config.port),
        "-i",
        str(config.identity_file),
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "IdentitiesOnly=yes",
        str(local_path),
        f"{config.destination}:{remote_path}",
    ]
    _run(arguments, timeout=timeout)


def _create_remote_directory(config: SyncConfig) -> str:
    command = "umask 077 && " + shlex.join(["mktemp", "-d", REMOTE_UPLOAD_PREFIX + "X" * 10])
    output = _ssh(config, command).stdout.splitlines()
    if len(output) != 1 or not _REMOTE_UPLOAD_PATTERN.fullmatch(output[0]):
        raise SyncError("远端返回的临时目录路径无效")
    return output[0]


def _remote_json(config: SyncConfig, arguments: Sequence[str]) -> dict[str, object]:
    command = shlex.join(arguments)
    result = _ssh(config, command)
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError:
        raise SyncError("远端校验返回无效数据") from None
    if not isinstance(document, dict):
        raise SyncError("远端校验返回无效数据")
    return document


def _inspect_remote(
    config: SyncConfig,
    remote_directory: str,
    release_id: str,
) -> dict[str, object]:
    command = [
        "python3",
        "-c",
        _REMOTE_INSPECT_SCRIPT,
        config.server_source,
        f"{remote_directory}/manifest.json",
        release_id,
    ]
    document = _remote_json(config, command)
    state = document.get("state")
    if state in {"missing", "conflict"}:
        if set(document) != {"state"}:
            raise SyncError("远端校验返回无效数据")
        return document
    if state != "same" or set(document) != {"state", "hash", "bytes"}:
        raise SyncError("远端校验返回无效数据")
    digest, size = document.get("hash"), document.get("bytes")
    if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest, re.ASCII) is None:
        raise SyncError("远端校验返回无效数据")
    if type(size) is not int or size <= 0:
        raise SyncError("远端校验返回无效数据")
    return document


def _verify_remote_upload(config: SyncConfig, remote_video: str) -> dict[str, object]:
    result = _remote_json(config, ["python3", "-c", _REMOTE_HASH_SCRIPT, remote_video])
    if set(result) != {"hash", "bytes"}:
        raise SyncError("远端视频校验返回无效数据")
    digest, size = result.get("hash"), result.get("bytes")
    if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest, re.ASCII) is None:
        raise SyncError("远端视频校验返回无效数据")
    if type(size) is not int or size <= 0:
        raise SyncError("远端视频校验返回无效数据")
    return result


def _cleanup_remote(config: SyncConfig, remote_directory: str) -> None:
    if not _REMOTE_UPLOAD_PATTERN.fullmatch(remote_directory):
        raise SyncError("拒绝清理不受信任的远端路径")
    _ssh(config, shlex.join(["rm", "-rf", "--", remote_directory]))


def _result(release_id: str, digest: str, size: int) -> dict[str, object]:
    return {"release_id": release_id, "hash": digest, "bytes": size, "synced": True}


def sync_release(
    config: SyncConfig,
    video: str | os.PathLike[str],
    release_id: str,
    title: str,
    description: str,
    source: str | None = None,
) -> dict[str, object]:
    release_id = releases.validate_release_id(release_id)
    video_path, initial_state = _validate_video_path(video, config.projects_root)

    with tempfile.TemporaryDirectory(prefix=".tmp-tangyuge-sync-", dir=video_path.parent) as local_directory:
        manifest_path = Path(local_directory) / "manifest.json"
        manifest = stage_release.prepare_release(
            video_path,
            release_id,
            title,
            description,
            manifest_path,
            source=source,
        )
        manifest = releases.validate_manifest(manifest)
        digest = manifest["video_sha256"]
        size = manifest["video_bytes"]
        if not isinstance(digest, str) or type(size) is not int:
            raise SyncError("本机 manifest 字段无效")
        _assert_video_unchanged(video_path, initial_state, digest, size)

        remote_directory = _create_remote_directory(config)
        verified = False
        try:
            remote_manifest = f"{remote_directory}/manifest.json"
            remote_video = f"{remote_directory}/video.mp4"
            _scp(config, manifest_path, remote_manifest)
            installed = _inspect_remote(config, remote_directory, release_id)
            if installed["state"] == "conflict":
                raise SyncError("相同 release_id 已存在不同内容，拒绝覆盖")
            if installed["state"] == "same":
                if installed["hash"] != digest or installed["bytes"] != size:
                    raise SyncError("远端现有作品与本机成品不一致")
                _assert_video_unchanged(video_path, initial_state, digest, size)
                verified = True
                return _result(release_id, digest, size)

            _assert_video_unchanged(video_path, initial_state, digest, size)
            _scp(config, video_path, remote_video)
            _assert_video_unchanged(video_path, initial_state, digest, size)
            remote_video_info = _verify_remote_upload(config, remote_video)
            if remote_video_info["hash"] != digest or remote_video_info["bytes"] != size:
                raise SyncError("原片与远端视频 SHA256 或大小不一致")

            install_command = shlex.join(
                [
                    "python3",
                    f"{config.server_source}/stage_release.py",
                    "install",
                    "--source-dir",
                    remote_directory,
                ]
            )
            _ssh(config, install_command, timeout=1800)
            installed = _inspect_remote(config, remote_directory, release_id)
            if installed["state"] != "same" or installed["hash"] != digest or installed["bytes"] != size:
                raise SyncError("远端安装后的作品校验失败")
            _assert_video_unchanged(video_path, initial_state, digest, size)
            verified = True
            return _result(release_id, digest, size)
        finally:
            active_error = sys.exc_info()[1]
            try:
                _cleanup_remote(config, remote_directory)
            except SyncError as cleanup_error:
                if active_error is None:
                    if verified:
                        raise SyncError(
                            "作品已入库并验证，但远端临时目录清理失败；不要重复制作，重新校验并清理"
                        ) from cleanup_error
                    raise SyncError("远端临时目录清理失败") from cleanup_error
                if isinstance(active_error, (SyncError, releases.ReleaseError)):
                    active_error.args = (f"{active_error}；远端临时目录清理失败",)
                    raise active_error
                if not isinstance(active_error, Exception):
                    raise active_error
                raise SyncError("同步过程中发生错误，且远端临时目录清理失败") from active_error


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="同步 OpenFilm 已导出的单个抖音成品")
    parser.add_argument("--config", required=True, help="SSH 与 OpenFilm 项目目录配置 JSON")
    parser.add_argument("--video", required=True, help="已导出的单个 MP4 文件")
    parser.add_argument("--release-id", required=True)
    parser.add_argument("--title", required=True)
    parser.add_argument("--description", required=True)
    parser.add_argument("--source")
    args = parser.parse_args(argv)

    try:
        result = sync_release(
            load_config(args.config),
            args.video,
            args.release_id,
            args.title,
            args.description,
            args.source,
        )
    except releases.ReleaseError as exc:
        print(json.dumps({"error": str(exc)}, ensure_ascii=False), file=sys.stderr)
        return 1
    except SyncError as exc:
        print(json.dumps({"error": str(exc)}, ensure_ascii=False), file=sys.stderr)
        return 1
    except (OSError, ValueError):
        print(json.dumps({"error": "同步失败"}, ensure_ascii=False), file=sys.stderr)
        return 1

    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
