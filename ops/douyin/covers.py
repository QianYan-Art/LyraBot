from __future__ import annotations

import base64
import hashlib
import io
import json
import math
import os
import re
import shutil
import stat
import subprocess
import tempfile
import warnings
from pathlib import Path
from typing import Any, Sequence

from PIL import Image, ImageOps

from releases import validate_release_id


MAX_IMAGE_BYTES = 8 * 1024 * 1024
MAX_FRAME_BYTES = 32 * 1024 * 1024
MAX_SOURCE_BYTES = 64 * 1024 * 1024
MAX_PIXELS = 16_000_000
MAX_PREVIEW_BYTES = 32_768
FFMPEG_TIMEOUT_SECONDS = 30

_HEX_DIGEST = re.compile(r"[0-9a-f]{64}", re.ASCII)
_ALLOWED_FORMATS = {"JPEG", "PNG", "WEBP"}
_ASSET_NAMES = ("source.jpg", "landscape.jpg", "portrait.jpg")
_COVER_SIZES = {"landscape": (1200, 900), "portrait": (900, 1200)}
_PREVIEW_SIZES = {"landscape": (320, 240), "portrait": (240, 320)}
_PREVIEW_QUALITIES = (40, 32, 24, 18, 12, 8, 4, 1)
_MANIFEST_NAME = "cover.json"


class CoverError(ValueError):
    """Raised when a cover asset or its backing files are invalid."""


def _is_link(info: os.stat_result) -> bool:
    reparse_point = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0)
    attributes = getattr(info, "st_file_attributes", 0)
    return stat.S_ISLNK(info.st_mode) or bool(reparse_point and attributes & reparse_point)


def _assert_directory_tree(path: Path) -> None:
    absolute = Path(os.path.abspath(path))
    for candidate in reversed((absolute, *absolute.parents)):
        try:
            info = os.lstat(candidate)
        except OSError as exc:
            raise CoverError("封面数据目录不可访问") from exc
        if _is_link(info):
            raise CoverError("封面数据目录不能包含符号链接")
        if not stat.S_ISDIR(info.st_mode):
            raise CoverError("封面数据路径不是目录")


def _ensure_private_directory(path: Path) -> None:
    try:
        path.mkdir(mode=0o700)
    except FileExistsError:
        pass
    except OSError as exc:
        raise CoverError("无法创建封面数据目录") from exc
    _assert_directory_tree(path)
    if os.name == "posix":
        try:
            os.chmod(path, 0o700, follow_symlinks=False)
        except OSError as exc:
            raise CoverError("无法设置封面目录权限") from exc


def _assert_private_directory(path: Path) -> None:
    _assert_directory_tree(path)
    if os.name == "posix" and stat.S_IMODE(os.lstat(path).st_mode) != 0o700:
        raise CoverError("封面目录权限无效")


def _read_regular_file(path: Path, max_bytes: int, *, private: bool = False) -> bytes:
    _assert_directory_tree(path.parent)
    try:
        before = os.lstat(path)
    except OSError as exc:
        raise CoverError("封面资产文件不存在") from exc
    if _is_link(before) or not stat.S_ISREG(before.st_mode):
        raise CoverError("封面资产必须是普通文件")
    if private and os.name == "posix" and stat.S_IMODE(before.st_mode) != 0o600:
        raise CoverError("封面资产权限无效")
    if before.st_size < 0 or before.st_size > max_bytes:
        raise CoverError("封面资产大小超出允许范围")

    flags = os.O_RDONLY | getattr(os, "O_BINARY", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path, flags)
    except OSError as exc:
        raise CoverError("无法读取封面资产") from exc
    try:
        opened = os.fstat(fd)
        if not stat.S_ISREG(opened.st_mode) or opened.st_size > max_bytes:
            raise CoverError("封面资产文件类型或大小无效")
        chunks: list[bytes] = []
        total = 0
        while True:
            chunk = os.read(fd, min(1024 * 1024, max_bytes + 1 - total))
            if not chunk:
                break
            total += len(chunk)
            if total > max_bytes:
                raise CoverError("封面资产大小超出允许范围")
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)


def _write_private_file(directory: Path, name: str, contents: bytes) -> None:
    path = directory / name
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
    try:
        fd = os.open(path, flags, 0o600)
    except OSError as exc:
        raise CoverError("无法写入封面资产") from exc
    try:
        view = memoryview(contents)
        while view:
            written = os.write(fd, view)
            if written <= 0:
                raise OSError("short write")
            view = view[written:]
    except OSError as exc:
        raise CoverError("封面资产写入失败") from exc
    finally:
        os.close(fd)


def _digest(contents: bytes) -> str:
    return hashlib.sha256(contents).hexdigest()


def _finite_number(value: object) -> bool:
    if type(value) not in (int, float):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False


def _cover_identity(release_id: str, video_sha256: str, cover_sha256: str) -> str:
    payload = (
        b"douyin-cover-v1\0"
        + release_id.encode("ascii")
        + b"\0"
        + video_sha256.encode("ascii")
        + b"\0"
        + cover_sha256.encode("ascii")
    )
    return hashlib.sha256(payload).hexdigest()


def _paired_cover_sha256(landscape_sha256: str, portrait_sha256: str) -> str:
    if _HEX_DIGEST.fullmatch(landscape_sha256) is None or _HEX_DIGEST.fullmatch(portrait_sha256) is None:
        raise CoverError("配对封面摘要无效")
    payload = (
        b"douyin-paired-cover-v2\0"
        + bytes.fromhex(landscape_sha256)
        + bytes.fromhex(portrait_sha256)
    )
    return hashlib.sha256(payload).hexdigest()


def _unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise CoverError("封面清单包含重复字段")
        result[key] = value
    return result


def _decode_rgb(image_bytes: bytes, *, max_pixels: int) -> Image.Image:
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error", Image.DecompressionBombWarning)
            with Image.open(io.BytesIO(image_bytes)) as image:
                if image.format not in _ALLOWED_FORMATS:
                    raise CoverError("仅接受 JPEG、PNG 或 WebP 图片")
                width, height = image.size
                if width < 1 or height < 1 or width * height > max_pixels:
                    raise CoverError("图片像素数超出允许范围")
                if getattr(image, "is_animated", False) or getattr(image, "n_frames", 1) != 1:
                    raise CoverError("不接受动画图片")
                image.seek(0)
                image.load()
                oriented = ImageOps.exif_transpose(image)
                converted = oriented.convert("RGB")
                return Image.frombytes("RGB", converted.size, converted.tobytes())
    except CoverError:
        raise
    except (OSError, ValueError, Image.DecompressionBombError, Image.DecompressionBombWarning) as exc:
        raise CoverError("图片数据无效") from exc


def _save_jpeg(image: Image.Image, *, quality: int) -> bytes:
    output = io.BytesIO()
    image.save(
        output,
        format="JPEG",
        quality=quality,
        optimize=True,
        progressive=False,
        subsampling=0,
    )
    return output.getvalue()


def _normalize_image(image_bytes: bytes | bytearray | memoryview, *, max_bytes: int) -> bytes:
    if not isinstance(image_bytes, (bytes, bytearray, memoryview)):
        raise CoverError("图片必须以字节形式提供")
    byte_count = image_bytes.nbytes if isinstance(image_bytes, memoryview) else len(image_bytes)
    if not byte_count or byte_count > max_bytes:
        raise CoverError("图片文件大小超出允许范围")
    rgb = _decode_rgb(bytes(image_bytes), max_pixels=MAX_PIXELS)
    return _save_jpeg(rgb, quality=95)


def _cover_jpegs(source: Image.Image) -> dict[str, bytes]:
    outputs: dict[str, bytes] = {}
    for name, size in _COVER_SIZES.items():
        padded = ImageOps.pad(
            source,
            size,
            method=Image.Resampling.LANCZOS,
            color=(0, 0, 0),
            centering=(0.5, 0.5),
        )
        outputs[name] = _save_jpeg(padded, quality=90)
    return outputs


def _preview_jpeg(cover_jpeg: bytes, name: str) -> bytes:
    size = _PREVIEW_SIZES[name]
    rgb = _decode_rgb(cover_jpeg, max_pixels=MAX_PIXELS)
    resized = rgb.resize(size, Image.Resampling.LANCZOS)
    for quality in _PREVIEW_QUALITIES:
        data = _save_jpeg(resized, quality=quality)
        if len(data) <= MAX_PREVIEW_BYTES:
            return data
    raise CoverError("预览图片无法压缩到大小限制内")


class CoverStore:
    CATEGORIES = {
        "qq_image": "uploads",
        "video_frame": "frames",
        "library_image": "reused",
        "saved_image": "saved",
        "paired_images": "pairs",
    }

    def __init__(self, data_dir: str | os.PathLike[str], release_store: Any) -> None:
        self.data_dir = Path(os.path.abspath(data_dir))
        self.covers_dir = self.data_dir / "covers"
        self.release_store = release_store

    def _release(self, release_id: str, *, metadata_only: bool = False) -> dict[str, object]:
        release_id = validate_release_id(release_id)
        release = (self.release_store.load_metadata(release_id) if metadata_only
                   else self.release_store.load(release_id))
        if not isinstance(release, dict) or release.get("release_id") != release_id:
            raise CoverError("发布作品信息无效")
        video_sha256 = release.get("video_sha256")
        duration = release.get("duration")
        video_path = release.get("video_path")
        if not isinstance(video_sha256, str) or _HEX_DIGEST.fullmatch(video_sha256) is None:
            raise CoverError("原视频摘要无效")
        if not _finite_number(duration) or duration <= 0:
            raise CoverError("原视频时长无效")
        if not metadata_only and not isinstance(video_path, (str, os.PathLike)):
            raise CoverError("原视频路径无效")
        return {
            "release_id": release_id,
            "video_sha256": video_sha256,
            "duration": float(duration),
            "video_path": Path(video_path) if isinstance(video_path, (str, os.PathLike)) else None,
        }

    def _ensure_layout(self, release_id: str) -> Path:
        _assert_directory_tree(self.data_dir)
        _ensure_private_directory(self.covers_dir)
        release_dir = self.covers_dir / release_id
        _ensure_private_directory(release_dir)
        return release_dir

    def _public_dict(
        self,
        private: dict[str, object],
        *,
        source: str,
        time: float | None,
    ) -> dict[str, object]:
        previews: list[dict[str, str]] = []
        for name in ("landscape", "portrait"):
            path = private[f"{name}_path"]
            assert isinstance(path, Path)
            cover_bytes = _read_regular_file(path, 16 * 1024 * 1024, private=True)
            preview = _preview_jpeg(cover_bytes, name)
            previews.append({"format": name, "jpeg_base64": base64.b64encode(preview).decode("ascii")})
        public: dict[str, object] = {
            "release_id": private["release_id"],
            "cover_id": private["cover_id"],
            "cover_sha256": private["cover_sha256"],
            "video_sha256": private["video_sha256"],
            "source": source,
            "previews": previews,
        }
        if time is not None:
            public["time"] = time
        return public

    def _asset_directory(self, release_id: str, cover_id: str) -> Path | None:
        """兼容旧扁平目录，同一编号只允许存在一份资产。"""
        release_dir = self.covers_dir / release_id
        _assert_private_directory(self.covers_dir)
        _assert_private_directory(release_dir)
        found = []
        for parent in [release_dir, *(release_dir / name for name in self.CATEGORIES.values())]:
            if not os.path.lexists(parent):
                continue
            _assert_private_directory(parent)
            candidate = parent / cover_id
            if os.path.lexists(candidate):
                _assert_private_directory(candidate)
                found.append(candidate)
        if len(found) > 1:
            raise CoverError("同一封面编号出现多份资产，拒绝歧义引用")
        return found[0] if found else None

    def _load_asset(
        self,
        release: dict[str, object],
        cover_id: str,
        *,
        allow_pair: bool = True,
    ) -> dict[str, object]:
        release_id = str(release["release_id"])
        if not isinstance(cover_id, str) or _HEX_DIGEST.fullmatch(cover_id) is None:
            raise CoverError("cover_id 格式无效")
        directory = self._asset_directory(release_id, cover_id)
        if directory is None:
            raise CoverError("封面资产不存在")
        try:
            names = {entry.name for entry in directory.iterdir()}
        except OSError as exc:
            raise CoverError("无法列出封面资产") from exc

        manifest_bytes = _read_regular_file(directory / _MANIFEST_NAME, 64 * 1024, private=True)
        try:
            manifest = json.loads(manifest_bytes, object_pairs_hook=_unique_object)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise CoverError("封面清单无效") from exc
        base_required = {
            "schema_version",
            "release_id",
            "cover_id",
            "cover_sha256",
            "video_sha256",
            "files",
            "source",
            "time",
        }
        if not isinstance(manifest, dict):
            raise CoverError("封面清单字段无效")
        schema_version = manifest.get("schema_version")
        if type(schema_version) is not int or schema_version not in (1, 2):
            raise CoverError("封面清单版本无效")
        if schema_version == 2:
            if not allow_pair:
                raise CoverError("配对封面不能引用其他配对封面")
            required = base_required | {"references"}
            asset_names = ("landscape.jpg", "portrait.jpg")
            if names != {*asset_names, _MANIFEST_NAME}:
                raise CoverError("配对封面目录内容不完整或包含未知文件")
        else:
            required = base_required
            asset_names = _ASSET_NAMES
            if names != {*asset_names, _MANIFEST_NAME}:
                raise CoverError("封面目录内容不完整或包含未知文件")
        if set(manifest) != required:
            raise CoverError("封面清单字段无效")
        if manifest["release_id"] != release_id or manifest["cover_id"] != cover_id:
            raise CoverError("封面清单身份不匹配")

        cover_sha256 = manifest["cover_sha256"]
        video_sha256 = manifest["video_sha256"]
        source = manifest["source"]
        if not isinstance(cover_sha256, str) or _HEX_DIGEST.fullmatch(cover_sha256) is None:
            raise CoverError("封面摘要无效")
        if video_sha256 != release["video_sha256"]:
            raise CoverError("封面关联的原视频已变化")
        if not isinstance(source, str) or source not in self.CATEGORIES:
            raise CoverError("封面来源无效")
        frame_time = manifest["time"]

        references: dict[str, str] | None = None
        expected_assets: dict[str, bytes] | None = None
        if schema_version == 2:
            if source != "paired_images" or frame_time is not None:
                raise CoverError("配对封面来源或时间字段无效")
            if directory.parent != self.covers_dir / release_id / self.CATEGORIES["paired_images"]:
                raise CoverError("配对封面分类与记录来源不一致")
            raw_references = manifest["references"]
            reference_fields = {"release_id", "landscape_cover_id", "portrait_cover_id"}
            if not isinstance(raw_references, dict) or set(raw_references) != reference_fields:
                raise CoverError("配对封面引用字段无效")
            try:
                source_release_id = validate_release_id(raw_references["release_id"])
            except (TypeError, ValueError) as exc:
                raise CoverError("配对封面来源作品编号无效") from exc
            landscape_cover_id = raw_references["landscape_cover_id"]
            portrait_cover_id = raw_references["portrait_cover_id"]
            if (
                not isinstance(landscape_cover_id, str)
                or _HEX_DIGEST.fullmatch(landscape_cover_id) is None
                or not isinstance(portrait_cover_id, str)
                or _HEX_DIGEST.fullmatch(portrait_cover_id) is None
            ):
                raise CoverError("配对封面引用编号无效")
            try:
                source_release = self._release(source_release_id, metadata_only=True)
                landscape = self._load_asset(source_release, landscape_cover_id, allow_pair=False)
                portrait = self._load_asset(source_release, portrait_cover_id, allow_pair=False)
            except Exception as exc:
                raise CoverError("配对封面引用的单图资产无效") from exc
            landscape_source_path = landscape.get("source_path")
            portrait_source_path = portrait.get("source_path")
            if not isinstance(landscape_source_path, Path) or not isinstance(portrait_source_path, Path):
                raise CoverError("配对封面引用必须是原始单图")
            landscape_source = _decode_rgb(
                _read_regular_file(landscape_source_path, MAX_SOURCE_BYTES, private=True),
                max_pixels=MAX_PIXELS,
            )
            portrait_source = _decode_rgb(
                _read_regular_file(portrait_source_path, MAX_SOURCE_BYTES, private=True),
                max_pixels=MAX_PIXELS,
            )
            if landscape_source.width * 3 != landscape_source.height * 4:
                raise CoverError("横版封面原图必须为严格 4:3")
            if portrait_source.width * 4 != portrait_source.height * 3:
                raise CoverError("竖版封面原图必须为严格 3:4")
            references = {
                "release_id": source_release_id,
                "landscape_cover_id": landscape_cover_id,
                "portrait_cover_id": portrait_cover_id,
            }
            expected_assets = {
                "landscape.jpg": _read_regular_file(
                    landscape["landscape_path"], 16 * 1024 * 1024, private=True
                ),
                "portrait.jpg": _read_regular_file(
                    portrait["portrait_path"], 16 * 1024 * 1024, private=True
                ),
            }
            expected_cover_sha256 = _paired_cover_sha256(
                str(landscape["cover_sha256"]), str(portrait["cover_sha256"])
            )
            if cover_sha256 != expected_cover_sha256:
                raise CoverError("配对封面身份摘要不匹配")
        else:
            if source == "paired_images":
                raise CoverError("配对封面清单版本无效")
            if directory.parent != self.covers_dir / release_id and directory.parent.name != self.CATEGORIES[source]:
                raise CoverError("封面分类与记录来源不一致")
            if source == "video_frame":
                if not _finite_number(frame_time):
                    raise CoverError("视频帧时间无效")
                if not 0 <= frame_time < float(release["duration"]):
                    raise CoverError("视频帧时间超出原片时长")
            elif frame_time is not None:
                raise CoverError("用户图片不应包含视频帧时间")

        if _cover_identity(release_id, str(video_sha256), cover_sha256) != cover_id:
            raise CoverError("封面身份摘要不匹配")

        file_digests = manifest["files"]
        if not isinstance(file_digests, dict) or set(file_digests) != set(asset_names):
            raise CoverError("封面资产摘要字段无效")
        asset_bytes: dict[str, bytes] = {}
        for name in asset_names:
            expected_digest = file_digests[name]
            if not isinstance(expected_digest, str) or _HEX_DIGEST.fullmatch(expected_digest) is None:
                raise CoverError("封面资产摘要无效")
            contents = _read_regular_file(
                directory / name,
                MAX_SOURCE_BYTES if name == "source.jpg" else 16 * 1024 * 1024,
                private=True,
            )
            if _digest(contents) != expected_digest:
                raise CoverError("封面资产与清单摘要不一致")
            asset_bytes[name] = contents

        if schema_version == 2:
            assert expected_assets is not None
            if asset_bytes != expected_assets:
                raise CoverError("配对封面图片与其原始单图不一致")
        else:
            if _digest(asset_bytes["source.jpg"]) != cover_sha256:
                raise CoverError("规范化原图摘要不一致")
            source_image = _decode_rgb(asset_bytes["source.jpg"], max_pixels=MAX_PIXELS)
            if source_image.size[0] * source_image.size[1] > MAX_PIXELS:
                raise CoverError("规范化原图像素数超出允许范围")
            expected_covers = _cover_jpegs(source_image)
            # 以规范化原图重算封面字节，不能把清单中的摘要当作派生图真实性证明。
            for name, expected in expected_covers.items():
                if asset_bytes[f"{name}.jpg"] != expected:
                    raise CoverError("封面图片与规范化原图导出结果不一致")

        for name, size in _COVER_SIZES.items():
            with warnings.catch_warnings():
                warnings.simplefilter("error", Image.DecompressionBombWarning)
                try:
                    with Image.open(io.BytesIO(asset_bytes[f"{name}.jpg"])) as image:
                        if image.format != "JPEG" or image.size != size or getattr(image, "n_frames", 1) != 1:
                            raise CoverError("封面图片尺寸或格式无效")
                        image.load()
                except CoverError:
                    raise
                except (OSError, ValueError, Image.DecompressionBombError, Image.DecompressionBombWarning) as exc:
                    raise CoverError("封面图片数据无效") from exc

        private: dict[str, object] = {
            "release_id": release_id,
            "cover_id": cover_id,
            "cover_sha256": cover_sha256,
            "video_sha256": video_sha256,
            "landscape_path": directory / "landscape.jpg",
            "portrait_path": directory / "portrait.jpg",
            "source": source,
            "time": float(frame_time) if frame_time is not None else None,
        }
        if schema_version == 2:
            assert references is not None
            private["layout"] = "paired"
            private["references"] = references
        else:
            private["source_path"] = directory / "source.jpg"
        return private

    def load(self, release_id: str, cover_id: str) -> dict[str, object]:
        release = self._release(release_id, metadata_only=True)
        return self._load_asset(release, cover_id)

    def catalog(self, release_id: str | None = None, offset: int = 0) -> dict[str, object]:
        """分页提供已有封面预览；每页最多五组，不重新提帧。"""
        if type(offset) is not int or not 0 <= offset <= 10000:
            raise CoverError("图库分页偏移无效")
        releases = ([self._release(release_id, metadata_only=True)] if release_id is not None
                    else self.release_store.list_metadata())
        identities: set[tuple[str, str]] = set()
        for release in releases:
            rid = str(release["release_id"])
            root = self.covers_dir / rid
            if not os.path.lexists(root):
                continue
            _assert_private_directory(root)
            for parent in [root, *(root / name for name in self.CATEGORIES.values())]:
                if not os.path.lexists(parent):
                    continue
                _assert_private_directory(parent)
                for entry in parent.iterdir():
                    if _HEX_DIGEST.fullmatch(entry.name):
                        identities.add((rid, entry.name))
        ordered = sorted(identities)
        result = []
        for rid, cid in ordered[offset:offset + 5]:
            private = self.load(rid, cid)
            result.append(self._public_dict(private, source=str(private["source"]), time=private["time"]))
        next_offset = offset + len(result)
        return {"covers": result, "total": len(ordered),
                "next_offset": next_offset if next_offset < len(ordered) else None}

    def reuse(self, release_id: str, from_release_id: str, cover_id: str) -> dict[str, object]:
        """校验已入库封面，再绑定到目标作品，不沿用源作品身份。"""
        source = self.load(from_release_id, cover_id)
        if source.get("layout") == "paired":
            references = source.get("references")
            if not isinstance(references, dict):
                raise CoverError("配对封面来源引用无效")
            return self.pair(
                release_id,
                str(references["landscape_cover_id"]),
                str(references["portrait_cover_id"]),
                from_release_id=str(references["release_id"]),
            )
        contents = _read_regular_file(source["source_path"], MAX_SOURCE_BYTES, private=True)
        return self._install(self._release(release_id), contents, source="library_image", time=None)

    def pair(
        self,
        release_id: str,
        landscape_cover_id: str,
        portrait_cover_id: str,
        from_release_id: str | None = None,
    ) -> dict[str, object]:
        """将已校验的原始 4:3、3:4 单图组合成一个配对封面。"""
        release = self._release(release_id)
        source_release_id = (
            validate_release_id(from_release_id)
            if from_release_id is not None
            else str(release["release_id"])
        )
        source_release = self._release(source_release_id, metadata_only=True)
        landscape = self._load_asset(source_release, landscape_cover_id, allow_pair=False)
        portrait = self._load_asset(source_release, portrait_cover_id, allow_pair=False)
        landscape_source_path = landscape.get("source_path")
        portrait_source_path = portrait.get("source_path")
        if not isinstance(landscape_source_path, Path) or not isinstance(portrait_source_path, Path):
            raise CoverError("配对封面只能使用已入库的原始单图")
        landscape_source = _decode_rgb(
            _read_regular_file(landscape_source_path, MAX_SOURCE_BYTES, private=True),
            max_pixels=MAX_PIXELS,
        )
        portrait_source = _decode_rgb(
            _read_regular_file(portrait_source_path, MAX_SOURCE_BYTES, private=True),
            max_pixels=MAX_PIXELS,
        )
        if landscape_source.width * 3 != landscape_source.height * 4:
            raise CoverError("横版封面原图必须为严格 4:3")
        if portrait_source.width * 4 != portrait_source.height * 3:
            raise CoverError("竖版封面原图必须为严格 3:4")
        landscape_jpeg = _read_regular_file(
            landscape["landscape_path"], 16 * 1024 * 1024, private=True
        )
        portrait_jpeg = _read_regular_file(
            portrait["portrait_path"], 16 * 1024 * 1024, private=True
        )
        references = {
            "release_id": source_release_id,
            "landscape_cover_id": str(landscape["cover_id"]),
            "portrait_cover_id": str(portrait["cover_id"]),
        }
        cover_sha256 = _paired_cover_sha256(
            str(landscape["cover_sha256"]), str(portrait["cover_sha256"])
        )
        return self._install_pair(
            release,
            landscape_jpeg,
            portrait_jpeg,
            references=references,
            cover_sha256=cover_sha256,
        )


    def _install_pair(
        self,
        release: dict[str, object],
        landscape_jpeg: bytes,
        portrait_jpeg: bytes,
        *,
        references: dict[str, str],
        cover_sha256: str,
    ) -> dict[str, object]:
        release_id = str(release["release_id"])
        video_sha256 = str(release["video_sha256"])
        if _HEX_DIGEST.fullmatch(cover_sha256) is None:
            raise CoverError("配对封面摘要无效")
        cover_id = _cover_identity(release_id, video_sha256, cover_sha256)
        release_dir = self._ensure_layout(release_id)
        existing_dir = self._asset_directory(release_id, cover_id)
        if existing_dir is not None:
            existing = self._load_asset(release, cover_id)
            if existing.get("layout") != "paired" or existing["cover_sha256"] != cover_sha256:
                raise CoverError("拒绝覆盖同一身份下的不同封面资产")
            return self._public_dict(existing, source="paired_images", time=None)

        category_dir = release_dir / self.CATEGORIES["paired_images"]
        _ensure_private_directory(category_dir)
        destination = category_dir / cover_id
        try:
            os.lstat(destination)
        except FileNotFoundError:
            pass
        except OSError as exc:
            raise CoverError("无法检查配对封面资产目录") from exc
        else:
            existing = self._load_asset(release, cover_id)
            if existing.get("layout") != "paired" or existing["cover_sha256"] != cover_sha256:
                raise CoverError("拒绝覆盖同一身份下的不同封面资产")
            return self._public_dict(existing, source="paired_images", time=None)

        assets = {
            "landscape.jpg": landscape_jpeg,
            "portrait.jpg": portrait_jpeg,
        }
        manifest = {
            "schema_version": 2,
            "release_id": release_id,
            "cover_id": cover_id,
            "cover_sha256": cover_sha256,
            "video_sha256": video_sha256,
            "files": {name: _digest(contents) for name, contents in assets.items()},
            "source": "paired_images",
            "time": None,
            "references": dict(references),
        }
        manifest_bytes = json.dumps(manifest, separators=(",", ":"), sort_keys=True).encode("utf-8")
        temporary = Path(tempfile.mkdtemp(prefix=".tmp-cover-", dir=category_dir))
        if os.name == "posix":
            os.chmod(temporary, 0o700)
        try:
            for name, contents in assets.items():
                _write_private_file(temporary, name, contents)
            _write_private_file(temporary, _MANIFEST_NAME, manifest_bytes)
            if os.name == "posix":
                for path in temporary.iterdir():
                    os.chmod(path, 0o600, follow_symlinks=False)
            try:
                os.rename(temporary, destination)
            except OSError as exc:
                try:
                    existing = self._load_asset(release, cover_id)
                except (FileNotFoundError, CoverError):
                    raise CoverError("无法原子安装配对封面资产") from exc
                if existing.get("layout") != "paired" or existing["cover_sha256"] != cover_sha256:
                    raise CoverError("拒绝覆盖同一身份下的不同封面资产") from exc
                return self._public_dict(existing, source="paired_images", time=None)
            private = self._load_asset(release, cover_id)
            return self._public_dict(private, source="paired_images", time=None)
        finally:
            if temporary.exists():
                shutil.rmtree(temporary)


    def _install(
        self,
        release: dict[str, object],
        source_jpeg: bytes,
        *,
        source: str,
        time: float | None,
    ) -> dict[str, object]:
        release_id = str(release["release_id"])
        video_sha256 = str(release["video_sha256"])
        cover_sha256 = _digest(source_jpeg)
        cover_id = _cover_identity(release_id, video_sha256, cover_sha256)
        release_dir = self._ensure_layout(release_id)
        existing_dir = self._asset_directory(release_id, cover_id)
        if existing_dir is not None:
            existing = self._load_asset(release, cover_id)
            # 提帧响应保留本次请求时刻；其他重复导入沿用已持久化来源。
            return self._public_dict(existing,
                                     source=source if source == "video_frame" else str(existing["source"]),
                                     time=time if source == "video_frame" else existing["time"])
        category_dir = release_dir / self.CATEGORIES[source]
        _ensure_private_directory(category_dir)
        destination = category_dir / cover_id

        try:
            os.lstat(destination)
        except FileNotFoundError:
            pass
        except OSError as exc:
            raise CoverError("无法检查封面资产目录") from exc
        else:
            existing = self._load_asset(release, cover_id)
            if existing["cover_sha256"] != cover_sha256 or existing["video_sha256"] != video_sha256:
                raise CoverError("拒绝覆盖同一身份下的不同封面资产")
            return self._public_dict(
                existing,
                source=source if source == "video_frame" else str(existing["source"]),
                time=time if source == "video_frame" else existing["time"],
            )

        source_image = _decode_rgb(source_jpeg, max_pixels=MAX_PIXELS)
        covers = _cover_jpegs(source_image)
        assets = {
            "source.jpg": source_jpeg,
            **{f"{name}.jpg": contents for name, contents in covers.items()},
        }
        manifest = {
            "schema_version": 1,
            "release_id": release_id,
            "cover_id": cover_id,
            "cover_sha256": cover_sha256,
            "video_sha256": video_sha256,
            "files": {name: _digest(contents) for name, contents in assets.items()},
            "source": source,
            "time": time,
        }
        manifest_bytes = json.dumps(manifest, separators=(",", ":"), sort_keys=True).encode("utf-8")
        temporary = Path(tempfile.mkdtemp(prefix=".tmp-cover-", dir=category_dir))
        if os.name == "posix":
            os.chmod(temporary, 0o700)
        try:
            for name, contents in assets.items():
                _write_private_file(temporary, name, contents)
            _write_private_file(temporary, _MANIFEST_NAME, manifest_bytes)
            if os.name == "posix":
                for path in temporary.iterdir():
                    os.chmod(path, 0o600, follow_symlinks=False)
            try:
                os.rename(temporary, destination)
            except OSError as exc:
                try:
                    existing = self._load_asset(release, cover_id)
                except (FileNotFoundError, CoverError):
                    raise CoverError("无法原子安装封面资产") from exc
                if existing["cover_sha256"] != cover_sha256 or existing["video_sha256"] != video_sha256:
                    raise CoverError("拒绝覆盖同一身份下的不同封面资产") from exc
                return self._public_dict(
                    existing,
                    source=source if source == "video_frame" else str(existing["source"]),
                    time=time if source == "video_frame" else existing["time"],
                )
            private = self._load_asset(release, cover_id)
            return self._public_dict(private, source=source, time=time)
        finally:
            if temporary.exists():
                shutil.rmtree(temporary)

    def import_image(
        self,
        release_id: str,
        image_bytes: bytes,
        *,
        source: str = "qq_image",
    ) -> dict[str, object]:
        if not isinstance(source, str) or source not in {"qq_image", "saved_image"}:
            raise CoverError("导入封面来源无效")
        release = self._release(release_id)
        source_jpeg = _normalize_image(image_bytes, max_bytes=MAX_IMAGE_BYTES)
        return self._install(release, source_jpeg, source=source, time=None)

    def frames(
        self,
        release_id: str,
        times: Sequence[float] | None = None,
    ) -> list[dict[str, object]]:
        release = self._release(release_id)
        duration = float(release["duration"])
        if times is None:
            selected_times = [duration * fraction for fraction in (0.25, 0.5, 0.75)]
        else:
            if isinstance(times, (str, bytes, bytearray)):
                raise CoverError("times 必须是有限数字序列")
            try:
                selected_times = list(times)
            except TypeError as exc:
                raise CoverError("times 必须是有限数字序列") from exc
            if len(selected_times) > 5:
                raise CoverError("最多提取 5 个视频帧")
        normalized_times: list[float] = []
        for value in selected_times:
            if not _finite_number(value):
                raise CoverError("视频帧时间必须是有限数字")
            selected = float(value)
            if not 0 <= selected < duration:
                raise CoverError("视频帧时间必须位于原片时长内")
            normalized_times.append(selected)

        results: list[dict[str, object]] = []
        for selected in normalized_times:
            with tempfile.TemporaryDirectory(prefix="douyin-cover-frame-") as temporary_dir:
                frame_path = Path(temporary_dir) / "frame.jpg"
                command = [
                    "ffmpeg",
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-ss",
                    format(selected, ".9g"),
                    "-i",
                    os.fspath(release["video_path"]),
                    "-map",
                    "0:v:0",
                    "-vf",
                    "scale=4000:4000:force_original_aspect_ratio=decrease",
                    "-frames:v",
                    "1",
                    "-q:v",
                    "5",
                    os.fspath(frame_path),
                ]
                subprocess.run(
                    command,
                    shell=False,
                    check=True,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    timeout=FFMPEG_TIMEOUT_SECONDS,
                )
                frame_bytes = _read_regular_file(frame_path, MAX_FRAME_BYTES)
            source_jpeg = _normalize_image(frame_bytes, max_bytes=MAX_FRAME_BYTES)
            results.append(
                self._install(release, source_jpeg, source="video_frame", time=selected)
            )
        return results


__all__ = ["CoverError", "CoverStore"]
