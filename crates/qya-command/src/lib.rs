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
    List,
    Forget(i64),
    On,
    Off,
    /// 立即对尚未学习的新消息执行一次学习
    Now,
}

/// 命令表：名字、用法、说明。
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "/help [命令]", "命令列表或单个命令说明"),
    ("status", "/status", "模型、思考等级、上下文占用、缓存命中、内存、运行时长"),
    ("new", "/new", "唐语歌开启新会话（记忆和关系不变，只是技术会话重开；旧会话标为已结束）"),
    ("sessions", "/sessions", "查看唐语歌的当前与历史会话、后台任务数量和数据库大小"),
    ("gc", "/gc [天数]", "清理早于 N 天（默认 30）的历史会话与已归档后台任务；当前会话、常驻专员与长期记忆不受影响"),
    ("stop", "/stop", "停止唐语歌当前这一轮"),
    ("compact", "/compact", "立即压缩当前会话"),
    ("model", "/model [名称]", "查看或切换唐语歌的模型"),
    ("effort", "/effort [等级]", "查看或切换唐语歌的思考等级（low/medium/high/xhigh/max）"),
    ("learn", "/learn [list|forget <id>|on|off|now]", "表达与黑话学习：查看、删除条目、开关、立即学习"),
    ("agents", "/agents", "列出后台任务与专员：编号、状态、模型、耗时、进度"),
    ("agent", "/agent <编号> [stop|要说的话]", "查看某个后台任务；stop 停止；带一句话则续接或插话"),
    ("provider", "/provider [类别 字段 值 | 类别 use 名字]", "查看或修改生图/语音/搜索/grok 提供方（如 /provider image model gpt-image-2）"),
    ("reload", "/reload", "重新加载配置与 secret 文件"),
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
    let mut it = arg.unwrap_or("list").split_whitespace();
    Some(match it.next()? {
        "list" => LearnCmd::List,
        "on" => LearnCmd::On,
        "off" => LearnCmd::Off,
        "now" => LearnCmd::Now,
        "forget" => LearnCmd::Forget(it.next()?.parse().ok()?),
        _ => return None,
    })
}

pub fn help(topic: Option<&str>) -> String {
    match topic.map(|t| t.trim_start_matches('/')) {
        Some(t) => COMMANDS
            .iter()
            .find(|(n, _, _)| *n == t)
            .map(|(_, u, d)| format!("{u}\n{d}"))
            .unwrap_or_else(|| format!("没有 /{t} 这个命令，发 /help 看全部")),
        None => {
            let mut s = String::from("可用命令：\n");
            for (_, u, d) in COMMANDS {
                s.push_str(&format!("{u} — {d}\n"));
            }
            s.trim_end().to_owned()
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
        assert_eq!(parse("/learn"), Some(Ok(Command::Learn(LearnCmd::List))));
        assert_eq!(parse("/learn forget 12"), Some(Ok(Command::Learn(LearnCmd::Forget(12)))));
        assert_eq!(parse("/learn forget x"), Some(Err(ParseError::BadArgs("learn".into()))));
    }

    #[test]
    fn help_covers_every_command() {
        let h = help(None);
        for (n, _, _) in COMMANDS {
            assert!(h.contains(&format!("/{n}")));
            assert!(parse(&format!("/{n}")).is_some(), "表中的 /{n} 必须是已知命令");
        }
        assert!(help(Some("/model")).contains("切换"));
    }
}
