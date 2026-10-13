//! Persistent move occurrences for IDeA. Every node keeps its actual path; a repeated
//! board is not merged with another history (threefold repetition depends on the path).
use super::{position_after, DeepAnalysisLine};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use shakmaty::{fen::Fen, uci::UciMove, EnPassantMode, Position};

pub(super) fn create_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS deep_tree_nodes (
            id INTEGER PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES deep_projects(id) ON DELETE CASCADE,
            source_task_id TEXT NOT NULL REFERENCES deep_tasks(id) ON DELETE CASCADE,
            analyzed_task_id TEXT UNIQUE REFERENCES deep_tasks(id) ON DELETE SET NULL,
            parent_id INTEGER REFERENCES deep_tree_nodes(id) ON DELETE CASCADE,
            move_uci TEXT,
            fen TEXT NOT NULL,
            history TEXT NOT NULL,
            ply INTEGER NOT NULL,
            direct_score_kind TEXT,
            direct_score_value INTEGER,
            direct_depth INTEGER,
            minimax_score_kind TEXT,
            minimax_score_value INTEGER,
            UNIQUE(parent_id, move_uci)
        );
        CREATE INDEX IF NOT EXISTS deep_tree_nodes_project ON deep_tree_nodes(project_id, ply);
        CREATE INDEX IF NOT EXISTS deep_tree_nodes_source ON deep_tree_nodes(source_task_id, ply, fen);
        CREATE TABLE IF NOT EXISTS deep_tree_analyses (
            task_id TEXT PRIMARY KEY REFERENCES deep_tasks(id) ON DELETE CASCADE,
            node_id INTEGER NOT NULL REFERENCES deep_tree_nodes(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS deep_tree_analyses_node ON deep_tree_analyses(node_id);
        CREATE TABLE IF NOT EXISTS deep_tree_backfill_deferred (
            task_id TEXT PRIMARY KEY REFERENCES deep_tasks(id) ON DELETE CASCADE,
            reason TEXT NOT NULL
        );",
    )
    .map_err(|e| e.to_string())?;
    let has_occurrence = conn
        .prepare("PRAGMA table_info(deep_tasks)")
        .map_err(|e| e.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|column| column == "occurrence_id");
    if !has_occurrence {
        conn.execute_batch("ALTER TABLE deep_tasks ADD COLUMN occurrence_id INTEGER REFERENCES deep_tree_nodes(id)")
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// Pre-tree builds stored completed PVs but no occurrence nodes. Replay them once;
// completed tasks with nodes are skipped, so reopening a large project is cheap.
pub(super) fn backfill_legacy(conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let task_ids: Vec<String> = {
        let mut query = tx.prepare(
            "SELECT t.id FROM deep_tasks t LEFT JOIN deep_tree_analyses a ON a.task_id=t.id
             LEFT JOIN deep_tree_backfill_deferred d ON d.task_id=t.id
             WHERE t.status='completed' AND a.task_id IS NULL AND d.task_id IS NULL ORDER BY t.rowid",
        ).map_err(|e| e.to_string())?;
        let ids = query
            .query_map([], |row| row.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        ids
    };
    for task_id in task_ids {
        // A single malformed/ambiguous legacy task must not roll back unrelated projects.
        tx.execute_batch("SAVEPOINT deep_tree_backfill_task")
            .map_err(|e| e.to_string())?;
        let outcome: Result<(), String> = (|| {
            // A pre-link tree may have guessed this child's occurrence by FEN/ply.
            // Release its unique pointer inside the savepoint before resolving its real path.
            let parent_task_id: Option<String> = tx
                .query_row(
                    "SELECT parent_task_id FROM deep_tasks WHERE id=?1",
                    [&task_id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if parent_task_id.is_some() {
                clear_untrusted_legacy_link(&tx, &task_id)?;
            }
            let lines: Vec<DeepAnalysisLine> = {
                let mut query = tx
                    .prepare(
                        "SELECT multipv,depth,score_kind,score_value,score_perspective,uci_moves
                 FROM deep_results WHERE task_id=?1 ORDER BY multipv",
                    )
                    .map_err(|e| e.to_string())?;
                let lines = query
                    .query_map([&task_id], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    })
                    .map_err(|e| e.to_string())?
                    .map(|result| {
                        let (
                            multipv,
                            depth,
                            score_kind,
                            score_value,
                            score_perspective,
                            moves_json,
                        ) = result.map_err(|e| e.to_string())?;
                        Ok(DeepAnalysisLine {
                            multipv,
                            depth,
                            score_kind,
                            score_value,
                            score_perspective,
                            uci_moves: serde_json::from_str(&moves_json)
                                .map_err(|e| e.to_string())?,
                        })
                    })
                    .collect::<Result<_, String>>()?;
                lines
            };
            if lines.is_empty() {
                return Err("completed task has no scored PV".into());
            }
            store_completed(&tx, &task_id, &lines)
        })();
        match outcome {
            Ok(()) => tx
                .execute_batch("RELEASE deep_tree_backfill_task")
                .map_err(|e| e.to_string())?,
            Err(reason) => {
                tx.execute_batch(
                    "ROLLBACK TO deep_tree_backfill_task; RELEASE deep_tree_backfill_task",
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "INSERT INTO deep_tree_backfill_deferred(task_id,reason) VALUES(?1,?2)",
                    params![task_id, reason],
                )
                .map_err(|e| e.to_string())?;
                clear_untrusted_legacy_link(&tx, &task_id)?;
                log::warn!("Deferred legacy tree migration for task {task_id}: {reason}");
            }
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

// The staged one-to-one column may point at a guessed transposition. Once its task
// cannot be unambiguously linked, discard that cached score but keep deep_results.
fn clear_untrusted_legacy_link(tx: &Transaction<'_>, task_id: &str) -> Result<(), String> {
    let node_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM deep_tree_nodes WHERE analyzed_task_id=?1",
            [task_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(node_id) = node_id {
        let latest: Option<(String, String, i32, u32)> = tx
            .query_row(
                "SELECT a.task_id,r.score_kind,r.score_value,r.depth
             FROM deep_tree_analyses a JOIN deep_tasks t ON t.id=a.task_id
             JOIN deep_results r ON r.task_id=a.task_id
             WHERE a.node_id=?1 AND r.multipv=(
                 SELECT MIN(multipv) FROM deep_results WHERE task_id=a.task_id
             ) ORDER BY t.rowid DESC LIMIT 1",
                [node_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE deep_tree_nodes SET analyzed_task_id=?2,direct_score_kind=?3,
             direct_score_value=?4,direct_depth=?5 WHERE id=?1",
            params![
                node_id,
                latest.as_ref().map(|v| &v.0),
                latest.as_ref().map(|v| &v.1),
                latest.as_ref().map(|v| v.2),
                latest.as_ref().map(|v| v.3)
            ],
        )
        .map_err(|e| e.to_string())?;
        recompute_ancestors(tx, node_id)?;
    }
    Ok(())
}

pub(super) fn resolve_occurrence(
    conn: &Connection,
    project_id: &str,
    parent_task_id: &str,
    fen: &str,
    ply: u32,
    requested_id: Option<i64>,
) -> Result<i64, String> {
    let parent_history: String = conn
        .query_row(
            "SELECT n.history FROM deep_tree_analyses a JOIN deep_tree_nodes n ON n.id=a.node_id
         WHERE a.task_id=?1 AND n.project_id=?2",
            params![parent_task_id, project_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let parent_history: Vec<String> =
        serde_json::from_str(&parent_history).map_err(|e| e.to_string())?;
    let pvs: Vec<Vec<String>> = {
        let mut query = conn
            .prepare("SELECT uci_moves FROM deep_results WHERE task_id=?1")
            .map_err(|e| e.to_string())?;
        let pvs = query
            .query_map([parent_task_id], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .map(|result| {
                serde_json::from_str(&result.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
            })
            .collect::<Result<_, _>>()?;
        pvs
    };
    let mut query = conn
        .prepare(
            "WITH RECURSIVE branch(id) AS (
            SELECT n.id FROM deep_tree_nodes n JOIN deep_tree_analyses a ON a.node_id=n.id
             WHERE n.project_id=?1 AND a.task_id=?2
            UNION ALL SELECT n.id FROM deep_tree_nodes n JOIN branch b ON n.parent_id=b.id
         ) SELECT n.id,n.history FROM deep_tree_nodes n JOIN branch b ON n.id=b.id
           WHERE n.fen=?3 AND n.ply=?4 AND (?5 IS NULL OR n.id=?5)",
        )
        .map_err(|e| e.to_string())?;
    let candidates = query
        .query_map(
            params![project_id, parent_task_id, fen, ply, requested_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| e.to_string())?;
    let mut matches = Vec::new();
    for candidate in candidates {
        let (id, history_json) = candidate.map_err(|e| e.to_string())?;
        let history: Vec<String> =
            serde_json::from_str(&history_json).map_err(|e| e.to_string())?;
        if let Some(suffix) = history.strip_prefix(parent_history.as_slice()) {
            if !suffix.is_empty() && pvs.iter().any(|pv| pv.starts_with(suffix)) {
                matches.push(id);
                if matches.len() == 2 {
                    break;
                }
            }
        }
    }
    match matches.as_slice() {
        [id] => Ok(*id),
        [] if requested_id.is_some() => {
            Err("requested occurrence does not follow the named parent PV".into())
        }
        [] => Err("completed parent has no matching tree occurrence on its PV".into()),
        _ => Err("ambiguous continuation occurrence; specify the exact occurrence ID".into()),
    }
}

pub(super) fn store_completed(
    tx: &Transaction<'_>,
    task_id: &str,
    lines: &[DeepAnalysisLine],
) -> Result<(), String> {
    let (project_id, parent_task_id, fen, moves_json, ply, occurrence_id): (
        String,
        Option<String>,
        String,
        String,
        u32,
        Option<i64>,
    ) = tx
        .query_row(
            "SELECT project_id,parent_task_id,fen,moves,ply,occurrence_id FROM deep_tasks WHERE id=?1",
            [task_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?;
    let moves: Vec<String> = serde_json::from_str(&moves_json).map_err(|e| e.to_string())?;
    let board = position_after(&fen, &moves)?;
    let root_fen = Fen::from_position(board.clone(), EnPassantMode::Legal).to_string();
    let root_id = if let Some(parent_task_id) = parent_task_id {
        let id = resolve_occurrence(
            tx,
            &project_id,
            &parent_task_id,
            &root_fen,
            ply,
            occurrence_id,
        )?;
        tx.execute(
            "UPDATE deep_tree_nodes SET analyzed_task_id=COALESCE(analyzed_task_id,?2) WHERE id=?1",
            params![id, task_id],
        )
        .map_err(|e| e.to_string())?;
        id
    } else {
        tx.execute(
            "INSERT INTO deep_tree_nodes(project_id,source_task_id,analyzed_task_id,fen,history,ply)
             VALUES(?1,?2,?2,?3,?4,?5) ON CONFLICT(analyzed_task_id) DO NOTHING",
            params![project_id, task_id, root_fen, moves_json, ply],
        )
        .map_err(|e| e.to_string())?;
        tx.query_row(
            "SELECT id FROM deep_tree_nodes WHERE analyzed_task_id=?1",
            [task_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?
    };
    if tx.execute(
        "UPDATE deep_tasks SET occurrence_id=?2 WHERE id=?1 AND (occurrence_id IS NULL OR occurrence_id=?2)",
        params![task_id, root_id],
    ).map_err(|e| e.to_string())? != 1 {
        return Err("task already points to a different occurrence".into());
    }
    let best = lines
        .iter()
        .min_by_key(|line| line.multipv)
        .ok_or("empty PV")?;
    if best.score_perspective != "white" {
        return Err("tree scores must use White's perspective".into());
    }
    tx.execute(
        "INSERT INTO deep_tree_analyses(task_id,node_id) VALUES(?1,?2) ON CONFLICT(task_id) DO NOTHING",
        params![task_id, root_id],
    ).map_err(|e| e.to_string())?;
    let linked_id: i64 = tx
        .query_row(
            "SELECT node_id FROM deep_tree_analyses WHERE task_id=?1",
            [task_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if linked_id != root_id {
        return Err("task already linked to a different occurrence".into());
    }
    let (score_task_id, kind, value, depth): (String, String, i32, u32) = tx
        .query_row(
            "SELECT a.task_id,r.score_kind,r.score_value,r.depth FROM deep_tree_analyses a
         JOIN deep_tasks t ON t.id=a.task_id JOIN deep_results r ON r.task_id=a.task_id
         WHERE a.node_id=?1 AND r.multipv=(
             SELECT MIN(multipv) FROM deep_results WHERE task_id=a.task_id
         ) ORDER BY t.rowid DESC LIMIT 1",
            [root_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|e| e.to_string())?;
    tx.execute(
        "UPDATE deep_tree_nodes SET analyzed_task_id=?2,direct_score_kind=?3,direct_score_value=?4,direct_depth=?5,
         minimax_score_kind=?3,minimax_score_value=?4 WHERE id=?1",
        params![root_id, score_task_id, kind, value, depth],
    )
    .map_err(|e| e.to_string())?;
    let root_history: String = tx
        .query_row(
            "SELECT history FROM deep_tree_nodes WHERE id=?1",
            [root_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let history: Vec<String> = serde_json::from_str(&root_history).map_err(|e| e.to_string())?;
    for line in lines {
        let mut board = board.clone();
        let mut history = history.clone();
        let mut parent_id = root_id;
        for (offset, text) in line.uci_moves.iter().enumerate() {
            let uci = UciMove::from_ascii(text.as_bytes()).map_err(|e| e.to_string())?;
            let mv = uci.to_move(&board).map_err(|e| e.to_string())?;
            board.play_unchecked(&mv);
            history.push(text.clone());
            let next_fen = Fen::from_position(board.clone(), EnPassantMode::Legal).to_string();
            let next_ply = u32::try_from(offset)
                .ok()
                .and_then(|offset| ply.checked_add(offset))
                .and_then(|ply| ply.checked_add(1))
                .ok_or("analysis move exceeds maximum ply")?;
            tx.execute(
                "INSERT INTO deep_tree_nodes(project_id,source_task_id,parent_id,move_uci,fen,history,ply)
                 VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(parent_id,move_uci) DO NOTHING",
                params![project_id, task_id, parent_id, text, next_fen,
                    serde_json::to_string(&history).map_err(|e| e.to_string())?, next_ply],
            )
            .map_err(|e| e.to_string())?;
            parent_id = tx
                .query_row(
                    "SELECT id FROM deep_tree_nodes WHERE parent_id=?1 AND move_uci=?2",
                    params![parent_id, text],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
        }
    }
    recompute_ancestors(tx, root_id)?;
    Ok(())
}

fn score_rank(kind: &str, value: i32) -> i64 {
    match kind {
        "mate" if value > 0 => 1_000_000 - i64::from(value),
        "mate" if value < 0 => -1_000_000 - i64::from(value),
        _ => i64::from(value).clamp(-500_000, 500_000),
    }
}

// Only scored children participate. The result is provisional while the tree is incomplete.
fn recompute_ancestors(tx: &Transaction<'_>, start: i64) -> Result<(), String> {
    let mut next = Some(start);
    while let Some(id) = next {
        let (parent, fen, direct_kind, direct_value):
            (Option<i64>, String, Option<String>, Option<i32>) = tx
            .query_row(
                "SELECT parent_id,fen,direct_score_kind,direct_score_value FROM deep_tree_nodes WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| e.to_string())?;
        let mut statement = tx
            .prepare(
                "SELECT minimax_score_kind,minimax_score_value FROM deep_tree_nodes
             WHERE parent_id=?1 AND minimax_score_kind IS NOT NULL",
            )
            .map_err(|e| e.to_string())?;
        let child_scores: Vec<(String, i32)> = statement
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        let turn = super::position(&fen)?.turn();
        let score = if turn == shakmaty::Color::White {
            child_scores
                .into_iter()
                .max_by_key(|(kind, value)| score_rank(kind, *value))
        } else {
            child_scores
                .into_iter()
                .min_by_key(|(kind, value)| score_rank(kind, *value))
        }
        .or_else(|| direct_kind.zip(direct_value));
        tx.execute(
            "UPDATE deep_tree_nodes SET minimax_score_kind=?2,minimax_score_value=?3 WHERE id=?1",
            params![
                id,
                score.as_ref().map(|v| v.0.as_str()),
                score.as_ref().map(|v| v.1)
            ],
        )
        .map_err(|e| e.to_string())?;
        next = parent;
    }
    Ok(())
}
