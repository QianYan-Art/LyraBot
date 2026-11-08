from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import stat
import subprocess
import tempfile
from datetime import datetime, timezone
from fractions import Fraction
from pathlib import Path

from releases import (
    MAX_MANIFEST_BYTES,
    MAX_VIDEO_BYTES,
    ReleaseError,
    ReleaseStore,
    _absolute_path,
    _hash_video,
    _load_package,
    _open_regular_file,
    _path_info,
    assert_safe_directory,
    manifests_have_same_content,
    validate_manifest,
    validate_release_id,
)


def _same_file_state(first: os.stat_result, second: os.stat_result) -> bool:
    return (
        first.st_dev,
        first.st_ino,
        first.st_size,
        getattr(first, "st_mtime_ns", None),
    ) == (
        second.st_dev,
        second.st_ino,
        second.st_size,
        getattr(second, "st_mtime_ns", None),
    )


def _positive_float(value: object, field: str) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ReleaseError(f"ffprobe 的 {field} 无效") from exc
    if not math.isfinite(number) or number <= 0:
        raise ReleaseError(f"ffprobe 的 {field} 必须是正数")
    return number


def _frame_rate(value: object) -> float:
    if not isinstance(value, str) or not value:
        raise ReleaseError("ffprobe 未提供有效帧率")
    try:
        rate = float(Fraction(value))
    except (ValueError, ZeroDivisionError, OverflowError) as exc:
        raise ReleaseError("ffprobe 未提供有效帧率") from exc
    if not math.isfinite(rate) or rate <= 0:
        raise ReleaseError("ffprobe 未提供有效帧率")
    return rate


def _metadata_from_probe(document: object) -> dict[str, int | float]:
    if not isinstance(document, dict):
        raise ReleaseError("ffprobe 输出结构无效")
    streams = document.get("streams")
    if not isinstance(streams, list):
        raise ReleaseError("ffprobe 未返回媒体流")
    video_streams = [stream for stream in streams if isinstance(stream, dict) and stream.get("codec_type") == "video"]
    audio_streams = [stream for stream in streams if isinstance(stream, dict) and stream.get("codec_type") == "audio"]
    if len(video_streams) != 1 or video_streams[0].get("codec_name") != "h264":
        raise ReleaseError("视频必须包含一个 H.264 视频流")
    if not audio_streams or any(stream.get("codec_name") != "aac" for stream in audio_streams):
        raise ReleaseError("视频必须包含 AAC 音频流")

    video = video_streams[0]
    try:
        width, height = int(video["width"]), int(video["height"])
    except (KeyError, TypeError, ValueError) as exc:
        raise ReleaseError("ffprobe 未提供有效视频尺寸") from exc
    if width <= 0 or height <= 0:
        raise ReleaseError("ffprobe 未提供有效视频尺寸")

    format_info = document.get("format")
    duration_value = format_info.get("duration") if isinstance(format_info, dict) else None
    if duration_value is None:
        duration_value = video.get("duration")
    duration = _positive_float(duration_value, "duration")

    fps = None
    for key in ("avg_frame_rate", "r_frame_rate"):
        try:
            fps = _frame_rate(video.get(key))
            break
        except ReleaseError:
            continue
    if fps is None:
        raise ReleaseError("ffprobe 未提供有效帧率")
    return {"duration": duration, "width": width, "height": height, "fps": fps}


def probe_video(video_path: str | os.PathLike[str]) -> dict[str, int | float]:
    video_path = _absolute_path(video_path)
    info = _path_info(video_path)
    if info is None or not stat.S_ISREG(info.st_mode):
        raise ReleaseError("视频必须是普通文件")
    if not 1 <= info.st_size <= MAX_VIDEO_BYTES:
        raise ReleaseError("视频大小必须在 1 字节到 2 GiB 之间")
    command = [
        "ffprobe",
        "-v",
        "error",
        "-show_entries",
        "format=duration:stream=codec_type,codec_name,width,height,duration,avg_frame_rate,r_frame_rate",
        "-show_format",
        "-show_streams",
        "-of",
        "json",
        str(video_path),
    ]
    try:
        result = subprocess.run(command, capture_output=True, text=True, check=False, timeout=30)
    except FileNotFoundError as exc:
        raise ReleaseError("找不到 ffprobe") from exc
    except subprocess.TimeoutExpired as exc:
        raise ReleaseError("ffprobe 检查视频超时") from exc
    except OSError as exc:
        raise ReleaseError("无法启动 ffprobe") from exc
    if result.returncode != 0:
        raise ReleaseError("ffprobe 无法读取视频")
    try:
        document = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise ReleaseError("ffprobe 输出 JSON 无效") from exc
    metadata = _metadata_from_probe(document)
    after_probe = _path_info(video_path)
    if after_probe is None or not _same_file_state(info, after_probe):
        raise ReleaseError("视频在检查期间发生变化")
    return metadata


def _write_manifest(output: Path, manifest: dict[str, object]) -> None:
    try:
        parent = output.parent
        assert_safe_directory(parent)
        existing = _path_info(output, allow_missing=True)
        if existing is not None and not stat.S_ISREG(existing.st_mode):
            raise ReleaseError("manifest 输出目标必须是普通文件")
        encoded = (json.dumps(manifest, ensure_ascii=False, indent=2, allow_nan=False) + "\n").encode("utf-8")
        if len(encoded) > MAX_MANIFEST_BYTES:
            raise ReleaseError("manifest 超过 64 KiB 限制")

        fd, temporary_name = tempfile.mkstemp(prefix=".manifest-", suffix=".tmp", dir=parent)
        temporary = Path(temporary_name)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(encoded)
                stream.flush()
                os.fsync(stream.fileno())
                if hasattr(os, "fchmod"):
                    os.fchmod(stream.fileno(), 0o600)
            if not hasattr(os, "fchmod"):
                os.chmod(temporary, 0o600)
            os.replace(temporary, output)
        finally:
            temporary.unlink(missing_ok=True)
    except OSError as exc:
        raise ReleaseError("无法安全写入 manifest") from exc


def prepare_release(
    video: str | os.PathLike[str],
    release_id: str,
    title: str,
    description: str,
    output: str | os.PathLike[str],
    *,
    source: str | None = None,
) -> dict[str, object]:
    release_id = validate_release_id(release_id)
    video_path = _absolute_path(video)
    output_path = _absolute_path(output)
    if os.path.normcase(os.path.abspath(video_path)) == os.path.normcase(os.path.abspath(output_path)):
        raise ReleaseError("manifest 输出不能覆盖原视频")
    info = _path_info(video_path)
    if info is None or not stat.S_ISREG(info.st_mode):
        raise ReleaseError("视频必须是普通文件")
    if not 1 <= info.st_size <= MAX_VIDEO_BYTES:
        raise ReleaseError("视频大小必须在 1 字节到 2 GiB 之间")

    metadata = probe_video(video_path)
    after_probe = _path_info(video_path)
    if after_probe is None or not _same_file_state(info, after_probe):
        raise ReleaseError("视频在检查期间发生变化")
    digest, size = _hash_video(video_path, MAX_VIDEO_BYTES)
    after_hash = _path_info(video_path)
    if after_hash is None or not _same_file_state(info, after_hash) or size != info.st_size:
        raise ReleaseError("视频在计算 hash 期间发生变化")

    manifest: dict[str, object] = {
        "schema_version": 1,
        "release_id": release_id,
        "title": title,
        "description": description,
        "ai_generated": True,
        "video_sha256": digest,
        "video_bytes": size,
        **metadata,
        "created_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }
    if source is not None:
        manifest["source"] = source
    manifest = validate_manifest(manifest)
    _write_manifest(output_path, manifest)
    return manifest


def _set_owner_and_mode(path: Path, mode: int, uid: int | None, gid: int | None) -> None:
    if (uid is None) != (gid is None):
        raise ReleaseError("runner UID 和 GID 必须同时指定")
    try:
        if uid is not None and gid is not None:
            os.chown(path, uid, gid)
        os.chmod(path, mode)
    except OSError as exc:
        raise ReleaseError("无法设置作品目录权限") from exc


def _copy_regular_file(source: Path, destination: Path, max_bytes: int, uid: int | None, gid: int | None) -> None:
    source_fd, source_info = _open_regular_file(source, max_bytes)
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        destination_fd = os.open(destination, flags, 0o600)
    except OSError as exc:
        os.close(source_fd)
        raise ReleaseError("无法创建作品暂存文件") from exc

    copied = 0
    try:
        with os.fdopen(source_fd, "rb") as source_stream, os.fdopen(destination_fd, "wb") as destination_stream:
            while chunk := source_stream.read(1024 * 1024):
                copied += len(chunk)
                if copied > max_bytes:
                    raise ReleaseError("作品包文件超过大小限制")
                destination_stream.write(chunk)
            if copied != source_info.st_size:
                raise ReleaseError("作品包源文件在复制时发生变化")
            destination_stream.flush()
            os.fsync(destination_stream.fileno())
            if uid is not None and gid is not None:
                os.fchown(destination_stream.fileno(), uid, gid)
            if hasattr(os, "fchmod"):
                os.fchmod(destination_stream.fileno(), 0o600)
            else:
                os.chmod(destination, 0o600)
    except OSError as exc:
        raise ReleaseError("无法复制作品包文件") from exc


def _prepare_data_directory(data_dir: Path, uid: int | None, gid: int | None) -> None:
    try:
        if not assert_safe_directory(data_dir, allow_missing=True):
            data_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
            assert_safe_directory(data_dir)
            _set_owner_and_mode(data_dir, 0o700, uid, gid)
        releases_dir = data_dir / "releases"
        if not assert_safe_directory(releases_dir, allow_missing=True):
            releases_dir.mkdir(mode=0o700)
        _set_owner_and_mode(releases_dir, 0o700, uid, gid)
    except OSError as exc:
        raise ReleaseError("无法准备作品数据目录") from exc


def _existing_release(store: ReleaseStore, release_id: str) -> dict[str, object] | None:
    target = store.releases_dir / release_id
    info = _path_info(target, allow_missing=True)
    if info is None:
        return None
    if not stat.S_ISDIR(info.st_mode):
        raise ReleaseError("release_id 已被非目录路径占用")
    return store.load(release_id)


def install_release(
    source_dir: str | os.PathLike[str],
    data_dir: str | os.PathLike[str] = "/var/lib/tangyuge-douyin",
    *,
    runner_uid: int | None = None,
    runner_gid: int | None = None,
) -> str:
    if (runner_uid is None) != (runner_gid is None):
        raise ReleaseError("runner UID 和 GID 必须同时指定")
    source_path = _absolute_path(source_dir)
    package = _load_package(source_path)
    release_id = validate_release_id(package["release_id"])
    manifest = {key: value for key, value in package.items() if key != "video_path"}
    data_path = _absolute_path(data_dir)
    _prepare_data_directory(data_path, runner_uid, runner_gid)
    store = ReleaseStore(data_path)

    existing = _existing_release(store, release_id)
    if existing is not None:
        if manifests_have_same_content(existing, manifest):
            return release_id
        raise ReleaseError("相同 release_id 已存在不同内容，拒绝覆盖")

    recorded = store.data_dir / "published-records" / release_id
    if os.path.lexists(recorded) and not manifests_have_same_content(store.load_metadata(release_id), manifest):
        raise ReleaseError("相同 release_id 已有不同发布清单，拒绝覆盖；请使用修订编号")

    temporary_dir = Path(tempfile.mkdtemp(prefix=".release-", dir=data_path))
    try:
        _copy_regular_file(source_path / "manifest.json", temporary_dir / "manifest.json", MAX_MANIFEST_BYTES, runner_uid, runner_gid)
        _copy_regular_file(source_path / "video.mp4", temporary_dir / "video.mp4", MAX_VIDEO_BYTES, runner_uid, runner_gid)
        _set_owner_and_mode(temporary_dir, 0o700, runner_uid, runner_gid)
        staged = _load_package(temporary_dir, release_id)
        staged_manifest = {key: value for key, value in staged.items() if key != "video_path"}
        if not manifests_have_same_content(staged_manifest, manifest):
            raise ReleaseError("暂存作品包与来源内容不一致")

        target = store.releases_dir / release_id
        if os.path.lexists(recorded) and not manifests_have_same_content(store.load_metadata(release_id), staged_manifest):
            raise ReleaseError("相同 release_id 已有不同发布清单，拒绝覆盖；请使用修订编号")
        if _existing_release(store, release_id) is not None:
            existing = store.load(release_id)
            if manifests_have_same_content(existing, staged_manifest):
                return release_id
            raise ReleaseError("相同 release_id 已存在不同内容，拒绝覆盖")
        try:
            os.rename(temporary_dir, target)
        except FileExistsError:
            existing = store.load(release_id)
            if manifests_have_same_content(existing, staged_manifest):
                return release_id
            raise ReleaseError("相同 release_id 已存在不同内容，拒绝覆盖") from None
        if os.name == "posix":
            directory_fd = os.open(store.releases_dir, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        return release_id
    finally:
        if temporary_dir.exists():
            shutil.rmtree(temporary_dir)


def _runner_identity() -> tuple[int, int]:
    if os.name != "posix" or not hasattr(os, "geteuid") or os.geteuid() != 0:
        raise ReleaseError("install 子命令必须由 root 在服务器上执行")
    try:
        import grp
        import pwd

        runner = pwd.getpwnam("douyin-runner")
        group = grp.getgrnam("tangyuge-douyin")
    except (ImportError, KeyError) as exc:
        raise ReleaseError("找不到 douyin-runner 或 tangyuge-douyin") from exc
    return runner.pw_uid, group.gr_gid


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="准备或安装受限抖音作品包")
    commands = parser.add_subparsers(dest="command", required=True)

    prepare = commands.add_parser("prepare", help="验证已导出 MP4 并生成 manifest")
    prepare.add_argument("--video", required=True)
    prepare.add_argument("--release-id", required=True)
    prepare.add_argument("--title", required=True)
    prepare.add_argument("--description", required=True)
    prepare.add_argument("--source")
    prepare.add_argument("--output", required=True)

    install = commands.add_parser("install", help="安装已暂存的作品包")
    install.add_argument("--source-dir", required=True)
    install.add_argument("--data-dir", default="/var/lib/tangyuge-douyin")

    args = parser.parse_args(argv)
    try:
        if args.command == "prepare":
            manifest = prepare_release(
                args.video,
                args.release_id,
                args.title,
                args.description,
                args.output,
                source=args.source,
            )
            print(json.dumps(manifest, ensure_ascii=False))
        else:
            uid, gid = _runner_identity()
            release_id = install_release(args.source_dir, args.data_dir, runner_uid=uid, runner_gid=gid)
            print(json.dumps({"release_id": release_id}, ensure_ascii=False))
    except ReleaseError as exc:
        parser.exit(2, f"stage_release: {exc}\n")
    except OSError as exc:
        parser.exit(2, f"stage_release: 文件系统操作失败（{type(exc).__name__}）\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
