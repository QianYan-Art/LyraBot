//! lyrabot 入口：解析参数、装载配置、装配各层并运行。

mod runtime;
mod watchdog;

use anyhow::{Context, Result};
use lyrabot_config::ConfigHandle;
use lyrabot_platform::{Platform, cli::CliPlatform, qq::QqPlatform};
use runtime::Runtime;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 版本：包版本 + 构建所在 git 提交。
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("LYRABOT_GIT_HASH"), ")");

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Args {
    config: PathBuf,
    mode: Mode,
}

enum Mode {
    /// 本地命令行；`streaming = false` 时模拟 QQ 的整段分段发送
    Cli { streaming: bool },
    /// 正式运行：接入 QQ 官方机器人
    Qq,
    /// 自检：配置、数据库、模型连通性
    Check,
}

fn parse_args() -> Result<Args> {
    let mut config = PathBuf::from("lyrabot.toml");
    let mut mode = Mode::Cli { streaming: true };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => config = it.next().context("--config 需要路径")?.into(),
            "--cli" => mode = Mode::Cli { streaming: true },
            "--no-stream" => mode = Mode::Cli { streaming: false },
            "--check" => mode = Mode::Check,
            "--qq" => mode = Mode::Qq,
            "--version" | "-V" => {
                println!("lyrabot {VERSION}");
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("用法：lyrabot [--config lyrabot.toml] [--qq | --cli | --no-stream | --check]");
                std::process::exit(0);
            }
            o => anyhow::bail!("未知参数 `{o}`，见 --help"),
        }
    }
    Ok(Args { config, mode })
}

fn main() -> Result<()> {
    let args = parse_args()?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LYRABOT_LOG").unwrap_or_else(|_| "warn,lyrabot=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .init();
    lyrabot_llm::install_crypto();

    // 单用户场景 2 个工作线程足够，降低常驻内存
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.block_on(async_main(args))
}

async fn async_main(args: Args) -> Result<()> {
    let cfg = ConfigHandle::open(&args.config).with_context(|| format!("装载配置 {}", args.config.display()))?;
    let runtime = Arc::new(Runtime::open(cfg).await?);

    match args.mode {
        Mode::Check => runtime.check().await,
        Mode::Cli { streaming } => serve(runtime, Arc::new(CliPlatform::new(streaming))).await,
        Mode::Qq => {
            let qcfg = runtime.config().qq.clone().context("配置缺少 [platforms.qq]")?;
            let platform = Arc::new(QqPlatform::new(qcfg, runtime.store().clone()));
            serve(runtime, platform).await
        }
    }
}

/// 接入平台并处理消息，直到平台退出或收到停止信号。
async fn serve(runtime: Arc<Runtime>, platform: Arc<dyn Platform>) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(64);
    let p = platform.clone();
    let mut reader = tokio::spawn(async move { p.run(tx).await });
    runtime.clone().attach(platform.clone());
    let notifier = watchdog::Notifier::from_env().map(Arc::new);
    if let Some(n) = &notifier {
        n.send("READY=1");
        if let Some(every) = watchdog::interval() {
            watchdog::spawn(n.clone(), every, platform, runtime.store().clone());
        }
    }
    let result = loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => runtime.clone().handle(m).await,
                None => break Ok(()),
            },
            r = &mut reader => {
                break match r {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(anyhow::anyhow!("平台退出：{e}")),
                    Err(e) => Err(anyhow::anyhow!("平台任务异常：{e}")),
                };
            }
            _ = shutdown_signal() => {
                tracing::info!("收到停止信号，正在退出");
                break Ok(());
            }
        }
    };
    if let Some(n) = &notifier {
        n.send("STOPPING=1");
    }
    runtime.shutdown().await;
    reader.abort();
    result
}

/// Ctrl+C 或 systemd 的 SIGTERM。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("注册 SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
