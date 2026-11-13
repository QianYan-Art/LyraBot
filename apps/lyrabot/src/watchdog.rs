//! systemd 集成（sd_notify 协议，直接写 `NOTIFY_SOCKET`，不引入依赖）：
//! 进入服务循环时发 `READY=1`，退出前发 `STOPPING=1`；单元配置了 `WatchdogSec` 时，
//! 独立任务按看门狗周期的 1/3 探活，只有平台接收循环仍在推进、存储线程能及时响应才发 `WATCHDOG=1`。
//! 卡死时停发心跳，由 systemd 杀掉并按 `Restart=always` 拉起。
//!
//! 探活不看主处理循环：`/compact` 等命令会在主循环里等待模型数分钟，属于正常阻塞。
//! 没有 `NOTIFY_SOCKET`（本地 CLI、非 systemd 启动、非 unix 平台）时全部为空操作。

use lyrabot_platform::Platform;
use lyrabot_store::Store;
use std::sync::Arc;
use std::time::Duration;

/// 存储线程探活的等待上限：正常查询为微秒级，超时说明存储线程被长时间占用或已卡死。
const STORE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Notifier {
    #[cfg(unix)]
    sock: std::os::unix::net::UnixDatagram,
    #[cfg(unix)]
    addr: std::os::unix::net::SocketAddr,
}

impl Notifier {
    /// 从 `NOTIFY_SOCKET` 建立通知通道；不在 systemd 通知模式下运行时返回 `None`。
    #[cfg(unix)]
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var("NOTIFY_SOCKET").ok()?;
        let n = Self::connect(&raw);
        if n.is_none() {
            // Type=notify 下收不到 READY 会启动超时，这里必须留下可诊断的记录
            tracing::error!(socket = %raw, "无法建立 systemd 通知通道");
        }
        n
    }

    #[cfg(unix)]
    fn connect(raw: &str) -> Option<Self> {
        use std::os::unix::net::{SocketAddr, UnixDatagram};
        let addr = match raw.strip_prefix('@') {
            #[cfg(target_os = "linux")]
            Some(name) => {
                use std::os::linux::net::SocketAddrExt;
                SocketAddr::from_abstract_name(name).ok()?
            }
            #[cfg(not(target_os = "linux"))]
            Some(_) => return None,
            None => SocketAddr::from_pathname(raw).ok()?,
        };
        let sock = UnixDatagram::unbound().ok()?;
        sock.set_nonblocking(true).ok()?;
        Some(Self { sock, addr })
    }

    #[cfg(not(unix))]
    pub fn from_env() -> Option<Self> {
        None
    }

    pub fn send(&self, state: &str) {
        #[cfg(unix)]
        if let Err(e) = self.sock.send_to_addr(state.as_bytes(), &self.addr) {
            tracing::warn!(error = %e, state, "通知 systemd 失败");
        }
        #[cfg(not(unix))]
        let _ = state;
    }
}

/// 看门狗探活周期：`WATCHDOG_USEC` 的 1/3；未启用或心跳不归本进程（`WATCHDOG_PID` 不符）时为 `None`。
pub fn interval() -> Option<Duration> {
    interval_from(std::env::var("WATCHDOG_PID").ok().as_deref(), std::env::var("WATCHDOG_USEC").ok().as_deref(), std::process::id())
}

fn interval_from(pid: Option<&str>, usec: Option<&str>, me: u32) -> Option<Duration> {
    if let Some(p) = pid
        && p.parse::<u32>().ok() != Some(me)
    {
        return None;
    }
    // 周期不足 1 秒属于配置错误（也避免零周期 interval 直接 panic）
    Some(Duration::from_micros(usec?.parse::<u64>().ok()? / 3)).filter(|d| *d >= Duration::from_secs(1))
}

/// 启动探活任务；进程退出时随运行时结束。
/// 存储线程被单个长作业占住（如大库 VACUUM）超过看门狗周期也会被视为卡死；单用户库很小，正常维护远低于该时长。
pub fn spawn(notifier: Arc<Notifier>, every: Duration, platform: Arc<dyn Platform>, store: Store) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let store_ok = matches!(tokio::time::timeout(STORE_PROBE_TIMEOUT, store.call(|_| Ok(()))).await, Ok(Ok(())));
            let platform_ok = platform.alive();
            if store_ok && platform_ok {
                notifier.send("WATCHDOG=1");
            } else {
                tracing::warn!(store_ok, platform_ok, "探活未通过，暂停看门狗心跳");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn interval_is_a_third_of_watchdog_and_respects_pid() {
        assert_eq!(super::interval_from(None, Some("60000000"), 7), Some(Duration::from_secs(20)));
        assert_eq!(super::interval_from(Some("7"), Some("60000000"), 7), Some(Duration::from_secs(20)));
        assert_eq!(super::interval_from(Some("8"), Some("60000000"), 7), None, "心跳归其他进程");
        assert_eq!(super::interval_from(Some("abc"), Some("60000000"), 7), None, "PID 非法时不认领");
        assert_eq!(super::interval_from(None, Some("0"), 7), None);
        assert_eq!(super::interval_from(None, Some("2"), 7), None, "周期过短");
        assert_eq!(super::interval_from(None, None, 7), None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sends_state_to_notify_socket() {
        use std::os::unix::net::UnixDatagram;
        let path = std::env::temp_dir().join(format!("lyrabot-notify-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let server = UnixDatagram::bind(&path).unwrap();
        let addr = std::os::unix::net::SocketAddr::from_pathname(&path).unwrap();
        let n = super::Notifier { sock: UnixDatagram::unbound().unwrap(), addr };
        n.send("READY=1");
        let mut buf = [0u8; 32];
        let len = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"READY=1");
        let _ = std::fs::remove_file(&path);
    }
}
