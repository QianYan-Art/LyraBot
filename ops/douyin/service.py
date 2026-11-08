#!/usr/bin/env python3
"""为抖音创作者后台提供受 Unix peer 凭据保护的本机登录服务。"""

from __future__ import annotations

import asyncio
import base64
import json
import logging
import os
import select
import signal
import socket
import stat
import struct
import sys
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Literal
from urllib.parse import urlsplit

LOGIN_URL = "https://creator.douyin.com/creator-micro/home"
MAX_REQUEST_BYTES = 4096
MAX_COVER_REQUEST_BYTES = 12 * 1024 * 1024
MAX_QR_BYTES = 2 * 1024 * 1024
REQUEST_TIMEOUT_SECONDS = 30
LOGIN_WAIT_SECONDS = 8
READ_TIMEOUT_SECONDS = 5
WRITE_TIMEOUT_SECONDS = 5
GATEWAY_UNIT = os.environ.get("DOUYIN_GATEWAY_UNIT", "qya.service")
GATEWAY_MAIN_PID_TIMEOUT_SECONDS = 1.0
BROWSER_IDLE_SECONDS = 180.0
BROWSER_CONTEXT_CLOSE_TIMEOUT_SECONDS = 15.0
PLAYWRIGHT_STOP_TIMEOUT_SECONDS = 5.0
MANUAL_WINDOW_GRACE_SECONDS = 15 * 60
MANUAL_WINDOW_RECHECK_SECONDS = 5.0
MANUAL_WINDOW_PASSWORD_FILE = Path("/run/tangyuge-douyin/manual.pass")
QR_SELECTORS = (
    'img[alt*="二维码"]',
    'img[aria-label*="二维码"]',
    'img[alt*="QR"]',
    'svg[aria-label*="二维码"]',
    'canvas[class*="qr" i]',
    '[class*="qrcode"] img',
    '[class*="qrcode"] canvas',
    '[class*="qrCode"] img',
    '[class*="qrCode"] canvas',
    '[data-e2e*="qrcode"] img',
    '[data-e2e*="qrcode"] canvas',
)

LOGGER = logging.getLogger("douyin_login")

PAGE_SNAPSHOT_SCRIPT = r"""(expectedAccount) => {
  const isVisible = (element) => {
    const style = window.getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    return style.display !== "none" && style.visibility !== "hidden" &&
      Number(style.opacity) !== 0 && rect.width > 0 && rect.height > 0;
  };
  const bodyText = document.body?.innerText || "";
  const normalize = (value) => (value || "").replace(/\s+/g, " ").trim();
  const navRoots = Array.from(document.querySelectorAll(
    'nav, aside, [role="navigation"], [class*="sidebar"], [class*="navigation"]'
  )).filter(isVisible);
  const navText = navRoots.map((element) => element.innerText || "").join(" ");
  const creatorLabels = ["内容管理", "数据中心", "作品管理", "创作者服务", "直播管理", "互动管理"];
  const hasCreatorNavigation = creatorLabels.some((label) => navText.includes(label));
  const loginNames = new Set(["登录", "立即登录", "登录/注册", "登录 / 注册"]);
  const loginControlVisible = Array.from(document.querySelectorAll("button, [role=button], a"))
    .some((element) => isVisible(element) && loginNames.has(normalize(
      element.innerText || element.getAttribute("aria-label")
    )));
  const loginPromptVisible = ["扫码登录", "请登录后继续", "登录后继续"]
    .some((phrase) => bodyText.includes(phrase));
  const challengeSelectors = [
    "iframe[src*=captcha]",
    ".secsdk-captcha-drag-wrapper",
    ".captcha-verify-container",
    '[data-testid*="captcha-challenge"]',
    '[class*="captcha"] [class*="drag"]'
  ];
  const challengeWidgetVisible = challengeSelectors.some((selector) =>
    Array.from(document.querySelectorAll(selector)).some(isVisible)
  );
  const challengePhrases = [
    "拖动滑块完成验证", "拖动滑块验证", "请按住滑块", "请完成验证后继续",
    "请进行安全验证", "验证后继续"
  ];
  const verificationRequired = challengeWidgetVisible ||
    challengePhrases.some((phrase) => bodyText.includes(phrase)) ||
    ["接收短信验证码", "验证登录密码", "发送短信验证"]
      .filter((phrase) => bodyText.includes(phrase)).length >= 2;
  const idSelectors = [
    '[data-e2e*="user-id"]', '[data-e2e*="uid"]', '[data-e2e*="account-id"]',
    '[data-testid*="user-id"]', '[class*="userId"]', '[class*="accountId"]'
  ];
  const accountIds = idSelectors.flatMap((selector) =>
    Array.from(document.querySelectorAll(selector)).filter(isVisible).map((element) => {
      const raw = element.innerText || element.getAttribute("title") ||
        element.getAttribute("aria-label") || element.getAttribute("data-account-id") || "";
      return normalize(raw).replace(/^(抖音号|账号ID|账号|ID)\s*[:：]?\s*/i, "");
    })
  ).filter((value) => value.length > 0 && value.length <= 100);
  for (const match of bodyText.matchAll(/抖音号\s*[:：]?\s*([A-Za-z0-9._-]+)/g)) {
    accountIds.push(match[1]);
  }
  return {
    has_creator_navigation: hasCreatorNavigation,
    expected_account_visible: Boolean(expectedAccount) && accountIds.includes(expectedAccount),
    account_ids: accountIds,
    verification_required: verificationRequired,
    login_control_visible: loginControlVisible,
    login_prompt_visible: loginPromptVisible,
    qr_expired: ["二维码已过期", "二维码已失效", "二维码失效", "二维码过期"].some((phrase) => bodyText.includes(phrase)),
    phone_confirmation_pending: ["请在手机上确认登录", "请在抖音上确认登录", "扫码成功"].some((phrase) => bodyText.includes(phrase)),
    qr_visible: false,
    page_error: location.protocol === "chrome-error:"
  };
}"""


class ConfigError(ValueError):
    """服务配置不完整或不安全。"""


class ProtocolError(ValueError):
    """Unix socket 请求不符合单行 JSON 协议。"""


class BrowserUnavailable(RuntimeError):
    """Playwright 或其 Chromium 运行时不可用。"""


@dataclass(frozen=True)
class ServiceConfig:
    data_dir: Path
    socket_path: Path
    allowed_uid: int
    expected_account: str

    @classmethod
    def from_env(cls, env: dict[str, str] | None = None) -> ServiceConfig:
        values = os.environ if env is None else env

        def required(name: str) -> str:
            value = values.get(name, "").strip()
            if not value:
                raise ConfigError("必需环境变量未设置")
            return value

        data_dir = Path(required("DOUYIN_DATA_DIR"))
        socket_path = Path(required("DOUYIN_SOCKET_PATH"))
        expected_account = required("DOUYIN_EXPECTED_ACCOUNT")
        try:
            allowed_uid = int(required("DOUYIN_ALLOWED_UID"), 10)
        except ValueError as exc:
            raise ConfigError("允许的 UID 配置无效") from exc
        if allowed_uid < 0:
            raise ConfigError("允许的 UID 配置无效")
        if not data_dir.is_absolute() or not socket_path.is_absolute():
            raise ConfigError("数据目录和 socket 路径必须为绝对路径")
        if data_dir == Path(data_dir.anchor) or socket_path == Path(socket_path.anchor):
            raise ConfigError("数据目录或 socket 路径无效")
        if not hasattr(socket, "SO_PEERCRED"):
            raise ConfigError("当前系统不支持 Unix peer 凭据校验")
        return cls(
            data_dir=data_dir,
            socket_path=socket_path,
            allowed_uid=allowed_uid,
            expected_account=expected_account,
        )

    @property
    def profile_dir(self) -> Path:
        return self.data_dir / "chromium-profile"


def parse_request(payload: bytes) -> str | dict:
    """校验单行协议，区分登录操作与严格受限的作品请求。"""
    if len(payload) > MAX_COVER_REQUEST_BYTES:
        raise ProtocolError("请求超过封面上传限制")
    line = payload[:-1] if payload.endswith(b"\n") else payload
    if line.endswith(b"\r"):
        line = line[:-1]
    if b"\n" in line or b"\r" in line:
        raise ProtocolError("每个连接只允许一行请求")
    try:
        text = line.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ProtocolError("请求必须使用 UTF-8 编码") from exc

    def unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
        result: dict[str, object] = {}
        for key, value in pairs:
            if key in result:
                raise ProtocolError("请求包含重复字段")
            result[key] = value
        return result

    try:
        value = json.loads(
            text,
            object_pairs_hook=unique_object,
            parse_constant=lambda _constant: (_ for _ in ()).throw(ProtocolError("请求 JSON 无效")),
        )
    except ProtocolError:
        raise
    except (json.JSONDecodeError, TypeError) as exc:
        if len(payload) > MAX_REQUEST_BYTES:
            raise ProtocolError("普通请求超过 4096 字节限制") from exc
        raise ProtocolError("请求 JSON 无效") from exc
    if isinstance(value, dict) and value.get("action") == "content" and set(value) == {"action", "request"}:
        request = value["request"]
        is_cover = isinstance(request, dict) and request.get("action") in {"cover_import", "cover_import_saved"}
        if not is_cover and len(payload) > MAX_REQUEST_BYTES:
            raise ProtocolError("请求超过 4096 字节限制")
        from content import validate_request
        try:
            return validate_request(value["request"])
        except ValueError as exc:
            raise ProtocolError(str(exc)) from exc
    if len(payload) > MAX_REQUEST_BYTES:
        raise ProtocolError("请求超过 4096 字节限制")
    if isinstance(value, dict) and set(value) == {"action", "recover"}:
        if value["action"] == "login" and value["recover"] is True:
            return "recover_login"
        raise ProtocolError("自动恢复仅支持明确的 login 请求")
    if not isinstance(value, dict) or set(value) != {"action"}:
        raise ProtocolError("请求只能包含 action 字段")
    action = value["action"]
    if not isinstance(action, str) or action not in {"status", "login"}:
        raise ProtocolError("action 仅支持 status 或 login")
    return action


def _peer_credentials(peer_socket: socket.socket) -> tuple[int, int]:
    """从 Linux SO_PEERCRED 读取连接方 PID 和 UID。"""
    if not hasattr(socket, "SO_PEERCRED"):
        raise OSError("SO_PEERCRED 不可用")
    credentials = peer_socket.getsockopt(
        socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i")
    )
    if isinstance(credentials, int):
        raise OSError("SO_PEERCRED 返回了无效凭据")
    pid, uid, _gid = struct.unpack("3i", credentials)
    return pid, uid


def peer_uid(peer_socket: socket.socket) -> int:
    """从 Linux SO_PEERCRED 读取连接方 UID。"""
    return _peer_credentials(peer_socket)[1]


def peer_pid(peer_socket: socket.socket) -> int:
    """从 Linux SO_PEERCRED 读取连接方 PID。"""
    return _peer_credentials(peer_socket)[0]


def is_peer_authorized(peer_socket: socket.socket, allowed_uid: int) -> bool:
    return peer_uid(peer_socket) == allowed_uid


async def _gateway_main_pid_matches(peer_process_id: int) -> bool:
    if peer_process_id <= 0:
        return False
    process: asyncio.subprocess.Process | None = None
    try:
        process = await asyncio.create_subprocess_exec(
            "systemctl",
            "show",
            GATEWAY_UNIT,
            "--property=MainPID",
            "--value",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        stdout, _stderr = await asyncio.wait_for(
            process.communicate(), timeout=GATEWAY_MAIN_PID_TIMEOUT_SECONDS
        )
        if process.returncode != 0:
            return False
        value = stdout.decode("ascii").strip()
        if not value.isdecimal():
            return False
        main_pid = int(value)
        return main_pid > 0 and main_pid == peer_process_id
    except Exception as exc:
        LOGGER.debug(
            "无法确认 gateway 主进程 PID，异常类型：%s", type(exc).__name__
        )
        return False
    finally:
        if process is not None and process.returncode is None:
            try:
                process.kill()
            except (ProcessLookupError, OSError):
                pass
            try:
                await asyncio.wait_for(process.wait(), timeout=0.25)
            except Exception:
                pass


async def is_peer_gateway_main_process(peer_socket: socket.socket) -> bool:
    try:
        return await _gateway_main_pid_matches(peer_pid(peer_socket))
    except Exception as exc:
        LOGGER.debug(
            "无法读取发布调用方 PID，异常类型：%s", type(exc).__name__
        )
        return False


def _is_douyin_host(url: str) -> bool:
    parsed = urlsplit(url)
    hostname = parsed.hostname or ""
    return parsed.scheme == "https" and (
        hostname == "douyin.com" or hostname.endswith(".douyin.com")
    )


def classify_page(snapshot: dict[str, object], expected_account: str) -> str:
    """按账号标识、创作者导航和明确挑战信息保守判别页面状态。"""
    if snapshot.get("page_error"):
        return "page_error"
    if snapshot.get("verification_required"):
        return "verification_required"
    if snapshot.get("qr_visible") or snapshot.get("login_prompt_visible"):
        return "login_required"
    if snapshot.get("has_creator_navigation"):
        if snapshot.get("expected_account_visible") and expected_account:
            return "logged_in"
        account_ids = snapshot.get("account_ids", [])
        if isinstance(account_ids, list) and any(
            isinstance(account_id, str)
            and account_id.strip()
            and account_id.strip() != expected_account
            for account_id in account_ids
        ):
            return "account_mismatch"
        return "unknown"
    if any(
        snapshot.get(key)
        for key in ("login_control_visible", "login_prompt_visible", "qr_visible")
    ):
        return "login_required"
    return "unknown"


def make_response(
    state: str,
    message: str | None = None,
    qr_png: bytes | None = None,
) -> dict[str, object]:
    messages = {
        "logged_in": "已登录预期的抖音账号。",
        "login_required": "需要登录抖音创作者后台。",
        "verification_required": "页面需要人工完成安全验证。",
        "account_mismatch": "当前登录的是其他抖音账号。",
        "unknown": "无法确认当前登录状态。",
        "page_error": "抖音页面暂时不可用，请稍后重试。",
        "unavailable": "抖音登录服务暂时不可用。",
    }
    if state not in messages:
        state = "unknown"
    result: dict[str, object] = {
        "ok": state not in {"page_error", "unavailable"},
        "state": state,
        "message": message or messages[state],
    }
    if qr_png is not None and len(qr_png) <= MAX_QR_BYTES:
        result["qr_png_base64"] = base64.b64encode(qr_png).decode("ascii")
    return result


def _ensure_private_directory(path: Path) -> None:
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.is_symlink() or not path.is_dir():
        raise OSError("数据目录类型无效")
    os.chmod(path, 0o700)


class PlaywrightBrowser:
    """懒启动独立持久化 Chromium，不连接已有浏览器调试端口。"""

    def __init__(self, config: ServiceConfig) -> None:
        self.config = config
        self._playwright = None
        self._context = None
        self._initial_navigation_done = False

    async def _ensure_context(self):
        if self._context is not None:
            return self._context
        try:
            _ensure_private_directory(self.config.data_dir)
            _ensure_private_directory(self.config.profile_dir)
            from playwright.async_api import async_playwright

            self._playwright = await async_playwright().start()
            self._context = await self._playwright.chromium.launch_persistent_context(
                user_data_dir=str(self.config.profile_dir),
                executable_path="/usr/bin/chromium",
                headless=False,
                chromium_sandbox=True,
            )
            return self._context
        except asyncio.CancelledError:
            # wait_for 超时也通过取消中断启动，须回收已创建的驱动再传播取消。
            await self.close()
            raise
        except Exception as exc:
            await self.close()
            raise BrowserUnavailable from exc

    async def _page(self):
        context = await self._ensure_context()
        page = next((page for page in context.pages if not page.is_closed()), None)
        if page is None:
            page = await context.new_page()
        if not self._initial_navigation_done:
            await page.goto(LOGIN_URL, wait_until="domcontentloaded", timeout=15000)
            await page.wait_for_timeout(1200)
            self._initial_navigation_done = True
        return page

    async def _snapshot(self, page) -> dict[str, object]:
        snapshot = await page.evaluate(PAGE_SNAPSHOT_SCRIPT, self.config.expected_account)
        if not isinstance(snapshot, dict):
            raise RuntimeError("页面状态结果无效")
        snapshot["qr_visible"] = await self._qr_element(page) is not None
        return snapshot

    async def _qr_element(self, page):
        for selector in QR_SELECTORS:
            locator = page.locator(selector)
            for index in range(min(await locator.count(), 8)):
                candidate = locator.nth(index)
                if not await candidate.is_visible():
                    continue
                bounds = await candidate.bounding_box()
                if not bounds or bounds["width"] < 80 or bounds["height"] < 80:
                    continue
                ratio = bounds["width"] / bounds["height"]
                if 0.65 <= ratio <= 1.45:
                    return candidate
        return None

    async def _click_explicit_login(self, page) -> bool:
        for role in ("button", "link"):
            for name in ("登录", "立即登录", "登录/注册", "登录 / 注册"):
                matches = page.get_by_role(role, name=name, exact=True)
                if await matches.count() == 1 and await matches.is_visible():
                    await matches.click(timeout=4000)
                    return True
        return False

    async def _wait_for_login_ui(
        self, page, wait_for_qr: bool, timeout_seconds: float = LOGIN_WAIT_SECONDS
    ) -> dict[str, object]:
        deadline = asyncio.get_running_loop().time() + timeout_seconds
        while True:
            snapshot = await self._snapshot(page)
            state = classify_page(snapshot, self.config.expected_account)
            if state in {"logged_in", "account_mismatch", "verification_required", "page_error"}:
                return snapshot
            if state == "login_required" and (
                not wait_for_qr or snapshot.get("qr_visible")
            ):
                return snapshot
            if asyncio.get_running_loop().time() >= deadline:
                return snapshot
            await page.wait_for_timeout(250)

    async def handle(self, action: str) -> dict[str, object]:
        page = await self._page()
        if action == "status":
            snapshot = await self._wait_for_login_ui(page, wait_for_qr=False)
            state = classify_page(snapshot, self.config.expected_account)
            return make_response(state)

        snapshot = await self._wait_for_login_ui(page, wait_for_qr=False)
        state = classify_page(snapshot, self.config.expected_account)
        if state in {"logged_in", "account_mismatch", "verification_required"}:
            return make_response(state)
        if snapshot.get("phone_confirmation_pending"):
            return make_response("login_required", "已扫码，请在抖音手机端确认登录后再检查状态。")

        current_url = page.url
        if not _is_douyin_host(current_url) or snapshot.get("qr_expired"):
            await page.goto(LOGIN_URL, wait_until="domcontentloaded", timeout=15000)
            await page.wait_for_timeout(1200)

        snapshot = await self._wait_for_login_ui(page, wait_for_qr=False)
        state = classify_page(snapshot, self.config.expected_account)
        if snapshot.get("phone_confirmation_pending"):
            return make_response("login_required", "已扫码，请在抖音手机端确认登录后再检查状态。")
        if state == "login_required" and not snapshot.get("qr_visible"):
            if await self._click_explicit_login(page):
                snapshot = await self._wait_for_login_ui(page, wait_for_qr=True)
                state = classify_page(snapshot, self.config.expected_account)

        if state == "login_required":
            qr_element = await self._qr_element(page)
            qr_png = None
            if qr_element is not None:
                qr_png = await qr_element.screenshot(animations="disabled", timeout=5000)
                if len(qr_png) > MAX_QR_BYTES:
                    return make_response(
                        state, "二维码图像超过 2 MiB 限制，未发送图片。"
                    )
            message = "登录二维码已就绪。" if qr_png else "页面已进入登录状态，但暂未找到可用的二维码。"
            return make_response(state, message, qr_png)
        return make_response(state)

    async def close(self) -> None:
        context, playwright = self._context, self._playwright
        self._context = None
        self._playwright = None
        self._initial_navigation_done = False
        if context is not None:
            try:
                await asyncio.wait_for(
                    context.close(), timeout=BROWSER_CONTEXT_CLOSE_TIMEOUT_SECONDS
                )
            except Exception as exc:
                LOGGER.warning("浏览器上下文关闭失败，异常类型：%s", type(exc).__name__)
        if playwright is not None:
            try:
                await asyncio.wait_for(
                    playwright.stop(), timeout=PLAYWRIGHT_STOP_TIMEOUT_SECONDS
                )
            except Exception as exc:
                LOGGER.warning("浏览器驱动关闭失败，异常类型：%s", type(exc).__name__)


class BrowserActivityLock(asyncio.Lock):
    def __init__(self, on_activity: Callable[[], None]) -> None:
        super().__init__()
        self._on_activity = on_activity

    async def acquire(self) -> Literal[True]:
        await super().acquire()
        self._on_activity()
        return True

    def release(self) -> None:
        super().release()
        self._on_activity()


class LoginService:
    """串行化共享浏览器操作并将异常压缩为安全响应。"""

    def __init__(
        self,
        browser: PlaywrightBrowser,
        operation_timeout: float = REQUEST_TIMEOUT_SECONDS,
        *,
        clock: Callable[[], float] | None = None,
        wall_clock: Callable[[], float] | None = None,
    ) -> None:
        self.browser = browser
        self.operation_timeout = operation_timeout
        self._clock = clock or time.monotonic
        self._wall_clock = wall_clock or time.time
        self._last_browser_activity: float | None = None
        self._activity_generation = 0
        self._activity_event = asyncio.Event()
        self._idle_task: asyncio.Task[None] | None = None
        self._browser_lock = BrowserActivityLock(self._record_browser_activity)
        self._content = None

    def _record_browser_activity(self) -> None:
        self._activity_generation += 1
        self._last_browser_activity = self._clock()
        self._activity_event.set()

    def _manual_window_remaining(self) -> float | None:
        try:
            modified = MANUAL_WINDOW_PASSWORD_FILE.stat().st_mtime
        except OSError:
            return None
        age = self._wall_clock() - modified
        if age < 0 or age >= MANUAL_WINDOW_GRACE_SECONDS:
            return None
        return MANUAL_WINDOW_GRACE_SECONDS - age

    async def _wait_for_activity(self, timeout: float | None = None) -> None:
        try:
            if timeout is None:
                await self._activity_event.wait()
            else:
                await asyncio.wait_for(
                    self._activity_event.wait(), timeout=max(timeout, 0.001)
                )
        except asyncio.TimeoutError:
            pass

    async def _close_browser_if_idle(self) -> bool:
        last_activity = self._last_browser_activity
        generation = self._activity_generation
        if (
            last_activity is None
            or self._clock() - last_activity < BROWSER_IDLE_SECONDS
        ):
            return False

        async with self._browser_lock:
            # 取得检查锁会记录一次活动；代数再变化说明等待期间执行了浏览器操作。
            if self._activity_generation != generation + 1:
                return False
            if self._clock() - last_activity < BROWSER_IDLE_SECONDS:
                return False
            if self._manual_window_remaining() is not None or (self._content is not None and self._content.editor_lease_remaining() > 0):
                return False
            close_task = asyncio.create_task(self.browser.close())
            try:
                await asyncio.shield(close_task)
            except asyncio.CancelledError:
                try:
                    await asyncio.shield(close_task)
                except Exception as exc:
                    LOGGER.warning(
                        "空闲浏览器关闭失败，异常类型：%s", type(exc).__name__
                    )
                raise

        self._last_browser_activity = None
        return True

    async def _idle_monitor(self) -> None:
        while True:
            self._activity_event.clear()
            last_activity = self._last_browser_activity
            if last_activity is None:
                await self._wait_for_activity()
                continue

            idle_remaining = BROWSER_IDLE_SECONDS - (
                self._clock() - last_activity
            )
            if idle_remaining > 0:
                await self._wait_for_activity(idle_remaining)
                continue

            manual_remaining = self._manual_window_remaining()
            if manual_remaining is None and self._content is not None:
                manual_remaining = self._content.editor_lease_remaining() or None
            if manual_remaining is not None:
                await self._wait_for_activity(
                    min(MANUAL_WINDOW_RECHECK_SECONDS, manual_remaining)
                )
                continue

            try:
                await self._close_browser_if_idle()
            except Exception as exc:
                LOGGER.warning(
                    "空闲浏览器关闭失败，异常类型：%s", type(exc).__name__
                )
                await self._wait_for_activity(MANUAL_WINDOW_RECHECK_SECONDS)

    def start_idle_monitor(self) -> None:
        if self._idle_task is None or self._idle_task.done():
            self._idle_task = asyncio.create_task(
                self._idle_monitor(), name="douyin-browser-idle"
            )

    async def stop_idle_monitor(self) -> None:
        task = self._idle_task
        if task is None:
            return
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        self._idle_task = None

    async def process(self, action: str | dict) -> dict[str, object]:
        if isinstance(action, dict):
            try:
                if self._content is None:
                    from content import ContentManager
                    self._content = ContentManager(self.browser, self._browser_lock)
                return await self._content.process(action)
            except (ImportError, ValueError, OSError) as exc:
                LOGGER.warning("作品服务初始化失败，异常类型：%s", type(exc).__name__)
                return make_response("unavailable", "作品服务状态无效，请人工检查；登录目录未被清空。")
        recovering = action == "recover_login"
        if recovering and self._browser_lock.locked():
            return make_response("unavailable", "浏览器正在处理作品，未自动切换登录页；请先查询当前任务。")
        async with self._browser_lock:
            if recovering and self._content is not None and (
                self._content.tasks or self._content.editor_lease_remaining() > 0
            ):
                return make_response("unavailable", "仍有活动作品或有效编辑预览，未自动切换登录页。")
            if self._content is not None and not recovering:
                self._content.invalidate_editor()
            try:
                return await asyncio.wait_for(
                    self.browser.handle("login" if recovering else action), timeout=self.operation_timeout
                )
            except asyncio.TimeoutError:
                return make_response("page_error", "页面操作超时，请稍后重试。")
            except BrowserUnavailable:
                return make_response("unavailable")
            except Exception as exc:
                LOGGER.warning("页面操作失败，异常类型：%s", type(exc).__name__)
                if type(exc).__name__ == "TargetClosedError":
                    await self.browser.close()
                return make_response("page_error")


def _encode_response(payload: dict[str, object]) -> bytes:
    return (json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n").encode(
        "utf-8"
    )


async def _write_response(writer: asyncio.StreamWriter, payload: dict[str, object]) -> None:
    writer.write(_encode_response(payload))
    await asyncio.wait_for(writer.drain(), timeout=WRITE_TIMEOUT_SECONDS)


async def _handle_connection(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    service: LoginService,
    allowed_uid: int,
) -> None:
    try:
        peer_socket = writer.get_extra_info("socket")
        if peer_socket is None or not is_peer_authorized(peer_socket, allowed_uid):
            await _write_response(writer, make_response("unavailable", "调用方未获授权。"))
            return
        try:
            payload = await asyncio.wait_for(reader.readline(), timeout=READ_TIMEOUT_SECONDS)
            action = parse_request(payload)
        except asyncio.TimeoutError:
            await _write_response(writer, make_response("page_error", "请求读取超时。"))
            return
        except (ProtocolError, ValueError) as exc:
            message = str(exc) if isinstance(exc, ProtocolError) else "请求行超过长度限制。"
            await _write_response(writer, make_response("page_error", message))
            return
        if (
            isinstance(action, dict)
            and action.get("action") in {
                "publish",
                "verification_request_sms",
                "verification_submit",
            }
            and not await is_peer_gateway_main_process(peer_socket)
        ):
            await _write_response(
                writer, make_response("unavailable", "发布及短信验证操作仅允许由 gateway 主进程发起。")
            )
            return
        await _write_response(writer, await service.process(action))
    except (ConnectionError, OSError, asyncio.TimeoutError):
        LOGGER.info("Unix socket 连接已关闭。")
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except OSError:
            pass


def notify_systemd_ready() -> None:
    """使 systemd 只向真正的服务进程发送退出信号。"""
    address = os.environ.pop("NOTIFY_SOCKET", None)
    if not address:
        return
    if address.startswith("@"):
        address = "\0" + address[1:]
    with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as notifier:
        notifier.settimeout(2)
        notifier.connect(address)
        notifier.sendall(f"MAINPID={os.getpid()}\nNOTIFYACCESS=main\nREADY=1".encode())


async def run_server(config: ServiceConfig) -> None:
    if not hasattr(socket, "SO_PEERCRED"):
        raise ConfigError("当前系统不支持 Unix peer 凭据校验")
    _ensure_private_directory(config.data_dir)
    socket_parent = config.socket_path.parent
    socket_parent.mkdir(mode=0o750, parents=True, exist_ok=True)
    if config.socket_path.exists() or config.socket_path.is_symlink():
        raise ConfigError("socket 路径已存在")

    service = LoginService(PlaywrightBrowser(config))
    server = None
    socket_identity = None
    loop = asyncio.get_running_loop()
    stopping = asyncio.Event()
    installed_signals = []
    for signum in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(signum, stopping.set)
            installed_signals.append(signum)
        except (NotImplementedError, RuntimeError, ValueError):
            pass

    connection_tasks: set[asyncio.Task[None]] = set()

    def client_connected(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.create_task(
            _handle_connection(reader, writer, service, config.allowed_uid)
        )
        connection_tasks.add(task)
        task.add_done_callback(connection_tasks.discard)

    try:
        server = await asyncio.start_unix_server(
            client_connected,
            path=str(config.socket_path),
            limit=MAX_COVER_REQUEST_BYTES,
        )
        socket_info = config.socket_path.lstat()
        if not stat.S_ISSOCK(socket_info.st_mode):
            raise ConfigError("创建的 socket 类型无效")
        socket_identity = (socket_info.st_dev, socket_info.st_ino)
        os.chmod(config.socket_path, 0o660)
        service.start_idle_monitor()
        notify_systemd_ready()
        LOGGER.info("抖音登录服务已启动。")
        async with server:
            await stopping.wait()
    finally:
        if server is not None:
            server.close()
            await server.wait_closed()
        await service.stop_idle_monitor()
        if connection_tasks:
            await asyncio.gather(*tuple(connection_tasks), return_exceptions=True)
        if service._content is not None:
            await service._content.close()
        await service.browser.close()
        if socket_identity is not None:
            try:
                socket_info = config.socket_path.lstat()
                if (socket_info.st_dev, socket_info.st_ino) == socket_identity:
                    config.socket_path.unlink()
            except FileNotFoundError:
                pass
        for signum in installed_signals:
            loop.remove_signal_handler(signum)


def stop_service(pid: int, *, proc_root: Path = Path("/proc")) -> None:
    """同步等待主进程保存浏览器状态，避免 systemd 提前清理整个进程组。"""
    if pid <= 1 or pid == os.getpid():
        raise ConfigError("停止目标无效")
    try:
        descriptor = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    try:
        if select.select([descriptor], [], [], 0)[0]:
            return
        process = proc_root / str(pid)
        try:
            command = (process / "cmdline").read_bytes().split(b"\0")
            owner = process.stat().st_uid
        except FileNotFoundError:
            return
        expected = [os.fsencode(sys.executable), os.fsencode(Path(__file__).resolve())]
        if owner != os.getuid() or command[:2] != expected:
            raise ConfigError("停止目标不是当前登录服务")
        try:
            signal.pidfd_send_signal(descriptor, signal.SIGTERM)
        except ProcessLookupError:
            return
        if not select.select([descriptor], [], [], 40)[0]:
            raise TimeoutError("等待抖音登录服务退出超时")
    finally:
        os.close(descriptor)


def main() -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "--stop":
            stop_service(int(sys.argv[2]))
            return 0
        if len(sys.argv) != 1:
            raise ConfigError("服务参数无效")
        config = ServiceConfig.from_env()
        asyncio.run(run_server(config))
    except ConfigError:
        LOGGER.error("抖音登录服务配置无效或不安全。")
        return 2
    except KeyboardInterrupt:
        return 0
    except Exception as exc:
        LOGGER.error("抖音登录服务启动失败，异常类型：%s", type(exc).__name__)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
