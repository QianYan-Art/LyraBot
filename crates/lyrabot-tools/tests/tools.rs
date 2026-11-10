//! 内置工具集成测试：真实执行 bash、读写临时文件、读取仓库技能目录。
use lyrabot_agent::{Tool, ToolCtx};
use lyrabot_config::ShellSettings;
use lyrabot_tools::util::Audit;
use lyrabot_tools::{FsPatch, FsRead, FsWrite, Shell, SkillView};
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

fn ctx() -> ToolCtx {
    ToolCtx { session: 1, call_id: "c".into(), cancel: CancellationToken::new() }
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lyrabot-tools-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Windows 上 `bash` 可能解析到 WSL 的 bash.exe（会改写参数里的 $()），优先用 Git Bash。
fn bash() -> String {
    if cfg!(windows) {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        for p in [format!("{home}\\scoop\\apps\\git\\current\\bin\\bash.exe"), r"C:\Program Files\Git\bin\bash.exe".to_owned()] {
            if std::path::Path::new(&p).exists() {
                return p;
            }
        }
    }
    "bash".to_owned()
}

fn shell(dir: &std::path::Path) -> Shell {
    let cfg = ShellSettings { shell: bash(), rtk: None, output_limit: 200, ..ShellSettings::default() };
    Shell::new(cfg, Audit::new(dir.join("audit.jsonl"))).with_spill(dir.join("tool-output"))
}

#[tokio::test]
async fn shell_runs_truncates_and_audits() {
    let d = tmp("shell");
    let sh = shell(&d);
    let ok = sh.call(ctx(), json!({"command": "echo hello && echo oops 1>&2"})).await;
    assert!(!ok.is_error && ok.text.contains("hello") && ok.text.contains("[stderr]\noops"), "{}", ok.text);
    let fail = sh.call(ctx(), json!({"command": "exit 3"})).await;
    assert!(fail.is_error && fail.text.contains("退出码 3"));
    let long = sh.call(ctx(), json!({"command": "for i in $(seq 1 500); do echo line$i; done"})).await;
    assert!(long.text.contains("省略") && long.text.contains("line500"), "{}", long.text);
    // 超长输出：完整内容存成文件，返回里给出路径
    let saved = long.text.split("已保存到 ").nth(1).and_then(|r| r.split('；').next()).expect("应给出完整输出路径");
    let full = std::fs::read_to_string(saved).unwrap();
    assert_eq!(full.lines().count(), 500);
    assert!(full.contains("line250"), "中间被省略的部分在文件里");
    let denied = sh.call(ctx(), json!({"command": "rm -rf /etc"})).await;
    assert!(denied.is_error && denied.text.contains("拒绝执行"));
    let audit = std::fs::read_to_string(d.join("audit.jsonl")).unwrap();
    assert_eq!(audit.lines().count(), 4);
    assert!(audit.contains("\"denied\""));
}

#[tokio::test]
async fn shell_large_output_does_not_deadlock() {
    // 输出远超管道容量（约 64 KB）：读取必须与等待并发，否则会一直卡到超时
    let d = tmp("bigout");
    let started = Instant::now();
    let out = shell(&d).call(ctx(), json!({"command": "yes x | head -c 3000000; echo END", "timeout_s": 20})).await;
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("END"), "{}", &out.text[out.text.len().saturating_sub(200)..]);
    assert!(started.elapsed().as_secs() < 15, "不应等到超时");
}

#[tokio::test]
async fn shell_timeout_kills() {
    let d = tmp("timeout");
    let started = Instant::now();
    let out = shell(&d).call(ctx(), json!({"command": "sleep 30", "timeout_s": 1})).await;
    assert!(out.is_error && out.text.contains("超时"), "{}", out.text);
    assert!(started.elapsed().as_secs() < 10, "超时后应尽快返回");
}

#[tokio::test]
async fn fs_write_patch_read() {
    let d = tmp("fs");
    let f = d.join("sub/a.txt").display().to_string();
    assert!(!FsWrite::default().call(ctx(), json!({"path": f, "content": "a\nb\na\n"})).await.is_error);
    let dup = FsPatch::default().call(ctx(), json!({"path": f, "old": "a", "new": "x"})).await;
    assert!(dup.is_error && dup.text.contains("2 次"));
    assert!(!FsPatch::default().call(ctx(), json!({"path": f, "old": "b", "new": "B"})).await.is_error);
    let r = FsRead::default().call(ctx(), json!({"path": f, "offset": 2, "limit": 1})).await;
    assert!(r.text.contains("     2\tB") && !r.text.contains("     1\t"), "{}", r.text);
}

#[tokio::test]
async fn skills_from_repo() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills");
    let sv = SkillView::new(&dir).expect("仓库应有技能");
    assert!(sv.spec().description.contains("grill-me") && sv.spec().description.contains("lyrabot-docs"));
    let r = sv.call(ctx(), json!({"name": "lyrabot-docs"})).await;
    assert!(!r.is_error && r.text.contains("docs/01-architecture.md") && r.text.contains("docs/09-backlog.md"));
    let bad = sv.call(ctx(), json!({"name": "lyrabot-docs", "file": "../../Cargo.toml"})).await;
    assert!(bad.is_error);
}
