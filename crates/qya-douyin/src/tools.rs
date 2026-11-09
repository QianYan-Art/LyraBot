//! 抖音工具（给创作专员）：douyin_login、douyin_content（不含 publish）、douyin_verification。

use crate::covers::{decode_covers, save_qr, saved_sources};
use crate::sanitize::{is_hex32, is_release_id, is_sha256, is_token, scan};
use crate::Douyin;
use async_trait::async_trait;
use base64::Engine;
use qya_agent::{Tool, ToolCtx, ToolOutput};
use qya_core::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

fn reply(v: Value, media: Vec<PathBuf>) -> ToolOutput {
    let is_error = v["ok"] == false;
    ToolOutput { text: serde_json::to_string(&v).unwrap_or_default(), is_error, media }
}

fn fail(state: &str, msg: &str) -> ToolOutput {
    reply(json!({"ok": false, "state": state, "message": msg}), Vec::new())
}

pub struct DouyinLogin {
    spec: ToolSpec,
    dy: Arc<Douyin>,
}

impl DouyinLogin {
    pub fn new(dy: Arc<Douyin>) -> Self {
        let spec = ToolSpec {
            name: "douyin_login".into(),
            description: "检查服务器上的抖音登录，或应要求/登录失效时获取扫码二维码。status 不刷新页面；login 只取登录二维码，不上传、不发布。\
                二维码会自动随结果发出，说明用抖音手机端扫码即可。已有二维码时不要重复 login，扫码完成后用 status 检查。\
                验证受阻请人工处理，不反复重试。服务器离线或页面未知不等于已退出登录。".into(),
            parameters: json!({"type": "object", "properties": {"action": {"type": "string", "enum": ["status", "login"]}}, "required": ["action"], "additionalProperties": false}),
        };
        Self { spec, dy }
    }
}

/// 登录响应：只保留 ok/state/message，二维码落盘为媒体。
async fn login_response(dy: &Douyin, action: &str, recover: bool) -> Result<(Value, Vec<PathBuf>), String> {
    let r = dy.client.login(action, recover).await?;
    let mut out = json!({"ok": r["ok"], "state": r["state"], "message": r["message"]});
    let mut media = Vec::new();
    if let Some(qr) = r.get("qr_png_base64").and_then(Value::as_str) {
        if action != "login" || r["state"] != "login_required" {
            return Err("意外的二维码响应".into());
        }
        media.push(save_qr(qr, &dy.cache_dir)?);
        out["delivery"] = json!("二维码会随结果发出，提醒对方用抖音手机端扫码。");
    }
    Ok((out, media))
}

#[async_trait]
impl Tool for DouyinLogin {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let action = args["action"].as_str().unwrap_or_default();
        if !matches!(action, "status" | "login") {
            return fail("unavailable", "仅支持 status 或 login。");
        }
        match login_response(&self.dy, action, false).await {
            Ok((v, m)) => reply(v, m),
            Err(e) => fail("unavailable", &format!("抖音登录服务暂不可用或响应无效（{e}）；未修改现有登录数据。")),
        }
    }
}

pub struct DouyinContent {
    spec: ToolSpec,
    dy: Arc<Douyin>,
}

const ACTIONS: &[&str] = &[
    "list", "metrics", "covers", "cover_sources", "cover_import", "cover_saved_sources", "cover_import_saved", "cover_pair",
    "cover_library", "cover_reuse", "prepare", "stage", "resume_stage", "draft", "job",
];

impl DouyinContent {
    pub fn new(dy: Arc<Douyin>) -> Self {
        let spec = ToolSpec {
            name: "douyin_content".into(),
            description: "管理抖音作品。先用 list 选择服务器上的成品；后台横封面槽为 4:3、竖封面槽为 3:4；9:16 只是可能的素材版本，不是竖槽比例。\
已有封面优先用 cover_library 浏览（可传 release_id 筛选、offset 按 next_offset 翻页），区分来源并检查双槽预览。\
已在固定制作目录放好素材时，用 cover_saved_sources 加 release_id 查看本地候选及实际比例，不要让用户重新发送或传任意路径；分别用两张候选的 image_id 调 cover_import_saved 入库，再用 cover_pair 传 landscape_cover_id 和 portrait_cover_id 组合，单图导入只是一个候选。\
QQ 图片可用 cover_sources / cover_import 单图入库。复用库中封面用 cover_reuse（目标 release_id、来源 from_release_id、来源 cover_id），取得目标的新 cover_id 后再 prepare，不能把别的作品的旧 ID 直接用于目标作品。\
封面预览会自动发出供核对。选定后用 prepare（必须传 cover_id）确认标题、简介、封面、可见范围和可选定时（publish_at 带时区）；prepare 返回 plan.plan_id，stage/resume_stage/draft 必须完整传入，不缩略。\
需要发布时用 stage 上传并生成平台待确认预览；editor_ready 只表示预览就绪，不等于已获授权。要存草稿就调用 draft。\
公开发布不由你执行：stage 完成后提醒在 QQ 私聊发送“确认发布 <plan_id>”，程序会直接提交并跟进；你没有、也不需要发布动作。\
上传是后台任务，in_progress 时如实汇报当前阶段并结束本轮，不要循环轮询；之后可用 job 查询同一任务。预览最长有效 15 分钟，stage 后不要调用 login/status/metrics 或另一次 prepare（会导航使预览失效），job 查询不会导航。\
按 recovery.action 处理错误：resume_stage 用相同 plan_id 调 resume_stage；prepare 表示可重新准备；login 会自动附二维码；manual_verification/check_account 需要人工处理；inspect_result 表示已尝试提交且结果待核查，禁止重试。\
合集、位置、下载权限、付费与商品链接尚未接入，被要求时说明限制。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ACTIONS},
                    "release_id": {"type": "string"},
                    "from_release_id": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 0, "maximum": 10000},
                    "image_id": {"type": "string"},
                    "landscape_cover_id": {"type": "string"},
                    "portrait_cover_id": {"type": "string"},
                    "times": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"type": "number", "minimum": 0}},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "cover_id": {"type": "string"},
                    "visibility": {"type": "string", "enum": ["public", "friends", "private"]},
                    "publish_at": {"type": "string"},
                    "plan_id": {"type": "string", "description": "prepare 返回的完整计划引用"},
                    "job_id": {"type": "string"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        };
        Self { spec, dy }
    }
}

fn keys(args: &Value) -> Vec<&str> {
    args.as_object().map(|m| m.keys().map(String::as_str).collect()).unwrap_or_default()
}

/// 参数校验（与插件 _content_payload 一致，publish 已移除）。
pub fn validate(args: &Value) -> Result<String, String> {
    let a = args["action"].as_str().ok_or("缺少 action")?.to_owned();
    let k = keys(args);
    let only = |allowed: &[&str]| k.iter().all(|x| allowed.contains(x));
    let has = |req: &[&str]| req.iter().all(|x| k.contains(x));
    let s = |f: &str| args[f].as_str().unwrap_or_default();
    let ok = match a.as_str() {
        "list" | "metrics" | "cover_sources" => k == ["action"],
        "covers" => {
            has(&["release_id"]) && only(&["action", "release_id", "times"]) && is_release_id(s("release_id"))
                && args.get("times").is_none_or(|t| t.as_array().is_some_and(|a| (1..=5).contains(&a.len()) && a.iter().all(|x| x.as_f64().is_some_and(|f| f.is_finite() && (0.0..=1e12).contains(&f)))))
        }
        "cover_saved_sources" => only(&["action", "release_id"]) && is_release_id(s("release_id")),
        "cover_library" => only(&["action", "release_id", "offset"]) && args.get("release_id").is_none_or(|_| is_release_id(s("release_id"))),
        "cover_reuse" => only(&["action", "release_id", "from_release_id", "cover_id"]) && is_release_id(s("release_id")) && is_release_id(s("from_release_id")) && is_sha256(s("cover_id")),
        "cover_import" | "cover_import_saved" => only(&["action", "release_id", "image_id"]) && is_release_id(s("release_id")) && is_hex32(s("image_id")),
        "cover_pair" => {
            has(&["release_id", "landscape_cover_id", "portrait_cover_id"]) && only(&["action", "release_id", "landscape_cover_id", "portrait_cover_id", "from_release_id"])
                && is_release_id(s("release_id")) && is_sha256(s("landscape_cover_id")) && is_sha256(s("portrait_cover_id"))
                && args.get("from_release_id").is_none_or(|_| is_release_id(s("from_release_id")))
        }
        "prepare" => {
            has(&["release_id", "cover_id"]) && only(&["action", "release_id", "title", "description", "cover_id", "visibility", "publish_at"])
                && is_release_id(s("release_id")) && is_sha256(s("cover_id"))
                && args.get("publish_at").is_none_or(|v| v.as_str().is_some_and(|p| p.parse::<jiff::Timestamp>().is_ok()))
        }
        "stage" | "resume_stage" | "draft" => only(&["action", "plan_id"]) && is_token(s("plan_id")),
        "job" => only(&["action", "job_id"]) && is_hex32(s("job_id")),
        "publish" => return Err("公开发布由用户在 QQ 发送“确认发布 <plan_id>”后由程序直接提交，你不能调用发布。".into()),
        _ => false,
    };
    if ok { Ok(a) } else { Err(format!("{a} 的参数无效；未执行任何操作。stage/draft 使用 prepare 返回的完整 plan_id。")) }
}

impl DouyinContent {
    async fn run(&self, args: Value) -> Result<ToolOutput, String> {
        let action = validate(&args)?;
        let dy = &self.dy;
        // 纯本地动作
        if action == "cover_sources" {
            return Ok(reply(json!({"ok": true, "state": "ready", "message": "最近收到的 QQ 私聊图片候选；选择后用 cover_import 导入。", "cover_sources": dy.cover_candidates()}), vec![]));
        }
        if action == "cover_saved_sources" {
            let release = args["release_id"].as_str().unwrap_or_default();
            return Ok(match saved_sources(&dy.saved_root, release) {
                Err(e) => reply(json!({"ok": false, "state": "needs_attention", "message": format!("固定制作目录或素材无效：{e}"), "saved_sources": []}), vec![]),
                Ok((false, _)) => reply(json!({"ok": true, "state": "ready", "message": "固定制作目录尚未交付，没有可列出的本地封面素材。", "saved_sources": []}), vec![]),
                Ok((true, recs)) => {
                    let list: Vec<Value> = recs.into_iter().map(|r| r.0).collect();
                    let msg = if list.is_empty() { "该作品目录中没有符合命名、尺寸和比例要求的本地封面素材。" } else { "已列出该作品固定制作目录中的规范图片素材；image_id 只标识当前文件内容。" };
                    reply(json!({"ok": true, "state": "ready", "message": msg, "saved_sources": list}), vec![])
                }
            });
        }
        let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
        let request = match action.as_str() {
            "stage" | "resume_stage" | "draft" => json!({"action": action, "token": args["plan_id"]}),
            "cover_import" => {
                let path = dy.registered_image(args["image_id"].as_str().unwrap_or_default()).ok_or("这张图片来源已过期或不可用；请重新发送图片。")?;
                let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if !meta.file_type().is_file() || meta.len() > crate::covers::MAX_IMAGE_BYTES {
                    return Err("图片来源不可用".into());
                }
                let data = std::fs::read(&path).map_err(|e| e.to_string())?;
                json!({"action": "cover_import", "release_id": args["release_id"], "image_base64": b64(&data)})
            }
            "cover_import_saved" => {
                let release = args["release_id"].as_str().unwrap_or_default();
                let id = args["image_id"].as_str().unwrap_or_default();
                let (_, recs) = saved_sources(&dy.saved_root, release)?;
                let data = recs.into_iter().find(|r| r.0["image_id"] == id).map(|r| r.1).ok_or("该制作素材已变更或不可用；请重新查看 cover_saved_sources。")?;
                json!({"action": "cover_import_saved", "release_id": release, "image_base64": b64(&data)})
            }
            _ => args.clone(),
        };
        let mut v = dy.client.content(request).await.map_err(|e| format!("抖音内容服务暂不可用或响应无效（{e}）；操作结果未知，请先查询状态，不要重复提交。"))?;
        let mut media = Vec::new();
        if let Some(covers) = v.get("covers").cloned() {
            let (clean, files) = decode_covers(&covers, None, &dy.cache_dir)?;
            v["covers"] = clean;
            media.extend(files);
        } else if let Some(cover) = v.get("cover").cloned() {
            let expected = (action == "cover_pair").then_some("paired_images");
            let (clean, files) = decode_covers(&json!([cover]), expected, &dy.cache_dir)?;
            v["cover"] = clean[0].clone();
            media.extend(files);
        }
        scan(&v)?;
        if action == "prepare"
            && let Some(plan) = v.get_mut("plan").and_then(Value::as_object_mut)
        {
            // 模型只看稳定的计划引用 plan_id
            let token = plan.remove("token").and_then(|t| t.as_str().map(str::to_owned)).filter(|t| is_token(t)).ok_or("准备响应缺少有效计划引用")?;
            plan.insert("plan_id".into(), json!(token));
        }
        if matches!(action.as_str(), "prepare" | "metrics" | "job") && v["state"] == "login_required" && v.pointer("/recovery/action") == Some(&json!("login")) {
            match login_response(dy, "login", true).await {
                Ok((login, m)) => {
                    v["login"] = login;
                    media.extend(m);
                }
                Err(_) => v["login"] = json!({"ok": false, "state": "unavailable", "message": "未能取得登录二维码；请检查状态，不重提作品。"}),
            }
            v["recovery_instruction"] = json!("先说明登录状态；有二维码时已随结果发出。扫码后检查登录状态，重新 prepare 核对作品；旧预览不沿用，不自动重新上传。");
        }
        Ok(reply(v, media))
    }
}

#[async_trait]
impl Tool for DouyinContent {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        match self.run(args).await {
            Ok(o) => o,
            Err(e) => fail("needs_attention", &e),
        }
    }
}

pub struct DouyinVerification {
    spec: ToolSpec,
    dy: Arc<Douyin>,
}

impl DouyinVerification {
    pub fn new(dy: Arc<Douyin>) -> Self {
        let spec = ToolSpec {
            name: "douyin_verification".into(),
            description: "发布任务明确等待短信验证、而程序没有自动发码时，传当前 job_id 请求发送验证码；不重新上传或再点发布。\
                成功后提醒在 QQ 私聊键入纯文本六位数字，程序会安全接收、代填并点击验证。不要向工具参数传验证码。发码不等于发布成功，不要反复发码。".into(),
            parameters: json!({"type": "object", "properties": {"job_id": {"type": "string"}}, "required": ["job_id"], "additionalProperties": false}),
        };
        Self { spec, dy }
    }
}

#[async_trait]
impl Tool for DouyinVerification {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let job = args["job_id"].as_str().unwrap_or_default();
        if keys(&args) != ["job_id"] || !is_hex32(job) {
            return fail("needs_attention", "请提供当前等待短信验证的完整任务编号；不要把验证码放进工具参数。");
        }
        match self.dy.request_sms(job).await {
            Ok(v) => reply(v, vec![]),
            Err(e) => fail("needs_attention", &e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::validate;
    use serde_json::json;

    #[test]
    fn payload_validation() {
        let h64 = "a".repeat(64);
        assert_eq!(validate(&json!({"action": "list"})).unwrap(), "list");
        assert!(validate(&json!({"action": "list", "x": 1})).is_err());
        assert!(validate(&json!({"action": "prepare", "release_id": "ep02", "cover_id": h64, "publish_at": "2026-11-08T20:00:00+08:00"})).is_ok());
        assert!(validate(&json!({"action": "prepare", "release_id": "ep02", "cover_id": h64, "publish_at": "2026-11-08 20:00"})).is_err(), "定时必须带时区");
        assert!(validate(&json!({"action": "stage", "plan_id": "abcdefghijklmnopqrstuvwxyz012-_9"})).is_ok());
        assert!(validate(&json!({"action": "stage", "plan_id": "abc***"})).is_err());
        assert!(validate(&json!({"action": "publish"})).unwrap_err().contains("确认发布"));
        assert!(validate(&json!({"action": "covers", "release_id": "ep02", "times": [1, 2.5]})).is_ok());
        assert!(validate(&json!({"action": "covers", "release_id": "ep02", "times": []})).is_err());
    }
}
