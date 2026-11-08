//! qya 内置工具（docs/01-architecture.md 8.1）。
//!
//! 通用：shell（RTK 改写、硬禁止、审计）、fs_read/fs_write/fs_patch/fs_search、memory、todo、cron、skill_view；
//! 运维：komari_report、blog_sync、mail。
//! [`register_builtin`] 按配置把可用工具注册进注册表；memory 因每个角色可见的文件不同，由调用方按角色构造。

mod blog;
pub mod cron;
mod fs;
mod komari;
mod mail;
pub mod memory;
mod recall;
mod shell;
mod skills;
mod todo;
pub mod util;

pub use blog::BlogSync;
pub use cron::CronTool;
pub use fs::{FsPatch, FsRead, FsSearch, FsWrite};
pub use komari::KomariReport;
pub use mail::Mail;
pub use memory::{Memory, MemoryTarget};
pub use shell::Shell;
pub use recall::Recall;
pub use skills::SkillView;
pub use todo::Todo;

use qya_agent::ToolRegistry;
use qya_config::Config;
use qya_store::Store;
use std::sync::Arc;
use util::Audit;

/// 注册全部按配置可用的内置工具，返回实际注册的名字。
pub fn register_builtin(reg: &mut ToolRegistry, c: &Config, store: &Store) -> Vec<String> {
    let t = &c.tools;
    let audit = Audit::new(t.audit_log.clone());
    // 长输出存进媒体目录下的 tool-output，随每日媒体清理回收
    reg.register(Arc::new(Shell::new(t.shell.clone(), audit.clone()).with_spill(t.media_dir.join("tool-output"))));
    reg.register(Arc::new(FsRead::default()));
    reg.register(Arc::new(FsWrite::default()));
    reg.register(Arc::new(FsPatch::default()));
    reg.register(Arc::new(FsSearch::default()));
    reg.register(Arc::new(Todo::default()));
    let specialists: Vec<String> = c.orchestra.specialists.keys().cloned().collect();
    reg.register(Arc::new(CronTool::new(store.clone(), &specialists)));
    reg.register(Arc::new(Recall::new(store.clone())));
    if let Some(s) = SkillView::new(&t.skills_dir) {
        reg.register(Arc::new(s));
    }
    if let Some(p) = &t.komari_config {
        reg.register(Arc::new(KomariReport::new(p.clone())));
    }
    if let Some(p) = &t.blog_script {
        reg.register(Arc::new(BlogSync::new(p.clone(), t.blog_timeout_s, audit.clone())));
    }
    if let Some(m) = &t.mail {
        reg.register(Arc::new(Mail::new(m.clone(), audit)));
    }
    reg.names()
}
