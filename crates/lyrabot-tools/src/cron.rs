//! cron：定时任务（5 段 cron 表达式，按北京时间解释）。
//! 触发动作由运行时执行：remind → 交给主对话角色提醒用户；task → 派给后台执行者。
//! 动作带 `once: true` 的任务触发一次后由运行时删除（具体某天的提醒）。

use crate::util::{arg_str, now_ms};
use async_trait::async_trait;
use croner::Cron;
use jiff::{Timestamp, tz::TimeZone};
use lyrabot_agent::{Tool, ToolCtx, ToolOutput};
use lyrabot_core::ToolSpec;
use lyrabot_store::{CronJob, Store};
use serde_json::{Value, json};
use std::str::FromStr;

fn tz() -> TimeZone {
    TimeZone::get("Asia/Shanghai").unwrap_or(TimeZone::fixed(jiff::tz::offset(8)))
}

pub fn parse(schedule: &str) -> Result<Cron, String> {
    let fields = schedule.split_whitespace().count();
    if fields != 5 {
        return Err(format!("需要 5 段 cron 表达式（分 时 日 月 周），收到 {fields} 段"));
    }
    Cron::from_str(schedule).map_err(|e| format!("cron 表达式无效：{e}"))
}

/// `after` 之后（不含）的下一次触发时刻。
pub fn next_after(schedule: &str, after: Timestamp) -> Option<Timestamp> {
    let cron = parse(schedule).ok()?;
    cron.find_next_occurrence(&after.to_zoned(tz()), false).ok().map(|z| z.timestamp())
}

/// 到期的任务：以上次运行（或创建）时间为基准的下一次触发已不晚于 `now`。
/// 停机期间错过的多次触发只补一次。
pub fn due(jobs: &[CronJob], now: Timestamp) -> Vec<CronJob> {
    jobs.iter()
        .filter(|j| j.enabled)
        .filter(|j| {
            let base = Timestamp::from_millisecond(j.last_run.unwrap_or(j.created_at)).unwrap_or(now);
            next_after(&j.schedule, base).is_some_and(|n| n <= now)
        })
        .cloned()
        .collect()
}

fn fmt_ts(t: Timestamp) -> String {
    t.to_zoned(tz()).strftime("%m-%d %H:%M").to_string()
}

/// 是否只触发一次。
pub fn is_once(job: &CronJob) -> bool {
    job.action["once"].as_bool().unwrap_or(false)
}

pub fn render(jobs: &[CronJob]) -> String {
    if jobs.is_empty() {
        return "没有定时任务".into();
    }
    let now = Timestamp::now();
    jobs.iter()
        .map(|j| {
            let what = match j.action["kind"].as_str() {
                Some("task") => format!("后台任务（{}）：{}", j.action["to"].as_str().unwrap_or("worker"), j.action["task"].as_str().unwrap_or("")),
                _ => format!("提醒：{}", j.action["text"].as_str().unwrap_or("")),
            };
            let next = if j.enabled { next_after(&j.schedule, now).map(fmt_ts).unwrap_or("-".into()) } else { "已暂停".into() };
            let repeat = if is_once(j) { "仅一次" } else { j.schedule.as_str() };
            format!("#{} {} [{repeat}] 下次 {next} · {what}", j.id, j.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub struct CronTool {
    spec: ToolSpec,
    store: Store,
}

impl CronTool {
    pub fn new(store: Store, targets: &[String]) -> Self {
        let mut to = vec!["worker".to_owned()];
        to.extend(targets.iter().cloned());
        let spec = ToolSpec {
            name: "cron".into(),
            description: "管理定时任务（时间按北京时间）。schedule 是 5 段 cron：分 时 日 月 周，例如每天 9 点“0 9 * * *”、工作日 18:30“30 18 * * 1-5”。\
                          kind=remind 到点由你提醒对方 text 的内容；kind=task 到点把 task 派给后台（to 选执行者或专员），结果再交回给你。                          只在某一天发生的事（“明天 9 点”“11 月 10 日”）要设 once=true，触发一次后自动删除；不设就是每年、每月或每天重复。\
                          add 前先和对方确认时间与内容。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "list", "remove", "pause", "resume"]},
                    "id": {"type": "integer"},
                    "name": {"type": "string", "description": "简短名称"},
                    "schedule": {"type": "string"},
                    "kind": {"type": "string", "enum": ["remind", "task"]},
                    "text": {"type": "string", "description": "remind 的提醒内容"},
                    "task": {"type": "string", "description": "task 的任务说明"},
                    "to": {"type": "string", "enum": to},
                    "once": {"type": "boolean", "description": "只触发一次，触发后自动删除"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        };
        Self { spec, store }
    }
}

#[async_trait]
impl Tool for CronTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn call(&self, _ctx: ToolCtx, args: Value) -> ToolOutput {
        let id = args.get("id").and_then(Value::as_i64);
        let r: Result<String, String> = async {
            match arg_str(&args, "action") {
                Some("list") => Ok(render(&self.store.cron_list().await.map_err(|e| e.to_string())?)),
                Some("add") => {
                    let schedule = arg_str(&args, "schedule").ok_or("add 需要 schedule")?.trim();
                    parse(schedule)?;
                    let name = arg_str(&args, "name").unwrap_or("定时任务");
                    let once = args.get("once").and_then(Value::as_bool).unwrap_or(false);
                    let mut action = match arg_str(&args, "kind") {
                        Some("task") => json!({"kind": "task", "task": arg_str(&args, "task").ok_or("task 类型需要 task")?, "to": arg_str(&args, "to").unwrap_or("worker")}),
                        _ => json!({"kind": "remind", "text": arg_str(&args, "text").ok_or("remind 类型需要 text")?}),
                    };
                    if once {
                        action["once"] = json!(true);
                    }
                    let id = self.store.cron_add(name, schedule, action).await.map_err(|e| e.to_string())?;
                    let next = next_after(schedule, Timestamp::now()).map(fmt_ts).unwrap_or("-".into());
                    let repeat = if once { "，只触发这一次" } else { "，之后按规则重复" };
                    Ok(format!("已添加 #{id}，下次触发 {next}{repeat}"))
                }
                Some("remove") => {
                    let id = id.ok_or("remove 需要 id")?;
                    if self.store.cron_remove(id).await.map_err(|e| e.to_string())? { Ok(format!("已删除 #{id}")) } else { Err(format!("没有 #{id}")) }
                }
                Some(a @ ("pause" | "resume")) => {
                    let id = id.ok_or("需要 id")?;
                    let enabled = a == "resume";
                    if enabled {
                        // 恢复时以当前时间为基准，避免把暂停期间错过的触发补发出来
                        self.store.cron_mark_run(id, now_ms()).await.map_err(|e| e.to_string())?;
                    }
                    self.store.cron_set_enabled(id, enabled).await.map_err(|e| e.to_string())?;
                    Ok(format!("已{} #{id}", if enabled { "恢复" } else { "暂停" }))
                }
                _ => Err("action 无效".into()),
            }
        }
        .await;
        match r {
            Ok(t) => ToolOutput::ok(t),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(schedule: &str, last: i64) -> CronJob {
        CronJob { id: 1, name: "x".into(), schedule: schedule.into(), action: json!({"kind":"remind","text":"t"}), enabled: true, last_run: Some(last), created_at: 0 }
    }

    #[test]
    fn beijing_time_and_due() {
        // 2026-11-07 00:30 UTC = 08:30 北京
        let t: Timestamp = "2026-11-07T00:30:00Z".parse().unwrap();
        let n = next_after("0 9 * * *", t).unwrap();
        assert_eq!(n.to_string(), "2026-11-07T01:00:00Z");
        assert!(parse("0 9 * *").is_err() && parse("x y z a b").is_err());
        let last = t.as_millisecond();
        assert!(due(&[job("0 9 * * *", last)], "2026-11-07T00:59:00Z".parse().unwrap()).is_empty());
        assert_eq!(due(&[job("0 9 * * *", last)], "2026-11-07T01:00:30Z".parse().unwrap()).len(), 1);
        let mut paused = job("* * * * *", last);
        paused.enabled = false;
        assert!(due(&[paused], "2026-11-08T00:00:00Z".parse().unwrap()).is_empty());
    }

    #[test]
    fn once_flag_rendered() {
        let mut j = job("0 9 10 11 *", 0);
        assert!(!is_once(&j) && render(std::slice::from_ref(&j)).contains("[0 9 10 11 *]"));
        j.action["once"] = json!(true);
        assert!(is_once(&j) && render(&[j]).contains("[仅一次]"));
    }
}
