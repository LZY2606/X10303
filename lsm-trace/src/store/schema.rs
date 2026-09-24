/// 全部 DDL。投影表都带 `branch_id`，按分支事件链重放重建。
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- 案例包（一个数据库可容纳多个；默认一个离线包）
CREATE TABLE IF NOT EXISTS case_meta (
    id TEXT PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    narrative TEXT NOT NULL,
    coordinate_basis TEXT NOT NULL,   -- 坐标基准说明
    created_event_id TEXT NOT NULL
);

-- 分支
CREATE TABLE IF NOT EXISTS branch (
    id TEXT PRIMARY KEY,
    case_id TEXT NOT NULL REFERENCES case_meta(id),
    name TEXT NOT NULL,
    parent_event_id TEXT,            -- 从哪个事件分出（主线为 NULL）
    head_event_id TEXT,              -- 当前游标
    created_seq INTEGER NOT NULL,
    is_main INTEGER NOT NULL DEFAULT 0,
    UNIQUE(case_id, name)
);

-- 不可变事件链
CREATE TABLE IF NOT EXISTS event (
    id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL REFERENCES branch(id),
    seq INTEGER NOT NULL,            -- 分支内逻辑时钟
    parent_id TEXT,                  -- 父事件（同分支上一事件；分支点用 branch.parent_event_id）
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,           -- JSON
    payload_digest TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL, -- 墙上时钟，不参与身份
    UNIQUE(branch_id, seq)
);

-- 原始表示（不可变材料）。raw_json 原样保存，永不规范化丢失。
CREATE TABLE IF NOT EXISTS raw_record (
    id TEXT PRIMARY KEY,
    case_id TEXT NOT NULL,
    import_event_id TEXT NOT NULL,
    origin_kind TEXT NOT NULL,       -- wal | flush | sst_summary | snapshot | ruleset
    pointer TEXT NOT NULL,           -- 导入载荷中的 JSON Pointer
    raw_json TEXT NOT NULL,
    raw_ref TEXT NOT NULL            -- 来源材料的稳定引用（如 wal-0#r3）
);

-- 诊断（无法转换的值留在 raw_record 原位，诊断挂在这里）
CREATE TABLE IF NOT EXISTS diagnostic (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT REFERENCES event(id),
    branch_id TEXT NOT NULL,
    raw_ref TEXT,
    code TEXT NOT NULL,
    severity TEXT NOT NULL,
    path TEXT NOT NULL,
    message TEXT NOT NULL
);

-- 参数版本（规则修订）
CREATE TABLE IF NOT EXISTS ruleset (
    id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    body TEXT NOT NULL,              -- JSON
    note TEXT NOT NULL,
    UNIQUE(branch_id, version)
);

-- 人工决定
CREATE TABLE IF NOT EXISTS decision (
    id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    subject TEXT NOT NULL,           -- 例如 compaction split
    choice TEXT NOT NULL,
    rationale TEXT NOT NULL,
    supersedes TEXT
);

-- 压实运行
CREATE TABLE IF NOT EXISTS run (
    id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL,            -- planned | staged | installed | interrupted | aborted
    installed_event_id TEXT,
    input_digest TEXT NOT NULL,
    ruleset_id TEXT NOT NULL,
    parent_event_id TEXT NOT NULL
);

-- ===================== 投影（按分支重放） =====================

CREATE TABLE IF NOT EXISTS p_batch (
    branch_id TEXT NOT NULL,
    id TEXT NOT NULL,
    origin TEXT NOT NULL,            -- wal | memtable
    name TEXT NOT NULL,
    state TEXT NOT NULL,             -- active | flushed
    flush_file_id TEXT,
    PRIMARY KEY(branch_id, id)
);

CREATE TABLE IF NOT EXISTS p_file (
    branch_id TEXT NOT NULL,
    id TEXT NOT NULL,
    level INTEGER NOT NULL,          -- -1 = memtable, 0..=N SSTable 层
    origin TEXT NOT NULL,            -- flush | sst_summary | compaction
    smallest TEXT,
    largest TEXT,
    size_bytes INTEGER,
    size_unit TEXT,
    born_event_id TEXT NOT NULL,
    dead_event_id TEXT,
    PRIMARY KEY(branch_id, id)
);

CREATE TABLE IF NOT EXISTS p_entry (
    branch_id TEXT NOT NULL,
    id TEXT NOT NULL,                -- 内容身份（稳定）
    key TEXT NOT NULL,
    seq INTEGER NOT NULL,
    kind INTEGER NOT NULL,
    value TEXT,
    value_unit TEXT,
    batch_id TEXT,                   -- 位于 WAL/memtable 中
    file_id TEXT,                    -- 或位于文件中
    born_event_id TEXT NOT NULL,
    dropped_event_id TEXT,
    dropped_reason TEXT,
    PRIMARY KEY(branch_id, id)
);

CREATE TABLE IF NOT EXISTS p_range (
    branch_id TEXT NOT NULL,
    id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    start TEXT NOT NULL,
    end TEXT NOT NULL,
    file_id TEXT,
    born_event_id TEXT NOT NULL,
    dropped_event_id TEXT,
    dropped_reason TEXT,
    PRIMARY KEY(branch_id, id)
);

CREATE TABLE IF NOT EXISTS p_snapshot (
    branch_id TEXT NOT NULL,
    id TEXT NOT NULL,
    last_seq INTEGER NOT NULL,
    seq_unit TEXT NOT NULL,
    state TEXT NOT NULL,             -- active | released
    born_event_id TEXT NOT NULL,
    dead_event_id TEXT,
    PRIMARY KEY(branch_id, id)
);

CREATE TABLE IF NOT EXISTS p_run_output (
    branch_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    file_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    smallest TEXT NOT NULL,
    largest TEXT NOT NULL,
    PRIMARY KEY(branch_id, run_id, file_id)
);

-- ===================== 半写暂存（崩溃钩子） =====================

CREATE TABLE IF NOT EXISTS scratch_run (
    run_id TEXT PRIMARY KEY,
    branch_id TEXT NOT NULL,
    phase TEXT NOT NULL,             -- staging_outputs | staged_pending_install
    staged_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS scratch_output (
    run_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    file_json TEXT NOT NULL,
    entries_json TEXT NOT NULL,
    ranges_json TEXT NOT NULL,
    PRIMARY KEY(run_id, idx)
);
"#;

pub fn ensure_meta(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES('schema_version','1')",
        [],
    )?;
    Ok(())
}
