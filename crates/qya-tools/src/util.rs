//! 工具共用：参数读取、输出截断、审计日志、带超时与进程组清理的子进程执行。

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub fn arg_str<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

pub fn arg_u64(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(Value::as_u64)
}

pub fn arg_bool(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// 超长文本保留首 40% 与尾 60%，中间标注省略量。
pub fn truncate_middle(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit {
        return s.to_owned();
    }
    let head = limit * 2 / 5;
    let tail = limit - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}\n…（省略 {} 字）…\n{t}", n - head - tail)
}

/// 只保留末尾 `limit` 字（日志类输出）。
pub fn tail_chars(s: &str, limit: usize) -> String {
    let n = s.chars().count();
    if n <= limit { s.to_owned() } else { format!("…{}", s.chars().skip(n - limit).collect::<String>()) }
}

/// 工具调用审计（JSON Lines，追加写；失败只记日志不影响工具）。
#[derive(Debug, Clone)]
pub struct Audit {
    path: PathBuf,
}

impl Audit {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub async fn record(&self, entry: Value) {
        use tokio::io::AsyncWriteExt;
        let mut line = entry.to_string();
        line.push('\n');
        let res = async {
            if let Some(dir) = self.path.parent() {
                tokio::fs::create_dir_all(dir).await?;
            }
            let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&self.path).await?;
            f.write_all(line.as_bytes()).await
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, path = %self.path.display(), "写审计日志失败");
        }
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[derive(Debug)]
pub struct ExecOutput {
    /// 合并后的 stdout + stderr（stderr 追加在后，带标记）
    pub output: String,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub elapsed: Duration,
}

/// 执行子进程：超时或取消时终止整个进程组（Unix 下以 setsid 启动，kill 负 pgid）。
pub async fn exec(
    program: &str,
    args: &[String],
    workdir: Option<&Path>,
    env: &[(String, String)],
    timeout: Duration,
    cancel: &CancellationToken,
) -> std::io::Result<ExecOutput> {
    let started = Instant::now();
    #[cfg(unix)]
    let mut cmd = {
        let mut c = Command::new("setsid");
        c.arg(program).args(args);
        c
    };
    #[cfg(not(unix))]
    let mut cmd = {
        let mut c = Command::new(program);
        c.args(args);
        c
    };
    if let Some(d) = workdir {
        cmd.current_dir(d);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // future 被丢弃（例如外层取消）时也要终止整个进程组；kill_on_drop 只管直接子进程
    let mut group = GroupGuard(pid);
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    // 读取与等待并发进行：否则输出超过管道容量时子进程写阻塞、永远等不到退出
    let reader = tokio::spawn(async move { tokio::join!(read_capped(stdout, MAX_CAPTURE), read_capped(stderr, MAX_CAPTURE)) });
    let (mut timed_out, mut cancelled) = (false, false);
    let status = tokio::select! {
        s = child.wait() => Some(s?),
        _ = tokio::time::sleep(timeout) => { timed_out = true; None }
        _ = cancel.cancelled() => { cancelled = true; None }
    };
    if status.is_none() {
        kill_group(pid).await;
        let _ = child.kill().await;
    }
    group.0 = None;
    // 进程结束后管道会关闭；给读取留一点时间，避免孙进程持有管道导致挂起
    let abort = reader.abort_handle();
    let (o, e) = match tokio::time::timeout(Duration::from_secs(3), reader).await {
        Ok(Ok(r)) => r,
        _ => {
            abort.abort();
            Default::default()
        }
    };
    let mut output = String::from_utf8_lossy(&o).into_owned();
    let err = String::from_utf8_lossy(&e);
    if !err.trim().is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[stderr]\n");
        output.push_str(&err);
    }
    Ok(ExecOutput { output, code: status.and_then(|s| s.code()), timed_out, cancelled, elapsed: started.elapsed() })
}

/// 单路输出最多保留的字节数；超出部分继续读走丢弃，保证子进程不会因管道写满阻塞。
const MAX_CAPTURE: usize = 4 * 1024 * 1024;

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    out
}

/// 未解除时在析构中发出进程组终止（不等待结果）。
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        // 用 tokio 的 Command：丢弃的子进程由 tokio 回收，不留僵尸；不在运行时内则无法回收，跳过
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        #[cfg(unix)]
        let _ = Command::new("kill").arg("-KILL").arg("--").arg(format!("-{pid}")).spawn();
        #[cfg(not(unix))]
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).spawn();
    }
}

#[cfg(unix)]
async fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // setsid 使子进程成为新会话与进程组的组长，pgid == pid
        let _ = Command::new("kill").arg("-KILL").arg("--").arg(format!("-{pid}")).status().await;
    }
}

#[cfg(not(unix))]
async fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).status().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation() {
        assert_eq!(truncate_middle("abc", 5), "abc");
        let t = truncate_middle(&"x".repeat(100), 10);
        assert!(t.starts_with("xxxx\n…（省略 90 字）…\nxxxxxx"));
        assert_eq!(tail_chars("abcdef", 3), "…def");
    }
}
