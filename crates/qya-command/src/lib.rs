//! 聊天命令解析。命令不经过模型，立即响应；帮助文本与解析共用一张表，保证一致。
//! 完整命令设计见 docs/01-architecture.md 第 11 节，后续阶段逐步补齐。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help(Option<String>),
    Status,
    New,
    Stop,
    Compact,
    Model(Option<String>),
    Effort(Option<String>),
    Learn(LearnCmd),
    Agents,
    Agent { id: i64, action: AgentAction },
    Sessions,
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

/// 命令表：名字、用法、一句话说明（/help 列表用）、详细说明（/help 命令名 用）。
/// QQ 单聊只显示纯文本、手机一行约 20 字，说明尽量短，详细内容按需查看。
pub const COMMANDS: &[(&str, &str, &str, &str)] = &[
    ("help", "/help [命令]", "命令列表", "不带参数列出全部命令；带命令名查看它的详细用法。"),
    ("status", "/status", "运行状态", "版本、模型与思考等级、上下文占用、缓存命中、后台任务、内存与运行时长。"),
    ("new", "/new", "开新会话", "唐语歌开启新会话：记忆和关系不变，只是技术上的会话重开；旧会话标为已结束。"),
    ("sessions", "/sessions", "会话与数据库", "唐语歌最近的会话、各类会话数量、记录条数与数据库大小。"),
    ("gc", "/gc [天数]", "清理历史会话", "删除已结束且超过 N 天（默认 30）没有活动的会话与已归档后台任务。\n当前会话、常驻专员、长期记忆、还有未转达汇报的会话不受影响。\n/gc 0 清理全部已结束的会话。"),
    ("stop", "/stop", "停止当前回复", "停止唐语歌正在进行的这一轮。"),
    ("compact", "/compact", "压缩当前会话", "立即压缩当前会话：先折叠较早的工具结果，不够再生成摘要。"),
    ("model", "/model [名称]", "查看或切换模型", "不带参数查看当前模型与可选模型；带名称切换唐语歌的模型。"),
    ("effort", "/effort [等级]", "查看或切换思考等级", "等级可选 low / medium / high / xhigh / max。"),
    ("learn", "/learn [子命令]", "学习与记忆", "/learn　查看总览\n/learn list 说法 或 /learn list 记忆\n/learn forget 12（说法编号）或 u3 / m2（记忆编号）\n/learn now 立即学习并整理\n/learn undo 撤销最近一次整理\n/learn on 或 off 开关学习"),
    ("agents", "/agents", "后台任务列表", "列出后台任务与常驻专员：编号、状态、模型、耗时、进度。"),
    ("agent", "/agent <编号> [stop|话]", "查看、停止或续接", "/agent 12 查看详情与最近回复\n/agent 12 stop 停止\n/agent 12 再查一下日志：运行中则插话，已结束则续接"),
    ("provider", "/provider [类别 字段 值]", "生图、语音、搜索、grok", "不带参数查看当前提供方。\n修改：/provider image model gpt-image-2\n切换：/provider image use 名字"),
    ("reload", "/reload", "重载配置", "重新加载配置、secret 文件与人格资产。"),
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
    Some(Ok(match name.as_str() {
        "help" | "h" => Command::Help(arg),
        "status" => Command::Status,
        "new" => Command::New,
        "stop" => Command::Stop,
        "compact" => Command::Compact,
        "model" => Command::Model(arg),
        "effort" => Command::Effort(arg),
        "learn" => match parse_learn(arg.as_deref()) {
            Some(c) => Command::Learn(c),
            None => return Some(Err(ParseError::BadArgs("learn".into()))),
        },
        "agents" => Command::Agents,
        "sessions" => Command::Sessions,
        "gc" => match arg.as_deref().map(str::parse::<u32>) {
            None => Command::Gc(None),
            Some(Ok(d)) => Command::Gc(Some(d)),
            Some(Err(_)) => return Some(Err(ParseError::BadArgs("gc".into()))),
        },
        "agent" => match parse_agent(arg.as_deref()) {
            Some(c) => c,
            None => return Some(Err(ParseError::BadArgs("agent".into()))),
        },
        "provider" => match arg.as_deref() {
            None => Command::Provider(None),
            Some(a) => {
                let mut it = a.splitn(3, char::is_whitespace);
                match (it.next(), it.next(), it.next().map(str::trim).filter(|v| !v.is_empty())) {
                    (Some(c), Some(f), Some(v)) => Command::Provider(Some((c.into(), f.into(), v.into()))),
                    _ => return Some(Err(ParseError::BadArgs("provider".into()))),
                }
            }
        },
        "reload" => Command::Reload,
        _ => return None,
    }))
}

fn parse_agent(arg: Option<&str>) -> Option<Command> {
    let arg = arg?.trim().trim_start_matches('#');
    let (id, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let id: i64 = id.parse().ok()?;
    let rest = rest.trim();
    let action = match rest {
        "" => AgentAction::Show,
        "stop" => AgentAction::Stop,
        t => AgentAction::Say(t.to_owned()),
    };
    Some(Command::Agent { id, action })
}

fn parse_learn(arg: Option<&str>) -> Option<LearnCmd> {
    let Some(arg) = arg else { return Some(LearnCmd::Overview) };
    let mut it = arg.split_whitespace();
    Some(match it.next()? {
        "list" => match it.next() {
            None | Some("说法" | "表达" | "expr") => LearnCmd::List(LearnScope::Expressions),
            Some("记忆" | "memory" | "mem") => LearnCmd::List(LearnScope::Memory),
            Some(_) => return None,
        },
        "on" => LearnCmd::On,
        "off" => LearnCmd::Off,
        "now" => LearnCmd::Now,
        "undo" => LearnCmd::Undo,
        "forget" => {
            let id = it.next()?.trim_start_matches('#').to_ascii_lowercase();
            let entry = id.strip_prefix('u').or_else(|| id.strip_prefix('m'));
            let valid = id.parse::<i64>().is_ok() || entry.and_then(|n| n.parse::<usize>().ok()).is_some_and(|n| n > 0);
            if !valid {
                return None;
            }
            LearnCmd::Forget(id)
        }
        _ => return None,
    })
}

pub fn help(topic: Option<&str>) -> String {
    match topic.map(|t| t.trim_start_matches('/')) {
        Some(t) => COMMANDS
            .iter()
            .find(|(n, _, _, _)| *n == t)
            .map(|(_, u, _, d)| format!("{u}\n{d}"))
            .unwrap_or_else(|| format!("没有 /{t} 这个命令，发 /help 看全部")),
        None => {
            let mut s = String::from("【命令】\n");
            for (n, _, short, _) in COMMANDS {
                s.push_str(&format!("/{n}  {short}\n"));
            }
            s.push_str("\n发 /help 命令名 查看详细用法");
            s
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
        assert_eq!(parse(" /Model  k3-256k "), Some(Ok(Command::Model(Some("k3-256k".into())))));
        assert_eq!(parse("／new"), Some(Ok(Command::New)));
        assert_eq!(parse("/effort"), Some(Ok(Command::Effort(None))));
        assert_eq!(parse("/笑"), None, "不是已知命令的照常当聊天");
        assert_eq!(parse("/foo bar"), None);
        assert_eq!(parse("/agent #7 stop"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Stop })));
        assert_eq!(parse("/agent 7 再查一下日志"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Say("再查一下日志".into()) })));
        assert_eq!(parse("/agent 7"), Some(Ok(Command::Agent { id: 7, action: AgentAction::Show })));
        assert_eq!(parse("/agent"), Some(Err(ParseError::BadArgs("agent".into()))));
        assert_eq!(parse("/provider image model gpt-image-2"), Some(Ok(Command::Provider(Some(("image".into(), "model".into(), "gpt-image-2".into()))))));
        assert_eq!(parse("/provider"), Some(Ok(Command::Provider(None))));
        assert_eq!(parse("/provider image"), Some(Err(ParseError::BadArgs("provider".into()))));
        assert_eq!(parse("/gc 7"), Some(Ok(Command::Gc(Some(7)))));
        assert_eq!(parse("/gc"), Some(Ok(Command::Gc(None))));
        assert_eq!(parse("/gc x"), Some(Err(ParseError::BadArgs("gc".into()))));
        assert_eq!(parse("/learn"), Some(Ok(Command::Learn(LearnCmd::Overview))));
        assert_eq!(parse("/learn list"), Some(Ok(Command::Learn(LearnCmd::List(LearnScope::Expressions)))));
        assert_eq!(parse("/learn list 记忆"), Some(Ok(Command::Learn(LearnCmd::List(LearnScope::Memory)))));
        assert_eq!(parse("/learn list 别的"), Some(Err(ParseError::BadArgs("learn".into()))));
        assert_eq!(parse("/learn forget 12"), Some(Ok(Command::Learn(LearnCmd::Forget("12".into())))));
        assert_eq!(parse("/learn forget #12"), Some(Ok(Command::Learn(LearnCmd::Forget("12".into())))));
        assert_eq!(parse("/learn forget U3"), Some(Ok(Command::Learn(LearnCmd::Forget("u3".into())))));
        assert_eq!(parse("/learn forget m0"), Some(Err(ParseError::BadArgs("learn".into()))));
        assert_eq!(parse("/learn forget x"), Some(Err(ParseError::BadArgs("learn".into()))));
        for bad in ["/learn forget 猫1", "/learn forget u", "/learn forget 😀", "/learn forget u猫"] {
            assert_eq!(parse(bad), Some(Err(ParseError::BadArgs("learn".into()))), "{bad}");
        }
        assert_eq!(parse("/learn undo"), Some(Ok(Command::Learn(LearnCmd::Undo))));
    }

    #[test]
    fn help_covers_every_command() {
        let h = help(None);
        for (n, _, _, _) in COMMANDS {
            assert!(h.contains(&format!("/{n}")));
            assert!(parse(&format!("/{n}")).is_some(), "表中的 /{n} 必须是已知命令");
        }
        assert!(help(Some("/model")).contains("切换"));
    }
}
