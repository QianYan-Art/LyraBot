from __future__ import annotations

import hashlib
import json
import math
import os
import re
import stat
from datetime import datetime, timedelta
from pathlib import Path


MAX_VIDEO_BYTES = 2 * 1024 * 1024 * 1024
MAX_MANIFEST_BYTES = 64 * 1024
RELEASE_ID_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]{0,63}", re.ASCII)

_REQUIRED_FIELDS = {
    "schema_version",
    "release_id",
    "title",
    "description",
    "ai_generated",
    "video_sha256",
    "video_bytes",
    "duration",
    "width",
    "height",
    "fps",
    "created_at",
}
_ALLOWED_FIELDS = _REQUIRED_FIELDS | {"source"}
_CONTENT_FIELDS = _ALLOWED_FIELDS - {"created_at"}


class ReleaseError(ValueError):
    """A release package is invalid or cannot be accessed safely."""


def validate_release_id(release_id: object) -> str:
    if not isinstance(release_id, str) or RELEASE_ID_PATTERN.fullmatch(release_id) is None:
        raise ReleaseError("release_id 格式无效")
    return release_id


def validate_manifest(value: object) -> dict[str, object]:
    if not isinstance(value, dict):
        raise ReleaseError("manifest 必须是 JSON 对象")
    if set(value) - _ALLOWED_FIELDS or _REQUIRED_FIELDS - set(value):
        raise ReleaseError("manifest 字段不符合 schema_version=1")

    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ReleaseError("manifest schema_version 必须为 1")
    validate_release_id(value["release_id"])

    title = value["title"]
    if (
        not isinstance(title, str)
        or not title.strip()
        or len(title) > 30
        or any(char in title for char in "\r\n\0")
    ):
        raise ReleaseError("标题须为 1 到 30 个字符，且不能包含换行或空字符")
    description = value["description"]
    if (
        not isinstance(description, str)
        or len(description) > 1000
        or "\0" in description
    ):
        raise ReleaseError("简介不能超过 1000 个字符或包含空字符")
    if value["ai_generated"] is not True:
        raise ReleaseError("ai_generated 必须为 true")

    digest = value["video_sha256"]
    if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest, re.ASCII) is None:
        raise ReleaseError("video_sha256 必须是 64 位小写十六进制字符串")
    size = value["video_bytes"]
    if type(size) is not int or not 1 <= size <= MAX_VIDEO_BYTES:
        raise ReleaseError("video_bytes 超出允许范围")

    for field in ("duration", "fps"):
        number = value[field]
        if type(number) not in (int, float) or not math.isfinite(number) or number <= 0:
            raise ReleaseError(f"{field} 必须是正数")
    for field in ("width", "height"):
        dimension = value[field]
        if type(dimension) is not int or dimension <= 0:
            raise ReleaseError(f"{field} 必须是正整数")

    created_at = value["created_at"]
    if not isinstance(created_at, str):
        raise ReleaseError("created_at 必须是 UTC 时间字符串")
    try:
        parsed = datetime.fromisoformat(
            created_at[:-1] + "+00:00" if created_at.endswith("Z") else created_at
        )
    except ValueError as exc:
        raise ReleaseError("created_at 必须是 UTC 时间字符串") from exc
    if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
        raise ReleaseError("created_at 必须是 UTC 时间字符串")

    if "source" in value and (
        not isinstance(value["source"], str) or len(value["source"]) > 200
    ):
        raise ReleaseError("source 必须是不超过 200 个字符的字符串")
    return dict(value)


def manifests_have_same_content(first: dict[str, object], second: dict[str, object]) -> bool:
    return all(first.get(field) == second.get(field) for field in _CONTENT_FIELDS)


def _absolute_path(path: str | os.PathLike[str]) -> Path:
    candidate = Path(path)
    if ".." in candidate.parts:
        raise ReleaseError("路径不能包含父目录跳转")
    return candidate if candidate.is_absolute() else Path.cwd() / candidate


def _path_info(path: str | os.PathLike[str], *, allow_missing: bool = False) -> os.stat_result | None:
    candidate = _absolute_path(path)
    current = Path(candidate.anchor)
    if not candidate.anchor:
        raise ReleaseError("路径必须可解析为绝对路径")
    parts = candidate.parts[1:]
    for index, part in enumerate(parts):
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError:
            if allow_missing:
                return None
            raise ReleaseError("作品包路径不存在") from None
        except OSError as exc:
            raise ReleaseError("无法安全检查作品包路径") from exc
        if stat.S_ISLNK(info.st_mode):
            raise ReleaseError("作品包路径不能包含符号链接")
        if index < len(parts) - 1 and not stat.S_ISDIR(info.st_mode):
            raise ReleaseError("作品包路径中间层必须是目录")
    return info if parts else current.lstat()


def assert_safe_directory(path: str | os.PathLike[str], *, allow_missing: bool = False) -> bool:
    info = _path_info(path, allow_missing=allow_missing)
    if info is None:
        return False
    if not stat.S_ISDIR(info.st_mode):
        raise ReleaseError("作品包目录类型无效")
    return True


def _open_regular_file(path: Path, max_bytes: int) -> tuple[int, os.stat_result]:
    info = _path_info(path)
    if info is None or not stat.S_ISREG(info.st_mode):
        raise ReleaseError("作品包文件必须是普通文件")
    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path, flags)
    except OSError as exc:
        raise ReleaseError("无法安全打开作品包文件") from exc
    opened = os.fstat(fd)
    if not stat.S_ISREG(opened.st_mode) or opened.st_size > max_bytes:
        os.close(fd)
        raise ReleaseError("作品包文件类型或大小无效")
    if (info.st_dev, info.st_ino, info.st_size) != (opened.st_dev, opened.st_ino, opened.st_size):
        os.close(fd)
        raise ReleaseError("作品包文件在读取时发生变化")
    return fd, opened


def _read_manifest(path: Path) -> dict[str, object]:
    fd, info = _open_regular_file(path, MAX_MANIFEST_BYTES)
    try:
        with os.fdopen(fd, "rb") as stream:
            raw = stream.read(MAX_MANIFEST_BYTES + 1)
    except OSError as exc:
        raise ReleaseError("无法读取 manifest") from exc
    if len(raw) != info.st_size or len(raw) > MAX_MANIFEST_BYTES:
        raise ReleaseError("manifest 文件大小无效")

    def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
        result: dict[str, object] = {}
        for key, value in pairs:
            if key in result:
                raise ReleaseError("manifest 包含重复字段")
            result[key] = value
        return result

    try:
        value = json.loads(
            raw.decode("utf-8"),
            object_pairs_hook=unique_object,
            parse_constant=lambda _constant: (_ for _ in ()).throw(
                ReleaseError("manifest JSON 数值无效")
            ),
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ReleaseError("manifest JSON 无效") from exc
    return validate_manifest(value)


def _hash_video(path: Path, max_bytes: int) -> tuple[str, int]:
    fd, info = _open_regular_file(path, max_bytes)
    digest = hashlib.sha256()
    size = 0
    try:
        with os.fdopen(fd, "rb") as stream:
            while chunk := stream.read(1024 * 1024):
                size += len(chunk)
                if size > max_bytes:
                    raise ReleaseError("视频超过 2 GiB 限制")
                digest.update(chunk)
    except OSError as exc:
        raise ReleaseError("无法读取视频文件") from exc
    if size != info.st_size:
        raise ReleaseError("视频文件在读取时发生变化")
    return digest.hexdigest(), size


def _load_package(directory: str | os.PathLike[str], expected_release_id: str | None = None, *, verify_hash: bool = True) -> dict[str, object]:
    package_dir = _absolute_path(directory)
    assert_safe_directory(package_dir)
    try:
        entries = {entry.name for entry in package_dir.iterdir()}
    except OSError as exc:
        raise ReleaseError("无法列出作品包") from exc
    if entries != {"manifest.json", "video.mp4"}:
        raise ReleaseError("作品包必须仅包含 manifest.json 和 video.mp4")

    manifest = _read_manifest(package_dir / "manifest.json")
    release_id = validate_release_id(manifest["release_id"])
    if expected_release_id is not None and release_id != expected_release_id:
        raise ReleaseError("release_id 与作品目录不一致")

    video_path = package_dir / "video.mp4"
    info = _path_info(video_path)
    if info is None or not stat.S_ISREG(info.st_mode):
        raise ReleaseError("video.mp4 必须是普通文件")
    if not 1 <= info.st_size <= MAX_VIDEO_BYTES or info.st_size != manifest["video_bytes"]:
        raise ReleaseError("视频文件大小与 manifest 不一致")
    if verify_hash:
        digest, size = _hash_video(video_path, MAX_VIDEO_BYTES)
        if size != manifest["video_bytes"] or digest != manifest["video_sha256"]:
            raise ReleaseError("视频 hash 或大小与 manifest 不一致")

    return {**manifest, "video_path": video_path}


class ReleaseStore:
    def __init__(self, data_dir: str | os.PathLike[str]) -> None:
        self.data_dir = _absolute_path(data_dir)

    @property
    def releases_dir(self) -> Path:
        return self.data_dir / "releases"

    def list_releases(self) -> list[dict[str, object]]:
        if not assert_safe_directory(self.data_dir, allow_missing=True):
            return []
        if not assert_safe_directory(self.releases_dir, allow_missing=True):
            return []
        try:
            entries = sorted(self.releases_dir.iterdir(), key=lambda entry: entry.name)
        except OSError as exc:
            raise ReleaseError("无法列出作品目录") from exc

        releases = []
        for entry in entries:
            release_id = validate_release_id(entry.name)
            loaded = _load_package(entry, release_id, verify_hash=False)
            releases.append({key: value for key, value in loaded.items() if key != "video_path"})
        return releases

    def load(self, release_id: str) -> dict[str, object]:
        release_id = validate_release_id(release_id)
        if not assert_safe_directory(self.data_dir):
            raise ReleaseError("作品数据目录不存在")
        if not assert_safe_directory(self.releases_dir):
            raise ReleaseError("作品目录不存在")
        return _load_package(self.releases_dir / release_id, release_id)

    def load_metadata(self, release_id: str) -> dict[str, object]:
        """仅供已存封面校验；发布记录不能代替可上传的完整作品包。"""
        release_id = validate_release_id(release_id)
        assert_safe_directory(self.data_dir)
        assert_safe_directory(self.releases_dir, allow_missing=True)
        if os.path.lexists(self.releases_dir / release_id):
            return self.load(release_id)
        records = self.data_dir / "published-records"
        assert_safe_directory(records)
        directory = records / release_id
        assert_safe_directory(directory)
        if {entry.name for entry in directory.iterdir()} != {"manifest.json"}:
            raise ReleaseError("发布清单目录内容无效")
        manifest = _read_manifest(directory / "manifest.json")
        if manifest["release_id"] != release_id:
            raise ReleaseError("发布清单作品编号不一致")
        return manifest

    def list_metadata(self) -> list[dict[str, object]]:
        """图库包含已移除原片的发布清单，上传列表仍只含完整原片。"""
        releases = {item["release_id"]: item for item in self.list_releases()}
        records = self.data_dir / "published-records"
        if not assert_safe_directory(records, allow_missing=True):
            return list(releases.values())
        for entry in sorted(records.iterdir(), key=lambda path: path.name):
            release_id = validate_release_id(entry.name)
            if release_id not in releases:
                releases[release_id] = self.load_metadata(release_id)
        return [releases[key] for key in sorted(releases)]


__all__ = ["ReleaseError", "ReleaseStore"]
