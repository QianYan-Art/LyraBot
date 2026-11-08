"""抖音编辑器设置与封面控件的失败关闭操作。"""

from __future__ import annotations

import asyncio
from datetime import datetime, timedelta, timezone
import hashlib
from pathlib import Path
import re
from time import monotonic
from zoneinfo import ZoneInfo


_SHANGHAI = ZoneInfo("Asia/Shanghai")
_VISIBILITY_LABELS = {
    "public": "公开",
    "friends": "好友可见",
    "private": "仅自己可见",
}
_SCHEDULE_NAME = re.compile(r"(?=.*(?:发布时间|日期|时间)).{1,40}")
_SCHEDULE_PLACEHOLDER = re.compile(
    r"(?=.*(?:发布时间|日期|时间|[Yy]{4}|\d{4})).{1,60}"
)
_ISO_WITH_OFFSET = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}"
    r"(?::\d{2}(?:\.\d{1,6})?)?(?:Z|[+-]\d{2}:?\d{2})$"
)
_COVER_MODAL_ACCEPT = "image/png,image/jpeg,image/jpg,image/bmp,image/webp,image/tif"


def validate_settings(
    visibility: str = "public",
    publish_at: str | None = None,
    *,
    now: datetime | None = None,
) -> dict[str, str | None]:
    """校验可见范围及可选的带时区定时发布时间。"""
    normalized, publish_utc = _normalize_settings(visibility, publish_at)
    if publish_utc is None:
        return normalized

    if now is not None and not isinstance(now, datetime):
        raise ValueError("now 必须是带时区的 datetime")
    current = now if now is not None else datetime.now(timezone.utc)
    if current.tzinfo is None or current.utcoffset() is None:
        raise ValueError("now 必须是带时区的 datetime")
    current_utc = current.astimezone(timezone.utc)
    if publish_utc < current_utc + timedelta(hours=2):
        raise ValueError("定时发布时间必须至少晚于当前时间 2 小时")
    if publish_utc > current_utc + timedelta(days=14):
        raise ValueError("定时发布时间不能超过当前时间 14 天")
    return normalized


def _normalize_settings(
    visibility: str, publish_at: str | None
) -> tuple[dict[str, str | None], datetime | None]:
    if not isinstance(visibility, str) or visibility not in _VISIBILITY_LABELS:
        raise ValueError("visibility 仅支持 public、friends 或 private")
    if publish_at is None:
        return (
            {"visibility": visibility, "publish_at": None, "display_time": None},
            None,
        )
    if not isinstance(publish_at, str) or not _ISO_WITH_OFFSET.fullmatch(publish_at):
        raise ValueError("publish_at 必须是带时区的 ISO8601 日期时间")
    try:
        parsed = datetime.fromisoformat(
            publish_at[:-1] + "+00:00" if publish_at.endswith("Z") else publish_at
        )
    except ValueError as exc:
        raise ValueError("publish_at 不是有效的 ISO8601 日期时间") from exc
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise ValueError("publish_at 必须包含时区偏移")
    publish_utc = parsed.astimezone(timezone.utc)
    return {
        "visibility": visibility,
        "publish_at": publish_utc.isoformat(),
        "display_time": publish_utc.astimezone(_SHANGHAI).strftime("%Y-%m-%d %H:%M"),
    }, publish_utc


def _settings_for_readback(settings: dict) -> dict[str, str | None]:
    if not isinstance(settings, dict):
        raise ValueError("settings 必须是 validate_settings 返回的字典")
    normalized, _ = _normalize_settings(
        settings.get("visibility"), settings.get("publish_at")
    )
    if settings.get("display_time") != normalized["display_time"]:
        raise ValueError("settings.display_time 与 publish_at 不一致")
    return normalized


async def _unique_visible(locator, description: str):
    count = await locator.count()
    if count > 20:
        raise ValueError(f"{description}候选数量过多，无法安全核实")
    matches = []
    for index in range(min(count, 20)):
        item = locator.nth(index) if count > 1 else locator
        if await item.is_visible():
            matches.append(item)
    if len(matches) != 1:
        raise ValueError(f"{description}无法唯一定位（可见数量 {len(matches)}）")
    return matches[0]


async def _option_control(page, label_text: str):
    label = page.locator("label:visible").filter(
        has_text=re.compile(r"^\s*" + re.escape(label_text) + r"\s*$")
    )
    label = await _unique_visible(label, f"单选项“{label_text}”")
    control = label.locator('input[type="radio"], input[type="checkbox"]')
    if await control.count() != 1:
        raise ValueError(f"单选项“{label_text}”未包含唯一 radio 或 checkbox input")
    input_type = await control.get_attribute("type")
    if input_type not in {"radio", "checkbox"}:
        raise ValueError(f"单选项“{label_text}”的 input 类型无法核实")
    return label, control


async def _option_group_state(page, labels: tuple[str, ...]) -> dict[str, bool]:
    states = {}
    for label_text in labels:
        _, control = await _option_control(page, label_text)
        states[label_text] = await control.is_checked()
    return states


async def _set_option_group(page, labels: tuple[str, ...], selected_label: str) -> None:
    label, control = await _option_control(page, selected_label)
    if not await control.is_checked():
        await label.click()
    if not await control.is_checked():
        raise ValueError(f"单选项“{selected_label}”点击后未选中")
    states = await _option_group_state(page, labels)
    if states != {name: name == selected_label for name in labels}:
        raise ValueError("设置选项组读回状态与目标不一致")


async def _verify_option_group(page, labels: tuple[str, ...], selected_label: str) -> None:
    states = await _option_group_state(page, labels)
    if states != {name: name == selected_label for name in labels}:
        raise ValueError("页面当前选项组与预期设置不一致")


async def _schedule_field(page):
    by_role = page.get_by_role("textbox", name=_SCHEDULE_NAME)
    role_matches = []
    role_count = await by_role.count()
    if role_count > 20:
        raise ValueError("定时发布时间文本框候选数量过多，无法安全核实")
    for index in range(role_count):
        item = by_role.nth(index)
        if await item.is_visible():
            role_matches.append(item)
    if len(role_matches) == 1:
        return role_matches[0]
    if len(role_matches) > 1:
        raise ValueError("定时发布时间文本框不唯一")

    by_placeholder = page.get_by_placeholder(_SCHEDULE_PLACEHOLDER)
    placeholder_matches = []
    placeholder_count = await by_placeholder.count()
    if placeholder_count > 20:
        raise ValueError("定时发布时间 placeholder 候选数量过多，无法安全核实")
    for index in range(placeholder_count):
        item = by_placeholder.nth(index)
        if await item.is_visible():
            placeholder_matches.append(item)
    if len(placeholder_matches) != 1:
        raise ValueError(
            "无法通过可见的文本框名称或 placeholder 唯一定位定时发布时间输入框"
        )
    return placeholder_matches[0]


def _local_input_value(local_time: datetime, input_type: str) -> str:
    timespec = "minutes" if local_time.second == 0 and local_time.microsecond == 0 else "auto"
    if input_type == "datetime-local":
        return local_time.isoformat(timespec=timespec)
    if input_type in ("text", ""):
        return local_time.isoformat(sep=" ", timespec=timespec)
    raise ValueError(f"不支持的定时发布时间输入类型：{input_type or '未知'}")


def _parse_local_input(value: str) -> datetime:
    try:
        parsed = datetime.fromisoformat(value.strip().replace(" ", "T", 1))
    except (AttributeError, ValueError) as exc:
        raise ValueError("定时发布时间输入框读回值不是有效日期时间") from exc
    if parsed.tzinfo is not None:
        return parsed.astimezone(_SHANGHAI).replace(tzinfo=None)
    return parsed


async def apply_settings(page, settings: dict) -> dict[str, str | None]:
    """设置编辑器选项，并核实页面读回值。"""
    normalized = _settings_for_readback(settings)
    normalized = validate_settings(
        visibility=normalized["visibility"], publish_at=normalized["publish_at"]
    )

    visibility_labels = tuple(_VISIBILITY_LABELS.values())
    visibility_label = _VISIBILITY_LABELS[normalized["visibility"]]
    await _set_option_group(page, visibility_labels, visibility_label)
    publish_labels = ("立即发布", "定时发布")
    publish_label = "立即发布" if normalized["publish_at"] is None else "定时发布"
    await _set_option_group(page, publish_labels, publish_label)

    if normalized["publish_at"] is not None:
        expected_local = datetime.fromisoformat(
            normalized["publish_at"]
        ).astimezone(_SHANGHAI).replace(tzinfo=None)
        field = await _schedule_field(page)
        input_type = (await field.get_attribute("type") or "").lower()
        await field.fill(_local_input_value(expected_local, input_type))
        readback = _parse_local_input(await field.input_value())
        if readback != expected_local:
            raise ValueError("定时发布时间输入框读回值与设置不一致")

    return normalized


async def verify_settings(page, settings: dict) -> dict[str, str | None]:
    """只读核实编辑器选项仍与暂存设置一致。"""
    normalized = _settings_for_readback(settings)
    visibility_labels = tuple(_VISIBILITY_LABELS.values())
    visibility_label = _VISIBILITY_LABELS[normalized["visibility"]]
    await _verify_option_group(page, visibility_labels, visibility_label)
    publish_labels = ("立即发布", "定时发布")
    publish_label = "立即发布" if normalized["publish_at"] is None else "定时发布"
    await _verify_option_group(page, publish_labels, publish_label)

    if normalized["publish_at"] is not None:
        expected_local = datetime.fromisoformat(
            normalized["publish_at"]
        ).astimezone(_SHANGHAI).replace(tzinfo=None)
        field = await _schedule_field(page)
        readback = _parse_local_input(await field.input_value())
        if readback != expected_local:
            raise ValueError("页面定时发布时间读回值与预期设置不一致")
    return normalized


async def _orientation_card(page, orientation: str, *, require_action: bool = True):
    title = "横封面4:3" if orientation == "landscape" else "竖封面3:4"
    other_title = "竖封面3:4" if orientation == "landscape" else "横封面4:3"
    action_pattern = re.compile(r"^(?:选择封面|编辑封面)$")
    anchor = page.get_by_text(title, exact=True)
    anchor = await _unique_visible(anchor, f"封面卡片标签“{title}”")

    async def unique_visible_action(scope):
        candidates = scope.get_by_text(action_pattern)
        count = await candidates.count()
        if count > 20:
            raise ValueError(f"“{title}”封面卡片操作候选过多")
        visible = []
        for index in range(count):
            candidate = candidates.nth(index)
            if await candidate.is_visible():
                visible.append(candidate)
        if len(visible) > 1:
            raise ValueError(f"“{title}”封面卡片的选择/编辑操作不唯一")
        return visible[0] if visible else None

    scope = anchor
    for _ in range(3):
        scope = scope.locator("..")
        tag = await scope.evaluate("element => element.tagName")
        if str(tag).upper() in {"BODY", "HTML"}:
            break
        text = await scope.inner_text()
        if len(text) > 1000 or title not in text or other_title in text:
            continue

        if not require_action:
            if await _image_sources(scope):
                return scope, None
            continue

        action = await unique_visible_action(scope)
        if action is not None:
            return scope, action

        await scope.hover()
        deadline = monotonic() + 1
        while True:
            action = await unique_visible_action(scope)
            if action is not None:
                return scope, action
            remaining = deadline - monotonic()
            if remaining <= 0:
                break
            await asyncio.sleep(min(0.05, remaining))
    raise ValueError(f"无法在短祖先范围内唯一定位“{title}”的选择封面按钮")


async def _image_sources(card) -> list[str]:
    descendants = card.locator("*")
    descendant_count = await descendants.count()
    if descendant_count > 99:
        raise ValueError("封面卡片节点数量超过 100，无法安全核实")
    if not await card.is_visible():
        return []

    sources = []
    images = card.locator("img")
    image_count = await images.count()
    for index in range(image_count):
        image = images.nth(index)
        if await image.is_visible():
            source = await image.get_attribute("src")
            if source:
                sources.append(source)

    for element in [card, *(descendants.nth(index) for index in range(descendant_count))]:
        if not await element.is_visible():
            continue
        background = await element.evaluate(
            "element => getComputedStyle(element).backgroundImage"
        )
        if (
            isinstance(background, str)
            and background.strip().lower() != "none"
            and "url(" in background.lower()
        ):
            sources.append(background)

    return list(dict.fromkeys(sources))


def _safe_source_summary(source: str) -> str:
    return "sha256:" + hashlib.sha256(source.encode("utf-8")).hexdigest()


async def _visible_cover_dialog(page):
    deadline = monotonic() + 10
    while True:
        dialogs = page.get_by_role("dialog")
        visible_dialogs = []
        dialog_count = await dialogs.count()
        if dialog_count > 10:
            raise ValueError("封面确认弹窗候选数量过多，无法安全核实")
        for index in range(dialog_count):
            dialog = dialogs.nth(index)
            if await dialog.is_visible() and "封面" in await dialog.inner_text():
                visible_dialogs.append(dialog)
        if len(visible_dialogs) > 1:
            raise ValueError("无法唯一定位可见的封面确认弹窗")
        if visible_dialogs:
            return visible_dialogs[0]
        remaining = deadline - monotonic()
        if remaining <= 0:
            raise ValueError("等待封面确认弹窗超时")
        await asyncio.sleep(min(0.1, remaining))


async def _visible_dialogs(page):
    visible = []
    for role in ("dialog", "alertdialog"):
        candidates = page.get_by_role(role)
        count = await candidates.count()
        if count > 10:
            raise ValueError("确认后弹窗候选数量过多，无法安全核实")
        for index in range(count):
            candidate = candidates.nth(index)
            if await candidate.is_visible():
                visible.append(candidate)
    return visible


async def _visible_unique_button_or_none(scope, name: str):
    candidates = scope.get_by_role("button", name=name, exact=True)
    count = await candidates.count()
    if count > 10:
        raise ValueError(f"竖封面推荐弹窗的“{name}”按钮候选过多")
    visible = []
    for index in range(count):
        candidate = candidates.nth(index)
        if await candidate.is_visible():
            visible.append(candidate)
    if len(visible) > 1:
        raise ValueError(f"竖封面推荐弹窗的“{name}”按钮不唯一")
    return visible[0] if visible else None


async def _dismiss_portrait_recommendation(dialog, timeout_ms: int) -> None:
    heading_text = "设置竖封面获更多流量"
    heading = await _unique_visible(
        dialog.get_by_text(heading_text, exact=True),
        "竖封面推荐标题",
    )
    scope = heading
    for _ in range(5):
        scope = scope.locator("..")
        tag = await scope.evaluate("element => element.tagName")
        if str(tag).upper() in {"BODY", "HTML"}:
            break
        text = await scope.inner_text()
        if heading_text not in text:
            continue
        if len(text) > 1500:
            raise ValueError("竖封面推荐弹窗范围超过安全文本长度")
        defer = await _visible_unique_button_or_none(scope, "暂不设置")
        set_portrait = await _visible_unique_button_or_none(scope, "设置竖封面")
        if defer is not None and set_portrait is not None:
            await defer.click()
            await dialog.wait_for(state="hidden", timeout=timeout_ms)
            return
    raise ValueError("无法在短范围内唯一核实竖封面推荐弹窗的两个按钮")


async def _handle_post_confirmation_dialogs(page, cover_dialog, cover_dialog_handle) -> None:
    heading_text = "设置竖封面获更多流量"
    deadline = monotonic() + 10
    recommendation_dismissed = False

    while True:
        recommendation_dialogs = []
        for candidate in await _visible_dialogs(page):
            handles = await candidate.element_handles()
            if len(handles) != 1:
                continue
            handle = handles[0]
            if not await handle.is_visible():
                continue
            snapshot = await handle.evaluate(
                "(element, original) => ({"
                "same: element === original, "
                "connected: element.isConnected, "
                "text: element.innerText"
                "})",
                cover_dialog_handle,
            )
            if not snapshot["connected"]:
                continue
            same_as_cover = snapshot["same"]
            text = snapshot["text"]
            if heading_text in text:
                recommendation_dialogs.append(candidate)
            elif same_as_cover and (
                "设置横封面" in text or "设置竖封面" in text
            ):
                continue
            elif same_as_cover:
                raise ValueError("确认封面后原弹窗出现未识别内容")
            else:
                raise ValueError("确认封面后出现未识别的对话框")

        if len(recommendation_dialogs) > 1:
            raise ValueError("竖封面推荐对话框不唯一")
        if recommendation_dialogs:
            if recommendation_dismissed:
                raise ValueError("关闭竖封面推荐后再次出现推荐对话框")
            remaining = deadline - monotonic()
            if remaining <= 0:
                raise ValueError("等待竖封面推荐提示超过 10 秒")
            timeout_ms = max(1, int(remaining * 1000))
            await _dismiss_portrait_recommendation(
                recommendation_dialogs[0],
                timeout_ms,
            )
            recommendation_dismissed = True
            continue

        if not await cover_dialog.is_visible():
            return
        remaining = deadline - monotonic()
        if remaining <= 0:
            raise ValueError("点击封面完成后原弹窗未在 10 秒内关闭")
        await asyncio.sleep(min(0.1, remaining))


async def _confirm_cover_dialog(page) -> None:
    dialog = await _visible_cover_dialog(page)
    dialog_handle = await dialog.element_handle()
    if dialog_handle is None:
        raise ValueError("封面确认弹窗已脱离页面，无法安全确认")

    confirm = None
    for name in ("完成", "确定"):
        candidate = dialog.get_by_role("button", name=name, exact=True)
        count = await candidate.count()
        if count == 1 and await candidate.is_visible():
            if confirm is not None:
                raise ValueError("封面确认弹窗存在多个可用确认按钮")
            confirm = candidate
        elif count > 1:
            raise ValueError(f"封面确认弹窗的“{name}”按钮不唯一")
    if confirm is None:
        raise ValueError("封面确认弹窗缺少唯一的“完成”或“确定”按钮")

    await confirm.click()
    await _handle_post_confirmation_dialogs(page, dialog, dialog_handle)
    await dialog.wait_for(state="hidden", timeout=10000)


async def _wait_for_unique_thumbnail_change(
    card, before: list[str], timeout: float = 10
) -> str:
    deadline = monotonic() + timeout
    previous = None
    stable_count = 0
    while True:
        after = await _image_sources(card)
        changed = [source for source in after if source not in before]
        if len(changed) == 1:
            if changed[0] == previous:
                stable_count += 1
            else:
                previous = changed[0]
                stable_count = 1
            if stable_count >= 2:
                return changed[0]
        else:
            previous = None
            stable_count = 0

        remaining = deadline - monotonic()
        if remaining <= 0:
            raise ValueError("封面缩略图未在 10 秒内变为唯一且稳定的变化图")
        await asyncio.sleep(min(0.1, remaining))


async def _choose_cover_file(page, dialog, path: Path) -> None:
    anchor = None
    for text in ("点击上传文件", "点击上传文件或拖拽文件到这里"):
        candidate = dialog.get_by_text(text, exact=True)
        try:
            anchor = await _unique_visible(candidate, "封面上传操作区")
        except ValueError:
            continue
        break
    if anchor is None:
        raise ValueError("无法唯一定位封面弹窗内的上传操作区")

    try:
        async with page.expect_file_chooser(timeout=10000) as pending:
            await anchor.click()
        chooser = await pending.value
    except Exception as exc:
        raise ValueError("点击封面上传操作区后未能可靠触发文件选择器") from exc

    input_element = chooser.element
    input_type = await input_element.get_attribute("type")
    accept = await input_element.get_attribute("accept")
    if input_type != "file" or accept != _COVER_MODAL_ACCEPT:
        raise ValueError("封面文件选择器控件与已观察的图片输入框不一致")

    await chooser.set_files(str(path))
    selected_name = await input_element.evaluate(
        "element => element.files && element.files.length === 1 ? element.files[0].name : ''"
    )
    if selected_name != path.name:
        raise ValueError("封面文件选择器读回的文件名与目标 JPEG 不一致")


async def _apply_cover_orientation(page, orientation: str, path: Path) -> dict[str, str]:
    card, choose_button = await _orientation_card(page, orientation)
    before = await _image_sources(card)
    await choose_button.click()

    dialog = await _visible_cover_dialog(page)
    await _choose_cover_file(page, dialog, path)
    await _confirm_cover_dialog(page)
    changed = await _wait_for_unique_thumbnail_change(card, before)
    return {
        "orientation": orientation,
        "thumbnail": _safe_source_summary(changed),
    }


async def apply_cover(page, asset: dict) -> dict[str, object]:
    """应用两张 JPEG 封面，并核实编辑器中的缩略图变化。"""
    if not isinstance(asset, dict):
        raise ValueError("asset 必须是 CoverStore.load 返回的字典")
    cover_hash = asset.get("cover_sha256")
    if not isinstance(cover_hash, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", cover_hash):
        raise ValueError("asset.cover_sha256 格式无效")

    paths = {}
    for orientation, key in (
        ("landscape", "landscape_path"),
        ("portrait", "portrait_path"),
    ):
        raw_path = asset.get(key)
        if not isinstance(raw_path, (str, Path)):
            raise ValueError(f"asset.{key} 缺失或无效")
        path = Path(raw_path)
        if path.suffix.lower() not in {".jpg", ".jpeg"} or not path.is_file():
            raise ValueError(f"asset.{key} 必须指向存在的 JPEG 文件")
        paths[orientation] = path

    results = []
    for orientation in ("landscape", "portrait"):
        results.append(
            await _apply_cover_orientation(page, orientation, paths[orientation])
        )
    return {"cover_sha256": cover_hash.lower(), "covers": results, "verified": True}


async def verify_cover(page, asset_or_evidence: dict) -> dict[str, object]:
    """只读比对封面缩略图与暂存阶段产生的 SHA-256 证据。"""
    if not isinstance(asset_or_evidence, dict):
        raise ValueError("封面核验需要 apply_cover 返回的 evidence")
    evidence = (
        asset_or_evidence.get("editor_evidence")
        or asset_or_evidence.get("evidence")
        or asset_or_evidence
    )
    if not isinstance(evidence, dict) or not isinstance(evidence.get("covers"), list):
        raise ValueError("CoverStore 原始 asset 无法证明页面封面；需要 apply_cover 的 evidence")

    cover_hash = evidence.get("cover_sha256")
    if not isinstance(cover_hash, str) or not re.fullmatch(r"[0-9a-fA-F]{64}", cover_hash):
        raise ValueError("封面 evidence 的 cover_sha256 格式无效")
    asset_hash = asset_or_evidence.get("cover_sha256")
    if asset_hash is not None:
        if not isinstance(asset_hash, str) or not re.fullmatch(
            r"[0-9a-fA-F]{64}", asset_hash
        ):
            raise ValueError("asset.cover_sha256 格式无效")
        if asset_hash.lower() != cover_hash.lower():
            raise ValueError("封面 evidence 与 asset 的 cover_sha256 不一致")

    expected_by_orientation = {}
    for item in evidence["covers"]:
        if not isinstance(item, dict):
            raise ValueError("封面 evidence 项格式无效")
        orientation = item.get("orientation")
        thumbnail = item.get("thumbnail")
        if (
            not isinstance(orientation, str)
            or orientation not in {"landscape", "portrait"}
            or not isinstance(thumbnail, str)
        ):
            raise ValueError("封面 evidence 缺少方向或缩略图标识")
        expected = (
            thumbnail.lower()
            if re.fullmatch(r"sha256:[0-9a-fA-F]{64}", thumbnail)
            else _safe_source_summary(thumbnail)
        )
        if orientation in expected_by_orientation:
            raise ValueError("封面 evidence 中存在重复方向")
        expected_by_orientation[orientation] = expected
    if set(expected_by_orientation) != {"landscape", "portrait"}:
        raise ValueError("封面 evidence 必须同时包含横封面和竖封面")

    verified_covers = []
    for orientation in ("landscape", "portrait"):
        card, _ = await _orientation_card(page, orientation, require_action=False)
        current = {
            _safe_source_summary(source) for source in await _image_sources(card)
        }
        expected = expected_by_orientation[orientation]
        if expected not in current:
            raise ValueError(f"{orientation}封面缩略图与 staged evidence 不一致")
        verified_covers.append({"orientation": orientation, "thumbnail": expected})
    return {
        "cover_sha256": cover_hash.lower(),
        "covers": verified_covers,
        "verified": True,
    }
