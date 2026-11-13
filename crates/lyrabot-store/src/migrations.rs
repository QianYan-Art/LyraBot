//! 迁移按 `PRAGMA user_version` 顺序执行，每个版本一个事务；可重复调用。

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // v1：初始 schema（docs/08-storage-operations.md 第 1 节）
    r#"
CREATE TABLE sessions (
    id            INTEGER PRIMARY KEY,
    kind          TEXT NOT NULL,             -- persona / worker / specialist
    name          TEXT,                      -- 专员名或任务标题
    parent_session INTEGER REFERENCES sessions(id),
    fork_entry    INTEGER,                   -- 从父会话哪条条目分叉（零拷贝继承）
    leaf_id       INTEGER,                   -- 当前分支叶子
    model         TEXT NOT NULL,
    effort        TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'active',  -- active / archived / closed
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX idx_sessions_kind_status ON sessions(kind, status);

CREATE TABLE entries (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    parent_id  INTEGER REFERENCES entries(id),
    kind       TEXT NOT NULL,                -- message / compaction
    payload    TEXT NOT NULL,                -- JSON
    tokens     INTEGER NOT NULL DEFAULT 0,   -- 估算或实测 token
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_entries_session ON entries(session_id);

CREATE TABLE mailbox (
    id          INTEGER PRIMARY KEY,
    to_session  INTEGER NOT NULL REFERENCES sessions(id),
    from_session INTEGER NOT NULL REFERENCES sessions(id),
    payload     TEXT NOT NULL,
    delivered   INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_mailbox_pending ON mailbox(to_session, delivered);

CREATE TABLE expressions (
    id        INTEGER PRIMARY KEY,
    situation TEXT NOT NULL,
    text      TEXT NOT NULL,
    kind      TEXT NOT NULL,                 -- habit / jargon
    meaning   TEXT,
    hits      INTEGER NOT NULL DEFAULT 0,
    weight    REAL NOT NULL DEFAULT 1.0,
    last_used INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE cron_jobs (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL,
    schedule  TEXT NOT NULL,
    action    TEXT NOT NULL,                 -- JSON
    enabled   INTEGER NOT NULL DEFAULT 1,
    last_run  INTEGER,
    created_at INTEGER NOT NULL
);

CREATE TABLE quota (
    key    TEXT PRIMARY KEY,                 -- 例：qq_proactive:2026-11
    used   INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE TABLE kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#,
    // v2：表达/黑话学习需要触发词与去重（任务 08）
    r#"
ALTER TABLE expressions ADD COLUMN triggers TEXT NOT NULL DEFAULT '[]';
CREATE UNIQUE INDEX idx_expressions_kind_text ON expressions(kind, text);
"#,
    // v3：先存后做。执行者运行标记（重启后据此发现被中断的任务）与平台延迟发件箱
    r#"
ALTER TABLE sessions ADD COLUMN run_owner INTEGER;   -- 执行者排队或运行中时为汇报收件会话，结束时清空
CREATE TABLE outbox (
    id         INTEGER PRIMARY KEY,
    channel    TEXT NOT NULL,                -- 例：qq
    payload    TEXT NOT NULL,                -- JSON：{text, media}
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_outbox_channel ON outbox(channel, id);
CREATE TABLE run_queue (                    -- 交给运行中执行者、尚未确定写进其会话的插话/追问
    id         INTEGER PRIMARY KEY,
    worker     INTEGER NOT NULL REFERENCES sessions(id),
    owner      INTEGER NOT NULL,             -- 汇报收件会话
    payload    TEXT NOT NULL,                -- 消息 JSON
    tokens     INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_run_queue_worker ON run_queue(worker, id);
"#,
    // v4：记忆整理（候选与修订）、过往对话检索索引（FTS5 trigram，中文 3 字以上片段可直接匹配）
    r#"
CREATE TABLE memory_candidates (
    id         INTEGER PRIMARY KEY,
    target     TEXT NOT NULL,                -- user / memory
    text       TEXT NOT NULL,
    basis      TEXT NOT NULL DEFAULT 'stated', -- stated / agreed / corrected
    created_at INTEGER NOT NULL
);
CREATE TABLE memory_revisions (
    id         INTEGER PRIMARY KEY,
    summary    TEXT NOT NULL,                -- 变更说明
    before     TEXT NOT NULL,                -- JSON：目标 → 整理前全文
    after      TEXT NOT NULL,                -- JSON：目标 → 整理后全文
    created_at INTEGER NOT NULL
);
CREATE VIRTUAL TABLE recall_fts USING fts5(text, session UNINDEXED, entry UNINDEXED, role UNINDEXED, at UNINDEXED, tokenize='trigram');
"#,
    // v5：QQ 主动消息计数从 kv 迁到 v1 预留的 quota 表（原子占用），迁移后删除 kv 旧键
    r#"
INSERT INTO quota(key, used, updated_at)
    SELECT key, CAST(value AS INTEGER), CAST(strftime('%s','now') AS INTEGER) * 1000 FROM kv WHERE key LIKE 'qq_proactive:%'
    ON CONFLICT(key) DO UPDATE SET used = MAX(quota.used, excluded.used);
DELETE FROM kv WHERE key LIKE 'qq_proactive:%';
"#,
];

pub fn run(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
        tracing::info!(version = i + 1, "数据库迁移完成");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn idempotent() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        super::run(&mut c).unwrap();
        super::run(&mut c).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v as usize, super::MIGRATIONS.len());
    }

    #[test]
    fn v5_moves_proactive_count_from_kv_to_quota() {
        let mut c = rusqlite::Connection::open_in_memory().unwrap();
        for sql in &super::MIGRATIONS[..4] {
            c.execute_batch(sql).unwrap();
        }
        c.pragma_update(None, "user_version", 4).unwrap();
        c.execute_batch("INSERT INTO kv VALUES('qq_proactive:2026-11','3'), ('other','x');").unwrap();
        super::run(&mut c).unwrap();
        let used: i64 = c.query_row("SELECT used FROM quota WHERE key='qq_proactive:2026-11'", [], |r| r.get(0)).unwrap();
        let left: i64 = c.query_row("SELECT count(*) FROM kv", [], |r| r.get(0)).unwrap();
        assert_eq!((used, left), (3, 1), "计数迁入 quota，kv 只剩无关键");
    }
}
