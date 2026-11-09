//! 学习与记忆（docs/04-persona-memory.md 第 3 节）：后台一次调用同时学习说法与记忆候选，空闲时把候选整理进
//! `user.md` / `memory.md`（整理前后全文存为修订，可撤销），以及 `/learn` 命令。
//! 学习与整理只在唐语歌空闲时进行，不占聊天路径；记忆快照按会话冻结，改动在新会话生效。

use super::*;
use qya_command::LearnScope;
use qya_persona::consolidate::{self, Candidate, TARGETS};
use std::collections::BTreeMap;

/// 记忆候选攒到这个数量，或最早的候选超过一天时整理。
const CONSOLIDATE_AT: usize = 5;
const CONSOLIDATE_AFTER_MS: i64 = 86_400_000;

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn when(ms: i64) -> String {
    let tz = jiff::tz::TimeZone::get("Asia/Shanghai").unwrap_or(jiff::tz::TimeZone::fixed(jiff::tz::offset(8)));
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_zoned(tz).strftime("%m-%d %H:%M").to_string()).unwrap_or_default()
}

/// 变更说明的计数摘要，例如“新增 2 条、修改 1 条”。
fn tally(changes: &[String]) -> String {
    let n = |p: &str| changes.iter().filter(|c| c.starts_with(p)).count();
    let parts: Vec<String> =
        [("新增", n("新增")), ("修改", n("修改")), ("删除", n("删除"))].into_iter().filter(|(_, k)| *k > 0).map(|(l, k)| format!("{l} {k} 条")).collect();
    if parts.is_empty() { "没有改动".into() } else { parts.join("、") }
}

impl Runtime {
    fn memory_path(&self, name: &str) -> PathBuf {
        self.cfg.get().persona.memory_dir.join(format!("{name}.md"))
    }

    fn read_memory(&self) -> BTreeMap<String, Vec<String>> {
        TARGETS
            .iter()
            .map(|(name, _, _)| (name.to_string(), consolidate::entries(&std::fs::read_to_string(self.memory_path(name)).unwrap_or_default())))
            .collect()
    }

    /// 原子写入记忆文件（先写临时文件再改名），进程在任何时刻退出都不会留下半个文件。
    fn write_memory(&self, name: &str, list: &[String]) -> Result<()> {
        let path = self.memory_path(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("md.tmp");
        std::fs::write(&tmp, consolidate::render(list))?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 启动时调用：上次整理在写文件途中退出时（修订已记下，文件停在整理前或只写了一半），把整理结果补写完整。
    /// 只在每个文件都等于整理前或整理后时才动手；文件被手动改过则不碰。返回是否补写。
    pub(super) fn finish_interrupted_consolidation(&self, rev: &qya_store::MemoryRevision) -> Result<bool> {
        let before: BTreeMap<String, String> = serde_json::from_str(&rev.before)?;
        let after: BTreeMap<String, String> = serde_json::from_str(&rev.after)?;
        let current = self.read_memory();
        let mut incomplete = false;
        for (name, list) in &current {
            let is = |m: &BTreeMap<String, String>| m.get(name).map(|t| consolidate::entries(t)).unwrap_or_default() == *list;
            match (is(&after), is(&before)) {
                (true, _) => {}
                (false, true) => incomplete = true,
                (false, false) => return Ok(false),
            }
        }
        if incomplete {
            for (name, text) in &after {
                self.write_memory(name, &consolidate::entries(text))?;
            }
        }
        Ok(incomplete)
    }

    pub(super) async fn learning_enabled(&self) -> Result<bool> {
        Ok(self.cfg.get().persona.learner.enabled && self.store.kv_get(KV_LEARN_ENABLED).await?.as_deref() != Some("off"))
    }

    /// 后台学习循环：每分钟检查一次是否达到触发条件。
    pub(super) fn spawn_learner(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match self.learn_once(false).await {
                    Ok(Some(msg)) => tracing::info!(%msg, "学习完成"),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "学习失败"),
                }
            }
        });
    }

    /// 执行一次学习与整理。`force` 时忽略开关、阈值与空闲要求（/learn now）。未执行返回 `None`。
    pub(super) async fn learn_once(&self, force: bool) -> Result<Option<String>> {
        if !force && !self.learning_enabled().await? {
            return Ok(None);
        }
        let (session, phase, idle) = {
            let st = self.state();
            (st.session, st.phase, st.last_activity.map(|t| t.elapsed()))
        };
        // 只在空闲时进行，不与回复抢占
        if !force && phase != Phase::Idle {
            return Ok(None);
        }
        if self.learning.swap(true, Ordering::AcqRel) {
            return Ok(force.then(|| card::warn("后台正在学习中，请稍后再试")));
        }
        let result = self.learn_and_consolidate(session, idle, force).await;
        self.learning.store(false, Ordering::Release);
        result
    }

    async fn learn_and_consolidate(&self, session: SessionId, idle: Option<Duration>, force: bool) -> Result<Option<String>> {
        let lc = self.cfg.get().persona.learner.clone();
        let mut done = Vec::new();
        let ckpt: i64 = self.store.kv_get(&kv_learn_ckpt(session)).await?.and_then(|v| v.parse().ok()).unwrap_or(0);
        let count = self.store.count_user_messages_since(session, ckpt).await?;
        let idle_ok = idle.is_some_and(|d| d >= Duration::from_secs(lc.idle_minutes * 60)) && count >= i64::from(lc.min_messages);
        if count > 0 && (force || count >= i64::from(lc.batch_messages) || idle_ok) {
            done.push(self.learn_batch(session, ckpt, &lc).await?);
        }
        if let Some(m) = self.consolidate_memory(force, &lc).await? {
            done.push(m);
        }
        if done.is_empty() {
            return Ok(None);
        }
        Ok(Some(done.join("\n")))
    }

    async fn learn_batch(&self, session: SessionId, ckpt: i64, lc: &qya_config::LearnerSettings) -> Result<String> {
        let c = self.cfg.get();
        let entries = self.store.messages_since(session, ckpt, 200).await?;
        let last = entries.last().map(|e| e.id).unwrap_or(ckpt);
        let msgs: Vec<Message> = entries.into_iter().filter_map(|e| serde_json::from_value(e.payload).ok()).collect();
        let kit = self.kit();
        let text = learner::transcript(&msgs, kit.persona.user_name(), "唐语歌");
        let pcfg = c.provider(&c.persona.provider).context("persona provider 未配置")?;
        let provider = qya_llm::build(pcfg)?;
        let learned = learner::learn(provider.as_ref(), &lc.model, lc.effort, &text, CancellationToken::new()).await?;
        let n = learned.expressions.len();
        let (added, reinforced, removed) = self.store.learn_expressions(learned.expressions, lc.max_items).await?;
        let notes = learned.memories.into_iter().map(|m| (m.target, m.text, m.basis)).collect();
        let queued = self.store.add_memory_candidates(notes).await?;
        if self.store.session(session).await.is_ok() {
            self.store.kv_set(&kv_learn_ckpt(session), &last.to_string()).await?;
        }
        self.rebuild_index().await?;
        Ok(format!(
            "学习了 {} 条新消息：说法提取 {n} 条（新增 {added}，强化 {reinforced}，淘汰 {removed}），记忆候选 {queued} 条",
            msgs.len()
        ))
    }

    /// 整理记忆候选：攒够或放久了才整理，`force` 时只要有候选就整理。模型失败时候选保留。
    ///
    /// 等待模型期间不持锁：先读快照，模型返回后在锁内重读，文件已被改动（memory 工具、/learn forget）就放弃本轮、候选保留；
    /// 未改动则先写修订再写文件，写文件失败时撤销修订并放回候选。
    async fn consolidate_memory(&self, force: bool, lc: &qya_config::LearnerSettings) -> Result<Option<String>> {
        let cands = self.store.memory_candidates().await?;
        let Some(oldest) = cands.first().map(|c| c.created_at) else { return Ok(None) };
        if !force && cands.len() < CONSOLIDATE_AT && now_ms() - oldest < CONSOLIDATE_AFTER_MS {
            return Ok(None);
        }
        let c = self.cfg.get();
        let before = {
            let _g = self.memory_lock.lock().await;
            self.read_memory()
        };
        let originals: Vec<(String, String, String)> = cands.iter().map(|c| (c.target.clone(), c.text.clone(), c.basis.clone())).collect();
        let pcfg = c.provider(&c.persona.provider).context("persona provider 未配置")?;
        let provider = qya_llm::build(pcfg)?;
        let n = cands.len();
        let cands = cands.into_iter().map(|c| Candidate { id: c.id, target: c.target, text: c.text, basis: c.basis }).collect();
        let out = consolidate::consolidate(provider.as_ref(), &lc.model, lc.effort, &before, cands, CancellationToken::new()).await?;
        if out.files == before {
            self.store.drop_memory_candidates(out.processed).await?;
            return Ok(Some(format!("整理了 {n} 条记忆候选，没有需要改动的")));
        }
        let _g = self.memory_lock.lock().await;
        if self.read_memory() != before {
            return Ok(Some("整理期间记忆被修改过，本轮放弃，候选保留到下次".into()));
        }
        let render = |m: &BTreeMap<String, Vec<String>>| {
            serde_json::to_string(&m.iter().map(|(k, v)| (k.clone(), consolidate::render(v))).collect::<BTreeMap<_, _>>())
        };
        // 先记修订（含整理前全文）再写文件：任何时刻退出都不会丢掉撤销所需的原文
        let summary = out.changes.join("\n");
        let rev = self.store.commit_consolidation(out.processed, summary, render(&before)?, render(&out.files)?).await?;
        for (name, list) in &out.files {
            if before.get(name) == Some(list) {
                continue;
            }
            if let Err(e) = self.write_memory(name, list) {
                // 恢复已写的文件、撤销修订、放回候选，保持“修订 = 实际改动”
                for (n2, l2) in &before {
                    let _ = self.write_memory(n2, l2);
                }
                self.store.delete_memory_revision(rev).await?;
                self.store.add_memory_candidates(originals).await?;
                return Err(e.context("写入记忆文件失败，已恢复原状"));
            }
        }
        Ok(Some(format!("整理了 {n} 条记忆候选：{}", tally(&out.changes))))
    }

    pub(super) async fn rebuild_index(&self) -> Result<()> {
        let index = Arc::new(Index::build(self.store.expressions().await?));
        *self.index.lock().expect("index 锁中毒") = index;
        Ok(())
    }

    pub(super) async fn learn_command(&self, cmd: LearnCmd) -> Result<String> {
        Ok(match cmd {
            LearnCmd::Overview => self.learn_overview().await?,
            LearnCmd::List(LearnScope::Expressions) => {
                let all = self.store.expressions().await?;
                if all.is_empty() {
                    "💬 还没有学到任何说法".into()
                } else {
                    let mut c = Card::new("💬", "学到的说法");
                    c.line(format!("共 {} 条，按权重显示前 20 条", all.len()));
                    for e in all.iter().take(20) {
                        let kind = if e.kind == "habit" { "习惯" } else { "黑话" };
                        c.section(format!("#{} {}", e.id, e.text)).line(format!("{kind}｜{}", e.meaning.as_deref().unwrap_or("-")));
                    }
                    c.hint("删除：/learn forget 编号").build()
                }
            }
            LearnCmd::List(LearnScope::Memory) => {
                let files = self.read_memory();
                let mut c = Card::new("📝", "长期记忆");
                for (name, label, prefix) in TARGETS {
                    c.section(label);
                    let list = &files[name];
                    if list.is_empty() {
                        c.line("（空）");
                    }
                    for (i, e) in list.iter().enumerate() {
                        c.item(format!("{prefix}{} {e}", i + 1));
                    }
                }
                c.hint("删除：/learn forget u3 或 m2").hint("改动在新会话或 /compact 后生效").build()
            }
            LearnCmd::Forget(id) => self.forget(&id).await?,
            LearnCmd::On => {
                self.store.kv_set(KV_LEARN_ENABLED, "on").await?;
                if self.cfg.get().persona.learner.enabled {
                    card::ok("自动学习已开启（说法与记忆）")
                } else {
                    card::warn("已记录开启，但配置 persona.learner.enabled=false，仍不会自动学习")
                }
            }
            LearnCmd::Off => {
                self.store.kv_set(KV_LEARN_ENABLED, "off").await?;
                Card::new("✅", "自动学习已暂停")
                    .item("不再自动学习说法与整理记忆")
                    .item("已有的说法与记忆不受影响")
                    .hint("恢复开启：/learn on")
                    .build()
            }
            LearnCmd::Now => match self.learn_once(true).await? {
                None => "💤 没有新的消息可学习，也没有待整理的记忆".into(),
                Some(msg) if msg.starts_with("⚠️") => msg,
                Some(msg) => {
                    let mut c = Card::new("✅", "学习与整理完成");
                    for line in msg.lines() {
                        c.item(line);
                    }
                    c.build()
                }
            },
            LearnCmd::Undo => self.undo_consolidation().await?,
        })
    }

    async fn learn_overview(&self) -> Result<String> {
        let on = if self.learning_enabled().await? { "开启" } else { "暂停" };
        let exprs = self.store.expressions().await?.len();
        let files = self.read_memory();
        let size = |name: &str| files[name].iter().map(|e| e.chars().count()).sum::<usize>();
        let pending = self.store.memory_candidates().await?.len();
        let mut c = Card::new("🌱", "学习与记忆");
        c.line(format!("自动学习：{on}")).section("现有").item(format!("说法 {exprs} 条"));
        for (name, label, _) in TARGETS {
            c.item(format!("{label} {} 条（{}/{} 字）", files[name].len(), size(name), qya_persona::MEMORY_FILE_LIMIT));
        }
        c.item(format!("待整理的记忆候选 {pending} 条"));
        match self.store.last_memory_revision().await? {
            Some(r) => {
                let changes: Vec<String> = r.summary.lines().map(str::to_owned).collect();
                c.section(format!("最近一次整理｜{}", when(r.created_at))).line(tally(&changes));
                for line in changes.iter().take(5) {
                    c.item(line);
                }
                if changes.len() > 5 {
                    c.line(format!("…另有 {} 项", changes.len() - 5));
                }
            }
            None => {
                c.section("最近一次整理").line("还没有整理过");
            }
        }
        c.hint("看说法：/learn list 说法").hint("看记忆：/learn list 记忆").hint("撤销最近一次整理：/learn undo");
        Ok(c.build())
    }

    async fn forget(&self, id: &str) -> Result<String> {
        if let Ok(n) = id.parse::<i64>() {
            return Ok(if self.store.forget_expression(n).await? {
                self.rebuild_index().await?;
                card::ok(format!("已删除说法 #{n}"))
            } else {
                card::warn(format!("没有说法 #{n}"))
            });
        }
        let (prefix, idx) = id.split_at(1);
        let Some((name, label, _)) = TARGETS.iter().find(|(_, _, p)| p.to_string() == prefix) else { return Ok(card::warn(format!("没有 {id}"))) };
        let Some(i) = idx.parse::<usize>().ok().and_then(|n| n.checked_sub(1)) else { return Ok(card::warn(format!("没有 {id}"))) };
        let _g = self.memory_lock.lock().await;
        let mut list = self.read_memory().remove(*name).unwrap_or_default();
        if i >= list.len() {
            return Ok(card::warn(format!("「{label}」里没有 {id}，发 /learn list 记忆 查看编号")));
        }
        let old = list.remove(i);
        self.write_memory(name, &list)?;
        Ok(Card::new("✅", &format!("已删除记忆 {id}"))
            .item(old)
            .hint("改动在新会话或 /compact 后生效")
            .build())
    }

    /// 撤销最近一次整理：只有记忆文件仍是那次整理的结果时才恢复，避免覆盖之后的手动修改。
    async fn undo_consolidation(&self) -> Result<String> {
        let _g = self.memory_lock.lock().await;
        let Some(rev) = self.store.last_memory_revision().await? else { return Ok("💤 没有可撤销的整理".into()) };
        let before: BTreeMap<String, String> = serde_json::from_str(&rev.before)?;
        let after: BTreeMap<String, String> = serde_json::from_str(&rev.after)?;
        for (name, text) in &after {
            let current = std::fs::read_to_string(self.memory_path(name)).unwrap_or_default();
            if consolidate::entries(&current) != consolidate::entries(text) {
                return Ok(card::warn("记忆在那次整理之后又被改过，为免覆盖新的改动，没有撤销；可用 /learn list 记忆 查看后按编号删除"));
            }
        }
        for (name, text) in &before {
            self.write_memory(name, &consolidate::entries(text))?;
        }
        self.store.delete_memory_revision(rev.id).await?;
        let changes: Vec<String> = rev.summary.lines().map(str::to_owned).collect();
        Ok(Card::new("✅", &format!("已撤销 {} 的整理", when(rev.created_at)))
            .item(tally(&changes))
            .hint("改动在新会话或 /compact 后生效")
            .build())
    }
}

#[cfg(test)]
mod tests {
    use super::tally;

    #[test]
    fn tally_counts_change_kinds() {
        let changes = vec!["新增（关于阿颜）：甲".to_owned(), "修改 u1：乙 → 丙".to_owned(), "新增（约定与共同的事）：丁".to_owned(), "未新增（user 已接近上限）：戊".to_owned()];
        assert_eq!(tally(&changes), "新增 2 条、修改 1 条");
        assert_eq!(tally(&[]), "没有改动");
    }
}
