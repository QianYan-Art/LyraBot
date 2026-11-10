//! 后台周期任务：汇报消费、前缀缓存保活、定时任务 `cron`、媒体与历史会话自动清理。

use super::*;
use lyrabot_orchestra::{ContextMode, DelegateRequest, Target};
use std::collections::HashSet;

impl Runtime {
    /// 消费后台结果：转为〔后台结果〕排队交给主对话角色，按 notify 决定立即开一轮转述还是等用户下次说话。
    pub(super) fn spawn_report_consumer(self: Arc<Self>) {
        let Some(mut rx) = self.reports.lock().expect("reports 锁中毒").take() else { return };
        // 启动时装载的汇报若需要立即转述，先开一轮
        if self.state().has_work() {
            self.clone().arm_timer();
        }
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                let uname = self.kit().persona.user_name().to_owned();
                let start = {
                    let mut st = self.state();
                    let relay = r.notify == Notify::Relay;
                    Self::queue_report(&mut st, r, &uname) && relay && matches!(st.phase, Phase::Idle | Phase::Debouncing)
                };
                if start {
                    self.clone().arm_timer();
                }
            }
        });
    }

    /// 汇报入队（按信件 id 去重），返回是否新加入。
    pub(super) fn queue_report(st: &mut State, r: WorkerReport, user_name: &str) -> bool {
        if r.mail != 0 && st.mail.iter().chain(&st.inflight).any(|m| m.id == r.mail) {
            return false;
        }
        st.relay_media.extend(r.media.iter().cloned());
        st.mail.push(Relay { id: r.mail, msg: Message::user(relay_text(&r, user_name)), notify: r.notify });
        true
    }

    /// 每周自动清理一次历史会话（上次时间记在 kv，进程重启不影响周期）。
    pub(super) fn spawn_weekly_gc(self: Arc<Self>) {
        const KEY: &str = "gc:last";
        const WEEK_MS: i64 = 7 * 86_400_000;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let now = jiff::Timestamp::now().as_millisecond();
                let last = match self.store.kv_get(KEY).await {
                    Ok(v) => v.and_then(|v| v.parse::<i64>().ok()),
                    Err(e) => {
                        tracing::warn!(error = %e, "读取清理时间失败");
                        continue;
                    }
                };
                let Some(last) = last else {
                    // 首次运行只记下起点，一周后再清理
                    let _ = self.store.kv_set(KEY, &now.to_string()).await;
                    continue;
                };
                if now - last < WEEK_MS || self.state().phase != Phase::Idle || self.sup.running() > 0 {
                    continue;
                }
                let days = self.cfg.get().agent.gc_days;
                match self.gc(days).await {
                    Ok(msg) => tracing::info!(%msg, "每周自动清理完成"),
                    Err(e) => tracing::warn!(error = %e, "每周自动清理失败"),
                }
                let _ = self.store.kv_set(KEY, &now.to_string()).await;
            }
        });
    }

    /// 媒体缓存清理：每小时删除媒体目录下超过 24 小时的文件（沿用 Hermes 的保留期）。
    pub(super) fn spawn_media_gc(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let dir = self.cfg.get().tools.media_dir.clone();
                let keep = match self.referenced_media().await {
                    Ok(k) => k,
                    Err(e) => {
                        tracing::warn!(error = %e, "读取待投递媒体失败，本次跳过清理");
                        continue;
                    }
                };
                let removed = tokio::task::spawn_blocking(move || gc_dir(&dir, Duration::from_secs(24 * 3600), &keep)).await.unwrap_or(0);
                if removed > 0 {
                    tracing::info!(removed, "已清理过期媒体文件");
                }
            }
        });
    }

    /// 未确认的汇报、发件箱与待随回复发出的媒体文件（清理时保留）。
    async fn referenced_media(&self) -> Result<HashSet<PathBuf>> {
        let mut keep: HashSet<PathBuf> = {
            let st = self.state();
            st.relay_media.iter().chain(&st.media_inflight).cloned().collect()
        };
        for m in self.store.pending_mail().await? {
            keep.extend(m.payload["media"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(PathBuf::from));
        }
        for q in self.store.outbox_list("qq").await? {
            keep.extend(q.payload["media"].as_str().map(PathBuf::from));
        }
        Ok(keep)
    }

    /// 定时任务调度：每 30 秒检查一次到期任务。
    pub(super) fn spawn_cron(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let jobs = match self.store.cron_list().await {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::warn!(error = %e, "读取定时任务失败");
                        continue;
                    }
                };
                let now = jiff::Timestamp::now();
                for job in lyrabot_tools::cron::due(&jobs, now) {
                    // 先记下（一次性任务直接删除）再触发：重启不会重复触发
                    let recorded = if lyrabot_tools::cron::is_once(&job) {
                        self.store.cron_remove(job.id).await.map(|_| ())
                    } else {
                        self.store.cron_mark_run(job.id, now.as_millisecond()).await
                    };
                    if let Err(e) = recorded {
                        tracing::warn!(error = %e, id = job.id, "记录定时任务运行失败");
                        continue;
                    }
                    tracing::info!(id = job.id, name = %job.name, "定时任务触发");
                    self.clone().fire_cron(job).await;
                }
            }
        });
    }

    async fn fire_cron(self: Arc<Self>, job: lyrabot_store::CronJob) {
        let uname = self.kit().persona.user_name().to_owned();
        let action = &job.action;
        match action["kind"].as_str() {
            Some("task") => {
                let to = action["to"].as_str().unwrap_or("worker");
                let req = DelegateRequest {
                    parent: self.session(),
                    parent_call_id: format!("cron-{}", job.id),
                    task: action["task"].as_str().unwrap_or_default().to_owned(),
                    context: ContextMode::Brief,
                    context_note: Some(format!("这是定时任务 #{}「{}」自动触发的，{uname}此刻不一定在线。", job.id, job.name)),
                    target: if to == "worker" { Target::Worker } else { Target::Specialist(to.to_owned()) },
                    model: None,
                    effort: None,
                    notify: Notify::Relay,
                };
                if let Err(e) = self.sup.delegate(req).await {
                    let msg = format!("〔定时任务 #{}「{}」派发失败：{e}〕\n〔请告诉{uname}这次没能执行〕", job.id, job.name);
                    self.on_chat(Message::user(msg), false);
                }
            }
            _ => {
                let msg = format!(
                    "〔定时提醒 #{}「{}」到点了〕\n{}\n〔这是{uname}之前让你设的提醒，不是刚发的新消息。用你自己的话提醒{uname}。〕",
                    job.id,
                    job.name,
                    action["text"].as_str().unwrap_or_default()
                );
                self.on_chat(Message::user(msg), false);
            }
        }
    }

    /// 主会话前缀缓存保活循环：每 30 秒检查一次，到达间隔时发 1-token 保活请求续期。
    pub(super) fn spawn_keepalive(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.keepalive_if_due().await;
            }
        });
    }

    /// 检查主会话是否满足保活条件；满足则用与正式请求完全相同的前缀请求 1 个 token，结果丢弃不落库。
    async fn keepalive_if_due(&self) {
        let ka = self.cfg.get().persona.keepalive;
        if ka.interval_minutes == 0 || ka.max_count == 0 {
            return;
        }
        let interval = Duration::from_secs(ka.interval_minutes * 60);
        let (session, count, epoch, cancel) = {
            let mut st = self.state();
            if st.phase != Phase::Idle
                || st.has_work()
                || st.keepalive_cancel.is_some()
                || st.keepalive_count >= ka.max_count
                || st.last_llm_at.is_none_or(|t| t.elapsed() < interval)
            {
                return;
            }
            let c = CancellationToken::new();
            st.keepalive_cancel = Some(c.clone());
            (st.session, st.keepalive_count + 1, st.keepalive_epoch, c)
        };
        let idle_s = self.kernel.session_idle(session).map(|d| d.as_secs());
        let res = self.keepalive_once(session, cancel.clone()).await;
        let mut st = self.state();
        st.keepalive_cancel = None;
        if st.session != session || st.keepalive_epoch != epoch || cancel.is_cancelled() {
            return;
        }
        match res {
            Ok(u) => {
                st.last_llm_at = Some(Instant::now());
                st.keepalive_count = count;
                st.total_usage.add(&u);
                let hit_pct = (u.cached_tokens * 100).checked_div(u.prompt_tokens).unwrap_or(0);
                tracing::info!(
                    session,
                    count,
                    max = ka.max_count,
                    prompt_tokens = u.prompt_tokens,
                    cached_tokens = u.cached_tokens,
                    hit_pct = %format!("{hit_pct}%"),
                    completion_tokens = u.completion_tokens,
                    idle_s,
                    "缓存保活完成"
                );
            }
            Err(e) => {
                // 失败也推进计时与计数，避免每 30 秒反复重试消耗额度
                st.last_llm_at = Some(Instant::now());
                st.keepalive_count = count;
                tracing::warn!(session, count, error = %e, "缓存保活请求失败");
            }
        }
    }

    async fn keepalive_once(&self, session: SessionId, cancel: CancellationToken) -> Result<Usage> {
        let c = self.cfg.get();
        let s = self.store.session(session).await?;
        let spec = self.persona_spec(&c, &s, "").await?;
        let ctx = self.kernel.context(s.id).await?;
        let mut messages: Vec<Message> = ctx.into_iter().map(|m| m.message).collect();
        messages.push(Message::user("."));
        let req = lyrabot_llm::ChatRequest {
            model: spec.model,
            system: spec.system,
            messages,
            tools: spec.tools.specs(),
            effort: spec.effort,
            max_tokens: Some(1),
            cache_key: Some(format!("lyrabot-s-{}", s.id)),
        };
        let comp = spec.provider.complete(req, cancel).await?;
        self.kernel.touch_session(s.id);
        Ok(comp.usage)
    }
}

/// 后台结果交给主对话时的消息文本（以〔〕标明是系统信息，见 assets/persona/runtime/agency.md）。
fn relay_text(r: &WorkerReport, user_name: &str) -> String {
    let who = match &r.kind {
        WorkerKind::Worker => format!("后台 #{}「{}」", r.id, r.title),
        WorkerKind::Specialist(n) if r.title.is_empty() || r.title == *n => format!("专员 {n}（#{}）", r.id),
        WorkerKind::Specialist(n) => format!("专员 {n}（#{}「{}」）", r.id, r.title),
    };
    let mut s = format!("〔后台结果：{who}{}，用时 {} 秒〕\n{}", r.status.label(), r.elapsed_s, r.summary);
    if let Some(q) = &r.question {
        s.push_str(&format!("\n〔需要{user_name}决定〕{q}"));
    }
    s.push_str(&format!("\n〔这是后台交回的结果，不是{user_name}的新消息。用你自己的话把要点告诉{user_name}；没做成就如实说。"));
    if r.question.is_some() {
        s.push_str(&format!("需要{user_name}决定的问题要单独、简短地问出来，等{user_name}回答后再用 agent_ctl continue 交给它。"));
    }
    s.push('〕');
    s
}

/// 递归删除目录下修改时间早于 `max_age` 的普通文件（不跟随符号链接、跳过 `keep` 中的文件），返回删除数量。
fn gc_dir(dir: &std::path::Path, max_age: Duration, keep: &HashSet<PathBuf>) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        if meta.is_dir() {
            n += gc_dir(&e.path(), max_age, keep);
        } else if meta.is_file()
            && !keep.contains(&e.path())
            && meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > max_age)
            && std::fs::remove_file(e.path()).is_ok()
        {
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::gc_dir;

    #[test]
    fn gc_removes_only_old_files() {
        let d = std::env::temp_dir().join(format!("lyrabot-gc-{}", std::process::id()));
        std::fs::create_dir_all(d.join("inbox")).unwrap();
        std::fs::write(d.join("inbox/new.jpg"), b"x").unwrap();
        let none = std::collections::HashSet::new();
        assert_eq!(gc_dir(&d, std::time::Duration::from_secs(3600), &none), 0);
        let keep = std::collections::HashSet::from([d.join("inbox/new.jpg")]);
        assert_eq!(gc_dir(&d, std::time::Duration::ZERO, &keep), 0, "仍被引用的文件保留");
        assert_eq!(gc_dir(&d, std::time::Duration::ZERO, &none), 1);
        assert!(!d.join("inbox/new.jpg").exists());
    }
}
