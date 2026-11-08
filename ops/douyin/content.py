"""固定作品包、浏览器草稿与逐次确认的发布操作。"""

from __future__ import annotations

import asyncio
import base64
from dataclasses import dataclass, field
from datetime import datetime, timezone
import hashlib
import json
import logging
import os
from pathlib import Path
import re
import secrets
import stat
import time
import traceback
from urllib.parse import urlsplit
import uuid

from releases import ReleaseError, ReleaseStore
from covers import CoverStore
from editor import apply_cover, apply_settings, validate_settings, verify_cover, verify_settings

LOGGER = logging.getLogger("douyin_content")
HOME_URL = "https://creator.douyin.com/creator-micro/home"
UPLOAD_URL = "https://creator.douyin.com/creator-micro/content/upload"
MANAGE_URL = "https://creator.douyin.com/creator-micro/content/manage"
TOKEN_PATTERN = re.compile(r"[A-Za-z0-9_-]{32}\Z")
ID_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]{0,63}\Z")
JOB_PATTERN = re.compile(r"[a-f0-9]{32}\Z")
COVER_PATTERN = re.compile(r"[a-f0-9]{64}\Z")
PLAN_TTL = 60 * 60
EDITOR_LEASE_SECONDS = 15 * 60
JOB_TIMEOUT = 75 * 60
UPLOAD_TIMEOUT = 55 * 60
SMS_VERIFICATION_TIMEOUT = 30 * 60
SMS_RESEND_COOLDOWN_SECONDS = 60
SMS_MAX_SEND_REQUESTS = 5
SMS_SUBMIT_CONFIRM_SECONDS = 5
SMS_INVALIDATED_PHASE = "验证页已变化，发布结果待核实"
SMS_INVALIDATED_MESSAGE = "验证页已变化，已停止短信处理；发布结果待核实。请人工核查平台结果，不要重发短信或重新发布。"
PUBLISH_TIMEOUT = SMS_VERIFICATION_TIMEOUT + 120
PAGE_OPERATION_TIMEOUT = 40
ACCOUNT_WAIT_SECONDS = 30
ACCOUNT_RECOVERY = {
    "login_required": ("login", "抖音登录已失效，需要扫码后重新准备作品。"),
    "verification_required": ("manual_verification", "抖音要求本人完成设备或身份验证，不能自动绕过。"),
    "account_mismatch": ("check_account", "当前抖音账号不匹配，不能自动切号或继续发布。"),
    "unknown": ("check_status", "当前登录状态尚不明确，请先核查状态，不视为已经退出。"),
    "page_error": ("maintenance", "创作者页面暂不可用，请先核查页面，不重复提交。"),
}


class AccountStateError(ValueError):
    def __init__(self, state):
        action, message = ACCOUNT_RECOVERY[state]
        super().__init__(message)
        self.state = state
        self.recovery = {"action": action}


class UploadFailedError(ValueError):
    """平台明确报告上传失败；尚未形成可保存或发布的编辑预览。"""


@dataclass
class _VerificationAction:
    """单次内存请求；code 在填入页面后立即丢弃，不进入 job。"""

    action: str
    future: asyncio.Future
    code: str | None = field(default=None, repr=False)
    started: bool = False
    cancelled: bool = False


@dataclass
class _VerificationWaiter:
    """只绑定正在发布的同一 job、页面和浏览器上下文。"""

    job: dict
    page: object
    context: object
    url: str
    marker: str
    deadline: float
    queue: asyncio.Queue
    invalidated: bool = False
    invalidated_event: asyncio.Event = field(default_factory=asyncio.Event, repr=False)
    invalidated_complete: asyncio.Future | None = field(default=None, repr=False)
    sms_request_count: int = 0
    last_sms_request_at: float | None = None
    request_pending: bool = False
    submit_pending: bool = False
    pending_submit: _VerificationAction | None = None
    submit_timer: asyncio.Task | None = None


def _transient_navigation_error(exc):
    if isinstance(exc, (TimeoutError, ConnectionError)):
        return True
    try:
        from playwright.async_api import Error, TimeoutError as NavigationTimeout
    except ImportError:
        return False
    return isinstance(exc, NavigationTimeout) or (
        isinstance(exc, Error) and any(code in str(exc) for code in (
            "net::ERR_CONNECTION_RESET", "net::ERR_CONNECTION_CLOSED", "net::ERR_NETWORK_CHANGED",
            "net::ERR_TIMED_OUT", "net::ERR_CONNECTION_TIMED_OUT", "net::ERR_INTERNET_DISCONNECTED",
        ))
    )


def utc_now():
    return datetime.now(timezone.utc).isoformat()


def _sms_verification_visible(snapshot):
    """只识别平台短信验证提示，不读取、返回手机号或验证码。"""
    text = snapshot.get("text_tail", "") if isinstance(snapshot, dict) else ""
    return isinstance(text, str) and all(part in text for part in (
        "接收短信验证码", "为确保是本人操作抖音账号",
    ))


def response(state, message, **fields):
    return {"ok": state not in {"unavailable", "page_error", "needs_attention", "account_mismatch", "verification_required", "login_required", "unknown"},
            "state": state, "message": message, **fields}


def _has_operator_retry_resolution(job):
    """只接受私有账本中有来源和本人授权的运维结论，公开请求不能设置。"""
    resolution = job.get("operator_resolution")
    fields = {"outcome", "release_id", "fingerprint", "checked_at", "source", "owner_authorization_ref"}
    if (job.get("action") != "publish" or job.get("state") != "needs_attention"
            or job.get("commit_attempted") is not True or not isinstance(resolution, dict)
            or set(resolution) != fields or resolution.get("outcome") != "not_listed_at_check"
            or resolution.get("release_id") != job.get("release_id")
            or not isinstance(resolution.get("fingerprint"), str)
            or resolution["fingerprint"] != job.get("fingerprint") or not resolution["fingerprint"]
            or resolution.get("source") != MANAGE_URL
            or not isinstance(resolution.get("owner_authorization_ref"), str)
            or not 1 <= len(resolution["owner_authorization_ref"]) <= 200):
        return False
    try:
        checked = datetime.fromisoformat(resolution["checked_at"].replace("Z", "+00:00"))
        return checked.tzinfo is not None and checked.timestamp() <= time.time() + 60
    except (KeyError, TypeError, ValueError, AttributeError, OverflowError):
        return False


def validate_request(value):
    if not isinstance(value, dict) or not isinstance(value.get("action"), str):
        raise ValueError("作品请求无效")
    action = value["action"]
    allowed = {
        "list": {"action"}, "metrics": {"action"}, "inspect": {"action", "view"},
        "editor_check": {"action", "release_id", "cover_id", "visibility", "publish_at", "view"},
        "covers": {"action", "release_id", "times"},
        "cover_import": {"action", "release_id", "image_base64"},
        "cover_import_saved": {"action", "release_id", "image_base64"},
        "cover_pair": {"action", "release_id", "landscape_cover_id", "portrait_cover_id", "from_release_id"},
        "cover_library": {"action", "release_id", "offset"},
        "cover_reuse": {"action", "release_id", "from_release_id", "cover_id"},
        "prepare": {"action", "release_id", "title", "description", "cover_id", "visibility", "publish_at"},
        "stage": {"action", "token"},
        "resume_stage": {"action", "token"},
        "draft": {"action", "token"}, "publish": {"action", "token"},
        "verification_request_sms": {"action", "job_id"},
        "verification_submit": {"action", "job_id", "code"},
        "job": {"action", "job_id"},
    }
    if action not in allowed or set(value) - allowed[action]:
        raise ValueError("作品请求包含未支持的操作或字段")
    if action == "cover_library":
        if "release_id" in value and (not isinstance(value["release_id"], str) or not ID_PATTERN.fullmatch(value["release_id"])):
            raise ValueError("作品编号无效")
        if "offset" in value and (type(value["offset"]) is not int or not 0 <= value["offset"] <= 10000):
            raise ValueError("图库分页偏移无效")
    if action == "cover_reuse":
        if any(not isinstance(value.get(k), str) or not ID_PATTERN.fullmatch(value[k]) for k in ("release_id", "from_release_id")):
            raise ValueError("作品编号无效")
        if not isinstance(value.get("cover_id"), str) or not COVER_PATTERN.fullmatch(value["cover_id"]):
            raise ValueError("封面编号无效")
    if action == "inspect" and value.get("view", "current") not in {"current", "saved_editor"}:
        raise ValueError("只支持当前页或已有暂存编辑页核查")
    if action == "editor_check" and value.get("view", "current") not in {"current", "saved_editor"}:
        raise ValueError("只支持当前页或已有暂存编辑页核查")
    if action in {"prepare", "covers", "cover_import", "cover_import_saved", "cover_pair", "editor_check"}:
        if not isinstance(value.get("release_id"), str) or not ID_PATTERN.fullmatch(value["release_id"]):
            raise ValueError("作品编号无效")
    if action in {"cover_import", "cover_import_saved"}:
        encoded = value.get("image_base64")
        if not isinstance(encoded, str) or not 1 <= len(encoded) <= 12 * 1024 * 1024:
            raise ValueError("封面图片编码无效")
    if action == "cover_pair":
        if any(
            not isinstance(value.get(field), str) or not COVER_PATTERN.fullmatch(value[field])
            for field in ("landscape_cover_id", "portrait_cover_id")
        ):
            raise ValueError("单图封面编号无效")
        if "from_release_id" in value and (
            not isinstance(value["from_release_id"], str) or not ID_PATTERN.fullmatch(value["from_release_id"])
        ):
            raise ValueError("来源作品编号无效")
    if action == "covers" and "times" in value:
        times = value["times"]
        import math
        if not isinstance(times, list) or not 1 <= len(times) <= 5 or any(type(t) not in {int, float} or not 0 <= t <= 1e12 or not math.isfinite(t) for t in times):
            raise ValueError("提帧时刻须为1到5个有限非负秒数")
    if action in {"prepare", "editor_check"}:
        if "cover_id" in value and (not isinstance(value["cover_id"], str) or not COVER_PATTERN.fullmatch(value["cover_id"])):
            raise ValueError("封面编号无效")
        validate_settings(value.get("visibility", "public"), value.get("publish_at"))
    if action == "prepare":
        if "title" in value and (not isinstance(value["title"], str) or not 1 <= len(value["title"].strip()) <= 30):
            raise ValueError("标题须为1到30个字符")
        if "title" in value and any(char in value["title"] for char in "\r\n\0"):
            raise ValueError("标题不能包含换行或空字符")
        if "description" in value and (not isinstance(value["description"], str) or len(value["description"]) > 1000):
            raise ValueError("简介不能超过1000个字符")
        if "\0" in value.get("description", ""):
            raise ValueError("简介不能包含空字符")
    if action in {"stage", "resume_stage", "draft", "publish"} and (not isinstance(value.get("token"), str) or not TOKEN_PATTERN.fullmatch(value["token"])):
        raise ValueError("准备令牌无效")
    if action in {"job", "verification_request_sms", "verification_submit"} and (
        not isinstance(value.get("job_id"), str) or not JOB_PATTERN.fullmatch(value["job_id"])
    ):
        raise ValueError("任务编号无效")
    if action == "verification_submit" and (
        not isinstance(value.get("code"), str) or re.fullmatch(r"[0-9]{6}", value["code"], re.ASCII) is None
    ):
        raise ValueError("短信验证码必须是6位ASCII数字")
    return dict(value)


def _directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.is_symlink() or not path.is_dir():
        raise ValueError("作品状态目录不安全")
    path.chmod(0o700)
    return path


def _read_json(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size > 65536:
        raise ValueError("作品状态文件不安全")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "r", encoding="utf-8") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("作品状态文件不安全")
        raw = stream.read(65537)
    if len(raw.encode("utf-8")) > 65536:
        raise ValueError("作品状态文件过大")
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("作品状态无效")
    return value


def _write_json(path, value):
    temporary = path.with_name("." + path.name + "." + secrets.token_hex(6))
    try:
        with temporary.open("x", encoding="utf-8") as stream:
            temporary.chmod(0o600)
            json.dump(value, stream, ensure_ascii=False, sort_keys=True)
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(path)
        if os.name == "posix":
            descriptor = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
    finally:
        temporary.unlink(missing_ok=True)


def _safe_creator_origin(value):
    url = urlsplit(value)
    return url.scheme == "https" and url.netloc == "creator.douyin.com"


def _safe_editor_url(value):
    url = urlsplit(value)
    return _safe_creator_origin(value) and url.path.startswith("/creator-micro/content/")


class ContentManager:
    def __init__(self, browser, browser_lock):
        self.browser = browser
        self.browser_lock = browser_lock
        self.store = ReleaseStore(browser.config.data_dir)
        self.covers = CoverStore(browser.config.data_dir, self.store)
        self.plan_dir = _directory(browser.config.data_dir / "content-plans")
        self.job_dir = _directory(browser.config.data_dir / "content-jobs")
        self.jobs = {}
        self.tasks = {}
        self.state_lock = asyncio.Lock()
        self._staged = {}
        self._uploads = {}
        self._verification_waiters = {}
        for path in sorted(self.job_dir.glob("*.json")):
            if not JOB_PATTERN.fullmatch(path.stem):
                continue
            job = _read_json(path)
            if job.get("state") in {"in_progress", "editor_ready"}:
                job.update(state="needs_attention", phase="结果待核实", message="服务曾中断；操作结果不明，禁止自动重复上传或发布。", updated_at=utc_now())
                _write_json(path, job)
            self.jobs[path.stem] = job

    def _save_job(self, job):
        job["updated_at"] = utc_now()
        self.jobs[job["job_id"]] = job
        _write_json(self.job_dir / (job["job_id"] + ".json"), job)

    def invalidate_editor(self):
        self._staged.clear()
        self._uploads.clear()
        for job in self.jobs.values():
            if job.get("state") == "editor_ready":
                job.update(state="needs_attention", phase="编辑预览已失效", message="页面已切换或等待已过期；请重新准备并核对，不沿用旧确认。")
                self._save_job(job)

    def editor_lease_remaining(self):
        proofs = [*self._staged.values(), *self._uploads.values()]
        remaining = max((item["deadline"] - time.monotonic() for item in proofs), default=0)
        return max(0, min(EDITOR_LEASE_SECONDS, remaining))

    def _job_response(self, job):
        if job.get("state") == "editor_ready" and self.editor_lease_remaining() <= 0:
            self.invalidate_editor()
        public = {key: job[key] for key in ("job_id", "action", "release_id", "state", "phase", "updated_at", "message")}
        # 网页诊断快照保留在私有账本；公开状态不夹带任意页面文本，避免严格脱敏拒绝整包。
        extra = {}
        state = job["state"]
        if state == "needs_attention":
            if "recovery" in job:
                extra["recovery"] = job["recovery"]
            state = job.get("error_state", state)
        message = job["message"]
        if state == "in_progress":
            phase = public.get("phase") or "后台任务进行中"
            if job.get("action") == "publish" and job.get("verification_required") == "sms":
                if phase == SMS_INVALIDATED_PHASE:
                    message = SMS_INVALIDATED_MESSAGE
                else:
                    message = (
                        f"当前阶段：{phase}。抖音仍在等待本人短信验证，尚未确认发布成功；"
                        "请保持原验证页面，不要重复点击发布。"
                    )
            else:
                message = (
                    f"当前阶段：{phase}。后台任务已受理，尚无本任务成功回执；"
                    "计划参数不代表网页状态已核验。请及时回复当前阶段，不循环轮询，"
                    "也不要承诺任务完成后会自动推送。"
                )
            public["message"] = message
        if state == "needs_attention" and _sms_verification_visible(job.get("ui")):
            if public.get("phase") == SMS_INVALIDATED_PHASE:
                message = SMS_INVALIDATED_MESSAGE
                public.update(message=message)
            else:
                message = "当次发布遇到本人短信验证，未确认发布成功；请先人工核查，不要重复提交。验证码只用于抖音验证页面。"
                public.update(phase="发布时遇到本人短信验证，结果待核实", message=message)
        if _has_operator_retry_resolution(job):
            message = "历史提交已由运维核查当时平台未列出，并记录本人重试授权；可重新prepare/stage，正式发布仍需新预览和本人确认。"
            public.update(phase="历史提交已核查，可重新准备", message=message)
            extra = {"recovery": {"action": "prepare"}}
        return response(state, message, job=public, **extra)

    def _plan(self, token, *, allow_live=False):
        plan = _read_json(self.plan_dir / (token + ".json"))
        proof = self._staged.get(token) or self._uploads.get(token)
        live = allow_live and proof is not None and proof["deadline"] > time.monotonic()
        if plan["expires_unix"] < time.time() and not live:
            raise ValueError("准备令牌已过期，请重新准备作品")
        release = self.store.load(plan["release_id"])
        if release["video_sha256"] != plan["video_sha256"] or plan["account"] != self.browser.config.expected_account:
            raise ValueError("作品内容或目标账号发生变化，请重新准备")
        if plan.get("cover_id"):
            asset = self.covers.load(plan["release_id"], plan["cover_id"])
            if asset["cover_sha256"] != plan["cover_sha256"]:
                raise ValueError("封面发生变化，请重新准备")
        return plan, release

    async def _navigate_entry(self, page, url):
        if url not in {HOME_URL, UPLOAD_URL}:
            raise ValueError("只允许导航固定创作者入口")
        for attempt in range(2):
            try:
                await page.goto(url, wait_until="domcontentloaded", timeout=12000)
                if not _safe_creator_origin(page.url):
                    raise ValueError("固定入口最终页面不在允许范围")
                return
            except Exception as exc:
                if attempt or not _transient_navigation_error(exc):
                    raise
                LOGGER.info("固定入口发生瞬时网络错误，将在上传前重试一次。")
                await asyncio.sleep(0.5)

    async def _account_page(self):
        self.invalidate_editor()
        page = await self.browser._page()
        await self._navigate_entry(page, HOME_URL)
        # 空闲回收后的冷启动可能让账号导航晚于普通状态查询出现；仍在固定页面
        # 和有限时间内等待，并继续要求创作者导航与预期账号同时匹配。
        snapshot = await self.browser._wait_for_login_ui(
            page, wait_for_qr=False, timeout_seconds=ACCOUNT_WAIT_SECONDS
        )
        # 复用登录服务现有的账号与挑战判别，不将 Cookie 存在视为授权。
        import service
        state = service.classify_page(snapshot, self.browser.config.expected_account)
        if state != "logged_in":
            raise AccountStateError(state)
        return page

    async def _ui(self, page):
        result = await page.evaluate("""() => ({
          buttons:Array.from(document.querySelectorAll('button,[role="button"]')).filter(e=>e.getBoundingClientRect().width>0&&e.getBoundingClientRect().height>0).map(e=>(e.innerText||e.getAttribute('aria-label')||'').trim()).filter(Boolean).slice(0,60),
          inputs:Array.from(document.querySelectorAll('input')).map(e=>({type:e.type,accept:e.accept,placeholder:e.placeholder})).slice(0,30),
          editable:Array.from(document.querySelectorAll('[contenteditable="true"],textarea')).map(e=>({placeholder:e.getAttribute('data-placeholder')||e.getAttribute('placeholder')||'',role:e.getAttribute('role')||''})).slice(0,20),
          labels:Array.from(document.querySelectorAll('label')).map(e=>e.innerText.trim()).filter(Boolean).slice(0,30),
          ai_controls:Array.from(document.querySelectorAll('label')).filter(e=>e.innerText.trim()==='内容由AI生成').map(e=>({visible:!!(e.offsetWidth||e.offsetHeight),controls:Array.from(e.querySelectorAll('input')).map(i=>({type:i.type,checked:i.checked,visible:!!(i.offsetWidth||i.offsetHeight)}))})).slice(0,4),
          controls:Array.from(document.querySelectorAll('label,input,button,[role="dialog"]')).filter(e=>e.getBoundingClientRect().width>0&&e.getBoundingClientRect().height>0).slice(-60).map(e=>({tag:e.tagName,role:e.getAttribute('role')||'',text:e.tagName==='INPUT'?'':e.innerText.trim().slice(0,100),type:e.getAttribute('type')||'',placeholder:e.getAttribute('placeholder')||'',checked:e.matches('input[type=radio],input[type=checkbox]')?e.checked:undefined,parent:e.parentElement?.tagName||'',class_name:typeof e.className==='string'?e.className.slice(0,120):''})),
          cover_controls:Array.from(document.querySelectorAll('span,div,p')).filter(e=>e.children.length===0&&e.getBoundingClientRect().width>0&&['横封面4:3','竖封面3:4','选择封面','完成','确定','上传封面'].includes(e.innerText.trim())).slice(0,16).map(e=>{const nodes=[];for(let i=0;i<5&&e&&!['BODY','HTML'].includes(e.tagName);i++,e=e.parentElement){if(e.innerText.length>1800)break;nodes.push({tag:e.tagName,role:e.getAttribute('role')||'',text:e.innerText.slice(0,1000),class_name:typeof e.className==='string'?e.className.slice(0,120):''});}return nodes;}),
          cover_inputs:Array.from(document.querySelectorAll('[role="dialog"]')).filter(d=>d.getBoundingClientRect().width>0&&d.innerText.includes('封面')).flatMap(d=>Array.from(d.querySelectorAll('input[type=file]')).slice(0,6).map(e=>{const input={accept:e.accept,id:e.id,class_name:e.className,parents:[]};for(let i=0;i<6&&e&&e!==d;i++,e=e.parentElement){const r=e.getBoundingClientRect();input.parents.push({tag:e.tagName,class_name:typeof e.className==='string'?e.className.slice(0,100):'',text:(e.innerText||'').slice(0,150),visible:!!(r.width&&r.height)});}return input;})),
          cover_clicks:Array.from(document.querySelectorAll('[role="dialog"]')).filter(d=>d.getBoundingClientRect().width>0&&d.innerText.includes('封面')).flatMap(d=>Array.from(d.querySelectorAll('span,div,a,button')).filter(e=>!e.children.length&&e.getBoundingClientRect().width>0&&/上传|拖拽/.test(e.innerText)).slice(0,16).map(e=>({tag:e.tagName,text:e.innerText.slice(0,100),class_name:typeof e.className==='string'?e.className.slice(0,100):'',parent_class:typeof e.parentElement?.className==='string'?e.parentElement.className.slice(0,100):''}))),
          video_previews:Array.from(document.querySelectorAll('video')).slice(0,10).map(e=>({visible:!!(e.offsetWidth&&e.offsetHeight),has_src:!!(e.currentSrc||e.src),ready_state:e.readyState})),
          text_tail:document.body.innerText.slice(-4000)
        })""")
        expected = self.browser.config.expected_account
        if expected:
            result["text_tail"] = result["text_tail"].replace(expected, "[预期账号]")
        result["page"] = "https://creator.douyin.com" + urlsplit(page.url).path
        return result

    async def process(self, request):
        try:
            request = validate_request(request)
            action = request["action"]
            if action in {"verification_request_sms", "verification_submit"}:
                # These actions enqueue work for the publish coroutine. They must
                # bypass browser_lock because that coroutine holds it while
                # waiting on the same page for the SMS challenge to finish.
                return await self._verification_action(request)
            if action == "list":
                return response("ready", "仅列出已校验并暂存到服务器的作品。", releases=self.store.list_releases())
            if action == "job":
                job = self.jobs.get(request["job_id"])
                if job is None:
                    raise ValueError("任务不存在")
                return self._job_response(job)
            if action == "covers":
                covers = await asyncio.to_thread(self.covers.frames, request["release_id"], request.get("times"))
                return response("ready", "候选封面已生成，请由本人选择后准备作品。双比例图保留完整画面并加黑边；平台可能提示边框过大，请核对预览或提供专用封面。", covers=covers)
            if action == "cover_import":
                try:
                    image = base64.b64decode(request["image_base64"], validate=True)
                except ValueError as exc:
                    raise ValueError("封面图片编码无效") from exc
                cover = await asyncio.to_thread(self.covers.import_image, request["release_id"], image)
                return response("ready", "本人图片已导入并生成单图候选封面，尚未应用；请核对双槽预览，不能据此认为两种槽位都已匹配。", cover=cover)
            if action == "cover_import_saved":
                try:
                    image = base64.b64decode(request["image_base64"], validate=True)
                except ValueError as exc:
                    raise ValueError("封面图片编码无效") from exc
                cover = await asyncio.to_thread(
                    self.covers.import_image, request["release_id"], image, source="saved_image",
                )
                return response("ready", "本地制作素材已导入为单图候选封面，尚未应用；请核对预览或使用两张适配素材配对。", cover=cover)
            if action == "cover_pair":
                cover = await asyncio.to_thread(
                    self.covers.pair,
                    request["release_id"],
                    request["landscape_cover_id"],
                    request["portrait_cover_id"],
                    from_release_id=request.get("from_release_id"),
                )
                return response(
                    "ready",
                    "双素材封面已配对：横槽4:3、竖槽3:4。仅生成候选，尚未应用、上传或发布；请核对双槽预览。",
                    cover=cover,
                )
            if action == "cover_library":
                catalog = await asyncio.to_thread(self.covers.catalog, request.get("release_id"), request.get("offset", 0))
                return response("ready", "已读取保存的封面，按来源分类，每页最多五组；复用前由本人选择，不重新提帧。", **catalog)
            if action == "cover_reuse":
                cover = await asyncio.to_thread(self.covers.reuse, request["release_id"], request["from_release_id"], request["cover_id"])
                return response("ready", "已有封面已绑定到目标作品，尚未应用、上传或发布；请本人核对双比例预览。", cover=cover)
            if action in {"stage", "resume_stage", "draft", "publish"}:
                return await self._start_job(action, request["token"])
            if self.browser_lock.locked():
                return response("needs_attention", "浏览器正在处理其他任务，请先查询任务状态。")
            async with self.browser_lock:
                if action == "inspect":
                    return await asyncio.wait_for(self._inspect(request), timeout=PAGE_OPERATION_TIMEOUT)
                if action == "editor_check":
                    return await asyncio.wait_for(self._editor_check(request), timeout=120)
                if action == "prepare":
                    return await asyncio.wait_for(self._prepare(request), timeout=PAGE_OPERATION_TIMEOUT)
                return await asyncio.wait_for(self._metrics(), timeout=PAGE_OPERATION_TIMEOUT)
        except AccountStateError as exc:
            return response(exc.state, str(exc), recovery=exc.recovery)
        except FileNotFoundError:
            return response("needs_attention", "作品、任务或准备令牌不存在，请先列出并准备作品。")
        except (ValueError, ReleaseError) as exc:
            return response("needs_attention", str(exc))
        except Exception as exc:
            LOGGER.warning("作品操作失败，异常类型：%s", type(exc).__name__)
            return response("page_error", "作品操作失败，请检查服务或页面；没有自动重试。")

    async def _inspect(self, request):
        # 仅固定创作者页面的有界 DOM 观测，没有任意 URL、脚本或提交入口。
        if request.get("view", "current") == "saved_editor":
            page = await self._account_page()
            await page.goto(UPLOAD_URL, wait_until="domcontentloaded", timeout=30000)
            resume = page.get_by_text("继续编辑", exact=True)
            await resume.wait_for(state="visible", timeout=10000)
            if await resume.count() != 1:
                raise ValueError("没有唯一可恢复的暂存作品")
            await resume.click()
            await page.locator('input[placeholder*="作品标题"]').wait_for(state="visible", timeout=10000)
        else:
            page = await self.browser._page()
        if not _safe_editor_url(page.url):
            raise ValueError("当前不是允许的作品页面")
        return response("ready", "仅核查页面，没有保存或发布。", ui=await self._ui(page))

    async def _prepare(self, request):
        release = self.store.load(request["release_id"])
        title = request.get("title", release["title"]).strip()
        description = request.get("description", release["description"])
        page = await self._account_page()
        await self._navigate_entry(page, UPLOAD_URL)
        await page.wait_for_timeout(1500)
        ui = await self._ui(page)
        token = secrets.token_urlsafe(24)
        plan = {"token": token, "release_id": release["release_id"], "title": title,
                "description": description, "ai_generated": True, "video_sha256": release["video_sha256"],
                "account": self.browser.config.expected_account, "expires_unix": time.time() + PLAN_TTL,
                "expires_at": datetime.fromtimestamp(time.time() + PLAN_TTL, timezone.utc).isoformat()}
        plan["settings"] = validate_settings(request.get("visibility", "public"), request.get("publish_at"))
        if request.get("cover_id"):
            asset = self.covers.load(release["release_id"], request["cover_id"])
            plan.update(cover_id=request["cover_id"], cover_sha256=asset["cover_sha256"])
        stable = {key: value for key, value in plan.items() if key not in {"token", "expires_unix", "expires_at"}}
        plan["fingerprint"] = hashlib.sha256(json.dumps(stable, ensure_ascii=False, sort_keys=True).encode()).hexdigest()
        _write_json(self.plan_dir / (token + ".json"), plan)
        public = {key: plan[key] for key in ("token", "release_id", "title", "description", "ai_generated", "video_sha256", "expires_at")}
        public.update(settings=plan["settings"], cover_id=plan.get("cover_id"), cover_sha256=plan.get("cover_sha256"))
        return response("ready", "作品已准备，尚未上传或发布。正式发布须先stage核对完整物料，再由本人私聊发送：确认发布 " + token,
                        plan=public, ui=ui)

    async def _editor_check(self, request):
        # 运维仅核验现有暂存的编辑控件，不保存、不上传视频，也不建立发布证明。
        release = self.store.load(request["release_id"])
        await self._inspect({"view": request.get("view", "current")})
        page = await self.browser._page()
        await self._verify_metadata(page, release)
        try:
            settings = validate_settings(request.get("visibility", "public"), request.get("publish_at"))
            await apply_settings(page, settings)
            await verify_settings(page, settings)
            evidence = None
            if request.get("cover_id"):
                asset = self.covers.load(request["release_id"], request["cover_id"])
                evidence = await apply_cover(page, asset)
                await verify_cover(page, evidence)
            return response("ready", "已核验编辑控件，未保存或发布；此检查不能作为发布证明。", settings=settings, cover=evidence)
        except Exception:
            return response("needs_attention", "编辑控件核验未完成，未保存或发布。", ui=await self._ui(page))

    async def _start_job(self, action, token):
        async with self.state_lock:
            plan, release = self._plan(token, allow_live=action in {"resume_stage", "draft", "publish"})
            unknown_publish = any(j.get("release_id") == plan["release_id"] and j.get("action") == "publish"
                                  and j.get("commit_attempted") is True and j.get("state") == "needs_attention"
                                  and not _has_operator_retry_resolution(j)
                                  for j in self.jobs.values())
            if action == "resume_stage":
                if unknown_publish:
                    raise ValueError("这份视频已有结果未知的发布提交，不能恢复编辑或重新提交")
                candidates = [j for j in self.jobs.values() if j.get("token") == token
                              and j.get("action") == "stage" and j.get("fingerprint") == plan["fingerprint"]]
                if len(candidates) != 1:
                    raise ValueError("没有唯一的原上传任务可恢复")
                job = candidates[0]
                if job.get("state") == "in_progress":
                    return self._job_response(job)
                if any(j.get("state") == "in_progress" for j in self.jobs.values()):
                    raise ValueError("已有作品任务在进行，请查询原任务")
                if job.get("state") == "editor_ready" and token in self._staged:
                    return self._job_response(job)
                if (job.get("state") != "needs_attention" or job.get("commit_attempted")
                        or token not in self._uploads or self._uploads[token]["deadline"] <= time.monotonic()):
                    raise ValueError("原上传页面或证明已失效，不能无依据跳过上传")
                job.update(state="in_progress", phase="恢复已上传视频的编辑核验", resume_requested=True,
                           message="沿用同一页面中已核实的原视频，不重新上传；尚未保存或发布。")
                job.pop("recovery", None)
                self._save_job(job)
                task = asyncio.create_task(self._run(job, plan, release))
                self.tasks[job["job_id"]] = task
                task.add_done_callback(lambda _task: self.tasks.pop(job["job_id"], None))
                return self._job_response(job)
            if action == "draft" and token in self._uploads and token not in self._staged:
                raise ValueError("视频已上传但编辑核验未完成，请先resume_stage，不重新上传")
            if action == "stage" and not plan.get("cover_id"):
                raise ValueError("请先由本人选择封面，再重新准备作品")
            for job in self.jobs.values():
                if action == "publish" and _has_operator_retry_resolution(job):
                    continue
                if job.get("fingerprint") == plan["fingerprint"] and job.get("action") == action and (action != "stage" or job.get("token") == token):
                    if action == "stage" and job.get("state") == "editor_ready" and (token not in self._staged or self.editor_lease_remaining() <= 0):
                        self.invalidate_editor()
                    if (action == "draft" and job.get("root_resume_authorized") is True
                            and job.get("state") == "needs_attention" and not job.get("commit_attempted")):
                        if any(other.get("state") == "in_progress" for other in self.jobs.values()):
                            raise ValueError("已有作品任务在进行，暂不恢复")
                        job.pop("root_resume_authorized")
                        job.update(state="in_progress", phase="恢复保存前任务", message="运维已授权恢复，尚未尝试过保存或发布。")
                        self._save_job(job)
                        task = asyncio.create_task(self._run(job, plan, release))
                        self.tasks[job["job_id"]] = task
                        task.add_done_callback(lambda _task: self.tasks.pop(job["job_id"], None))
                    return self._job_response(job)
            if unknown_publish:
                raise ValueError("这份视频已有结果未知的发布提交；须先人工核实平台结果，不能换文案或封面后重提")
            if action == "publish" and (token not in self._staged or self.editor_lease_remaining() <= 0):
                raise ValueError("须先stage并核对这份物料的实时编辑预览；旧暂存槽不能直接发布")
            if any(j.get("state") == "in_progress" for j in self.jobs.values()):
                raise ValueError("已有作品任务在进行，请等待并查询原任务")
            job = {"job_id": uuid.uuid4().hex, "action": action, "release_id": release["release_id"],
                   "state": "in_progress", "phase": "排队", "message": "任务已接收，请按任务编号查询，不要重复提交。",
                   "fingerprint": plan["fingerprint"], "token": token, "created_at": utc_now()}
            self._save_job(job)
            task = asyncio.create_task(self._run(job, plan, release))
            self.tasks[job["job_id"]] = task
            task.add_done_callback(lambda _task: self.tasks.pop(job["job_id"], None))
            return self._job_response(job)

    async def _run(self, job, plan, release):
        try:
            async with self.browser_lock:
                timeout = PUBLISH_TIMEOUT if job["action"] == "publish" else JOB_TIMEOUT
                await asyncio.wait_for(self._execute(job, plan, release), timeout=timeout)
        except (Exception, asyncio.CancelledError) as exc:
            checkpoint = self._uploads.get(plan.get("token"))
            verification_waiter = self._verification_waiters.get(job.get("job_id"))
            verification_invalidated = bool(
                verification_waiter is not None and verification_waiter.invalidated
            )
            self.invalidate_editor()
            frames = [frame for frame in traceback.extract_tb(exc.__traceback__) if Path(frame.filename).name in {"content.py", "editor.py", "covers.py"}]
            location = frames[-1].lineno if frames else 0
            module = Path(frames[-1].filename).name if frames else "unknown"
            LOGGER.warning("作品任务停止，异常类型：%s，执行模块：%s，行号：%s", type(exc).__name__, module, location)
            phase = SMS_INVALIDATED_PHASE if verification_invalidated else job.get("phase", "未知步骤") + "：结果待核实"
            message = SMS_INVALIDATED_MESSAGE if verification_invalidated else "操作未确认完成；请人工核查草稿或作品，不要自动重复提交。"
            job.update(state="needs_attention", phase=phase, message=message)
            job.pop("error_state", None)
            job["recovery"] = {"action": "inspect_result" if job.get("commit_attempted") else "maintenance"}
            account_error = exc if isinstance(exc, AccountStateError) else None
            if not job.get("commit_attempted") and not isinstance(exc, asyncio.CancelledError):
                try:
                    context = self.browser._context
                    if account_error is None and context is not None and context.pages:
                        import service
                        snapshot = await asyncio.wait_for(self.browser._snapshot(context.pages[0]), timeout=2)
                        state = service.classify_page(snapshot, self.browser.config.expected_account)
                        if state in {"login_required", "verification_required", "account_mismatch"}:
                            account_error = AccountStateError(state)
                except Exception:
                    LOGGER.info("未能补充账号故障分类，不据此触发登录恢复。")
                if account_error is not None:
                    job.update(error_state=account_error.state, recovery=account_error.recovery,
                               message=str(account_error))
            if (job.get("action") == "stage" and checkpoint is not None and not job.get("commit_attempted")
                    and account_error is None and not isinstance(exc, (asyncio.CancelledError, UploadFailedError))):
                try:
                    await asyncio.wait_for(self._verified_upload(plan, checkpoint), timeout=10)
                except Exception:
                    LOGGER.info("已上传页面未通过恢复核验，不跳过原片上传。")
                else:
                    self._uploads[plan["token"]] = checkpoint
                    job.update(recovery={"action": "resume_stage"},
                               message="原视频已上传，编辑步骤未完成；可在当前页面保留期内用相同plan_id恢复核验，不重新上传。尚未保存或发布。")
            if not isinstance(exc, asyncio.CancelledError) and not verification_invalidated:
                if isinstance(exc, UploadFailedError):
                    job["message"] = str(exc)
                try:
                    context = self.browser._context
                    if context is not None and context.pages:
                        job["ui"] = await self._ui(context.pages[0])
                except Exception:
                    LOGGER.info("未能取得作品页面的受限诊断信息。")
            self._save_job(job)
            if verification_invalidated and verification_waiter is not None:
                await self._close_verification_waiter(verification_waiter)
            if isinstance(exc, asyncio.CancelledError):
                raise

    def _phase(self, job, value):
        job.update(phase=value, message=value)
        self._save_job(job)

    async def _upload_failed(self, page):
        failure = page.get_by_text(re.compile(r"^上传失败[，,]\s*重新上传$"))
        return await failure.count() == 1 and await failure.is_visible()

    async def _button(self, page, names):
        for name in names:
            target = page.get_by_role("button", name=name, exact=True)
            if await target.count() == 1 and await target.is_visible():
                return target
        raise ValueError("没有唯一可操作的目标按钮")

    async def _fill_metadata(self, page, plan):
        title = page.locator('input[placeholder*="作品标题"],input[placeholder*="填写标题"]')
        await title.wait_for(state="visible", timeout=180000)
        if await title.count() != 1:
            raise ValueError("作品标题输入框不唯一")
        await title.fill(plan["title"])
        description = self._description_field(page)
        if await description.count() != 1:
            raise ValueError("作品简介输入框不唯一")
        await description.fill(plan["description"])

    def _description_field(self, page):
        # 实际编辑页是无 placeholder 的唯一可见 contenteditable。
        return page.locator('[contenteditable="true"]:visible, textarea[placeholder*="简介"]:visible')

    async def _ai_is_selected(self, page):
        checkbox = page.get_by_role("checkbox", name=re.compile(r"^(内容由AI生成|AI生成|声明内容由AI生成)$"))
        if await checkbox.count() == 1 and await checkbox.is_visible() and await checkbox.is_checked():
            return True
        selected = []
        for role in ("option", "radio"):
            candidate = page.get_by_role(role, name="内容由AI生成", exact=True)
            for index in range(min(await candidate.count(), 10)):
                item = candidate.nth(index)
                attribute = "aria-selected" if role == "option" else "aria-checked"
                checked = await item.get_attribute(attribute) == "true"
                if role == "radio" and await item.get_attribute("type") == "radio":
                    checked = await item.is_checked()
                if await item.is_visible() and checked:
                    selected.append(item)
        if len(selected) == 1:
            return True
        radio = page.locator("label:visible").filter(has_text=re.compile(r"^内容由AI生成$")).locator('input[type="radio"]')
        if await radio.count() == 1 and await radio.is_checked():
            return True
        return await self._ai_summary_selected(page)

    async def _ai_summary_selected(self, page):
        # 只接受平台“自主声明”行的短摘要，简介中自写的相同文字不算声明。
        return await page.evaluate("""() => {
          const visible=e=>!!(e.offsetWidth||e.offsetHeight);
          const labels=Array.from(document.querySelectorAll('span,div,p')).filter(e=>
            e.children.length===0&&visible(e)&&e.innerText.trim()==='自主声明');
          if(labels.length!==1)return false;
          let e=labels[0];
          for(let i=0;i<4;i++){
            e=e.parentElement;if(!e||['BODY','HTML'].includes(e.tagName)||e.innerText.length>160)return false;
            if(e.querySelector('textarea,[contenteditable=true]'))return false;
            const text=e.innerText.replace(/\\s+/g,' ').trim();
            if(/^自主声明\\s*内容由AI生成$/.test(text))return true;
          }return false;
        }""") is True

    async def _ai_declaration(self, page):
        if await self._ai_is_selected(page):
            await self._confirm_ai_dialog(page)
            return
        option = page.locator("label:visible").filter(has_text=re.compile(r"^内容由AI生成$"))
        if await option.count() != 1 or not await option.is_visible():
            declaration = page.get_by_text("自主声明", exact=True)
            if await declaration.count() == 1:
                await declaration.click()
                await option.wait_for(state="visible", timeout=10000)
        if await option.count() != 1 or not await option.is_visible():
            raise ValueError("AI生成声明控件不可确认")
        # 定制单选框的原生 input 可能隐藏，点击唯一可见的完整标签。
        await option.click()
        # 只接受页面重新显示的选中状态，不把简介中的文字当作平台声明。
        if not await self._ai_is_selected(page):
            raise ValueError("AI生成声明未能确认已选中")
        await self._confirm_ai_dialog(page)

    async def _confirm_ai_dialog(self, page):
        heading = page.get_by_text("请选择声明类型（单选）", exact=True)
        if await heading.count() == 1 and await heading.is_visible():
            scope = heading
            for _ in range(12):
                scope = scope.locator("..")
                if await scope.evaluate("e => ['BODY','HTML'].includes(e.tagName) || e.innerText.length > 1500"):
                    break
                confirm = scope.get_by_role("button", name="确定", exact=True)
                cancel = scope.get_by_role("button", name="取消", exact=True)
                if await confirm.count() == 1 and await cancel.count() == 1:
                    await confirm.click()
                    await heading.wait_for(state="hidden", timeout=10000)
                    await page.get_by_text("内容由AI生成", exact=True).and_(page.locator(":visible")).wait_for(state="visible", timeout=10000)
                    return
            raise ValueError("AI声明确认弹窗无法唯一定位")

    async def _dismiss_preview_tip(self, page):
        heading = page.get_by_text("视频预览功能", exact=True)
        if await heading.count() != 1 or not await heading.is_visible():
            return
        scope = heading
        for _ in range(6):
            scope = scope.locator("..")
            if await scope.evaluate("e => ['BODY','HTML'].includes(e.tagName) || e.innerText.length > 600"):
                break
            button = scope.get_by_role("button", name="我知道了", exact=True)
            if await button.count() == 1 and await button.is_visible():
                await button.click()
                return
        raise ValueError("视频预览说明无法安全关闭")

    async def _verify_metadata(self, page, plan):
        title = page.locator('input[placeholder*="作品标题"],input[placeholder*="填写标题"]')
        await title.wait_for(state="visible", timeout=15000)
        if await title.count() != 1 or await title.input_value() != plan["title"]:
            raise ValueError("草稿标题不匹配")
        description = self._description_field(page)
        if await description.count() != 1:
            raise ValueError("草稿简介无法唯一核对")
        actual = await description.input_value() if await description.evaluate("e => e.tagName === 'TEXTAREA'") else await description.inner_text()
        # 编辑器可能自动加入零宽分隔符，不改变用户可见的文案。
        if actual.replace("\u200b", "") != plan["description"].replace("\u200b", ""):
            raise ValueError("草稿简介发生变化")

    async def _verify_draft_banner(self, page):
        if not _safe_editor_url(page.url) or urlsplit(page.url).path != urlsplit(UPLOAD_URL).path:
            raise ValueError("没有返回允许的草稿入口")
        notice = page.get_by_text(re.compile(r"你还有上次未发布的视频"))
        await notice.wait_for(state="visible", timeout=15000)
        if await notice.count() != 1:
            raise ValueError("暂存提示不唯一")
        resume = page.get_by_text("继续编辑", exact=True)
        if await resume.count() != 1 or not await resume.is_visible():
            raise ValueError("无法唯一定位暂存作品")

    async def _execute(self, job, plan, release):
        # 再次校验文件，避免排队期间被替换的作品沿用旧确认。
        current = self.store.load(plan["release_id"])
        if current["video_sha256"] != plan["video_sha256"]:
            raise ValueError("待操作作品发生变化")
        if job["action"] == "publish":
            await self._publish(None, job, plan)
            return
        if job["action"] == "draft" and plan["token"] in self._staged:
            page = await self._verified_stage(plan)
            job["ai_declaration_verified"] = True
            await self._save_draft(page, job, plan)
            return
        resume = job.pop("resume_requested", False)
        if resume:
            page = await self._verified_upload(plan)
            await self._fill_metadata(page, plan)
            await self._ai_declaration(page)
            job["ai_declaration_verified"] = True
        else:
            page = await self._upload_video(job, plan, current)
        await self._dismiss_preview_tip(page)
        if plan.get("cover_id"):
            self._phase(job, "应用并核对双比例封面")
            asset = self.covers.load(plan["release_id"], plan["cover_id"])
            cover_evidence = await apply_cover(page, asset)
        else:
            cover_evidence = None
        settings = plan.get("settings", validate_settings())
        self._phase(job, "应用并核对可见范围与发布时间")
        await apply_settings(page, settings)
        await self._verify_metadata(page, plan)
        if job["action"] == "stage":
            if not job["ai_declaration_verified"]:
                raise ValueError("平台AI声明未确认，不能形成发布预览")
            await self._verified_upload(plan)
            source = await self._video_source(page, activate=True)
            await self._verify_metadata(page, plan)
            marker = secrets.token_hex(32)
            await page.evaluate("marker => { window.__tangyugeEditorProof = marker; }", marker)
            self._staged[plan["token"]] = {"page": page, "context": self.browser._context,
                "marker": marker, "video_source": source, "cover": cover_evidence,
                "deadline": time.monotonic() + EDITOR_LEASE_SECONDS}
            self._uploads.pop(plan["token"], None)
            job.update(state="editor_ready", phase="完整物料待本人确认", message="视频、双比例封面、文案、AI声明和发布设置已核对。尚未保存或发布；编辑完成后有独立15分钟确认窗口。")
            self._save_job(job)
            return
        await self._save_draft(page, job, plan)

    async def _upload_video(self, job, plan, current):
        page = await self._account_page()
        self._phase(job, "打开上传页")
        await self._navigate_entry(page, UPLOAD_URL)
        upload = page.locator('input[type="file"][accept*="video"], input[type="file"][accept*=".mp4"]')
        await upload.first.wait_for(state="attached", timeout=15000)
        if await upload.count() != 1:
            raise ValueError("视频上传控件不唯一或未加载")
        self._phase(job, "上传视频")
        if not _safe_editor_url(page.url):
            raise ValueError("作品上传页不在允许的编辑范围")
        await upload.set_input_files(str(current["video_path"]))
        await self._fill_metadata(page, plan)
        self._phase(job, "核对AI生成声明")
        try:
            await self._ai_declaration(page)
            job["ai_declaration_verified"] = True
        except ValueError:
            job["ai_declaration_verified"] = False
            LOGGER.info("草稿中的平台AI声明尚未确认，公开发布仍会强制核验。")
        await page.keyboard.press("Escape")
        self._phase(job, "等待视频处理")
        uploading = page.get_by_text("取消上传", exact=True)
        if await uploading.count() == 1:
            await uploading.wait_for(state="hidden", timeout=UPLOAD_TIMEOUT * 1000)
        if await self._upload_failed(page):
            raise UploadFailedError("平台报告视频上传失败，未形成发布预览；可按原片重新上传。")
        source = await self._video_source(page, activate=True)
        marker = secrets.token_hex(32)
        await page.evaluate("marker => { window.__tangyugeUploadProof = marker; }", marker)
        self._uploads[plan["token"]] = {"page": page, "context": self.browser._context,
            "marker": marker, "video_source": source, "fingerprint": plan["fingerprint"],
            "video_sha256": current["video_sha256"], "deadline": time.monotonic() + EDITOR_LEASE_SECONDS}
        return page

    async def _verified_upload(self, plan, proof=None):
        proof = proof if proof is not None else self._uploads.get(plan["token"])
        if proof is None or proof["deadline"] <= time.monotonic():
            raise ValueError("已上传视频的页面保留期已过，不能直接恢复")
        page = proof["page"]
        if (self.browser._context is not proof["context"] or page.is_closed() or not _safe_editor_url(page.url)
                or proof["fingerprint"] != plan["fingerprint"] or proof["video_sha256"] != plan["video_sha256"]
                or plan["account"] != self.browser.config.expected_account):
            raise ValueError("已上传视频的页面、账号或计划已变化")
        if await page.evaluate("() => window.__tangyugeUploadProof || null") != proof["marker"]:
            raise ValueError("已上传视频的文档已变化")
        # 编辑封面会切走视频预览；仅在构建或恢复stage时切回，再核对同一上传源。
        # 正式发布仍由_verified_stage只读核验，不借此重建发布证明。
        if await self._video_source(page, activate=True) != proof["video_source"] or await self._upload_failed(page):
            raise ValueError("页面视频已变化或平台上传失败，不能直接恢复")
        return page

    async def _save_draft(self, page, job, plan):
        save = await self._button(page, ("保存草稿", "暂存离开"))
        deadline = time.monotonic() + 300
        while not await save.is_enabled():
            if time.monotonic() > deadline:
                raise TimeoutError("视频处理未完成")
            await asyncio.sleep(2)
        editor_url = page.url
        if not _safe_editor_url(editor_url):
            raise ValueError("编辑页不在允许范围")
        self._phase(job, "保存草稿，结果待确认")
        job["editor_url"] = editor_url
        job["commit_attempted"] = True
        self._save_job(job)
        self.invalidate_editor()
        await save.click()
        dialog = page.get_by_role("dialog").filter(has_text=re.compile("草稿|暂存"))
        if await dialog.count() == 1 and await dialog.is_visible():
            confirm = await self._button(dialog, ("保存草稿", "暂存", "保存并离开", "确定"))
            await confirm.click()
        success = page.get_by_text(re.compile(r"^(草稿保存成功|已保存草稿|草稿已保存|保存草稿成功|已保存至草稿箱|保存成功|暂存成功|已暂存)$"))
        try:
            await success.first.wait_for(state="visible", timeout=5000)
        except Exception:
            # 当前平台的“暂存离开”返回上传页，不一定显示成功 toast。
            # 只核对恢复入口，不消费暂存进入一个尚未重新保存的编辑态。
            await self._verify_draft_banner(page)
            job["resume_mode"] = "browser_slot_unopened"
            job.update(state="needs_attention", phase="暂存入口已保留，身份待核实",
                       message="页面出现唯一暂存恢复入口，已保留且未重新打开；没有可核实的视频标识，不能据此正式发布。")
            self._save_job(job)
            return
        message = "页面已确认草稿保存成功，没有点击正式发布。"
        if not job["ai_declaration_verified"]:
            message += "平台AI声明未确认，正式发布前必须再核验。"
        job.update(state="draft_saved", phase="草稿已保存", message=message)
        self._save_job(job)

    async def _verified_stage(self, plan):
        proof = self._staged.get(plan.get("token"))
        if proof is None or proof["deadline"] <= time.monotonic():
            raise ValueError("没有可唯一定位的实时编辑预览，请重新准备，不重传或发布旧槽内容")
        page = proof["page"]
        if self.browser._context is not proof["context"] or page.is_closed() or not _safe_editor_url(page.url):
            raise ValueError("浏览器上下文或编辑页发生变化")
        marker = await page.evaluate("() => window.__tangyugeEditorProof || null")
        if marker != proof["marker"] or await self._video_source(page) != proof["video_source"]:
            raise ValueError("上传视频或编辑文档发生变化，旧确认无效")
        await self._verify_metadata(page, plan)
        await verify_settings(page, plan["settings"])
        await verify_cover(page, proof["cover"])
        if not await self._ai_is_selected(page):
            raise ValueError("平台AI声明发生变化，旧确认无效")
        return page

    def _verification_response(self, job, state, message, **extra):
        fields = {}
        if isinstance(job, dict):
            job_response = self._job_response(job)
            public_job = job_response.get("job")
            if isinstance(public_job, dict):
                fields["job"] = public_job
            if isinstance(job_response.get("recovery"), dict):
                fields["recovery"] = job_response["recovery"]
        fields.update(extra)
        return response(state, message, **fields)

    def _current_verification_waiter(self, job_id):
        waiter = self._verification_waiters.get(job_id)
        job = self.jobs.get(job_id)
        task = self.tasks.get(job_id)
        if (
            waiter is None
            or waiter.invalidated
            or waiter.job is not job
            or job.get("action") != "publish"
            or job.get("state") != "in_progress"
            or job.get("verification_required") != "sms"
            or task is None
            or task.done()
        ):
            return None
        active = []
        for candidate_id, job in self.jobs.items():
            task = self.tasks.get(candidate_id)
            if (
                job.get("action") == "publish"
                and job.get("state") == "in_progress"
                and job.get("verification_required") == "sms"
                and task is not None
                and not task.done()
            ):
                active.append((candidate_id, job))
        if len(active) != 1 or active[0][0] != job_id:
            return None
        if waiter.job is not active[0][1]:
            return None
        if (
            time.monotonic() >= waiter.deadline
            or waiter.context is None
            or self.browser._context is not waiter.context
        ):
            self._invalidate_verification_waiter(waiter)
            return None
        page = waiter.page
        try:
            pages = waiter.context.pages
            if not isinstance(pages, (list, tuple)) or not any(
                candidate is page for candidate in pages
            ):
                self._invalidate_verification_waiter(waiter)
                return None
            is_closed = getattr(page, "is_closed", None)
            if callable(is_closed) and is_closed():
                self._invalidate_verification_waiter(waiter)
                return None
            if not _safe_editor_url(page.url) or page.url != waiter.url:
                self._invalidate_verification_waiter(waiter)
                return None
        except Exception:
            self._invalidate_verification_waiter(waiter)
            return None
        return waiter

    def _invalidate_verification_waiter(self, waiter):
        if waiter.invalidated:
            return
        waiter.invalidated = True
        self._phase(waiter.job, SMS_INVALIDATED_PHASE)
        waiter.invalidated_event.set()

    async def _verification_invalidation_response(self, waiter):
        completion = waiter.invalidated_complete
        if completion is not None and not completion.done():
            await asyncio.shield(completion)
        if waiter.job.get("state") == "published":
            return self._verification_response(
                waiter.job, "ready", "抖音页面已确认发布成功；没有重复点击发布。",
            )
        return self._verification_response(
            waiter.job, "needs_attention", SMS_INVALIDATED_MESSAGE,
        )

    async def _verification_document_is_current(self, waiter):
        if self._current_verification_waiter(waiter.job.get("job_id")) is not waiter:
            return False
        try:
            marker = await waiter.page.evaluate(
                "() => window.__tangyugeVerificationProof || null"
            )
        except Exception:
            self._invalidate_verification_waiter(waiter)
            return False
        if marker != waiter.marker:
            self._invalidate_verification_waiter(waiter)
            return False
        return True

    async def _verification_page_is_current(self, waiter):
        if not await self._verification_document_is_current(waiter):
            return False
        try:
            return _sms_verification_visible(await self._ui(waiter.page))
        except Exception:
            return False

    async def _unique_visible_text(self, page, text):
        try:
            locator = page.get_by_text(text, exact=True).and_(page.locator(":visible"))
            if await locator.count() != 1 or not await locator.is_visible():
                return None
            if not await locator.is_enabled():
                return None
            get_attribute = getattr(locator, "get_attribute", None)
            if callable(get_attribute) and await get_attribute("aria-disabled") == "true":
                return None
            return locator
        except Exception:
            return None

    async def _request_sms_from_page(self, waiter):
        job = waiter.job
        if not await self._verification_page_is_current(waiter):
            return self._verification_response(
                job, "needs_attention", "当前抖音短信验证页面已变化；没有请求短信，请先查询同一任务。",
            )
        now = time.monotonic()
        if waiter.sms_request_count >= SMS_MAX_SEND_REQUESTS:
            return self._verification_response(
                job, "needs_attention", "本次验证已达到短信请求上限；请人工检查当前页面。",
            )
        if (
            waiter.last_sms_request_at is not None
            and now - waiter.last_sms_request_at < SMS_RESEND_COOLDOWN_SECONDS
        ):
            return self._verification_response(
                job, "in_progress", "抖音尚未开放重新发送；请等待页面倒计时后再请求。",
            )
        label = "获取验证码" if waiter.sms_request_count == 0 else "重新发送"
        locator = await self._unique_visible_text(waiter.page, label)
        if locator is None:
            return self._verification_response(
                job, "needs_attention", "当前验证页面没有唯一可用的短信发送入口；没有点击任何按钮。",
            )
        if not await self._verification_page_is_current(waiter):
            return self._verification_response(
                job, "needs_attention", "当前短信验证文档标记已变化；没有请求短信，请先查询同一任务。",
            )
        # Count before clicking so an uncertain UI result cannot cause repeated SMS sends.
        waiter.sms_request_count += 1
        waiter.last_sms_request_at = now
        try:
            await locator.click()
        except Exception:
            return self._verification_response(
                job, "needs_attention", "无法确认短信请求结果；请先查看当前页面，不要立即重复请求。",
            )
        return self._verification_response(
            job, "ready", "已请求抖音发送短信验证码，请本人查看绑定手机；验证码不会被保存。",
        )

    async def _submit_sms_to_page(self, waiter, event):
        job = waiter.job
        if not await self._verification_page_is_current(waiter):
            return self._verification_response(
                job, "needs_attention", "当前抖音短信验证页面已变化；没有提交验证码，请先查询同一任务。",
            )
        if waiter.sms_request_count < 1:
            return self._verification_response(
                job, "needs_attention", "当前任务尚未请求短信验证码；没有提交验证码。",
            )
        try:
            code_input = waiter.page.locator('input[placeholder="请输入验证码"]:visible')
            if await code_input.count() != 1 or not await code_input.is_visible() or not await code_input.is_enabled():
                return self._verification_response(
                    job, "needs_attention", "当前验证页面没有唯一可用的验证码输入框；没有提交验证码。",
                )
        except Exception:
            return self._verification_response(
                job, "needs_attention", "当前验证页面没有唯一可用的验证码输入框；没有提交验证码。",
            )

        code = event.code
        event.code = None
        if not isinstance(code, str) or re.fullmatch(r"[0-9]{6}", code, re.ASCII) is None:
            return self._verification_response(
                job, "needs_attention", "验证码格式无效；原验证页面仍保留。",
            )
        if not await self._verification_page_is_current(waiter):
            code = None
            return self._verification_response(
                job, "needs_attention", "当前短信验证文档标记已变化；没有提交验证码，请先查询同一任务。",
            )
        try:
            await code_input.fill(code)
        except Exception:
            return self._verification_response(
                job, "needs_attention", "无法确认验证码已填入；原验证页面仍保留，请查看页面后再试。",
            )
        finally:
            # Drop the local reference as soon as the page has received the value.
            code = None
            event.code = None

        verify = await self._unique_visible_text(waiter.page, "验证")
        if verify is None:
            try:
                await code_input.fill("")
            except Exception:
                pass
            return self._verification_response(
                job, "needs_attention", "当前验证页面没有唯一可用的“验证”入口；没有提交短信验证码。",
            )
        if not await self._verification_page_is_current(waiter):
            return self._verification_response(
                job, "needs_attention", "当前短信验证文档标记已变化；没有点击“验证”，请先查询同一任务。",
            )
        try:
            waiter.pending_submit = event
            await verify.click()
        except Exception:
            waiter.pending_submit = None
            waiter.submit_pending = False
            return self._verification_response(
                job, "needs_attention", "无法确认短信验证码是否已提交；原验证页面仍保留，请查询同一任务。",
            )
        self._phase(job, "等待抖音确认短信验证")
        waiter.submit_timer = asyncio.create_task(asyncio.sleep(SMS_SUBMIT_CONFIRM_SECONDS))
        return self._verification_response(
            job,
            "in_progress",
            "短信验证码已提交到当前抖音验证页，正在等待页面回执；没有再次点击发布。",
            verification_submitted=True,
        )

    def _resolve_verification_action(self, event, result):
        event.code = None
        if not event.future.done():
            event.future.set_result(result)

    async def _process_verification_event(self, waiter, event):
        event.started = True
        try:
            if event.cancelled or event.future.cancelled():
                if event.action == "verification_request_sms":
                    waiter.request_pending = False
                else:
                    waiter.submit_pending = False
                return
            if event.action == "verification_request_sms":
                result = await self._request_sms_from_page(waiter)
                waiter.request_pending = False
            else:
                result = await self._submit_sms_to_page(waiter, event)
                if result.get("state") != "in_progress":
                    waiter.submit_pending = False
                    if waiter.pending_submit is event:
                        waiter.pending_submit = None
            self._resolve_verification_action(event, result)
        except asyncio.CancelledError:
            if event.action == "verification_request_sms":
                waiter.request_pending = False
            else:
                waiter.submit_pending = False
                if waiter.pending_submit is event:
                    waiter.pending_submit = None
            self._resolve_verification_action(
                event,
                self._verification_response(
                    waiter.job,
                    "needs_attention",
                    "短信验证操作已取消，页面状态待核实；请查询同一任务。",
                ),
            )
            raise
        finally:
            event.code = None

    async def _finish_unconfirmed_sms_submit(self, waiter):
        if waiter.pending_submit is None:
            return
        if await self._verification_page_is_current(waiter):
            self._phase(
                waiter.job,
                "短信验证未确认；请核对验证码后再试，当前页面仍保留",
            )
        else:
            self._phase(
                waiter.job,
                "短信验证结果待核实；当前页面已变化，请先查询任务",
            )
        waiter.submit_pending = False
        waiter.pending_submit = None
        waiter.submit_timer = None

    async def _close_verification_waiter(self, waiter):
        job_id = waiter.job.get("job_id")
        if self._verification_waiters.get(job_id) is waiter:
            self._verification_waiters.pop(job_id, None)
        timer = waiter.submit_timer
        if timer is not None and not timer.done():
            timer.cancel()
            await asyncio.gather(timer, return_exceptions=True)
        waiter.submit_timer = None
        pending = []
        while not waiter.queue.empty():
            try:
                pending.append(waiter.queue.get_nowait())
            except asyncio.QueueEmpty:
                break
        if waiter.pending_submit is not None:
            pending.append(waiter.pending_submit)
        for event in pending:
            event.code = None
            if event.future.done():
                continue
            if waiter.job.get("state") == "published":
                result = self._verification_response(
                    waiter.job, "ready", "抖音页面已确认发布成功；没有重复点击发布。",
                )
            elif waiter.invalidated:
                result = self._verification_response(
                    waiter.job, "needs_attention", SMS_INVALIDATED_MESSAGE,
                )
            else:
                result = self._verification_response(
                    waiter.job, "in_progress", "短信验证窗口状态已变化；请查询同一任务，不要重复点击发布。",
                )
            self._resolve_verification_action(event, result)
        waiter.request_pending = False
        waiter.submit_pending = False
        waiter.pending_submit = None
        completion = waiter.invalidated_complete
        if completion is not None and not completion.done():
            completion.set_result(None)

    async def _verification_action(self, request):
        job_id = request["job_id"]
        job = self.jobs.get(job_id)
        waiter = self._current_verification_waiter(job_id)
        if waiter is None:
            invalidated_waiter = self._verification_waiters.get(job_id)
            if invalidated_waiter is not None and invalidated_waiter.invalidated:
                return await self._verification_invalidation_response(invalidated_waiter)
            return self._verification_response(
                job, "needs_attention", "没有唯一且有效的短信验证任务；没有请求短信或提交验证码。",
            )
        action = request["action"]
        if action == "verification_request_sms":
            if waiter.request_pending or waiter.submit_pending:
                return self._verification_response(
                    job,
                    "needs_attention",
                    "当前短信操作仍在处理；本次请求未被接收，请先查询同一任务状态。",
                )
        else:
            if waiter.sms_request_count < 1:
                return self._verification_response(
                    job, "needs_attention", "当前任务尚未请求短信验证码；没有提交验证码。",
                )
            if waiter.submit_pending or waiter.request_pending:
                return self._verification_response(
                    job,
                    "needs_attention",
                    "当前短信操作仍在处理；本次验证码未被接收，请查询同一任务后再提交。",
                )
        event = _VerificationAction(
            action=action,
            future=asyncio.get_running_loop().create_future(),
            code=request.get("code"),
        )
        if action == "verification_request_sms":
            waiter.request_pending = True
        else:
            waiter.submit_pending = True
        try:
            waiter.queue.put_nowait(event)
        except asyncio.QueueFull:
            event.code = None
            if action == "verification_request_sms":
                waiter.request_pending = False
            else:
                waiter.submit_pending = False
            return self._verification_response(
                job,
                "needs_attention",
                "当前短信操作正忙；本次验证码未被接收，请查询同一任务后再提交。",
            )
        try:
            result = await asyncio.shield(event.future)
            if waiter.invalidated:
                return await self._verification_invalidation_response(waiter)
            return result
        except asyncio.CancelledError:
            event.cancelled = True
            event.code = None
            if not event.future.done():
                event.future.cancel()
            if not event.started:
                if action == "verification_request_sms":
                    waiter.request_pending = False
                else:
                    waiter.submit_pending = False
            raise

    async def _publish(self, page, job, plan):
        page = await self._verified_stage(plan)
        await self._dismiss_preview_tip(page)
        publish = await self._button(page, ("发布",))
        if not await publish.is_enabled():
            raise ValueError("平台尚未允许发布")
        job["commit_attempted"] = True
        self._phase(job, "正式发布，结果待确认")
        self._staged.pop(plan["token"], None)
        await publish.click()
        success = page.get_by_text(re.compile(r"^(发布成功|作品发布成功|发布成功，正在审核)$")).first
        waiter = None
        event_task = None
        success_task = None
        invalidated_task = None
        try:
            await success.wait_for(state="visible", timeout=30000)
        except Exception:
            if not _sms_verification_visible(await self._ui(page)):
                raise
            context = self.browser._context
            try:
                pages = context.pages if context is not None else None
                page_is_in_context = isinstance(pages, (list, tuple)) and any(
                    candidate is page for candidate in pages
                )
            except Exception:
                page_is_in_context = False
            if not page_is_in_context:
                raise ValueError("短信验证页面不属于当前浏览器上下文") from None
            page_url = page.url
            if not _safe_editor_url(page_url):
                raise ValueError("短信验证页面地址已变化") from None
            marker = secrets.token_urlsafe(32)
            await page.evaluate(
                "marker => { window.__tangyugeVerificationProof = marker; }", marker
            )
            if await page.evaluate(
                "() => window.__tangyugeVerificationProof || null"
            ) != marker:
                raise ValueError("短信验证文档标记未能绑定到当前页面") from None
            job["verification_required"] = "sms"
            waiter = _VerificationWaiter(
                job=job,
                page=page,
                context=context,
                url=page_url,
                marker=marker,
                deadline=time.monotonic() + SMS_VERIFICATION_TIMEOUT,
                queue=asyncio.Queue(maxsize=1),
                invalidated_complete=asyncio.get_running_loop().create_future(),
            )
            self._verification_waiters[job["job_id"]] = waiter
            self._phase(job, "发布需本人短信验证；请在当前页面完成，尚未确认发布")
            # 只由持有 browser_lock 的本协程操作页面；UDS 请求只投递内存事件。
            success_task = asyncio.create_task(
                success.wait_for(state="visible", timeout=SMS_VERIFICATION_TIMEOUT * 1000)
            )
            invalidated_task = asyncio.create_task(waiter.invalidated_event.wait())
            try:
                while True:
                    if event_task is None:
                        event_task = asyncio.create_task(waiter.queue.get())
                    waiters = {success_task, event_task, invalidated_task}
                    submit_timer = waiter.submit_timer
                    if submit_timer is not None:
                        waiters.add(submit_timer)
                    done, _pending = await asyncio.wait(
                        waiters, return_when=asyncio.FIRST_COMPLETED,
                    )
                    if waiter.invalidated:
                        raise ValueError(SMS_INVALIDATED_PHASE)
                    if success_task in done:
                        if event_task in done:
                            # The page may have accepted a manual code at the same time;
                            # do not fill another code after the success signal.
                            queued = event_task.result()
                            waiter.queue.put_nowait(queued)
                            event_task = None
                        await success_task
                        break
                    if invalidated_task in done:
                        raise ValueError(SMS_INVALIDATED_PHASE)
                    if event_task in done:
                        event = event_task.result()
                        event_task = None
                        await self._process_verification_event(waiter, event)
                        continue
                    if submit_timer is not None and submit_timer in done:
                        await submit_timer
                        await self._finish_unconfirmed_sms_submit(waiter)
            except BaseException:
                if event_task is not None and not event_task.done():
                    event_task.cancel()
                if success_task is not None and not success_task.done():
                    success_task.cancel()
                if invalidated_task is not None and not invalidated_task.done():
                    invalidated_task.cancel()
                await asyncio.gather(
                    *(task for task in (event_task, success_task, invalidated_task) if task is not None),
                    return_exceptions=True,
                )
                if not waiter.invalidated:
                    await self._close_verification_waiter(waiter)
                raise
            finally:
                if event_task is not None and not event_task.done():
                    event_task.cancel()
                if invalidated_task is not None and not invalidated_task.done():
                    invalidated_task.cancel()
                await asyncio.gather(
                    *(task for task in (event_task, success_task, invalidated_task) if task is not None),
                    return_exceptions=True,
                )
        if waiter is not None and not await self._verification_document_is_current(waiter):
            raise ValueError(SMS_INVALIDATED_PHASE)
        job.pop("verification_required", None)
        job.update(state="published", phase="已提交发布", message="页面已确认提交发布；后续审核状态以平台为准。")
        self._save_job(job)
        if waiter is not None:
            await self._close_verification_waiter(waiter)

    async def _video_source(self, page, *, activate=False):
        if not _safe_editor_url(page.url):
            raise ValueError("视频预览不在允许的编辑页")
        if activate:
            preview = page.get_by_text("预览视频", exact=True).and_(page.locator(":visible"))
            if await preview.count() != 1:
                raise ValueError("无法唯一定位视频预览页签")
            await preview.click()
        deadline = time.monotonic() + (5 if activate else 0)
        while True:
            sources = await page.evaluate("""() => {
              const videos=Array.from(document.querySelectorAll('video')).filter(e=>e.offsetWidth&&e.offsetHeight);
              if(videos.length>10)return null;
              return [...new Set(videos.map(e=>e.currentSrc||e.src).filter(Boolean))];
            }""")
            if not isinstance(sources, list) or len(sources) > 1:
                raise ValueError("不能唯一确认本次上传视频的预览源")
            if len(sources) == 1:
                if not isinstance(sources[0], str) or not 1 <= len(sources[0]) <= 4096 or not _safe_editor_url(page.url):
                    raise ValueError("视频预览源或编辑页发生变化")
                return sources[0]
            if time.monotonic() >= deadline:
                raise ValueError("当前没有可核实的视频预览源，不恢复或猜测旧视频")
            await asyncio.sleep(0.1)

    async def _metrics(self):
        page = await self._account_page()
        try:
            label = page.get_by_text("粉丝指数", exact=True).and_(page.locator(":visible"))
            await label.first.wait_for(state="visible", timeout=8000)
        except Exception:
            LOGGER.info("首页统计卡片尚未显示，缺失字段不会记为零。")
        fields = await page.evaluate("""() => {
          const labels = ['关注','粉丝指数','获赞','粉丝总数','总粉丝','获赞总数','总获赞','作品总数','播放量','点赞量','评论量','分享量'];
          const fields = [];
          for (const label of labels) {
            const nodes = Array.from(document.querySelectorAll('span,div,p,a')).filter(e=>
              (e.offsetWidth||e.offsetHeight)&&e.innerText.trim()===label);
            const values = new Set();
            for (const node of nodes) {
              let scope=node;
              for(let depth=0;depth<4;depth++){
                scope=scope.parentElement;
                if(!scope||['BODY','HTML'].includes(scope.tagName)||scope.innerText.length>100)break;
                const text=scope.innerText.replace(/\\s+/g,' ').trim();
                const before=new RegExp('^([0-9]+(?:[.,][0-9]+)*(?:万|亿|[kKwW])?)\\\\s*'+label+'$');
                const after=new RegExp('^'+label+'\\\\s*([0-9]+(?:[.,][0-9]+)*(?:万|亿|[kKwW])?)$');
                const match=text.match(before)||text.match(after);
                if(match){values.add(match[1]);break;}
              }
            }
            if(values.size===1)fields.push({label,value:[...values][0]});
          }
          return fields;
        }""")
        message = "仅返回创作者中心首页当前可见数据，不是官方统计API。" if fields else "当前页面没有可可靠提取的统计字段，不能据此认为数值为零。"
        return response("ready", message, metrics={"source": HOME_URL, "collected_at": utc_now(), "fields": fields})

    async def close(self):
        tasks = list(self.tasks.values())
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
