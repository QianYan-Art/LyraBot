//! 用仓库真实资产验证人格渲染（assets/persona）。
use qya_agent::{Tool, ToolCtx};
use qya_persona::{MEMORY_FILE_LIMIT, Persona};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

fn assets() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/persona")
}

#[test]
fn identity_layout_and_hygiene() {
    let p = Persona::load(&assets()).unwrap();
    let s1 = p.identity();
    assert!(s1.starts_with("# 唐语歌\n"), "首块必须是身份");
    let pos = |needle: &str| s1.find(needle).unwrap_or_else(|| panic!("缺少 {needle}"));
    // 关系默认紧跟身份头，先于角色卡正文
    assert!(pos("## 关系默认") < pos("## 核心描述"));
    assert!(pos("## 聊天准则") < pos("## 内核与驱动"));
    assert!(pos("## 示例对话") < pos("## 补充设定"));
    assert!(pos("## 补充设定") < pos("## 能力与边界"));
    for banned in ["Hermes", "Nous Research", "You are Hermes Agent", "{{user}}", "{{char}}", "好感度", "状态栏", "skill_view", "MEDIA:", "/home/hermes"] {
        assert!(!s1.contains(banned), "S1 不应包含 `{banned}`");
    }
    assert!(s1.contains("阿颜: 想抱抱你。") || s1.contains("阿颜: 我只是想找你待一会儿"), "示例对话占位符应替换为阿颜");
    // 渲染确定性：重复装载字节一致
    assert_eq!(Persona::load(&assets()).unwrap().identity(), s1);
    println!("S1 {} 字符", s1.chars().count());
}

#[tokio::test]
async fn notes_tool_reads_and_rejects() {
    let p = Persona::load(&assets()).unwrap();
    let tool = p.notes_tool().expect("默认资产包含 notes");
    assert!(tool.spec().description.contains("appearance"));
    let ctx = || ToolCtx { session: 1, call_id: "c".into(), cancel: CancellationToken::new() };
    let out = tool.call(ctx(), serde_json::json!({"topic": "appearance"})).await;
    assert!(!out.is_error && out.text.contains("发"), "{}", out.text);
    let bad = tool.call(ctx(), serde_json::json!({"topic": "nope"})).await;
    assert!(bad.is_error && bad.text.contains("behavior"));
}

#[test]
fn custom_persona_minimal_card_without_notes() {
    let dir = std::env::temp_dir().join(format!("qya-custom-persona-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("persona.toml"), "user_name = \"小明\"\ncard = \"card.json\"\n").unwrap();
    std::fs::write(
        dir.join("card.json"),
        r#"{"spec":"chara_card_v2","spec_version":"2.0","data":{"name":"星野","description":"你是{{char}}，正在陪{{user}}聊天。","personality":"温和","mes_example":"{{user}}: 早\n{{char}}: 早安"}}"#,
    )
    .unwrap();
    let p = Persona::load(&dir).unwrap();
    assert_eq!(p.name(), "星野");
    assert_eq!(p.user_name(), "小明");
    assert!(p.notes_tool().is_none(), "未配置 notes 时不应注册空工具");
    let s1 = p.identity();
    assert!(s1.starts_with("# 星野\n"));
    assert!(s1.contains("你就是星野本人"));
    assert!(s1.contains("你是星野，正在陪小明聊天。"));
    assert!(!s1.contains("唐语歌") && !s1.contains("阿颜"));
    assert_eq!(p.snapshot(&dir), "## 关于小明\n（暂无）\n\n## 你的备忘\n（暂无）");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snapshot_entries_limit_and_empty() {
    let p = Persona::load(&assets()).unwrap();
    let dir = std::env::temp_dir().join(format!("qya-mem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let empty = p.snapshot(&dir);
    assert_eq!(empty, "## 关于阿颜\n（暂无）\n\n## 你的备忘\n（暂无）");
    std::fs::write(dir.join("user.md"), "喜欢少糖奶茶\n§\n日常用北京时间\n").unwrap();
    let big = "长".repeat(MEMORY_FILE_LIMIT);
    std::fs::write(dir.join("memory.md"), format!("短条目\n§\n{big}")).unwrap();
    let s = p.snapshot(&dir);
    assert!(s.contains("- 喜欢少糖奶茶\n- 日常用北京时间"));
    assert!(s.contains("- 短条目\n- （更多条目超出记忆上限，未载入）"));
    let sys = p.system(&s);
    assert_eq!(sys.len(), 2);
    assert_eq!(sys[0], p.identity());
}
