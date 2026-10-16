//! Immutable execution requirements captured when a task is queued.
use super::{scheduler::AnalysisOperation, Store};
use rusqlite::{Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub(super) struct TaskExecution {
    pub engine: String,
    pub engine_profile: String,
    pub multipv: u16,
    pub time_ms: u32,
    pub depth: u32,
    pub operation: AnalysisOperation,
    pub cache_policy: String,
}

// Both the insert trigger and one-time legacy backfill use the same snapshot rule.
// For pre-migration tasks this is a best-effort reconstruction: old settings history
// is unavailable. Newly inserted tasks never inherit later settings changes.
const STAMP: &str = "execution_engine=COALESCE(NULLIF((SELECT json_extract(s.settings,'$.engine_id') FROM deep_scheduler s WHERE s.project_id=deep_tasks.project_id AND s.enabled=1),''),(SELECT p.engine FROM deep_projects p WHERE p.id=deep_tasks.project_id)),
    engine_profile='default',
    requested_multipv=(SELECT p.multipv FROM deep_projects p WHERE p.id=deep_tasks.project_id),
    requested_time_ms=COALESCE((SELECT json_extract(s.settings,'$.analysis_time_ms') FROM deep_scheduler s WHERE s.project_id=deep_tasks.project_id AND s.enabled=1),(SELECT p.time_ms FROM deep_projects p WHERE p.id=deep_tasks.project_id)),
    requested_depth=COALESCE((SELECT json_extract(s.settings,'$.analysis_depth') FROM deep_scheduler s WHERE s.project_id=deep_tasks.project_id AND s.enabled=1),12),
    operation_mode=COALESCE((SELECT json_extract(s.settings,'$.analysis_operation') FROM deep_scheduler s WHERE s.project_id=deep_tasks.project_id AND s.enabled=1),'time_and_depth'),
    cache_policy='compatible_only'";

pub(super) fn migrate(conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let columns = tx
        .prepare("PRAGMA table_info(deep_tasks)")
        .map_err(|e| e.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    for (column, migration) in [
        ("execution_engine", "ALTER TABLE deep_tasks ADD COLUMN execution_engine TEXT NOT NULL DEFAULT ''"),
        ("engine_profile", "ALTER TABLE deep_tasks ADD COLUMN engine_profile TEXT NOT NULL DEFAULT 'default'"),
        ("requested_multipv", "ALTER TABLE deep_tasks ADD COLUMN requested_multipv INTEGER NOT NULL DEFAULT 1"),
        ("requested_time_ms", "ALTER TABLE deep_tasks ADD COLUMN requested_time_ms INTEGER NOT NULL DEFAULT 0"),
        ("requested_depth", "ALTER TABLE deep_tasks ADD COLUMN requested_depth INTEGER NOT NULL DEFAULT 12"),
        ("operation_mode", "ALTER TABLE deep_tasks ADD COLUMN operation_mode TEXT NOT NULL DEFAULT 'time_and_depth'"),
        ("cache_policy", "ALTER TABLE deep_tasks ADD COLUMN cache_policy TEXT NOT NULL DEFAULT 'compatible_only'"),
    ] {
        if !columns.iter().any(|existing| existing == column) {
            tx.execute_batch(migration).map_err(|e| e.to_string())?;
        }
    }
    tx.execute_batch(&format!(
        "UPDATE deep_tasks SET {STAMP} WHERE execution_engine='';
        CREATE TRIGGER IF NOT EXISTS deep_task_execution_snapshot AFTER INSERT ON deep_tasks
        BEGIN UPDATE deep_tasks SET {STAMP} WHERE id=NEW.id; END;
        PRAGMA user_version=3;"
    ))
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

impl Store {
    pub(super) fn task_execution(&self, id: &str) -> Result<TaskExecution, String> {
        let raw: Option<(String, String, u16, u32, u32, String, String)> = self.conn()?
            .query_row(
                "SELECT execution_engine,engine_profile,requested_multipv,requested_time_ms,requested_depth,operation_mode,cache_policy FROM deep_tasks WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let (engine, engine_profile, multipv, time_ms, depth, mode, cache_policy) =
            raw.ok_or("analysis task not found")?;
        let operation = match mode.as_str() {
            "time_or_depth" => AnalysisOperation::TimeOrDepth,
            "time_and_depth" => AnalysisOperation::TimeAndDepth,
            "ungated" => AnalysisOperation::Ungated,
            _ => return Err(format!("unsupported task operation: {mode}")),
        };
        if engine.is_empty()
            || engine_profile != "default"
            || cache_policy != "compatible_only"
            || multipv == 0
            || multipv > 32
            || time_ms == 0
            || time_ms > 3_600_000
        {
            return Err("unsupported or invalid persisted task execution requirements".into());
        }
        Ok(TaskExecution {
            engine,
            engine_profile,
            multipv,
            time_ms,
            depth,
            operation,
            cache_policy,
        })
    }
}
