//! Immutable execution requirements captured when a task is queued.
use super::{scheduler::AnalysisOperation, Store};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

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

/// Snapshot the canonical target and all interested occurrence paths. Legacy tasks
/// remain intact; equivalent queued searches are merged without dropping their reasons.
pub(super) fn migrate_graph(conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let has_target = tx
        .prepare("PRAGMA table_info(deep_tasks)")
        .map_err(|e| e.to_string())?
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|column| column == "position_id");
    if !has_target {
        tx.execute_batch("ALTER TABLE deep_tasks ADD COLUMN position_id INTEGER REFERENCES deep_position_records(id)")
            .map_err(|e|e.to_string())?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS deep_task_interests (
            task_id TEXT NOT NULL REFERENCES deep_tasks(id) ON DELETE CASCADE,
            node_id INTEGER NOT NULL REFERENCES deep_tree_nodes(id) ON DELETE CASCADE,
            reason TEXT NOT NULL,
            priority INTEGER NOT NULL DEFAULT 0,
            remaining_prolong INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(task_id,node_id,reason)
         );
         CREATE INDEX IF NOT EXISTS deep_task_interests_node ON deep_task_interests(node_id);
         CREATE INDEX IF NOT EXISTS deep_tasks_position_status ON deep_tasks(position_id,status);
         DROP INDEX IF EXISTS deep_tasks_one_equivalent_pending;
         UPDATE deep_tasks SET position_id=(
            SELECT n.position_id FROM deep_tree_nodes n
            WHERE n.project_id=deep_tasks.project_id AND (
                n.id=deep_tasks.occurrence_id OR
                (deep_tasks.occurrence_id IS NULL AND n.source_task_id=deep_tasks.id AND n.parent_id IS NULL))
            ORDER BY n.id LIMIT 1)
         WHERE position_id IS NULL;
         INSERT OR IGNORE INTO deep_task_interests(task_id,node_id,reason,priority,remaining_prolong)
         SELECT t.id,n.id,t.strategy,t.priority,t.remaining_prolong FROM deep_tasks t
         JOIN deep_tree_nodes n ON n.project_id=t.project_id AND (
            n.id=t.occurrence_id OR (t.occurrence_id IS NULL AND n.source_task_id=t.id AND n.parent_id IS NULL));",
    ).map_err(|e|e.to_string())?;
    type Key = (String, i64, String, String, u16, u32, u32, String, String);
    let pending = {
        let mut stmt = tx
            .prepare(
                "SELECT id,project_id,position_id,execution_engine,engine_profile,
            requested_multipv,requested_time_ms,requested_depth,operation_mode,moves,status
            FROM deep_tasks WHERE status IN ('queued','running') AND position_id IS NOT NULL
            ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END,rowid",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, u16>(5)?,
                        r.get::<_, u32>(6)?,
                        r.get::<_, u32>(7)?,
                        r.get::<_, String>(8)?,
                        r.get::<_, String>(9)?,
                    ),
                    r.get::<_, String>(10)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    let mut winners = std::collections::HashMap::<Key, String>::new();
    for (task_id, key, _status) in pending {
        if let Some(winner) = winners.get(&key) {
            tx.execute("INSERT OR IGNORE INTO deep_task_interests(task_id,node_id,reason,priority,remaining_prolong)
                SELECT ?1,node_id,reason,priority,remaining_prolong FROM deep_task_interests WHERE task_id=?2",
                params![winner,task_id]).map_err(|e|e.to_string())?;
            tx.execute("UPDATE deep_tasks SET priority=MAX(priority,(SELECT priority FROM deep_tasks WHERE id=?2)),
                remaining_prolong=MAX(remaining_prolong,(SELECT remaining_prolong FROM deep_tasks WHERE id=?2)) WHERE id=?1",
                params![winner,task_id]).map_err(|e|e.to_string())?;
            tx.execute(
                "UPDATE deep_tasks SET status='cancelled',error=?2 WHERE id=?1",
                params![task_id, format!("coalesced into {winner}")],
            )
            .map_err(|e| e.to_string())?;
            log::debug!("Coalesced persisted task {task_id} into {winner}");
        } else {
            winners.insert(key, task_id);
        }
    }
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS deep_tasks_one_equivalent_pending
        ON deep_tasks(project_id,position_id,execution_engine,engine_profile,
            requested_multipv,requested_time_ms,requested_depth,operation_mode,moves)
        WHERE position_id IS NOT NULL AND status IN ('queued','running');
        PRAGMA user_version=6;",
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

pub(super) fn add_interest(
    tx: &Transaction<'_>,
    task_id: &str,
    node_id: i64,
    reason: &str,
    priority: i64,
    remaining_prolong: u8,
) -> Result<(), String> {
    tx.execute(
        "INSERT INTO deep_task_interests(task_id,node_id,reason,priority,remaining_prolong)
        VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(task_id,node_id,reason) DO UPDATE SET
            priority=MAX(priority,excluded.priority),
            remaining_prolong=MAX(remaining_prolong,excluded.remaining_prolong)",
        params![task_id, node_id, reason, priority, remaining_prolong],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
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

#[cfg(test)]
mod graph_tests {
    use crate::deep_analysis::{position_after, start_fen, Store};
    use shakmaty::{fen::Fen, EnPassantMode};

    #[test]
    fn legacy_result_replay_installs_interests_schema_before_rebuilding_tree() {
        use crate::deep_analysis::parse_line;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-replay.sqlite");
        let store = Store::open(&path).unwrap();
        let project = store
            .create_project(
                "legacy replay",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        store.begin_running(&project.id, "paused").unwrap();
        let root = store.claim(&project.id).unwrap().unwrap();
        store
            .save(
                &root.id,
                &[parse_line("info depth 12 score cp 30 pv e2e4", &root.fen, &[]).unwrap()],
            )
            .unwrap();
        let conn = store.conn().unwrap();
        conn.execute(
            "DELETE FROM deep_tree_analyses WHERE task_id=?1",
            [&root.id],
        )
        .unwrap();
        conn.execute_batch("DROP TABLE deep_task_interests")
            .unwrap();
        drop(conn);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let conn = reopened.conn().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM deep_tree_analyses WHERE task_id=?1",
                [&root.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM deep_tree_backfill_deferred WHERE task_id=?1",
                [&root.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn shared_completed_search_materializes_legal_pv_under_every_interested_path() {
        use crate::deep_analysis::parse_line;
        use rusqlite::params;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("shared-pvs.sqlite")).unwrap();
        let project = store
            .create_project(
                "shared PV",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        store.begin_running(&project.id, "paused").unwrap();
        let root = store.claim(&project.id).unwrap().unwrap();
        let paths = ["g1f3 g8f6 b1c3 b8c6", "b1c3 b8c6 g1f3 g8f6"];
        store
            .save(
                &root.id,
                &paths
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        parse_line(
                            &format!("info depth 12 multipv {} score cp 20 pv {p}", i + 1),
                            &root.fen,
                            &[],
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let conn = store.conn().unwrap();
        let ids: Vec<i64> = paths
            .iter()
            .map(|p| {
                conn.query_row(
                    "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                    params![
                        project.id,
                        serde_json::to_string(&p.split_whitespace().collect::<Vec<_>>()).unwrap()
                    ],
                    |r| r.get(0),
                )
                .unwrap()
            })
            .collect();
        drop(conn);
        let fen = Fen::from_position(
            position_after(
                &root.fen,
                &paths[0]
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let first = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[0])
            .unwrap();
        assert_eq!(
            store
                .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[1])
                .unwrap()
                .id,
            first.id
        );
        store.claim(&project.id).unwrap();
        store
            .save(
                &first.id,
                &[parse_line("info depth 14 score cp 42 pv e2e4 e7e5", &fen, &[]).unwrap()],
            )
            .unwrap();
        let conn = store.conn().unwrap();
        for parent in ids {
            let child:(String,i64)=conn.query_row("SELECT history,position_id FROM deep_tree_nodes WHERE parent_id=?1 AND move_uci='e2e4'",[parent],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert!(child.0.contains("e2e4"));
            assert!(child.1 > 0);
            assert_eq!(conn.query_row("SELECT count(*) FROM deep_tree_nodes WHERE parent_id IN (SELECT id FROM deep_tree_nodes WHERE parent_id=?1 AND move_uci='e2e4') AND move_uci='e7e5'",[parent],|r|r.get::<_,i64>(0)).unwrap(),1);
        }
    }

    #[test]
    fn later_accepted_reuse_materializes_pv_on_new_path_without_new_search() {
        use crate::deep_analysis::parse_line;
        use rusqlite::params;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("later-pvs.sqlite")).unwrap();
        let project = store
            .create_project(
                "later PV",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        store.begin_running(&project.id, "paused").unwrap();
        let root = store.claim(&project.id).unwrap().unwrap();
        let paths = ["g1f3 g8f6 b1c3 b8c6", "b1c3 b8c6 g1f3 g8f6"];
        store
            .save(
                &root.id,
                &paths
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        parse_line(
                            &format!("info depth 12 multipv {} score cp 20 pv {p}", i + 1),
                            &root.fen,
                            &[],
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let conn = store.conn().unwrap();
        let ids: Vec<i64> = paths
            .iter()
            .map(|p| {
                conn.query_row(
                    "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                    params![
                        project.id,
                        serde_json::to_string(&p.split_whitespace().collect::<Vec<_>>()).unwrap()
                    ],
                    |r| r.get(0),
                )
                .unwrap()
            })
            .collect();
        drop(conn);
        let fen = Fen::from_position(
            position_after(
                &root.fen,
                &paths[0]
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let first = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[0])
            .unwrap();
        store.claim(&project.id).unwrap();
        store
            .save_cached(
                &first.id,
                &[
                    parse_line(
                        "info depth 16 multipv 1 score cp 42 pv e2e4 e7e5",
                        &fen,
                        &[],
                    )
                    .unwrap(),
                    parse_line(
                        "info depth 16 multipv 2 score cp 30 pv d2d4 d7d5",
                        &fen,
                        &[],
                    )
                    .unwrap(),
                ],
                200,
                &project.engine,
            )
            .unwrap();
        let shared = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[1])
            .unwrap();
        assert_eq!(first.id, shared.id);
        assert_eq!(shared.status, "completed");
        let conn = store.conn().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM deep_tree_nodes WHERE parent_id=?1 AND move_uci='e2e4'",
                [ids[1]],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(store.list_tasks(&project.id).unwrap().len(), 2);
    }

    #[test]
    fn scheduler_rejects_an_engine_that_would_mix_incompatible_minimax_scores() {
        use crate::deep_analysis::scheduler::AnalysisSettings;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("engine-profile.sqlite")).unwrap();
        let engine = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let project = store
            .create_project("single engine", &engine, 100, 1, &start_fen(), &[])
            .unwrap();
        let changed = AnalysisSettings {
            engine_id: "different-engine".into(),
            ..Default::default()
        };
        assert!(store.enable_automatic(&project.id, &changed).is_err());
        assert!(store.automatic(&project.id).unwrap().is_none());
    }

    #[test]
    fn migration_keeps_history_scoped_and_fen_only_pending_searches_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context.sqlite");
        let store = Store::open(&path).unwrap();
        let start = start_fen();
        let project = store
            .create_project(
                "history",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start,
                &["e2e4".to_owned()],
            )
            .unwrap();
        let original = store.list_tasks(&project.id).unwrap().remove(0);
        assert_eq!(original.moves, vec!["e2e4"]);
        let fen = Fen::from_position(
            position_after(&start, &original.moves).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let independent = store.queue_root(&project.id, &fen, 1).unwrap();
        assert_ne!(
            independent.id, original.id,
            "FEN-only and history-scoped searches cannot coalesce"
        );
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let pending = reopened.list_tasks(&project.id).unwrap();
        assert_eq!(pending.iter().filter(|t| t.status == "queued").count(), 2);
        assert_eq!(
            pending
                .iter()
                .map(|t| t.id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
    }
}
