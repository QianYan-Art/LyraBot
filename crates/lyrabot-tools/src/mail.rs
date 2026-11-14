//! mail：邮件 VPS 操作。复用 Hermes 已验证的 mail_vps_fetch.py（只用 Python 标准库，经受限 SSH key 连接），
//! 这里负责把结构化参数映射为命令行、解析 JSON 结果、把附件与验证链接文件交给投递通道。
//! 写操作（发信/回复/转发/移到废纸篓/删除）不重试：helper 本身也不重试，超时不代表未执行。

use crate::util::{Audit, arg_bool, arg_str, arg_u64, exec, now_ms, truncate_middle};
use async_trait::async_trait;
use lyrabot_agent::{Tool, ToolCtx, ToolOutput};
use lyrabot_config::MailSettings;
use lyrabot_core::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

const ACTIONS: &[(&str, &str, bool)] = &[
    // (动作, helper 子命令, 是否写操作)
    ("list_mailboxes", "list-mailboxes", false),
    ("read", "read-mail", false),
    ("list_attachments", "list-attachments", false),
    ("fetch_attachment", "fetch-attachment", false),
    ("send", "send-mail", true),
    ("reply", "reply-mail", true),
    ("forward", "forward-mail", true),
    ("trash", "move-mail-to-trash", true),
    ("delete", "delete-mail", true),
];

pub struct Mail {
    spec: ToolSpec,
    cfg: MailSettings,
    audit: Audit,
}

impl Mail {
    pub fn new(cfg: MailSettings, audit: Audit) -> Self {
        let actions: Vec<&str> = ACTIONS.iter().map(|a| a.0).collect();
        let spec = ToolSpec {
            name: "mail".into(),
            description: "操作邮件 VPS（可托管多个域名，以查询结果为准）。\
                list_mailboxes 列出邮箱：没有明确指定域名时不要传 domain 和 limit；若 matched 大于返回数说明被截断；aliases 是别名及其实际邮箱。\
                别名可以直接作 email 读信（读的是实际邮箱，两者共用同一个收件箱），也可以作 from_email 发信；回复发给别名的邮件时用该别名作发件人。\
                read 读最新邮件（默认 1 封），可用 from_contains/subject_contains/body_contains 筛选；找验证码加 require_code，找验证链接加 require_link。\
                list_attachments / fetch_attachment 取附件（附件文件会自动随回复发出）。验证链接较长时会导出成 txt 文件一并发出，文字里只能逐字原样给出链接，不得改写或打码。\
                send / reply / forward / trash / delete 是写操作：只执行任务里明确要求的那一封、那个收件人；失败或超时不要重试，先用 read 核实结果。\
                后续操作需要 message_ref，读邮件时保留它。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": actions},
                    "email": {"type": "string", "description": "完整邮箱地址（列邮箱以外的操作必填）"},
                    "domain": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1},
                    "message_ref": {"type": "string"},
                    "from_contains": {"type": "string"},
                    "subject_contains": {"type": "string"},
                    "body_contains": {"type": "string"},
                    "require_code": {"type": "boolean"},
                    "require_link": {"type": "boolean"},
                    "has_attachment": {"type": "boolean"},
                    "attachment_index": {"type": "integer", "minimum": 1},
                    "attachment_name": {"type": "string"},
                    "from_email": {"type": "string", "description": "发件邮箱（send/reply/forward）"},
                    "to": {"type": "string"},
                    "cc": {"type": "string"},
                    "subject": {"type": "string"},
                    "body": {"type": "string"},
                    "reply_all": {"type": "boolean"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        };
        Self { spec, cfg, audit }
    }
}

/// 把结构化参数映射为 helper 的命令行参数。返回（argv, 是否写操作）。
pub fn build_argv(args: &Value) -> Result<(Vec<String>, bool), String> {
    let action = arg_str(args, "action").ok_or("缺少 action")?;
    let &(_, sub, write) = ACTIONS.iter().find(|a| a.0 == action).ok_or_else(|| format!("未知动作 {action}"))?;
    let mut argv = vec![sub.to_owned()];
    let mut opt = |flag: &str, key: &str| {
        if let Some(v) = arg_str(args, key) {
            argv.push(flag.to_owned());
            argv.push(v.to_owned());
        }
    };
    if action == "list_mailboxes" {
        opt("--domain", "domain");
    } else if action != "send" {
        opt("--email", "email");
        opt("--message-ref", "message_ref");
        opt("--from-contains", "from_contains");
        opt("--subject-contains", "subject_contains");
        opt("--body-contains", "body_contains");
    }
    match action {
        "fetch_attachment" => opt("--attachment-name", "attachment_name"),
        "send" => {
            opt("--from-email", "from_email");
            opt("--to", "to");
            opt("--cc", "cc");
            opt("--subject", "subject");
            opt("--body", "body");
        }
        "reply" => {
            opt("--from-email", "from_email");
            opt("--body", "body");
        }
        "forward" => {
            opt("--from-email", "from_email");
            opt("--to", "to");
            opt("--body", "body");
        }
        _ => {}
    }
    if matches!(action, "list_mailboxes" | "read")
        && let Some(n) = arg_u64(args, "limit")
    {
        argv.extend(["--limit".into(), n.to_string()]);
    }
    if action == "fetch_attachment"
        && let Some(n) = arg_u64(args, "attachment_index")
    {
        argv.extend(["--attachment-index".into(), n.to_string()]);
    }
    if action != "list_mailboxes" && action != "send" {
        for (key, flag) in [("require_code", "--require-code"), ("require_link", "--require-link"), ("has_attachment", "--has-attachment")] {
            if arg_bool(args, key) {
                argv.push(flag.into());
            }
        }
    }
    if action == "reply" && arg_bool(args, "reply_all") {
        argv.push("--reply-all".into());
    }
    // 必填校验，避免把不完整的写操作交给远端
    let need = |k: &str| arg_str(args, k).map(|_| ()).ok_or(format!("{action} 需要 {k}"));
    match action {
        "list_mailboxes" => {}
        "send" => {
            for k in ["from_email", "to", "subject", "body"] {
                need(k)?;
            }
        }
        _ => need("email")?,
    }
    if matches!(action, "reply" | "forward" | "trash" | "delete") {
        need("message_ref")?;
    }
    if matches!(action, "reply" | "forward") {
        need("from_email")?;
        need(if action == "reply" { "body" } else { "to" })?;
    }
    Ok((argv, write))
}

/// 从 helper 输出中收集需要投递的文件（media_tag / preferred_media_tag）。
fn media_paths(v: &Value, out: &mut Vec<PathBuf>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if (k == "media_tag" || k == "preferred_media_tag")
                    && let Some(p) = x.as_str().and_then(|s| s.strip_prefix("MEDIA:"))
                {
                    let p = PathBuf::from(p.trim());
                    if !out.contains(&p) {
                        out.push(p);
                    }
                } else {
                    media_paths(x, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| media_paths(x, out)),
        _ => {}
    }
}

#[async_trait]
impl Tool for Mail {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, ctx: ToolCtx, args: Value) -> ToolOutput {
        let (argv, write) = match build_argv(&args) {
            Ok(x) => x,
            Err(e) => return ToolOutput::err(e),
        };
        let mut full = vec![self.cfg.helper.display().to_string()];
        full.extend(argv.iter().cloned());
        let env = vec![("LYRABOT_MAIL_VPS_CONFIG".to_owned(), self.cfg.config.display().to_string())];
        let res = exec(&self.cfg.python, &full, None, &env, Duration::from_secs(180), &ctx.cancel).await;
        self.audit
            .record(json!({"ts": now_ms(), "session": ctx.session, "tool": "mail", "action": args["action"], "write": write,
                           "email": args.get("email"), "to": args.get("to"), "ok": res.as_ref().map(|o| o.code == Some(0)).unwrap_or(false)}))
            .await;
        let out = match res {
            Ok(o) => o,
            Err(e) => return ToolOutput::err(format!("无法执行邮件 helper：{e}")),
        };
        let stdout = out.output.split("[stderr]").next().unwrap_or_default().trim();
        let parsed: Option<Value> = serde_json::from_str(stdout).ok();
        let mut media = Vec::new();
        if let Some(v) = &parsed {
            media_paths(v, &mut media);
        }
        let ok = parsed.as_ref().and_then(|v| v.get("ok")).and_then(Value::as_bool).unwrap_or(false) && !out.timed_out;
        let mut text = truncate_middle(if stdout.is_empty() { &out.output } else { stdout }, 12_000);
        if out.timed_out {
            text.push_str("\n[超时]");
        }
        if write && !ok {
            text.push_str("\n写操作未确认成功：不要重试，先用 read 核实远端实际状态再汇报。");
        }
        if !media.is_empty() {
            text.push_str(&format!("\n已准备发送 {} 个文件。", media.len()));
        }
        ToolOutput { text, is_error: !ok, media }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_mapping() {
        let (a, w) = build_argv(&json!({"action": "list_mailboxes"})).unwrap();
        assert_eq!((a, w), (vec!["list-mailboxes".to_owned()], false));
        let (a, _) = build_argv(&json!({"action": "read", "email": "x@mail.example.com", "require_link": true, "limit": 3})).unwrap();
        assert_eq!(a, ["read-mail", "--email", "x@mail.example.com", "--limit", "3", "--require-link"]);
        let (a, w) = build_argv(&json!({"action": "send", "from_email": "a@b", "to": "c@d", "subject": "s", "body": "b"})).unwrap();
        assert!(w && a[0] == "send-mail" && a.contains(&"--subject".to_owned()));
        assert!(build_argv(&json!({"action": "send", "to": "c@d"})).unwrap_err().contains("from_email"));
        assert!(build_argv(&json!({"action": "delete", "email": "a@b"})).unwrap_err().contains("message_ref"));
        let (a, _) = build_argv(&json!({"action": "reply", "email": "a@b", "message_ref": "cur/1", "from_email": "a@b", "body": "hi", "reply_all": true})).unwrap();
        assert!(a.ends_with(&["--reply-all".to_owned()]));
    }

    #[test]
    fn media_from_payload() {
        let v = json!({"ok": true, "media_tag": "MEDIA:/tmp/a.pdf", "items": [{"preferred_media_tag": "MEDIA:/tmp/l.txt"}]});
        let mut m = Vec::new();
        media_paths(&v, &mut m);
        m.sort();
        assert_eq!(m, vec![PathBuf::from("/tmp/a.pdf"), PathBuf::from("/tmp/l.txt")]);
    }
}
