//! 聊天命令解析。命令不经过模型，立即响应；帮助文本与解析共用一张表，保证一致。
//! 完整命令设计见 docs/07-platform-commands.md 第 4 节，后续阶段逐步补齐。

use qya_core::Effort;
use qya_core::card::Card;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help(Option<String>),
    Status,
    New,
    Stop,
    Compact,
    Model(Option<String>),
    Effort(Option<Effort>),
    Learn(LearnCmd),
    Agents,
    Agent { id: i64, action: AgentAction },
    /// None：查看会话与数据库；Some(id)：切回唐语歌的历史会话 #id
    Sessions(Option<i64>),
    /// 清理早于 N 天的历史会话（缺省用配置 gc_days）
    Gc(Option<u32>),
    /// None：列出；Some((类别, 字段, 值))：修改；字段为 use 时切换提供方
    Provider(Option<(String, String, String)>),
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Show,
    Stop,
    /// 续接或插话
    Say(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearnCmd {
    /// 总览：说法与记忆的数量、待整理候选、最近一次整理
    Overview,
    /// 列出说法或记忆
    List(LearnScope),
    /// 删除一条：纯数字为说法编号，u3 / m2 为记忆条目
    Forget(String),
    On,
    Off,
    /// 立即学习尚未学过的新消息，并整理记忆候选
    Now,
    /// 撤销最近一次记忆整理
    Undo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnScope {
    Expressions,
    Memory,
}

/// 单条命令的定义与帮助文案。
pub struct CommandSpec {
    pub group: &'static str,
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub detail: &'static str,
}

/// 命令表：帮助文本与解析共用同一份定义。
/// QQ 单聊只显示纯文本、手机一行约 20 字，说明尽量短，详细内容按需查看。
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        group: "对话与状态",
        name: "help",
        usage: "/help [命令]",
        summary: "命令列表",
        detail: "不带参数列出全部命令；带命令名查看它的详细用法。",
    },
    CommandSpec {
        group: "对话与状态",
        name: "status",
        usage: "/status",
        summary: "运行状态",
        detail: "查看版本、模型与思考等级、上下文占用、缓存命中、后台任务、内存与运行时长。",
    },
    CommandSpec {
        group: "对话与状态",
        name: "stop",
        usage: "/stop",
        summary: "停止当前回复",
        detail: "停止唐语歌正在进行的这一轮。",
    },
    CommandSpec {
        group: "对话与状态",
        name: "compact",
        usage: "/compact",
        summary: "压缩当前会话",
        detail: "立即压缩当前会话并刷新记忆快照：先折叠较早的工具结果，不够再生成摘要。",
    },
    CommandSpec {
        group: "会话与记忆",
        name: "new",
        usage: "/new",
        summary: "开新会话",
        detail: "唐语歌开启新会话：记忆和关系不变，只是技术上的会话重开；旧会话标为已结束。",
    },
    CommandSpec {
        group: "会话与记忆",
        name: "sessions",
        usage: "/sessions [编号]",
        summary: "查看或切换会话",
        detail: "不带参数查看唐语歌最近的会话与数据库情况；带编号切回某个历史会话。\n/sessions 查看最近会话与编号\n/sessions 5 切回唐语歌的历史会话 #5（沿用当时的记忆快照，需要最新记忆时发 /compact）",
    },
    CommandSpec {
        group: "会话与记忆",
        name: "gc",
        usage: "/gc [天数]",
        summary: "清理历史会话",
        detail: "删除已结束且超过 N 天（默认 30）没有活动的会话与已归档后台任务。\n当前会话、常驻专员、长期记忆、还有未转达汇报的会话不受影响\n/gc 0 清理全部已结束的会话",
    },
    CommandSpec {
        group: "会话与记忆",
        name: "learn",
        usage: "/learn [子命令]",
        summary: "学习与记忆",
        detail: "管理后台学到的口头禅与长期记忆。\n/learn 查看总览\n/learn list 说法 查看学到的口头禅\n/learn list 记忆 查看长期记忆\n/learn forget 12（说法编号）或 u3 / m2（记忆编号）\n/learn now 立即学习并整理\n/learn undo 撤销最近一次整理\n/learn on 或 off 开关自动学习",
    },
    CommandSpec {
        group: "模型与扩展",
        name: "model",
        usage: "/model [名称]",
        summary: "查看或切换模型",
        detail: "不带参数查看当前模型与可选模型；带名称切换唐语歌的模型。",
    },
    CommandSpec {
        group: "模型与扩展",
        name: "effort",
        usage: "/effort [等级]",
        summary: "查看或切换思考等级",
        detail: "不带参数查看当前思考等级；带等级切换。\n可选：low / medium / high / xhigh / max",
    },
    CommandSpec {
        group: "模型与扩展",
        name: "agents",
        usage: "/agents",
        summary: "后台任务列表",
        detail: "列出后台任务与常驻专员：编号、状态、模型、耗时、进度。",
    },
    CommandSpec {
        group: "模型与扩展",
        name: "agent",
        usage: "/agent <编号> [stop|话]",
        summary: "查看、停止或续接",
        detail: "管理单个后台任务或常驻专员。\n/agent 12 查看详情与最近回复\n/agent 12 stop 停止运行中的任务\n/agent 12 再查一下日志（运行中则插话，已结束则续接）",
    },
    CommandSpec {
        group: "模型与扩展",
        name: "provider",
        usage: "/provider [类别 字段 值]",
        summary: "生图、语音、搜索、grok",
        detail: "查看或热修改可插拔提供方配置。\n/provider 查看当前提供方\n/provider image model gpt-image-2 修改字段\n/provider image use 名字 切换激活提供方",
    },
    CommandSpec {
        group: "模型与扩展",
        name: "reload",
        usage: "/reload",
        summary: "重载配置",
        detail: "重新加载配置文件、secret 文件与人格资产。",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// 已知命令但参数不对，附命令名
    BadArgs(String),
}

/// 解析一行输入。返回 `None` 表示这是普通聊天消息：
/// 只有已知命令才拦截，`/笑` 这类以 / 开头的普通说话照常交给唐语歌。
pub fn parse(input: &str) -> Option<Result<Command, ParseError>> {
    let s = input.trim();
    let body = s.strip_prefix('/').or_else(|| s.strip_prefix('／'))?;
    let mut it = body.splitn(2, char::is_whitespace);
    let name = it.next()?.to_ascii_lowercase();
    if name.is_empty() {
        return None; // 单独一个 "/" 当普通消息
    }
    let arg = it.next().map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned);
    let no_arg = |cmd: Command, canonical: &str| {
        if arg.is_none() { Ok(cmd) } else { Err(ParseError::BadArgs(canonical.into())) }
    };
    Some(match name.as_str() {
        "help" | "h" => Ok(Command::Help(arg)),
        "status" => no_arg(Command::Status, "status"),
        "new" => no_arg(Command::New, "new"),
        "stop" => no_arg(Command::Stop, "stop"),
        "compact" => no_arg(Command::Compact, "compact"),
        "model" => Ok(Command::Model(arg)),
        "effort" => match arg.as_deref() {
            None => Ok(Command::Effort(None)),
            Some(a) => a.parse::<Effort>().map(|e| Command::Effort(Some(e))).map_err(|_| ParseError::BadArgs("effort".into())),
        },
        "learn" => parse_learn(arg.as_deref()).map(Command::Learn).ok_or_else(|| ParseError::BadArgs("learn".into())),
        "agents" => no_arg(Command::Agents, "agents"),
        "sessions" => match arg.as_deref() {
            None => Ok(Command::Sessions(None)),
            Some(a) => match a.trim_start_matches('#').parse::<i64>() {
                Ok(id) if id > 0 => Ok(Command::Sessions(Some(id))),
                _ => Err(ParseError::BadArgs("sessions".into())),
            },
        },
        "gc" => match arg.as_deref().map(str::parse::<u32>) {
            None => Ok(Command::Gc(None)),
            Some(Ok(d)) => Ok(Command::Gc(Some(d))),
            Some(Err(_)) => Err(ParseError::BadArgs("gc".into())),
        },
        "agent" => parse_agent(arg.as_deref()).ok_or_else(|| ParseError::BadArgs("agent".into())),
        "provider" => match arg.as_deref() {
            None => Ok(Command::Provider(None)),
            Some(a) => parse_provider(a).ok_or_else(|| ParseError::BadArgs("provider".into())),
        },
        "reload" => no_arg(Command::Reload, "reload"),
        _ => return None,
    })
}

fn parse_provider(arg: &str) -> Option<Command> {
    let (cat, rest) = arg.split_once(char::is_whitespace)?;
    let (field, val) = rest.trim_start().split_once(char::is_whitespace)?;
    let val = val.trim();
    (!cat.is_empty() && !field.is_empty() && !val.is_empty()).then(|| Command::Provider(Some((cat.to_owned(), field.to_owned(), val.to_owned()))))
}

fn parse_agent(arg: Option<&str>) -> Option<Command> {
    let arg = arg?.trim().trim_start_matches('#');
    let (id, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let id: i64 = id.parse().ok().filter(|&id| id > 0)?;
    let rest = rest.trim();
    let action = if rest.is_empty() {
        AgentAction::Show
    } else if rest.eq_ignore_ascii_case("stop") {
        AgentAction::Stop
    } else {
        AgentAction::Say(rest.to_owned())
    };
    Some(Command::Agent { id, action })
}

fn parse_learn(arg: Option<&str>) -> Option<LearnCmd> {
    let Some(arg) = arg else { return Some(LearnCmd::Overview) };
    let mut it = arg.split_whitespace();
    let sub = it.next()?.to_ascii_lowercase();
    let cmd = match sub.as_str() {
        "list" => match it.next() {
            // 必须写明看哪一类，避免以为记忆没显示
            Some(s) if s == "说法" || s == "表达" || s.eq_ignore_ascii_case("expr") => LearnCmd::List(LearnScope::Expressions),
            Some(s) if s == "记忆" || s.eq_ignore_ascii_case("memory") || s.eq_ignore_ascii_case("mem") => LearnCmd::List(LearnScope::Memory),
            _ => return None,
        },
        "on" => LearnCmd::On,
        "off" => LearnCmd::Off,
        "now" => LearnCmd::Now,
        "undo" => LearnCmd::Undo,
        "forget" => {
            let id = it.next()?.trim_start_matches('#').to_ascii_lowercase();
            let entry = id.strip_prefix('u').or_else(|| id.strip_prefix('m'));
            let valid = id.parse::<i64>().is_ok_and(|n| n > 0) || entry.and_then(|n| n.parse::<usize>().ok()).is_some_and(|n| n > 0);
            if !valid {
                return None;
            }
            LearnCmd::Forget(id)
        }
        _ => return None,
    };
    it.next().is_none().then_some(cmd)
}

pub fn help(topic: Option<&str>) -> String {
    let topic = topic.map(|t| t.trim().trim_start_matches(['/', '／']).trim().to_ascii_lowercase()).filter(|t| !t.is_empty());
    match topic.as_deref() {
        Some(t) => {
            let key = if t == "h" { "help" } else { t };
            COMMANDS
                .iter()
                .find(|cmd| cmd.name == key)
                .map(|cmd| {
                    let mut c = Card::new("📖", cmd.usage);
                    let mut lines = cmd.detail.lines();
                    if let Some(first) = lines.next() {
                        c.line(first);
                    }
                    let rest: Vec<&str> = lines.collect();
                    if !rest.is_empty() {
                        c.section("用法与说明");
                        for line in rest {
                            c.item(line);
                        }
                    }
                    c.build()
                })
                .unwrap_or_else(|| qya_core::card::warn(format!("没有 /{t} 这个命令，发 /help 看全部")))
        }
        None => {
            let mut c = Card::new("📖", "命令列表");
            let mut cur_group = "";
            for cmd in COMMANDS {
                if cmd.group != cur_group {
                    cur_group = cmd.group;
                    c.section(cmd.group);
                }
                c.item(format!("/{}｜{}", cmd.name, cmd.summary));
            }
            c.hint("详细用法：/help 命令名").build()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse("你好"), None);
        assert_eq!(parse("/"), None);
        assert_eq!(parse("/status"), Some(Ok(Command::Status)));
        assert_eq!(parse("/status extra"), Some(Err(ParseError::BadArgs("status".into()))));
        assert_eq!(parse("/new 5"), Some(Err(ParseError::BadArgs("new".into()))));
        assert_eq!(parse(" /Model  k3-256k "), Some(Ok(Command::Model(Some("k3-256k".into())))));
        assert_eq!(parse("／new"), Some(Ok(Command::New)));
        assert_eq!(parse("/effort"), Some(Ok(Command::Effort(None))));
        assert_eq!(parse("/effort HIGH"), Some(Ok(Command::Effort(Some(Effort::High)))));
        assert_eq!(parse("/effort invalid"), Some(Err(ParseError::BadArgs("effort".into()))));
        assert_eq!(parse("/笑"), None, "不是已知命令的照常当聊天");
        assert_eq!(parse("/foo bar"), None);
        assert_eq!(parse("/sessions"), Some(Ok(Command::Sessions(None))));
        assert_eq!(parse("/sessions 7"), Some(Ok(Command::Sessions(Some(7)))));
        assert_eq!(parse("/sessions #7"), Some(Ok(Command::Sessions(Some(7)))));
        assert_eq!(parse("/sessions 0"), Some(Err(ParseError::BadArgs("sessions".into()))));
        assert_eq!(parse("/sessions abc"), Some(Err(ParseError::BadArgs("sessions".into()))));
        assert_eq!(parse("/agent #7 stop"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Stop })));
        assert_eq!(parse("/agent 7 STOP"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Stop })));
        assert_eq!(parse("/agent 7 再查一下日志"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Say("再查一下日志".into()) })));
        assert_eq!(parse("/agent 7"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Show })));
        assert_eq!(parse("/agent 0"), Some(Err(ParseError::BadArgs("agent".into()))));
        assert_eq!(parse("/agent"), Some(Err(ParseError::BadArgs("agent".into()))));
        assert_eq!(parse("/provider image model gpt-image-2"), Some(Ok(Command::Provider(Some(("image".into(), "model".into(), "gpt-image-2".into()))))));
        assert_eq!(parse("/provider  image   model   gpt-image-2 "), Some(Ok(Command::Provider(Some(("image".into(), "model".into(), "gpt-image-2".into()))))));
        assert_eq!(parse("/provider"), Some(Ok(Command::Provider(None))));
        assert_eq!(parse("/provider image"), Some(Err(ParseError::BadArgs("provider".into()))));
        assert_eq!(parse("/provider image  "), Some(Err(ParseError::BadArgs("provider".into()))));
        assert_eq!(parse("/gc 7"), Some(Ok(Command::Gc(Some(7)))));
        assert_eq!(parse("/gc"), Some(Ok(Command::Gc(None))));
        assert_eq!(parse("/gc x"), Some(Err(ParseError::BadArgs("gc".into()))));
        assert_eq!(parse("/learn"), Some(Ok(Command::Learn(LearnCmd::Overview))));
        assert_eq!(parse("/learn LIST 说法"), Some(Ok(Command::Learn(LearnCmd::List(LearnScope::Expressions)))));
        assert_eq!(parse("/learn list"), Some(Err(ParseError::BadArgs("learn".into()))), "必须写明说法或记忆");
        assert_eq!(parse("/learn list 记忆"), Some(Ok(Command::Learn(LearnCmd::List(LearnScope::Memory)))));
        assert_eq!(parse("/learn list 记忆 extra"), Some(Err(ParseError::BadArgs("learn".into()))));
        assert_eq!(parse("/learn list 别的"), Some(Err(ParseError::BadArgs("learn".into()))));
        assert_eq!(parse("/learn forget 12"), Some(Ok(Command::Learn(LearnCmd::Forget("12".into())))));
        assert_eq!(parse("/learn forget #12"), Some(Ok(Command::Learn(LearnCmd::Forget("12".into())))));
        assert_eq!(parse("/learn forget U3"), Some(Ok(Command::Learn(LearnCmd::Forget("u3".into())))));
        assert_eq!(parse("/learn forget 0"), Some(Err(ParseError::BadArgs("learn".into()))));
        assert_eq!(parse("/learn forget m0"), Some(Err(ParseError::BadArgs("learn".into()))));
        assert_eq!(parse("/learn forget x"), Some(Err(ParseError::BadArgs("learn".into()))));
        for bad in ["/learn forget 猫1", "/learn forget u", "/learn forget 😀", "/learn forget u猫", "/learn now extra"] {
            assert_eq!(parse(bad), Some(Err(ParseError::BadArgs("learn".into()))), "{bad}");
        }
        assert_eq!(parse("/learn Undo"), Some(Ok(Command::Learn(LearnCmd::Undo))));
    }

    #[test]
    fn help_covers_every_command() {
        let h = help(None);
        for cmd in COMMANDS {
            assert!(h.contains(&format!("· /{}｜", cmd.name)));
            assert!(parse(&format!("/{}", cmd.name)).is_some(), "表中的 /{} 必须是已知命令", cmd.name);
        }
        assert!(help(Some(" /Model ")).contains("切换"));
        assert!(help(Some("/h")).contains("/help"));
        assert!(help(Some("learn")).contains("▍用法与说明"));
    }
}
