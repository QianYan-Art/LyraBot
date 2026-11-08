//! 把构建所在的 git 提交写进二进制（QYA_GIT_HASH），用于核对本地、仓库与服务器是否同一基线。
//! 工作区有未提交改动时追加 `-dirty`；不在 git 仓库中时为 `unknown`。

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    let hash = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    println!("cargo:rustc-env=QYA_GIT_HASH={hash}{}", if dirty { "-dirty" } else { "" });
    for p in ["../../.git/HEAD", "../../.git/index", "../../.git/refs/heads"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
