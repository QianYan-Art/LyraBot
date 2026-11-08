//! 本地命令行平台：逐行读 stdin，流式打印回复。开发与切换前测试用。

use crate::{Inbound, Outbound, Platform, PlatformCaps, PlatformId, SendResult, anyhow_like};
use async_trait::async_trait;
use qya_core::ContentPart;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

pub struct CliPlatform {
    out: Mutex<tokio::io::Stdout>,
    streaming: bool,
}

impl Default for CliPlatform {
    fn default() -> Self {
        Self::new(true)
    }
}

impl CliPlatform {
    /// `streaming = false` 时模拟 QQ：不逐字输出，按分段整条发送。
    pub fn new(streaming: bool) -> Self {
        Self { out: Mutex::new(tokio::io::stdout()), streaming }
    }

    async fn write(&self, s: &str) {
        let mut o = self.out.lock().await;
        let _ = o.write_all(s.as_bytes()).await;
        let _ = o.flush().await;
    }
}

#[async_trait]
impl Platform for CliPlatform {
    fn id(&self) -> PlatformId {
        PlatformId::Cli
    }

    fn caps(&self) -> PlatformCaps {
        PlatformCaps { streaming: self.streaming }
    }

    async fn run(&self, inbox: mpsc::Sender<Inbound>) -> anyhow_like::Result<()> {
        self.write("qya 命令行已就绪，直接输入聊天，/help 查看命令，Ctrl+C 退出。\n> ").await;
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                self.write("> ").await;
                continue;
            }
            let msg = Inbound { platform: PlatformId::Cli, user: "cli".into(), parts: vec![ContentPart::text(line)], msg_id: None };
            if inbox.send(msg).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn send(&self, out: Outbound) -> SendResult {
        let mut s = if out.text.trim().is_empty() { "\n".to_owned() } else { format!("\n{}\n", out.text.trim_end()) };
        for m in &out.media {
            s.push_str(&format!("[媒体] {}\n", m.display()));
        }
        s.push_str("> ");
        self.write(&s).await;
        Ok(())
    }

    async fn stream_delta(&self, text: &str) {
        self.write(text).await;
    }

    async fn stream_end(&self) {
        self.write("\n> ").await;
    }
}
