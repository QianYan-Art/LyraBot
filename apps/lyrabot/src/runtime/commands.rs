//! `/命令` 执行分发、状态卡片排版与主对话会话切换/清理。

use super::*;
use lyrabot_command::{AgentAction, AgentCmd, Command};

impl Runtime {
    pub(super) async fn gc(&self, days: u32) -> Result<String> {
        // 排队或运行中的执行者（包括刚被续接、数据库里仍是 archived 的）不能被清理
        let mut keep = vec![self.session()];
        keep.extend(self.sup.active_ids());
        let r = self.store.gc_sessions(days, keep).await?;
        self.sup.forget(&r.deleted_ids);
        self.kernel.forget_sessions(&r.deleted_ids);
        let mb = |b: i64| b as f64 / 1048576.0;
        let mut c = Card::new("🧹", "清理完成");
        c.line(format!("结束超过 {days} 天的会话"))
            .item(format!("删除会话 {} 个、记录 {} 条", r.sessions, r.entries))
            .item(format!("数据库 {:.2} → {:.2} MB", mb(r.bytes_before), mb(r.bytes_after)));
        if r.kept_referenced > 0 {
            c.item(format!("{} 个仍被较新的会话引用，暂时保留", r.kept_referenced));
        }
        Ok(c.build())
    }

    async fn sessions_report(&self) -> Result<String> {
        let cur = self.session();
        let list = self.store.persona_sessions(10).await?;
        let stats = self.store.stats().await?;
        let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
        let fmt = |ms: i64| {
            jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz.clone()).strftime("%m-%d %H:%M").to_string()).unwrap_or_default()
        };
        let pname = self.kit().persona.name().to_owned();
        let mut c = Card::new("📂", "会话与数据库");
        c.section(format!("{pname}最近的会话"));
        for x in &list {
            let state = if x.id == cur { "当前" } else if x.status == "closed" { "已结束" } else { "历史" };
            c.item(format!("#{} {state}｜{} 条｜{}", x.id, x.messages, fmt(x.updated_at)));
        }
        let active_lbl = format!("{pname}进行中");
        let closed_lbl = format!("{pname}已结束");
        let label = |k: &str, st: &str| match (k, st) {
            ("persona", "active") => active_lbl.as_str(),
            ("persona", "closed") => closed_lbl.as_str(),
            ("worker", "active") => "后台任务进行中",
            ("worker", "archived") => "后台任务已归档",
            ("specialist", "active") => "专员任务进行中",
            ("specialist", "archived") => "专员任务已归档",
            _ => "其他",
        };
        c.section("合计");
        for (k, st, n) in &stats.sessions {
            c.item(format!("{} {n} 个", label(k, st)));
        }
        c.item(format!("记录 {} 条，数据库 {:.2} MB", stats.entries, stats.bytes as f64 / 1048576.0))
            .hint("切换历史会话：/sessions 编号")
            .hint(format!("清理历史会话：/gc [天数]（每周自动清理 >{} 天）", self.cfg.get().agent.gc_days));
        Ok(c.build())
    }

    /// 占住空闲态（临时置为 Delivering，析构时恢复 Idle），防止会话切换或手动压缩期间启动新轮次或并发保活。
    fn lock_idle(&self) -> Option<IdleLock<'_>> {
        let mut st = self.state();
        if st.phase != Phase::Idle || st.has_work() {
            return None;
        }
        if let Some(c) = st.keepalive_cancel.take() {
            c.cancel();
        }
        st.phase = Phase::Delivering;
        Some(IdleLock(self))
    }

    async fn switch_session(&self, target: SessionId) -> Result<String> {
        let pname = self.kit().persona.name().to_owned();
        let Some(_idle) = self.lock_idle() else {
            return Ok(card::warn(format!("{pname}正在回复中，请等这轮结束或先发 /stop")));
        };
        let old = self.session();
        if target == old {
            return Ok(card::warn(format!("当前已经在会话 #{target}")));
        }
        let c = self.cfg.get();
        let default_s2 = self.kit().persona.snapshot(&c.persona.memory_dir);
        let mut row = match self.store.switch_persona_session(old, target, default_s2).await {
            Ok(r) => r,
            Err(lyrabot_store::StoreError::NotFound(_)) => {
                return Ok(card::warn(format!("没有会话 #{target}，发 /sessions 查看可切换的编号")));
            }
            Err(lyrabot_store::StoreError::Invalid(_)) => return Ok(card::warn(format!("#{target} 不是{pname}的会话，无法切换"))),
            Err(e) => return Err(e.into()),
        };
        if model_info(&c, &row.model).is_err() {
            row.model = c.persona.model.clone();
            self.store.update_session(target, Some(row.model.clone()), None, None).await?;
        }
        {
            let mut st = self.state();
            st.session = target;
            st.reset_keepalive(true);
        }
        Ok(Card::new("✅", &format!("已切回会话 #{target}"))
            .item(format!("{}｜思考 {}", row.model, row.effort))
            .item(format!("旧会话 #{old} 已标为已结束"))
            .hint("沿用该会话当时的记忆快照，需要最新记忆时发 /compact")
            .build())
    }

    pub(super) async fn command(&self, cmd: Command) -> Result<String> {
        let c = self.cfg.get();
        Ok(match cmd {
            Command::Help(t) => lyrabot_command::help(t.as_deref()),
            Command::Status => self.status().await?,
            Command::New => {
                self.stop_current();
                let old = self.session();
                let id = self.new_persona_session().await?;
                {
                    let mut st = self.state();
                    st.session = id;
                    st.reset_keepalive(true);
                }
                self.store.update_session(old, None, None, Some(lyrabot_store::SessionStatus::Closed)).await?;
                Card::new("✅", &format!("已开启新会话 #{id}"))
                    .item("记忆和关系不变")
                    .item(format!("旧会话 #{old} 已标为已结束（{} 天后可被清理）", c.agent.gc_days))
                    .hint(format!("切回旧会话：/sessions {old}"))
                    .build()
            }
            Command::Sessions(None) => self.sessions_report().await?,
            Command::Sessions(Some(id)) => self.switch_session(id).await?,
            Command::Gc(days) => self.gc(days.unwrap_or(c.agent.gc_days)).await?,
            Command::Stop => {
                if self.stop_current() {
                    card::ok("已停止")
                } else {
                    "💤 当前没有进行中的回复".into()
                }
            }
            Command::Compact => {
                let Some(_idle) = self.lock_idle() else {
                    return Ok(card::warn("正在回复中，稍后再压缩"));
                };
                let s = self.store.session(self.session()).await?;
                let mut spec = self.persona_spec(&c, &s, "").await?;
                spec.tail = None;
                let ctx = self.kernel.context(s.id).await?;
                let est = lyrabot_agent::context::estimate_messages(&ctx);
                let compacted = self.kernel.compact(&spec, s.id, &ctx, est, &CancellationToken::new()).await?;
                self.refresh_snapshot(s.id).await?;
                self.state().reset_keepalive(true);
                if compacted {
                    card::ok(format!("已压缩并刷新记忆快照，压缩前约 {} token", tokens(est)))
                } else {
                    card::ok("内容较少无需压缩历史，已刷新当前会话的记忆快照")
                }
            }
            Command::Model(None) => {
                let s = self.store.session(self.session()).await?;
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                let mut out = Card::new("🧠", "模型");
                out.line(format!("当前 {}", s.model)).section("可选");
                for m in p.models.keys() {
                    out.item(if *m == s.model { format!("{m}（当前）") } else { m.clone() });
                }
                out.hint("切换：/model 名称").build()
            }
            Command::Model(Some(m)) => {
                if model_info(&c, &m).is_err() {
                    return Ok(card::warn(format!("模型 {m} 不在模型目录中，发 /model 查看可选模型")));
                }
                self.store.update_session(self.session(), Some(m.clone()), None, None).await?;
                self.state().reset_keepalive(true);
                card::ok(format!("{}的模型已切换为 {m}", self.kit().persona.name()))
            }
            Command::Effort(None) => {
                let s = self.store.session(self.session()).await?;
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                let cur = s.effort.parse::<Effort>().unwrap_or(Effort::Low);
                Card::new("💭", "思考等级")
                    .line(format!("当前 {}（请求参数 {}）", s.effort, p.effort_param(cur)))
                    .section("可选")
                    .item("low / medium / high / xhigh / max")
                    .hint("切换：/effort 等级")
                    .build()
            }
            Command::Effort(Some(e)) => {
                self.store.update_session(self.session(), None, Some(e.to_string()), None).await?;
                self.state().reset_keepalive(true);
                let p = c.provider(&c.persona.provider).context("persona provider 未配置")?;
                Card::new("✅", &format!("思考等级已切换为 {e}"))
                    .item(format!("实际请求参数：{}", p.effort_param(e)))
                    .build()
            }
            Command::Learn(cmd) => self.learn_command(cmd).await?,
            Command::Provider(None) => lyrabot_plugins::provider_cmd::list(&self.cfg),
            Command::Provider(Some((cat, field, value))) => {
                lyrabot_plugins::provider_cmd::apply(&self.cfg, &cat, &field, &value).map_or_else(card::warn, card::ok)
            }
            Command::Agent(AgentCmd::List) => lyrabot_orchestra::render_list(&self.sup.list().await),
            Command::Agent(AgentCmd::Task { id, action }) => match action {
                AgentAction::Show => self.agent_detail(id).await?,
                AgentAction::Stop => self.sup.stop(id).map_or_else(card::warn, card::ok),
                AgentAction::Say(t) => self.sup.continue_worker(id, &t, self.session()).await.map_or_else(card::warn, card::ok),
            },
            Command::Reload => {
                let c = self.cfg.reload()?;
                let kit = PersonaKit::load(&c, &self.sup, &self.memory_lock)?;
                *self.kit.lock().expect("kit 锁中毒") = Arc::new(kit);
                self.state().reset_keepalive(true);
                Card::new("✅", "配置与人格资产已重新加载").hint("改动在新会话或 /compact 后生效").build()
            }
        })
    }

    /// 停止当前一轮并丢弃待处理缓冲；返回是否确有东西被停止。
    pub(super) fn stop_current(&self) -> bool {
        let mut st = self.state();
        st.reset_keepalive(true);
        st.timer_gen += 1;
        st.first_pending = None;
        let had_pending = !std::mem::take(&mut st.pending).is_empty();
        if st.phase == Phase::Debouncing {
            st.phase = Phase::Idle;
        }
        match st.running.take() {
            Some(c) => {
                st.discard_unconsumed = true;
                c.cancel();
                true
            }
            None => had_pending,
        }
    }

    async fn status(&self) -> Result<String> {
        let c = self.cfg.get();
        let s = self.store.session(self.session()).await?;
        let info = model_info(&c, &s.model)?;
        let ctx = self.kernel.context(s.id).await?;
        let est = lyrabot_agent::context::estimate_messages(&ctx);
        let workers = self.sup.running();
        let (phase, pending, last, total) = {
            let st = self.state();
            (st.phase, st.pending.len(), st.last_usage, st.total_usage)
        };
        let rss = memory_stats::memory_stats().map(|m| m.physical_mem / 1024 / 1024).unwrap_or(0);
        let up = self.started.elapsed().as_secs();
        let hit = |u: &Usage| if u.prompt_tokens == 0 { 0.0 } else { u.cached_tokens as f64 * 100.0 / u.prompt_tokens as f64 };
        let phase = match phase {
            Phase::Idle => "空闲",
            Phase::Debouncing => "等你说完",
            Phase::Generating => "回复中",
            Phase::Tools => "调用工具中",
            Phase::Delivering => "发送中",
        };
        Ok(Card::new("📊", "运行状态")
            .line(format!("lyrabot {}", crate::VERSION))
            .section("会话")
            .item(format!("#{}｜{phase}", s.id))
            .item(format!("{}｜思考 {}", s.model, s.effort))
            .section("上下文")
            .line(card::bar(est as f64 / info.context_length.max(1) as f64))
            .line(format!("约 {} / {} token", tokens(est), tokens(info.context_length)))
            .section("用量")
            .item(format!("上一轮 输入 {}，输出 {}", tokens(last.prompt_tokens), tokens(last.completion_tokens)))
            .item(format!("缓存命中 {:.0}%（累计 {:.0}%）", hit(&last), hit(&total)))
            .item(format!("累计输入 {}", tokens(total.prompt_tokens)))
            .section("运行")
            .item(format!("待合并 {pending} 条，后台 {workers} 个"))
            .item(format!("内存 {rss} MB，已运行 {}", duration(up)))
            .build())
    }

    /// /agent <id>：基本信息 + 该执行者最近一条回复。
    async fn agent_detail(&self, id: SessionId) -> Result<String> {
        let Ok(row) = self.store.session(id).await else {
            return Ok(card::warn(format!("没有后台任务 #{id}，发 /agent list 查看列表")));
        };
        if row.kind == SessionKind::Persona {
            return Ok(card::warn(format!("#{id} 是{}的会话，不是后台任务", self.kit().persona.name())));
        }
        let info = self.sup.list().await.into_iter().find(|w| w.id == id);
        let status = info.as_ref().map(|w| w.status.label()).unwrap_or("已归档");
        let mut c = Card::new("🤖", &format!("后台 #{id}｜{status}"));
        c.line(row.name.unwrap_or_default()).item(format!("{}｜思考 {}", row.model, row.effort));
        if let Some(p) = info.and_then(|w| w.progress) {
            c.item(format!("进度：{p}"));
        }
        let msgs = self.store.messages_since(id, 0, 50).await?;
        let last = msgs
            .iter()
            .rev()
            .filter_map(|e| serde_json::from_value::<Message>(e.payload.clone()).ok())
            .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty());
        if let Some(m) = last {
            let t: String = m.text().chars().take(800).collect();
            c.section("最近回复").line(t);
        }
        c.hint(format!("续接：/agent {id} 话"));
        Ok(c.build())
    }
}

struct IdleLock<'a>(&'a Runtime);

impl Drop for IdleLock<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state();
        if st.phase == Phase::Delivering {
            st.phase = Phase::Idle;
        }
    }
}

/// token 数的简写：1234 → 1.2k，1234567 → 1.2M。
fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// 时长的简写：3725 秒 → 1 小时 2 分。
fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} 秒"),
        60..3600 => format!("{} 分", secs / 60),
        3600..86400 => format!("{} 小时 {} 分", secs / 3600, secs % 3600 / 60),
        _ => format!("{} 天 {} 小时", secs / 86400, secs % 86400 / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::{duration, tokens};

    #[test]
    fn compact_numbers_for_chat() {
        assert_eq!((tokens(999), tokens(12_345), tokens(262_144), tokens(1_048_576)), ("999".into(), "12.3k".into(), "262.1k".into(), "1.0M".into()));
        assert_eq!((duration(42), duration(125), duration(3_725), duration(90_000)), ("42 秒".into(), "2 分".into(), "1 小时 2 分".into(), "1 天 1 小时".into()));
    }

    #[tokio::test]
    async fn switches_persona_session_and_keeps_snapshot() {
        let dir = std::env::temp_dir().join(format!("lyrabot-rt-sw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("k.key");
        std::fs::write(&key_path, "test-key").unwrap();
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/persona");
        let cfg_toml = format!(
            r#"
[paths]
data_dir = '{}'
[llm]
default_provider = "p"
[llm.providers.p]
base_url = "http://127.0.0.1:9"
key_file = '{}'
transport = "chat_completions"
[llm.providers.p.models.m]
context_length = 8192
[llm.providers.p.models."kimi-for-coding"]
context_length = 8192
[persona]
provider = "p"
model = "m"
effort = "low"
assets_dir = '{}'
[orchestra]
provider = "p"
default_model = "m"
default_effort = "low"
worker_prompt = '{}'
"#,
            dir.display().to_string().replace('\\', "/"),
            key_path.display().to_string().replace('\\', "/"),
            assets.display().to_string().replace('\\', "/"),
            assets.join("../agents/worker.md").display().to_string().replace('\\', "/"),
        );
        let cfg_path = dir.join("lyrabot.toml");
        std::fs::write(&cfg_path, cfg_toml).unwrap();
        let rt = std::sync::Arc::new(super::Runtime::open(lyrabot_config::ConfigHandle::open(&cfg_path).unwrap()).await.unwrap());
        let s1 = rt.session();
        rt.store().kv_set(&super::kv_snapshot(s1), "S2_FIRST").await.unwrap();

        // /new 开启新会话 s2，s1 变为 closed
        let out_new = rt.command(lyrabot_command::Command::New).await.unwrap();
        let s2 = rt.session();
        assert_ne!(s1, s2);
        assert!(out_new.contains(&format!("已开启新会话 #{s2}")));
        assert_eq!(rt.store().session(s1).await.unwrap().status, lyrabot_store::SessionStatus::Closed);

        // 非 Idle 时拒绝切换
        rt.state().phase = super::Phase::Generating;
        let busy = rt.command(lyrabot_command::Command::Sessions(Some(s1))).await.unwrap();
        assert!(busy.starts_with("⚠️"), "{busy}");
        rt.state().phase = super::Phase::Idle;

        // 后台任务会话不允许切换
        let w = rt.store().create_session(lyrabot_store::NewSession { kind: Some(lyrabot_store::SessionKind::Worker), model: "m".into(), effort: "low".into(), ..Default::default() }).await.unwrap();
        let bad = rt.command(lyrabot_command::Command::Sessions(Some(w))).await.unwrap();
        assert!(bad.contains(&format!("不是{}的会话", rt.kit().persona.name())), "{bad}");

        // 切回 s1：s2 变为 closed，s1 恢复 active，沿用 s1 原快照
        let sw = rt.command(lyrabot_command::Command::Sessions(Some(s1))).await.unwrap();
        assert!(sw.contains(&format!("已切回会话 #{s1}")), "{sw}");
        assert_eq!(rt.session(), s1);
        assert_eq!(rt.store().session(s1).await.unwrap().status, lyrabot_store::SessionStatus::Active);
        assert_eq!(rt.store().session(s2).await.unwrap().status, lyrabot_store::SessionStatus::Closed);
        assert_eq!(rt.snapshot(s1).await.unwrap(), "S2_FIRST");
        assert_eq!(rt.store().kv_get(super::KV_PERSONA_SESSION).await.unwrap().as_deref(), Some(s1.to_string().as_str()));

        // 验证保活状态在会话/模型切换、重载、/stop 与旧轮次收尾时的重置与代次保护
        let tok = tokio_util::sync::CancellationToken::new();
        let old_epoch = {
            let mut st = rt.state();
            st.keepalive_cancel = Some(tok.clone());
            st.keepalive_count = 3;
            st.last_llm_at = Some(std::time::Instant::now());
            st.keepalive_epoch
        };
        let _ = rt.command(lyrabot_command::Command::Model(Some("kimi-for-coding".into()))).await.unwrap();
        {
            let st = rt.state();
            assert!(tok.is_cancelled(), "切换模型应取消进行中的保活");
            assert_eq!(st.keepalive_count, 0);
            assert!(st.last_llm_at.is_none());
            assert!(st.keepalive_epoch > old_epoch, "代次应自增以拒绝旧轮次写回");
        }
        // 模拟代次变更前启动的旧轮次收尾：不应重新写入 last_llm_at
        rt.clone()
            .finish_turn(
                Ok(lyrabot_agent::TurnOutcome {
                    text: String::new(),
                    usage: lyrabot_core::Usage { prompt_tokens: 100, completion_tokens: 10, cached_tokens: 80, ..Default::default() },
                    reason: lyrabot_core::StopReason::Cancelled,
                    media: Vec::new(),
                    unconsumed: Vec::new(),
                    inputs_saved: false,
                    error: None,
                }),
                Vec::new(),
                s1,
                old_epoch,
            )
            .await;
        assert!(rt.state().last_llm_at.is_none(), "旧代次轮次收尾不得重开保活计时");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
