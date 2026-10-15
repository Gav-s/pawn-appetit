//! Stable UI read contracts, built from persisted analysis state (never SQLite rows on the wire).
use super::{position, position_after, score::AnalysisScore, Store};
use rusqlite::{params, Connection};
use serde::Serialize;
use shakmaty::{fen::Fen, san::SanPlus, uci::UciMove, EnPassantMode, Position};
use specta::Type;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EvaluationView {
    Centipawns { value: i32, display: String },
    Mate { distance: i32, display: String },
    Unknown,
}

impl From<AnalysisScore> for EvaluationView {
    fn from(score: AnalysisScore) -> Self {
        match score {
            AnalysisScore::Centipawns(value) => Self::Centipawns {
                value,
                display: format!("{:+.2}", f64::from(value) / 100.0),
            },
            AnalysisScore::Mate(distance) => Self::Mate {
                distance,
                display: format!("M{distance:+}"),
            },
            AnalysisScore::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskCounts {
    pub queued: u32,
    pub running: u32,
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
}
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisProjectSummary {
    pub id: String,
    pub name: String,
    pub root_fen: String,
    pub scheduler_state: String,
    pub current_stage: Option<String>,
    pub engine: String,
    pub created_at: String,
    pub updated_at: Option<String>,
    pub task_counts: TaskCounts,
    pub analysed_node_count: u32,
}
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisNodeView {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub move_uci: Option<String>,
    pub move_san: Option<String>,
    pub fen: String,
    pub side_to_move: String,
    pub raw_evaluation: EvaluationView,
    pub tree_index: Option<u8>,
    pub depth: Option<u32>,
    pub elapsed_ms: Option<u64>,
    pub pv: Vec<String>,
    pub engine: Option<String>,
    pub minimax_evaluation: EvaluationView,
    pub status: String,
    pub transposition_linked: bool,
}
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisTaskView {
    pub id: String,
    pub strategy: String,
    pub priority: u8,
    pub source_node_id: Option<i64>,
    pub position_fen: String,
    pub state: String,
    pub requested_time_ms: Option<u32>,
    pub requested_depth: Option<u32>,
    pub operation_mode: Option<String>,
    pub remaining_prolong_count: u8,
    pub engine: String,
    pub created_at: Option<String>,
}
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerView {
    pub state: String,
    pub cycle: u64,
    pub stage: Option<String>,
    pub queue_length: u32,
    pub active_task_id: Option<String>,
    pub completed_count: u32,
    pub empty_stage_count: u32,
    pub exhausted: bool,
    pub engine_state: String,
}

struct RawNode {
    id: i64,
    parent_id: Option<i64>,
    move_uci: Option<String>,
    fen: String,
    parent_fen: Option<String>,
    raw_kind: Option<String>,
    raw_value: Option<i32>,
    minimax_kind: Option<String>,
    minimax_value: Option<i32>,
    depth: Option<u32>,
    pv_json: Option<String>,
    analyzed_task: Option<String>,
    linked_status: Option<String>,
    source_status: String,
}

impl Store {
    pub(super) fn read_project_summary(&self, id: &str) -> Result<AnalysisProjectSummary, String> {
        let project = self.get_project(id)?.ok_or("analysis project not found")?;
        let conn = self.conn()?;
        let root_fen: String = conn.query_row("SELECT fen FROM deep_tree_nodes WHERE project_id=?1 AND parent_id IS NULL ORDER BY id LIMIT 1", [id], |r| r.get(0)).map_err(|e| e.to_string())?;
        let analysed_node_count: u32 = conn.query_row("SELECT count(*) FROM deep_tree_nodes WHERE project_id=?1 AND direct_score_kind IS NOT NULL", [id], |r| r.get(0)).map_err(|e| e.to_string())?;
        let mut counts = TaskCounts {
            queued: 0,
            running: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
        };
        for task in self.list_tasks(id)? {
            match task.status.as_str() {
                "queued" => counts.queued += 1,
                "running" => counts.running += 1,
                "completed" => counts.completed += 1,
                "failed" => counts.failed += 1,
                "cancelled" => counts.cancelled += 1,
                state => return Err(format!("invalid persisted task state: {state}")),
            }
        }
        let current_stage = self.automatic(id)?.and_then(|(settings, cycle, _)| {
            (cycle > 0).then(|| format!("{:?}", settings.stage(cycle)))
        });
        Ok(AnalysisProjectSummary {
            id: project.id,
            name: project.title,
            root_fen,
            scheduler_state: project.status,
            current_stage,
            engine: project.engine,
            created_at: project.created_at,
            updated_at: None,
            task_counts: counts,
            analysed_node_count,
        })
    }

    pub(super) fn read_tree(&self, id: &str) -> Result<Vec<AnalysisNodeView>, String> {
        let engine = self
            .get_project(id)?
            .ok_or("analysis project not found")?
            .engine;
        let conn = self.conn()?;
        let mut stmt=conn.prepare("SELECT n.id,n.parent_id,n.move_uci,n.fen,p.fen,n.direct_score_kind,n.direct_score_value,
           n.minimax_score_kind,n.minimax_score_value,n.direct_depth,r.uci_moves,n.analyzed_task_id,
           (SELECT t.status FROM deep_tasks t WHERE t.occurrence_id=n.id ORDER BY CASE t.status WHEN 'running' THEN 0 WHEN 'queued' THEN 1 ELSE 2 END,t.rowid DESC LIMIT 1),
           src.status FROM deep_tree_nodes n LEFT JOIN deep_tree_nodes p ON p.id=n.parent_id
           JOIN deep_tasks src ON src.id=n.source_task_id
           LEFT JOIN deep_results r ON r.task_id=n.analyzed_task_id AND r.multipv=1
           WHERE n.project_id=?1 ORDER BY n.ply,n.id").map_err(|e|e.to_string())?;
        let raw = stmt
            .query_map([id], |r| {
                Ok(RawNode {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    move_uci: r.get(2)?,
                    fen: r.get(3)?,
                    parent_fen: r.get(4)?,
                    raw_kind: r.get(5)?,
                    raw_value: r.get(6)?,
                    minimax_kind: r.get(7)?,
                    minimax_value: r.get(8)?,
                    depth: r.get(9)?,
                    pv_json: r.get(10)?,
                    analyzed_task: r.get(11)?,
                    linked_status: r.get(12)?,
                    source_status: r.get(13)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        raw.into_iter()
            .map(|n| {
                let board = position(&n.fen)?;
                let move_san = match (&n.parent_fen, &n.move_uci, n.parent_id) {
                    (Some(fen), Some(text), Some(_)) => {
                        let mut parent = position(fen)?;
                        let uci =
                            UciMove::from_ascii(text.as_bytes()).map_err(|e| e.to_string())?;
                        let mv = uci.to_move(&parent).map_err(|e| e.to_string())?;
                        let san =
                            SanPlus::from_move_and_play_unchecked(&mut parent, &mv).to_string();
                        if Fen::from_position(parent, EnPassantMode::Legal).to_string() != n.fen {
                            return Err(format!(
                                "analysis node {} has inconsistent move and FEN",
                                n.id
                            ));
                        }
                        Some(san)
                    }
                    (None, None, None) => None,
                    _ => return Err(format!("analysis node {} has an invalid parent edge", n.id)),
                };
                let raw_score = AnalysisScore::from_stored(n.raw_kind.as_deref(), n.raw_value)?;
                let minimax =
                    AnalysisScore::from_stored(n.minimax_kind.as_deref(), n.minimax_value)?;
                let pv = match (&n.analyzed_task, &n.pv_json) {
                    (Some(_), Some(json)) => {
                        serde_json::from_str(json).map_err(|e| e.to_string())?
                    }
                    (None, None) => Vec::new(),
                    _ => return Err(format!("analysis node {} has no matching PV result", n.id)),
                };
                let status = if n.linked_status.as_deref() == Some("running")
                    || (n.parent_id.is_none() && n.source_status == "running")
                {
                    "running"
                } else if n.linked_status.as_deref() == Some("queued")
                    || (n.parent_id.is_none() && n.source_status == "queued")
                {
                    "pending"
                } else if n.raw_kind.is_some() {
                    "analysed"
                } else {
                    "unanalysed"
                };
                Ok(AnalysisNodeView {
                    id: n.id,
                    parent_id: n.parent_id,
                    move_uci: n.move_uci,
                    move_san,
                    fen: n.fen,
                    side_to_move: if board.turn() == shakmaty::Color::White {
                        "white"
                    } else {
                        "black"
                    }
                    .into(),
                    raw_evaluation: raw_score.into(),
                    tree_index: raw_score.tree_index(),
                    depth: n.depth,
                    elapsed_ms: None,
                    pv,
                    engine: Some(engine.clone()),
                    minimax_evaluation: minimax.into(),
                    status: status.into(),
                    transposition_linked: false,
                })
            })
            .collect()
    }

    pub(super) fn read_tasks(&self, id: &str) -> Result<Vec<AnalysisTaskView>, String> {
        let engine = self
            .get_project(id)?
            .ok_or("analysis project not found")?
            .engine;
        let conn = self.conn()?;
        let mut query=conn.prepare("SELECT id,fen,status,strategy,priority,remaining_prolong,occurrence_id FROM deep_tasks WHERE project_id=?1 ORDER BY rowid").map_err(|e|e.to_string())?;
        let views = query
            .query_map([id], |r| {
                Ok(AnalysisTaskView {
                    id: r.get(0)?,
                    position_fen: r.get(1)?,
                    state: r.get(2)?,
                    strategy: r.get(3)?,
                    priority: r.get(4)?,
                    remaining_prolong_count: r.get(5)?,
                    source_node_id: r.get(6)?,
                    requested_time_ms: None,
                    requested_depth: None,
                    operation_mode: None,
                    engine: engine.clone(),
                    created_at: None,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(views)
    }

    pub(super) fn read_scheduler(&self, id: &str) -> Result<SchedulerView, String> {
        let summary = self.read_project_summary(id)?;
        let (cycle, empty_stage_count) = self
            .automatic(id)?
            .map_or((0, 0), |(_, cycle, empty)| (cycle, empty));
        let active_task_id = self
            .read_tasks(id)?
            .into_iter()
            .find(|t| t.state == "running")
            .map(|t| t.id);
        let engine_state = if summary.scheduler_state == "failed" {
            "failed"
        } else if active_task_id.is_some() {
            "searching"
        } else {
            "idle"
        };
        Ok(SchedulerView {
            state: summary.scheduler_state.clone(),
            cycle,
            stage: summary.current_stage,
            queue_length: summary.task_counts.queued,
            active_task_id,
            completed_count: summary.task_counts.completed,
            empty_stage_count,
            exhausted: summary.scheduler_state == "exhausted",
            engine_state: engine_state.into(),
        })
    }
}

/// Idempotently reconstruct unanalysed roots for pre-tree pending projects.
/// Invalid legacy tasks stay readable as tasks; they never abort other projects' recovery.
pub(super) fn restore_missing_roots(conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let missing: Vec<(String, String, String, String, u32)> = {
        let mut query = tx
            .prepare(
                "SELECT id,project_id,fen,moves,ply FROM deep_tasks t
          WHERE parent_task_id IS NULL AND NOT EXISTS(
            SELECT 1 FROM deep_tree_nodes n WHERE n.source_task_id=t.id AND n.parent_id IS NULL)
          ORDER BY t.rowid",
            )
            .map_err(|e| e.to_string())?;
        let rows = query
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };
    for (task_id, project_id, fen, moves_json, ply) in missing {
        let reconstructed = (|| -> Result<_, String> {
            let moves: Vec<String> =
                serde_json::from_str(&moves_json).map_err(|e| e.to_string())?;
            let board = position_after(&fen, &moves)?;
            let fen = Fen::from_position(board, EnPassantMode::Legal).to_string();
            Ok(fen)
        })();
        match reconstructed {
            Ok(actual_fen) => {
                tx.execute("INSERT INTO deep_tree_nodes(project_id,source_task_id,fen,history,ply) VALUES(?1,?2,?3,?4,?5)",
                params![project_id,task_id,actual_fen,moves_json,ply]).map_err(|e|e.to_string())?;
            }
            Err(e) => log::warn!("Skipped invalid legacy Deep Analysis root {task_id}: {e}"),
        }
    }
    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::EvaluationView;
    use crate::deep_analysis::{parse_line, start_fen, Store};

    #[test]
    fn pending_legacy_project_reopens_with_a_visible_root_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.sqlite");
        let store = Store::open(&path).unwrap();
        let p = store
            .create_project(
                "Legacy",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &["e2e4".into()],
            )
            .unwrap();
        store
            .conn()
            .unwrap()
            .execute("DELETE FROM deep_tree_nodes WHERE project_id=?1", [&p.id])
            .unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.read_tree(&p.id).unwrap().len(), 1);
        assert_eq!(
            reopened
                .read_project_summary(&p.id)
                .unwrap()
                .root_fen
                .split_whitespace()
                .nth(1),
            Some("b")
        );
    }

    #[test]
    fn root_and_pv_nodes_have_typed_scores_moves_and_task_statuses() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("analysis.sqlite")).unwrap();
        let p = store
            .create_project(
                "Study",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        let before = store.read_tree(&p.id).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].raw_evaluation, EvaluationView::Unknown);
        assert_eq!(before[0].status, "pending");
        let root = store.list_tasks(&p.id).unwrap().remove(0);
        store.begin_running(&p.id, "paused").unwrap();
        store.claim(&p.id).unwrap();
        let running = store.read_tree(&p.id).unwrap();
        assert_eq!(running[0].status, "running");
        store
            .save(
                &root.id,
                &[parse_line(
                    "info depth 12 multipv 1 score cp 0 pv e2e4 e7e5",
                    &root.fen,
                    &[],
                )
                .unwrap()],
            )
            .unwrap();
        let nodes = store.read_tree(&p.id).unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(
            nodes[0].raw_evaluation,
            EvaluationView::Centipawns {
                value: 0,
                display: "+0.00".into()
            }
        );
        assert_eq!(nodes[0].tree_index, Some(50));
        assert_eq!(nodes[0].pv, vec!["e2e4", "e7e5"]);
        assert_eq!(nodes[0].status, "analysed");
        assert_eq!(nodes[1].move_san.as_deref(), Some("e4"));
        assert_eq!(nodes[1].side_to_move, "black");
        assert_eq!(nodes[1].parent_id, Some(nodes[0].id));
        assert!(!nodes[1].transposition_linked);
        assert_eq!(nodes[1].elapsed_ms, None);
    }

    #[test]
    fn project_task_and_scheduler_views_reopen_without_inventing_search_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.sqlite");
        let store = Store::open(&path).unwrap();
        let p = store
            .create_project(
                "Study",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let manual = store.queue_root(&p.id, "startpos", 0).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        let summary = reopened.read_project_summary(&p.id).unwrap();
        assert_eq!(summary.name, "Study");
        assert_eq!(summary.root_fen, start_fen());
        assert_eq!(summary.task_counts.queued, 2);
        assert_eq!(summary.analysed_node_count, 0);
        let tasks = reopened.read_tasks(&p.id).unwrap();
        let priority = tasks.iter().find(|t| t.id == manual.id).unwrap();
        assert_eq!(priority.strategy, "manual");
        assert_eq!(priority.priority, 1);
        assert_eq!(priority.requested_time_ms, None); // legacy intent is unknown, not inferred
        let scheduler = reopened.read_scheduler(&p.id).unwrap();
        assert_eq!(scheduler.cycle, 0);
        assert_eq!(scheduler.queue_length, 2);
        assert_eq!(scheduler.active_task_id, None);
    }
}
