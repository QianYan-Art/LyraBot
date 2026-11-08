#!/usr/bin/env python3
"""从固定母版生成可导入的双比例期号封面。"""

from __future__ import annotations

import argparse
import os
import re
import stat
import sys
import tempfile
import unicodedata
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont, UnidentifiedImageError


DEFAULT_FONT = Path(
    "/usr/share/fonts/truetype/maple-mono/MapleMono-NF-CN-Regular.ttf"
)
_RELEASE_ID_RE = re.compile(r"[a-z0-9][a-z0-9-]{0,63}", re.ASCII)
_TEMPLATE_SPECS = {
    "3x4": ((1086, 1448), (104, 195)),
    "4x3": ((1448, 1086), (392, 145)),
}
_MAX_TEMPLATE_BYTES = 32 * 1024 * 1024
_MAX_TEXT_CHARACTERS = 80
_TEXT_COLOR = (24, 55, 80, 242)
_FILE_ATTRIBUTE_REPARSE_POINT = getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400)


class CoverTemplateError(ValueError):
    """封面母版、文字或文件路径不符合安全约束。"""


def _validate_release_id(release_id: object) -> str:
    if (
        not isinstance(release_id, str)
        or _RELEASE_ID_RE.fullmatch(release_id) is None
        or release_id == "templates"
    ):
        raise CoverTemplateError("release-id 格式无效")
    return release_id


def _validate_single_line(name: str, value: object) -> str:
    if not isinstance(value, str) or not value.strip():
        raise CoverTemplateError(f"{name} 不能为空")
    if any(
        unicodedata.category(char) in {"Cc", "Cf", "Zl", "Zp"}
        for char in value
    ):
        raise CoverTemplateError(f"{name} 必须是单行且不能含控制字符")
    return value


def _is_within(root: Path, candidate: Path) -> bool:
    try:
        candidate.relative_to(root)
    except ValueError:
        return False
    return True


def _is_link_or_reparse_point(info: os.stat_result) -> bool:
    return stat.S_ISLNK(info.st_mode) or bool(
        getattr(info, "st_file_attributes", 0) & _FILE_ATTRIBUTE_REPARSE_POINT
    )


def _checked_child(
    root: Path,
    relative: Path,
    *,
    must_exist: bool,
    expected: str | None = None,
) -> Path:
    """检查母版或输出路径的每一级，拒绝软链接及根目录外路径。"""
    if relative.is_absolute() or not relative.parts or any(
        part in {".", ".."} for part in relative.parts
    ):
        raise CoverTemplateError("封面路径无效")

    candidate = root.joinpath(relative)
    current = root
    for index, part in enumerate(relative.parts):
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError:
            if must_exist:
                raise CoverTemplateError("封面模板或目录不存在") from None
            resolved = candidate.resolve(strict=False)
            if not _is_within(root, resolved):
                raise CoverTemplateError("封面路径越出 root")
            return candidate

        if _is_link_or_reparse_point(info):
            raise CoverTemplateError("封面路径不能经过软链接")
        if index < len(relative.parts) - 1 and not stat.S_ISDIR(info.st_mode):
            raise CoverTemplateError("封面路径的父级不是目录")

    resolved = candidate.resolve(strict=must_exist)
    if not _is_within(root, resolved):
        raise CoverTemplateError("封面路径越出 root")

    info = candidate.lstat()
    if expected == "dir" and not stat.S_ISDIR(info.st_mode):
        raise CoverTemplateError("期号输出路径不是目录")
    if expected == "file" and not stat.S_ISREG(info.st_mode):
        raise CoverTemplateError("封面文件不是普通文件")
    return candidate


def _resolve_root(root_value: str | Path) -> Path:
    try:
        root = Path(root_value).expanduser().resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        raise CoverTemplateError("root 目录不存在或无法读取") from exc
    if not root.is_dir():
        raise CoverTemplateError("root 必须是目录")
    return root


def _load_template(root: Path, ratio: str) -> Image.Image:
    expected_size, _ = _TEMPLATE_SPECS[ratio]
    relative = Path("templates") / f"template_{ratio}.png"
    path = _checked_child(root, relative, must_exist=True, expected="file")
    try:
        if path.stat().st_size > _MAX_TEMPLATE_BYTES:
            raise CoverTemplateError("封面模板文件过大")
        with Image.open(path) as source:
            if source.format != "PNG" or source.size != expected_size:
                raise CoverTemplateError(
                    f"{relative.as_posix()} 必须是 {expected_size[0]}x{expected_size[1]} PNG"
                )
            if getattr(source, "n_frames", 1) != 1:
                raise CoverTemplateError("封面模板不能是动画图片")
            source.load()
            return source.convert("RGBA")
    except CoverTemplateError:
        raise
    except (OSError, ValueError, UnidentifiedImageError, Image.DecompressionBombError) as exc:
        raise CoverTemplateError(f"无法读取封面模板：{relative.as_posix()}") from exc


def _font(font_path: Path, size: int) -> ImageFont.FreeTypeFont:
    try:
        return ImageFont.truetype(
            str(font_path), size=size, layout_engine=ImageFont.Layout.BASIC
        )
    except (OSError, ValueError) as exc:
        raise CoverTemplateError("字体无法读取；请确认 --font 指向可用字体") from exc


def _render_png(
    root: Path,
    ratio: str,
    line: str,
    font_path: Path,
) -> bytes:
    image = _load_template(root, ratio)
    draw = ImageDraw.Draw(image, "RGBA")
    (width, height), (anchor_x, anchor_y) = _TEMPLATE_SPECS[ratio]
    scale_x = width / _TEMPLATE_SPECS[ratio][0][0]
    scale_y = height / _TEMPLATE_SPECS[ratio][0][1]
    x = round(anchor_x * scale_x)
    y = round(anchor_y * scale_y)

    margin = max(20, round(height * 0.035))
    if ratio == "3x4":
        safe_left = 0
        safe_right = width
    else:
        # 横版中央裁成 3:4 时，只保留中央区域内的文字。
        safe_width = height * 3 / 4
        safe_left = (width - safe_width) / 2
        safe_right = safe_left + safe_width
    available_width = int(safe_right - margin - x)
    if x < safe_left + margin or available_width <= 0:
        raise CoverTemplateError("模板文字锚点不在安全区域内")

    largest_size = round(height * 0.032)
    smallest_size = max(16, round(height * 0.02))
    chosen_font: ImageFont.FreeTypeFont | None = None
    for size in range(largest_size, smallest_size - 1, -1):
        candidate = _font(font_path, size)
        bounds = draw.textbbox((0, 0), line, font=candidate, anchor="lt")
        if bounds[2] - bounds[0] <= available_width and y + bounds[3] <= height - margin:
            chosen_font = candidate
            break
    if chosen_font is None:
        raise CoverTemplateError("文字过长，缩小后仍无法在安全区内清晰排成一行")

    draw.text((x, y), line, font=chosen_font, fill=_TEXT_COLOR, anchor="lt")
    output = BytesIO()
    image.save(output, format="PNG", optimize=False, compress_level=9)
    return output.getvalue()


def _file_identity(path: Path) -> tuple[int, int]:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise CoverTemplateError("临时封面文件不是普通文件")
    return info.st_dev, info.st_ino


def _unlink_if_owned(path: Path, temporary: Path, identity: tuple[int, int]) -> None:
    try:
        info = path.lstat()
        if (
            stat.S_ISREG(info.st_mode)
            and (info.st_dev, info.st_ino) == identity
            and os.path.samefile(path, temporary)
        ):
            path.unlink()
    except OSError:
        # 清理不能掩盖原始写入错误；临时目录仍由 finally 处理。
        return


def _stage_bytes(directory: Path, data: bytes) -> tuple[Path, tuple[int, int]]:
    descriptor, name = tempfile.mkstemp(
        prefix=".cover-template-", suffix=".tmp", dir=directory
    )
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
            info = os.fstat(stream.fileno())
            identity = (info.st_dev, info.st_ino)
        return temporary, identity
    except BaseException:
        try:
            os.close(descriptor)
        except OSError:
            pass
        try:
            temporary.unlink()
        except OSError:
            pass
        raise


def generate_cover_pair(
    root: str | Path,
    release_id: str,
    episode_label: str,
    subtitle: str,
    font_path: str | Path = DEFAULT_FONT,
) -> tuple[Path, Path]:
    """生成固定命名的 3:4、4:3 PNG，不接受自定义输出路径。"""
    release_id = _validate_release_id(release_id)
    episode_label = _validate_single_line("episode-label", episode_label)
    subtitle = _validate_single_line("subtitle", subtitle)
    line = f"{episode_label}｜{subtitle}"
    if len(line) > _MAX_TEXT_CHARACTERS:
        raise CoverTemplateError("文字过长，超过单行排字上限")

    root_path = _resolve_root(root)
    try:
        resolved_font = Path(font_path).expanduser().resolve(strict=True)
    except (OSError, RuntimeError, TypeError) as exc:
        raise CoverTemplateError("字体文件不存在或无法读取") from exc
    if not resolved_font.is_file():
        raise CoverTemplateError("--font 必须指向字体文件")

    # 先在内存完成两张图；模板、文字或字体无效时不会创建输出目录。
    rendered = {
        ratio: _render_png(root_path, ratio, line, resolved_font)
        for ratio in ("3x4", "4x3")
    }

    release_relative = Path(release_id)
    release_dir = root_path / release_relative
    created_directory = False
    temporary_files: list[tuple[Path, tuple[int, int]]] = []
    linked_files: list[tuple[Path, Path, tuple[int, int]]] = []
    success = False
    try:
        if os.path.lexists(release_dir):
            _checked_child(root_path, release_relative, must_exist=True, expected="dir")
        else:
            try:
                release_dir.mkdir(mode=0o700)
                created_directory = True
            except FileExistsError:
                pass
            _checked_child(root_path, release_relative, must_exist=True, expected="dir")

        outputs = {
            ratio: _checked_child(
                root_path,
                release_relative / f"{release_id}_{ratio}.png",
                must_exist=False,
            )
            for ratio in ("3x4", "4x3")
        }
        for output in outputs.values():
            if os.path.lexists(output):
                _checked_child(
                    root_path,
                    output.relative_to(root_path),
                    must_exist=True,
                    expected="file",
                )
                raise CoverTemplateError("目标封面已存在，拒绝覆盖")

        for ratio in ("3x4", "4x3"):
            temporary_files.append(_stage_bytes(release_dir, rendered[ratio]))

        for ratio, (temporary, identity) in zip(("3x4", "4x3"), temporary_files):
            output = outputs[ratio]
            _checked_child(
                root_path,
                temporary.relative_to(root_path),
                must_exist=True,
                expected="file",
            )
            if os.path.lexists(output):
                raise CoverTemplateError("目标封面已存在，拒绝覆盖")
            os.link(temporary, output)
            linked_files.append((output, temporary, identity))
            _checked_child(
                root_path,
                output.relative_to(root_path),
                must_exist=True,
                expected="file",
            )
            if _file_identity(output) != identity or not os.path.samefile(output, temporary):
                raise CoverTemplateError("封面写入后校验失败")

        success = True
        return outputs["3x4"], outputs["4x3"]
    except CoverTemplateError:
        raise
    except OSError as exc:
        raise CoverTemplateError("写入封面失败") from exc
    finally:
        if not success:
            for output, temporary, identity in reversed(linked_files):
                _unlink_if_owned(output, temporary, identity)
        cleanup_failed = False
        for temporary, _ in temporary_files:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass
            except OSError:
                cleanup_failed = True
        rollback_after_cleanup_failure = success and cleanup_failed
        if rollback_after_cleanup_failure:
            for output, temporary, identity in reversed(linked_files):
                _unlink_if_owned(output, temporary, identity)
            success = False
        if not success and created_directory:
            try:
                release_dir.rmdir()
            except OSError:
                pass
        if rollback_after_cleanup_failure:
            raise CoverTemplateError("封面已生成，但临时文件清理失败；已回滚输出")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="从序栞风格母版生成双比例期号封面")
    parser.add_argument(
        "--root", required=True, type=Path, help="含 templates/ 的运行目录"
    )
    parser.add_argument("--release-id", required=True, help="安全的期号目录名")
    parser.add_argument("--episode-label", required=True, help="封面期号标签")
    parser.add_argument("--subtitle", required=True, help="封面副标题")
    parser.add_argument("--font", type=Path, default=DEFAULT_FONT, help="字体文件路径")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        portrait, landscape = generate_cover_pair(
            args.root,
            args.release_id,
            args.episode_label,
            args.subtitle,
            args.font,
        )
    except CoverTemplateError as exc:
        print(f"错误：{exc}", file=sys.stderr)
        return 2

    print(f"{portrait} 1086x1448")
    print(f"{landscape} 1448x1086")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
