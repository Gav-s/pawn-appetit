//! Persistent, independent UCI analysis queue. This does not share interactive engine processes.
use crate::chess::{EngineOption, EngineOptions, EngineProcess, GoMode};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use scheduler::{AnalysisOperation, Completion};
use serde::{Deserialize, Serialize};
use shakmaty::{fen::Fen, uci::UciMove, CastlingMode, Chess, Color, EnPassantMode, Position};
use specta::Type;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tauri::{path::BaseDirectory, Manager};
use tauri_specta::Event;
use vampirc_uci::{parse_one, uci::ScoreValue, UciInfoAttribute, UciMessage};

#[path = "deep_analysis_position.rs"]
mod position_graph;
#[path = "deep_analysis_read.rs"]
mod read;
#[path = "deep_analysis_scheduler.rs"]
mod scheduler;
#[path = "deep_analysis_score.rs"]
mod score;
#[path = "deep_analysis_task.rs"]
mod task_spec;
#[path = "deep_analysis_tree.rs"]
mod tree;

// A separate file avoids changing the game database schema or competing with its connection pool.
fn database_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve("deep-analysis.sqlite", BaseDirectory::AppData)
        .map_err(|e| e.to_string())
}
fn start_fen() -> String {
    Fen::from_position(Chess::default(), EnPassantMode::Legal).to_string()
}
fn position(fen: &str) -> Result<Chess, String> {
    let fen = if fen == "startpos" {
        start_fen()
    } else {
        fen.to_owned()
    };
    fen.parse::<Fen>()
        .map_err(|e| e.to_string())?
        .into_position(CastlingMode::Chess960)
        .map_err(|e| e.to_string())
}

fn position_after(fen: &str, moves: &[String]) -> Result<Chess, String> {
    let mut board = position(fen)?;
    for text in moves {
        let uci = UciMove::from_ascii(text.as_bytes()).map_err(|e| e.to_string())?;
        let mv = uci.to_move(&board).map_err(|e| e.to_string())?;
        board.play_unchecked(&mv);
    }
    Ok(board)
}

fn validate_engine_path(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_absolute() || !p.is_file() {
        return Err("engine must be an installed local executable".into());
    }
    #[cfg(unix)]
    if std::os::unix::fs::PermissionsExt::mode(
        &p.metadata().map_err(|e| e.to_string())?.permissions(),
    ) & 0o111
        == 0
    {
        return Err("engine is not executable".into());
    }
    Ok(())
}

fn registered_engine(path: &str, registration: &Path) -> Result<(), String> {
    validate_engine_path(path)?;
    let data = std::fs::read_to_string(registration).map_err(|e| e.to_string())?;
    let engines: Vec<serde_json::Value> = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    let requested = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    if engines.iter().any(|e| {
        e.get("type").and_then(|v| v.as_str()) == Some("local")
            && e.get("path")
                .and_then(|v| v.as_str())
                .and_then(|s| std::fs::canonicalize(s).ok())
                .as_ref()
                == Some(&requested)
    }) {
        Ok(())
    } else {
        Err("engine is not registered as an installed local engine".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DeepAnalysisProject {
    pub id: String,
    pub title: String,
    pub engine: String,
    pub time_ms: u32,
    pub multipv: u16,
    pub status: String,
    pub created_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DeepAnalysisTask {
    pub id: String,
    pub project_id: String,
    pub parent_task_id: Option<String>,
    pub fen: String,
    pub moves: Vec<String>,
    pub ply: u32,
    pub status: String,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeepAnalysisLine {
    pub multipv: u16,
    pub depth: u32,
    pub score_kind: String,
    pub score_value: i32,
    pub score_perspective: String,
    pub uci_moves: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DeepAnalysisResult {
    pub task_id: String,
    pub lines: Vec<DeepAnalysisLine>,
}
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisChange {
    ProjectStateChanged,
    SchedulerStageChanged,
    TaskQueued,
    TaskStarted,
    TaskCompleted,
    TreeNodeUpdated,
    MinimaxUpdated,
    EngineStateChanged,
    ProjectExhausted,
    ProjectFailed,
}

#[derive(Debug, Clone, Serialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct AnalysisUpdate {
    pub project_id: String,
    pub kind: AnalysisChange,
    pub task_id: Option<String>,
}

fn emit_update(
    app: &tauri::AppHandle,
    project_id: &str,
    kind: AnalysisChange,
    task_id: Option<&str>,
) {
    let _ = AnalysisUpdate::emit(
        &AnalysisUpdate {
            project_id: project_id.into(),
            kind,
            task_id: task_id.map(str::to_owned),
        },
        app,
    );
}

#[derive(Debug, Clone, Serialize, Type, Event)]
#[serde(rename_all = "camelCase")]
pub struct DeepAnalysisProgress {
    pub project_id: String,
    pub task_id: Option<String>,
    pub status: String,
    pub completed: u32,
    pub total: u32,
    pub error: Option<String>,
}

#[derive(Clone)]
struct Store {
    path: PathBuf,
}
impl Store {
    fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let store = Self {
            path: path.to_owned(),
        };
        let mut conn = store.conn()?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS deep_projects (
            id TEXT PRIMARY KEY, title TEXT NOT NULL, engine TEXT NOT NULL, depth INTEGER NOT NULL DEFAULT 0, time_ms INTEGER NOT NULL DEFAULT 1000,
            multipv INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'paused',
            created_at TEXT NOT NULL DEFAULT (datetime('now')));
            CREATE TABLE IF NOT EXISTS deep_tasks (
            id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES deep_projects(id) ON DELETE CASCADE,
            parent_task_id TEXT REFERENCES deep_tasks(id), fen TEXT NOT NULL, moves TEXT NOT NULL DEFAULT '[]', ply INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'queued', error TEXT);
            CREATE INDEX IF NOT EXISTS deep_tasks_project ON deep_tasks(project_id, status);
            CREATE TABLE IF NOT EXISTS deep_results (
            task_id TEXT NOT NULL REFERENCES deep_tasks(id) ON DELETE CASCADE,
            multipv INTEGER NOT NULL, depth INTEGER NOT NULL, score_kind TEXT NOT NULL,
            score_value INTEGER NOT NULL, score_perspective TEXT NOT NULL DEFAULT 'white', uci_moves TEXT NOT NULL,
            PRIMARY KEY(task_id, multipv));").map_err(|e| e.to_string())?;
        // Migration for projects created by earlier development builds.
        let project_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(deep_projects)")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get(1))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        if !project_columns.iter().any(|c| c == "time_ms") {
            conn.execute_batch(
                "ALTER TABLE deep_projects ADD COLUMN time_ms INTEGER NOT NULL DEFAULT 1000",
            )
            .map_err(|e| e.to_string())?;
        }
        let task_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(deep_tasks)")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get(1))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        if !task_columns.iter().any(|c| c == "moves") {
            conn.execute_batch(
                "ALTER TABLE deep_tasks ADD COLUMN moves TEXT NOT NULL DEFAULT '[]'",
            )
            .map_err(|e| e.to_string())?;
        }
        let result_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(deep_results)")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get(1))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        if !result_columns.iter().any(|c| c == "score_perspective") {
            conn.execute_batch("ALTER TABLE deep_results ADD COLUMN score_perspective TEXT NOT NULL DEFAULT 'white'")
                .map_err(|e| e.to_string())?;
        }
        tree::create_schema(&conn)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS deep_scheduler (
            project_id TEXT PRIMARY KEY REFERENCES deep_projects(id) ON DELETE CASCADE,
            settings TEXT NOT NULL, cycle INTEGER NOT NULL DEFAULT 0,
            empty_stages INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS deep_analysis_cache (
                position_key TEXT NOT NULL, engine TEXT NOT NULL, multipv INTEGER NOT NULL,
                elapsed_ms INTEGER NOT NULL, depth INTEGER NOT NULL, lines_json TEXT NOT NULL,
                PRIMARY KEY(position_key,engine,multipv,elapsed_ms,depth));
            PRAGMA user_version=2;",
        )
        .map_err(|e| e.to_string())?;
        let task_columns: Vec<String> = conn
            .prepare("PRAGMA table_info(deep_tasks)")
            .map_err(|e| e.to_string())?
            .query_map([], |r| r.get(1))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        for (column, migration) in [
            (
                "priority",
                "ALTER TABLE deep_tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "remaining_prolong",
                "ALTER TABLE deep_tasks ADD COLUMN remaining_prolong INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "strategy",
                "ALTER TABLE deep_tasks ADD COLUMN strategy TEXT NOT NULL DEFAULT 'manual'",
            ),
        ] {
            if !task_columns.iter().any(|c| c == column) {
                conn.execute_batch(migration).map_err(|e| e.to_string())?;
            }
        }
        task_spec::migrate(&mut conn)?;
        position_graph::migrate(&mut conn)?;
        tree::backfill_legacy(&mut conn)?;
        read::restore_missing_roots(&mut conn)?;
        position_graph::backfill_accepted(&mut conn)?;
        task_spec::migrate_graph(&mut conn)?;
        Ok(store)
    }
    fn conn(&self) -> Result<Connection, String> {
        let c = Connection::open(&self.path).map_err(|e| e.to_string())?;
        c.busy_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        c.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| e.to_string())?;
        Ok(c)
    }
    fn enable_automatic(
        &self,
        id: &str,
        settings: &scheduler::AnalysisSettings,
    ) -> Result<(), String> {
        if settings.analysis_time_ms == 0
            || settings.analysis_time_ms > 3_600_000
            || settings.max_positions == 0
            || settings.max_ply == 0
            || settings.prolong_frequency > 64
        {
            return Err("invalid automatic-analysis settings".into());
        }
        let count = self.conn()?.execute("INSERT INTO deep_scheduler(project_id,settings,enabled) SELECT id,?2,1 FROM deep_projects WHERE id=?1
            ON CONFLICT(project_id) DO UPDATE SET settings=excluded.settings,enabled=1",
            params![id, serde_json::to_string(settings).map_err(|e| e.to_string())?]).map_err(|e| e.to_string())?;
        if count == 0 {
            Err("project not found".into())
        } else {
            Ok(())
        }
    }
    fn prepare_automatic_project(&self, id: &str) -> Result<(), String> {
        let project = self.get_project(id)?.ok_or("analysis project not found")?;
        if matches!(project.status.as_str(), "running" | "pausing" | "cancelled") {
            return Err("analysis project cannot start in its current state".into());
        }
        if self.automatic(id)?.is_none() {
            let settings = scheduler::AnalysisSettings {
                analysis_time_ms: project.time_ms,
                engine_id: project.engine,
                ..Default::default()
            };
            self.enable_automatic(id, &settings)?;
        }
        Ok(())
    }
    fn automatic(
        &self,
        id: &str,
    ) -> Result<Option<(scheduler::AnalysisSettings, u64, u32)>, String> {
        let raw: Option<(String, u64, u32)> = self.conn()?.query_row(
            "SELECT settings,cycle,empty_stages FROM deep_scheduler WHERE project_id=?1 AND enabled=1",
            [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|e| e.to_string())?;
        raw.map(|(s, cycle, empty)| {
            Ok((
                serde_json::from_str(&s).map_err(|e| e.to_string())?,
                cycle,
                empty,
            ))
        })
        .transpose()
    }
    fn scheduler_cycle(&self, id: &str) -> Result<u64, String> {
        Ok(self.automatic(id)?.map_or(0, |(_, cycle, _)| cycle))
    }
    /// Only known, unanalysed PV move occurrences qualify; paths never coalesce on FEN.
    fn generate_next(&self, project_id: &str) -> Result<bool, String> {
        if self.status(project_id)? != "running" {
            return Ok(false);
        }
        let Some((settings, mut cycle, mut empty)) = self.automatic(project_id)? else {
            return Ok(false);
        };
        let conn = self.conn()?;
        let root: Option<(String, i32, u32)> = conn.query_row("SELECT minimax_score_kind,minimax_score_value,ply FROM deep_tree_nodes WHERE project_id=?1 AND parent_id IS NULL AND minimax_score_kind IS NOT NULL ORDER BY id LIMIT 1", [project_id], |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?))).optional().map_err(|e| e.to_string())?;
        let Some((kind, value, root_ply)) = root else {
            return Ok(false);
        };
        let Some(root_index) =
            score::AnalysisScore::from_stored(Some(&kind), Some(value))?.tree_index()
        else {
            return Ok(false);
        };
        let mut query = conn.prepare("SELECT n.id,n.fen,n.ply,n.source_task_id,p.minimax_score_kind,p.minimax_score_value,
            (SELECT count(*) FROM deep_tree_nodes sibling WHERE sibling.parent_id=n.parent_id),
            EXISTS(SELECT 1 FROM deep_results r WHERE r.task_id=p.analyzed_task_id AND r.multipv=1 AND json_extract(r.uci_moves,'$[0]')=n.move_uci)
            FROM deep_tree_nodes n JOIN deep_tree_nodes p ON p.id=n.parent_id
            WHERE n.project_id=?1 AND n.direct_score_kind IS NULL AND n.ply<=?2
              AND p.minimax_score_kind IS NOT NULL AND NOT EXISTS(SELECT 1 FROM deep_tasks t WHERE t.occurrence_id=n.id)
            ORDER BY n.id").map_err(|e| e.to_string())?;
        let eligible: Vec<(scheduler::Candidate, String, String)> = query
            .query_map(
                params![project_id, root_ply.saturating_add(settings.max_ply)],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u32>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i32>(5)?,
                        r.get::<_, u32>(6)?,
                        r.get::<_, bool>(7)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?
            .map(|row| {
                let (id, fen, ply, source, kind, value, siblings, preferred) =
                    row.map_err(|e| e.to_string())?;
                let index =
                    score::AnalysisScore::from_stored(Some(&kind), Some(value))?.tree_index();
                Ok(index.map(|index| {
                    (
                        scheduler::Candidate::new(id, index, ply, preferred, siblings),
                        fen,
                        source,
                    )
                }))
            })
            .collect::<Result<Vec<_>, String>>()?
            .into_iter()
            .flatten()
            .collect();
        drop(query);
        drop(conn);
        let candidates: Vec<_> = eligible
            .iter()
            .map(|(candidate, _, _)| candidate.clone())
            .collect();
        let scan = settings
            .prolong_frequency
            .max(1)
            .min(64)
            .saturating_mul(3)
            .max(6);
        while empty < scan {
            cycle = cycle
                .checked_add(1)
                .ok_or("analysis stage counter overflow")?;
            let stage = settings.stage(cycle);
            let mut rng = rand::thread_rng();
            let chosen =
                scheduler::select_candidates(stage, root_index, &candidates, &settings, &mut rng);
            if chosen.is_empty() {
                empty += 1;
            } else {
                empty = 0;
            }
            self.conn()?
                .execute(
                    "UPDATE deep_scheduler SET cycle=?2,empty_stages=?3 WHERE project_id=?1",
                    params![project_id, cycle, empty],
                )
                .map_err(|e| e.to_string())?;
            if !chosen.is_empty() {
                for id in chosen {
                    let (_, fen, source) = eligible
                        .iter()
                        .find(|(c, _, _)| c.occurrence_id == id)
                        .ok_or("missing candidate")?;
                    let ply = eligible
                        .iter()
                        .find(|(c, _, _)| c.occurrence_id == id)
                        .unwrap()
                        .0
                        .ply;
                    let task = match self.queue_with_occurrence(
                        project_id,
                        Some(source),
                        fen,
                        ply,
                        Some(id),
                        true,
                    ) {
                        Ok(task) => task,
                        Err(_e) if self.status(project_id)? != "running" => return Ok(false),
                        Err(e) => return Err(e),
                    };
                    self.conn()?.execute("UPDATE deep_tasks SET strategy=?2,remaining_prolong=?3,priority=0 WHERE id=?1",
                        params![task.id,format!("{stage:?}"),if stage==scheduler::Stage::Prolong { i64::from(settings.prolong_depth) } else { 0 }]).map_err(|e|e.to_string())?;
                }
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn recover(&self) -> Result<(), String> {
        let c = self.conn()?;
        c.execute_batch(
            "BEGIN IMMEDIATE;
          UPDATE deep_tasks SET status='queued' WHERE status='running';
          UPDATE deep_projects SET status='paused' WHERE status IN ('running','pausing'); COMMIT;",
        )
        .map_err(|e| e.to_string())
    }
    fn create_project(
        &self,
        title: &str,
        engine: &str,
        time_ms: u32,
        multipv: u16,
        fen: &str,
        moves: &[String],
    ) -> Result<DeepAnalysisProject, String> {
        if title.trim().is_empty()
            || time_ms == 0
            || time_ms > 3_600_000
            || multipv == 0
            || multipv > 32
        {
            return Err("title, timeMs 1..3600000 and MultiPV 1..32 are required".into());
        }
        validate_engine_path(engine)?;
        let normalized = if fen == "startpos" {
            start_fen()
        } else {
            fen.to_owned()
        };
        let board = position_after(&normalized, moves)?;
        let root_fen = Fen::from_position(board, EnPassantMode::Legal).to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let task_id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn()?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO deep_projects(id,title,engine,depth,time_ms,multipv) VALUES(?1,?2,?3,0,?4,?5)",
            params![id, title, engine, time_ms, multipv]).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO deep_tasks(id,project_id,parent_task_id,fen,moves,ply) VALUES(?1,?2,NULL,?3,?4,?5)",
            params![task_id, id, normalized, serde_json::to_string(moves).map_err(|e| e.to_string())?, moves.len() as u32]).map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO deep_tree_nodes(project_id,source_task_id,fen,history,ply) VALUES(?1,?2,?3,?4,?5)",
            params![id,task_id,root_fen,serde_json::to_string(moves).map_err(|e|e.to_string())?,moves.len() as u32]).map_err(|e|e.to_string())?;
        let root_id = tx.last_insert_rowid();
        let position_id = position_graph::link_node(&tx, root_id)?;
        tx.execute(
            "UPDATE deep_tasks SET position_id=?2 WHERE id=?1",
            params![task_id, position_id],
        )
        .map_err(|e| e.to_string())?;
        task_spec::add_interest(&tx, &task_id, root_id, "Root", 0, 0)?;
        tx.commit().map_err(|e| e.to_string())?;
        self.get_project(&id)?
            .ok_or_else(|| "project not found".into())
    }
    fn get_project(&self, id: &str) -> Result<Option<DeepAnalysisProject>, String> {
        self.conn()?.query_row("SELECT id,title,engine,time_ms,multipv,status,created_at FROM deep_projects WHERE id=?1", [id], |r| Ok(DeepAnalysisProject {id:r.get(0)?,title:r.get(1)?,engine:r.get(2)?,time_ms:r.get(3)?,multipv:r.get(4)?,status:r.get(5)?,created_at:r.get(6)?})).optional().map_err(|e| e.to_string())
    }
    fn list_projects(&self) -> Result<Vec<DeepAnalysisProject>, String> {
        let c = self.conn()?;
        let mut q=c.prepare("SELECT id,title,engine,time_ms,multipv,status,created_at FROM deep_projects ORDER BY created_at DESC, rowid DESC").map_err(|e| e.to_string())?;
        let rows = q
            .query_map([], |r| {
                Ok(DeepAnalysisProject {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    engine: r.get(2)?,
                    time_ms: r.get(3)?,
                    multipv: r.get(4)?,
                    status: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
    fn queue(
        &self,
        project_id: &str,
        parent_task_id: Option<&str>,
        fen: &str,
        ply: u32,
    ) -> Result<DeepAnalysisTask, String> {
        self.queue_with_occurrence(project_id, parent_task_id, fen, ply, None, false)
    }
    fn queue_at_occurrence(
        &self,
        project_id: &str,
        parent_task_id: &str,
        fen: &str,
        ply: u32,
        occurrence_id: i64,
    ) -> Result<DeepAnalysisTask, String> {
        self.queue_with_occurrence(
            project_id,
            Some(parent_task_id),
            fen,
            ply,
            Some(occurrence_id),
            false,
        )
    }
    fn queue_with_occurrence(
        &self,
        project_id: &str,
        parent_task_id: Option<&str>,
        fen: &str,
        ply: u32,
        occurrence_id: Option<i64>,
        automatic: bool,
    ) -> Result<DeepAnalysisTask, String> {
        let normalized = if fen == "startpos" {
            start_fen()
        } else {
            fen.to_owned()
        };
        position(&normalized)?;
        if self.get_project(project_id)?.is_none() {
            return Err("project not found".into());
        }
        if occurrence_id.is_some() && parent_task_id.is_none() {
            return Err("root tasks cannot select a continuation occurrence".into());
        }
        let resolved_occurrence = if let Some(parent) = parent_task_id {
            let parent = self
                .list_tasks(project_id)?
                .into_iter()
                .find(|t| t.id == parent)
                .ok_or("parent task not in project")?;
            if parent.status != "completed" {
                return Err("parent task is not completed".into());
            }
            let lines = self
                .results(project_id)?
                .into_iter()
                .find(|r| r.task_id == parent.id)
                .ok_or("parent task has no PV".to_owned())?;
            let mut descendant = false;
            for line in lines.lines {
                let mut board = position_after(&parent.fen, &parent.moves)?;
                for (index, text) in line.uci_moves.iter().enumerate() {
                    let uci = UciMove::from_ascii(text.as_bytes()).map_err(|e| e.to_string())?;
                    let mv = uci.to_move(&board).map_err(|e| e.to_string())?;
                    board.play_unchecked(&mv);
                    if ply == parent.ply + index as u32 + 1
                        && Fen::from_position(board.clone(), EnPassantMode::Legal).to_string()
                            == normalized
                    {
                        descendant = true;
                        break;
                    }
                }
                if descendant {
                    break;
                }
            }
            if !descendant {
                return Err("continuation must follow a completed parent PV".into());
            }
            let conn = self.conn()?;
            Some(tree::resolve_occurrence(
                &conn,
                project_id,
                &parent.id,
                &normalized,
                ply,
                occurrence_id,
            )?)
        } else {
            None
        };
        let id = uuid::Uuid::new_v4().to_string();
        let mut conn = self.conn()?;
        // Serialize competing queue insertions before checking for a compatible search.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let position_id: i64 = if let Some(node_id) = resolved_occurrence {
            tx.query_row(
                "SELECT position_id FROM deep_tree_nodes WHERE id=?1 AND project_id=?2 AND fen=?3",
                params![node_id, project_id, normalized],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?
        } else {
            position_graph::get_or_create(&tx, project_id, &normalized)?
        };
        let priority = if automatic { 0 } else { 1 };
        // The existing insert trigger captures the current immutable search settings.
        // Keep the candidate unlinked until its snapshot can be compared to queued work.
        if tx.execute(
            "INSERT INTO deep_tasks(id,project_id,parent_task_id,fen,moves,ply,occurrence_id,priority)
             SELECT ?1,?2,?3,?4,'[]',?5,?6,?7 WHERE EXISTS (
                SELECT 1 FROM deep_projects WHERE id=?2 AND status!='cancelled' AND (?7=1 OR status='running'))",
            params![id,project_id,parent_task_id,normalized,ply,resolved_occurrence,priority],
        ).map_err(|e|e.to_string())? == 0 {
            return Err("cancelled project cannot accept tasks".into());
        }
        let equivalent: Option<String> = tx
            .query_row(
                "SELECT existing.id FROM deep_tasks existing JOIN deep_tasks wanted ON wanted.id=?1
             WHERE existing.id!=wanted.id AND existing.project_id=wanted.project_id
               AND existing.position_id=?2 AND existing.status IN ('queued','running')
               AND existing.execution_engine=wanted.execution_engine
               AND existing.engine_profile=wanted.engine_profile
               AND existing.cache_policy=wanted.cache_policy
               AND existing.requested_multipv>=wanted.requested_multipv
               AND existing.requested_time_ms>=wanted.requested_time_ms
               AND existing.requested_depth>=wanted.requested_depth
               AND existing.operation_mode=wanted.operation_mode
             ORDER BY CASE existing.status WHEN 'running' THEN 0 ELSE 1 END,
               existing.priority DESC,existing.rowid LIMIT 1",
                params![id, position_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let actual_id = if let Some(existing) = equivalent {
            tx.execute("DELETE FROM deep_tasks WHERE id=?1", [&id])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE deep_tasks SET priority=MAX(priority,?2) WHERE id=?1",
                params![existing, priority],
            )
            .map_err(|e| e.to_string())?;
            log::debug!(
                "Coalesced canonical task request for position {position_id} into {existing}"
            );
            existing
        } else if let Some(accepted) = position_graph::sufficient_task(&tx, position_id, &id)? {
            tx.execute("DELETE FROM deep_tasks WHERE id=?1", [&id])
                .map_err(|e| e.to_string())?;
            log::debug!("Reused accepted canonical analysis {accepted} at position {position_id}");
            accepted
        } else if let Some(weaker) = tx
            .query_row(
                "SELECT existing.id FROM deep_tasks existing JOIN deep_tasks wanted ON wanted.id=?1
             WHERE existing.id!=wanted.id AND existing.project_id=wanted.project_id
               AND existing.position_id=?2 AND existing.status='queued'
               AND existing.execution_engine=wanted.execution_engine
               AND existing.engine_profile=wanted.engine_profile
               AND existing.cache_policy=wanted.cache_policy
               AND existing.operation_mode=wanted.operation_mode
             ORDER BY existing.priority DESC,existing.rowid LIMIT 1",
                params![id, position_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
        {
            tx.execute("UPDATE deep_tasks SET
                requested_multipv=MAX(requested_multipv,(SELECT requested_multipv FROM deep_tasks WHERE id=?2)),
                requested_time_ms=MAX(requested_time_ms,(SELECT requested_time_ms FROM deep_tasks WHERE id=?2)),
                requested_depth=MAX(requested_depth,(SELECT requested_depth FROM deep_tasks WHERE id=?2)),
                priority=MAX(priority,?3) WHERE id=?1",
                params![weaker,id,priority]).map_err(|e|e.to_string())?;
            tx.execute("DELETE FROM deep_tasks WHERE id=?1", [&id])
                .map_err(|e| e.to_string())?;
            log::debug!(
                "Strengthened pending canonical search {weaker} for position {position_id}"
            );
            weaker
        } else {
            tx.execute(
                "UPDATE deep_tasks SET position_id=?2 WHERE id=?1",
                params![id, position_id],
            )
            .map_err(|e| e.to_string())?;
            log::debug!("Queued new canonical search for position {position_id}, task {id}");
            id
        };
        if let Some(node_id) = resolved_occurrence {
            task_spec::add_interest(
                &tx,
                &actual_id,
                node_id,
                if automatic { "Automatic" } else { "Manual" },
                priority,
                0,
            )?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.list_tasks(project_id)?
            .into_iter()
            .find(|task| task.id == actual_id)
            .ok_or_else(|| "queued task disappeared".into())
    }
    fn manual_prolong(
        &self,
        project_id: &str,
        node_id: i64,
        count: u8,
    ) -> Result<DeepAnalysisTask, String> {
        if count == 0 {
            return Err("prolong count must be positive".into());
        }
        let (fen,ply,source): (String,u32,String) = self.conn()?.query_row(
            "SELECT fen,ply,source_task_id FROM deep_tree_nodes WHERE project_id=?1 AND id=?2 AND parent_id IS NOT NULL AND direct_score_kind IS NULL",
            params![project_id,node_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
            .map_err(|e| e.to_string())?;
        let task = self.queue_at_occurrence(project_id, &source, &fen, ply, node_id)?;
        self.conn()?
            .execute(
                "UPDATE deep_tasks SET remaining_prolong=?2,strategy='Prolong' WHERE id=?1",
                params![task.id, count],
            )
            .map_err(|e| e.to_string())?;
        Ok(task)
    }
    fn queue_root(
        &self,
        project_id: &str,
        fen: &str,
        ply: u32,
    ) -> Result<DeepAnalysisTask, String> {
        self.queue(project_id, None, fen, ply)
    }
    fn list_tasks(&self, project_id: &str) -> Result<Vec<DeepAnalysisTask>, String> {
        let c = self.conn()?;
        let mut q=c.prepare("SELECT id,project_id,parent_task_id,fen,moves,ply,status,error FROM deep_tasks WHERE project_id=?1 ORDER BY rowid").map_err(|e| e.to_string())?;
        let rows = q
            .query_map([project_id], |r| {
                Ok(DeepAnalysisTask {
                    id: r.get(0)?,
                    project_id: r.get(1)?,
                    parent_task_id: r.get(2)?,
                    fen: r.get(3)?,
                    moves: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    ply: r.get(5)?,
                    status: r.get(6)?,
                    error: r.get(7)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
    fn results(&self, project_id: &str) -> Result<Vec<DeepAnalysisResult>, String> {
        let c = self.conn()?;
        let mut q=c.prepare("SELECT r.task_id,r.multipv,r.depth,r.score_kind,r.score_value,r.score_perspective,r.uci_moves FROM deep_results r JOIN deep_tasks t ON t.id=r.task_id WHERE t.project_id=?1 ORDER BY t.rowid,r.multipv").map_err(|e| e.to_string())?;
        let rows = q
            .query_map([project_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    DeepAnalysisLine {
                        multipv: r.get(1)?,
                        depth: r.get(2)?,
                        score_kind: r.get(3)?,
                        score_value: r.get(4)?,
                        score_perspective: r.get(5)?,
                        uci_moves: serde_json::from_str(&r.get::<_, String>(6)?)
                            .unwrap_or_default(),
                    },
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut out: Vec<DeepAnalysisResult> = vec![];
        for row in rows {
            let (task_id, line) = row.map_err(|e| e.to_string())?;
            if out.last().is_none_or(|v| v.task_id != task_id) {
                out.push(DeepAnalysisResult {
                    task_id,
                    lines: vec![],
                });
            }
            out.last_mut().unwrap().lines.push(line);
        }
        Ok(out)
    }
    fn stop(&self, id: &str, clear_pending: bool) -> Result<(), String> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        if tx
            .execute(
                "UPDATE deep_projects SET status='stopped' WHERE id=?1 AND status!='cancelled'",
                [id],
            )
            .map_err(|e| e.to_string())?
            == 0
        {
            return Err("project not found or cancelled".into());
        }
        if clear_pending {
            tx.execute("UPDATE deep_tasks SET status='cancelled' WHERE project_id=?1 AND status IN ('queued','running')",[id]).map_err(|e|e.to_string())?;
        } else {
            tx.execute(
                "UPDATE deep_tasks SET status='queued' WHERE project_id=?1 AND status='running'",
                [id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }
    fn cancel(&self, id: &str) -> Result<(), String> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let n = tx
            .execute(
                "UPDATE deep_projects SET status='cancelled' WHERE id=?1",
                [id],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("project not found".into());
        }
        tx.execute("UPDATE deep_tasks SET status='cancelled' WHERE project_id=?1 AND status IN ('queued','running')",[id]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
    fn status(&self, id: &str) -> Result<String, String> {
        self.get_project(id)?
            .map(|p| p.status)
            .ok_or_else(|| "project not found".into())
    }
    fn fail_worker(&self, id: &str, error: &str) -> Result<(), String> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let updated = tx
            .execute(
                "UPDATE deep_projects SET status='failed' WHERE id=?1 AND status IN ('running','pausing')",
                [id],
            )
            .map_err(|e| e.to_string())?;
        if updated != 0 {
            tx.execute("UPDATE deep_tasks SET status='failed',error=?2 WHERE project_id=?1 AND status='running'", params![id,error]).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())
    }
    fn pause_after_current(&self, id: &str) -> Result<(), String> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let updated = tx
            .execute(
                "UPDATE deep_projects SET status=CASE WHEN EXISTS(
                SELECT 1 FROM deep_tasks WHERE project_id=?1 AND status='running'
            ) THEN 'pausing' ELSE 'paused' END
            WHERE id=?1 AND status IN ('running','pausing','paused')",
                [id],
            )
            .map_err(|e| e.to_string())?;
        if updated == 0 {
            return Err("project is not running or paused".into());
        }
        tx.commit().map_err(|e| e.to_string())
    }
    fn complete_pause_after_current(&self, id: &str) -> Result<bool, String> {
        let updated = self
            .conn()?
            .execute(
                "UPDATE deep_projects SET status='paused' WHERE id=?1 AND status='pausing'
            AND NOT EXISTS(SELECT 1 FROM deep_tasks WHERE project_id=?1 AND status='running')",
                [id],
            )
            .map_err(|e| e.to_string())?;
        Ok(updated != 0)
    }
    fn pause(&self, id: &str) -> Result<(), String> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        if tx
            .execute(
                "UPDATE deep_projects SET status='paused' WHERE id=?1 AND status!='cancelled'",
                [id],
            )
            .map_err(|e| e.to_string())?
            == 0
        {
            return Err("project not found or cancelled".into());
        }
        tx.execute(
            "UPDATE deep_tasks SET status='queued' WHERE project_id=?1 AND status='running'",
            [id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
    fn finish(&self, id: &str) -> Result<bool, String> {
        let updated = self
            .conn()?
            .execute(
                "UPDATE deep_projects SET status=CASE WHEN EXISTS (
                    SELECT 1 FROM deep_tasks WHERE project_id=?1 AND status='failed'
                ) THEN 'failed' WHEN EXISTS (
                    SELECT 1 FROM deep_scheduler WHERE project_id=?1 AND enabled=1
                ) THEN 'exhausted' ELSE 'completed' END
                WHERE id=?1 AND status='running' AND NOT EXISTS (
                    SELECT 1 FROM deep_tasks WHERE project_id=?1 AND status IN ('queued','running')
                )",
                [id],
            )
            .map_err(|e| e.to_string())?;
        Ok(updated != 0)
    }
    fn begin_running(&self, id: &str, expected: &str) -> Result<(), String> {
        let count = self.conn()?.execute(
            "UPDATE deep_projects SET status='running' WHERE id=?1 AND status=?2 AND status!='cancelled'",
            params![id, expected],
        ).map_err(|e| e.to_string())?;
        if count == 0 {
            Err("project changed state before start".into())
        } else {
            Ok(())
        }
    }
    #[cfg(test)]
    fn set_status(&self, id: &str, status: &str) -> Result<(), String> {
        let n = self
            .conn()?
            .execute(
                "UPDATE deep_projects SET status=?2 WHERE id=?1",
                params![id, status],
            )
            .map_err(|e| e.to_string())?;
        if n == 0 {
            Err("project not found".into())
        } else {
            Ok(())
        }
    }
    fn claim(&self, project_id: &str) -> Result<Option<DeepAnalysisTask>, String> {
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let id:Option<String>=tx.query_row("SELECT t.id FROM deep_tasks t JOIN deep_projects p ON p.id=t.project_id WHERE t.project_id=?1 AND t.status='queued' AND p.status='running' ORDER BY t.priority DESC,t.rowid LIMIT 1",[project_id],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
        if let Some(ref id) = id {
            tx.execute(
                "UPDATE deep_tasks SET status='running',error=NULL WHERE id=?1",
                [id],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(self
            .list_tasks(project_id)?
            .into_iter()
            .find(|t| Some(&t.id) == id.as_ref()))
    }
    fn requeue(&self, id: &str) -> Result<(), String> {
        self.conn()?
            .execute(
                "UPDATE deep_tasks SET status='queued' WHERE id=?1 AND status='running'",
                [id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn fail(&self, id: &str, error: &str) -> Result<(), String> {
        self.conn()?
            .execute(
                "UPDATE deep_tasks SET status='failed',error=?2 WHERE id=?1 AND status='running'",
                params![id, error],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn snapshot(&self, task_id: &str, line: &DeepAnalysisLine) -> Result<(), String> {
        let c = self.conn()?;
        // A pause/cancel can race an engine line: the write and status check must be atomic.
        c.execute("INSERT INTO deep_results(task_id,multipv,depth,score_kind,score_value,score_perspective,uci_moves)
            SELECT t.id,?2,?3,?4,?5,?6,?7 FROM deep_tasks t JOIN deep_projects p ON p.id=t.project_id
            WHERE t.id=?1 AND t.status='running' AND p.status IN ('running','pausing')
            ON CONFLICT(task_id,multipv) DO UPDATE SET depth=excluded.depth,score_kind=excluded.score_kind,
                score_value=excluded.score_value,score_perspective=excluded.score_perspective,uci_moves=excluded.uci_moves
            WHERE excluded.depth >= deep_results.depth",
            params![task_id,line.multipv,line.depth,line.score_kind,line.score_value,line.score_perspective,
                serde_json::to_string(&line.uci_moves).map_err(|e|e.to_string())?])
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    fn cache_identity(&self, task: &DeepAnalysisTask) -> Result<String, String> {
        let conn = self.conn()?;
        let history: Option<String>=conn.query_row("SELECT n.history FROM deep_tasks t LEFT JOIN deep_tree_nodes n ON n.id=t.occurrence_id
            WHERE t.id=?1",[&task.id],|r|r.get(0)).map_err(|e|e.to_string())?;
        let history =
            history.unwrap_or(serde_json::to_string(&task.moves).map_err(|e| e.to_string())?);
        let fen = Fen::from_position(
            position_after(&task.fen, &task.moves)?,
            EnPassantMode::Legal,
        )
        .to_string();
        Ok(format!("{fen}|{history}"))
    }
    fn cached(
        &self,
        task: &DeepAnalysisTask,
        engine: &str,
        multipv: u16,
        time_ms: u32,
        depth: u32,
    ) -> Result<Option<Vec<DeepAnalysisLine>>, String> {
        let key = self.cache_identity(task)?;
        let raw:Option<String>=self.conn()?.query_row("SELECT lines_json FROM deep_analysis_cache WHERE position_key=?1 AND engine=?2 AND multipv=?3 AND elapsed_ms>=?4 AND depth>=?5
            ORDER BY depth DESC,elapsed_ms DESC LIMIT 1",params![key,engine,multipv,time_ms,depth],|r|r.get(0))
            .optional().map_err(|e|e.to_string())?;
        raw.map(|v| serde_json::from_str(&v).map_err(|e| e.to_string()))
            .transpose()
    }
    fn save_cached(
        &self,
        task_id: &str,
        lines: &[DeepAnalysisLine],
        elapsed_ms: u64,
        engine: &str,
    ) -> Result<(), String> {
        self.save_impl(task_id, lines, Some((elapsed_ms, engine)))
    }
    fn save(&self, task_id: &str, lines: &[DeepAnalysisLine]) -> Result<(), String> {
        self.save_impl(task_id, lines, None)
    }
    fn save_impl(
        &self,
        task_id: &str,
        lines: &[DeepAnalysisLine],
        quality: Option<(u64, &str)>,
    ) -> Result<(), String> {
        let cache_key = if quality.is_some() {
            let task=self.conn()?.query_row("SELECT project_id,parent_task_id,fen,moves,ply,status,error FROM deep_tasks WHERE id=?1",[task_id],|r|Ok(DeepAnalysisTask {
                id:task_id.into(),project_id:r.get(0)?,parent_task_id:r.get(1)?,fen:r.get(2)?,
                moves:serde_json::from_str(&r.get::<_,String>(3)?).unwrap_or_default(),ply:r.get(4)?,status:r.get(5)?,error:r.get(6)?
            })).map_err(|e|e.to_string())?;
            Some(self.cache_identity(&task)?)
        } else {
            None
        };
        if lines.is_empty() {
            return Err("no valid scored PV".into());
        }
        let mut c = self.conn()?;
        let tx = c.transaction().map_err(|e| e.to_string())?;
        let running:i64=tx.query_row("SELECT count(*) FROM deep_tasks t JOIN deep_projects p ON p.id=t.project_id WHERE t.id=?1 AND t.status='running' AND p.status IN ('running','pausing')",[task_id],|r|r.get(0)).map_err(|e|e.to_string())?;
        if running == 0 {
            return Err("task is no longer running".into());
        }
        tx.execute("DELETE FROM deep_results WHERE task_id=?1", [task_id])
            .map_err(|e| e.to_string())?;
        for line in lines {
            tx.execute("INSERT INTO deep_results(task_id,multipv,depth,score_kind,score_value,score_perspective,uci_moves) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![task_id,line.multipv,line.depth,line.score_kind,line.score_value,line.score_perspective,serde_json::to_string(&line.uci_moves).map_err(|e|e.to_string())?]).map_err(|e|e.to_string())?;
        }
        tree::store_completed(&tx, task_id, lines, quality.map(|(elapsed, _)| elapsed))?;
        if let (Some((elapsed, engine)), Some(key)) = (quality, cache_key) {
            let depth = lines
                .iter()
                .map(|line| line.depth)
                .min()
                .ok_or("no scored PV")?;
            tx.execute("INSERT OR REPLACE INTO deep_analysis_cache(position_key,engine,multipv,elapsed_ms,depth,lines_json)
                VALUES(?1,?2,?3,?4,?5,?6)",params![key,engine,lines.len(),elapsed,depth,serde_json::to_string(lines).map_err(|e|e.to_string())?])
                .map_err(|e|e.to_string())?;
        }
        let (remaining, priority, project_id): (u8, i64, String) = tx
            .query_row(
                "SELECT remaining_prolong,priority,project_id FROM deep_tasks WHERE id=?1",
                [task_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        if remaining > 1 {
            if let Some(next_move) = lines
                .iter()
                .min_by_key(|line| line.multipv)
                .and_then(|line| line.uci_moves.first())
            {
                let child: Option<(i64, String, u32)> = tx
                    .query_row(
                        "SELECT child.id,child.fen,child.ply FROM deep_tree_analyses a
                     JOIN deep_tree_nodes child ON child.parent_id=a.node_id
                     WHERE a.task_id=?1 AND child.move_uci=?2 AND child.direct_score_kind IS NULL
                       AND NOT EXISTS(SELECT 1 FROM deep_tasks t WHERE t.occurrence_id=child.id)",
                        params![task_id, next_move],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if let Some((child_id, fen, ply)) = child {
                    let next_id = uuid::Uuid::new_v4().to_string();
                    tx.execute("INSERT INTO deep_tasks(id,project_id,parent_task_id,fen,moves,ply,occurrence_id,priority,remaining_prolong,strategy)
                        VALUES(?1,?2,?3,?4,'[]',?5,?6,?7,?8,'Prolong')",
                        params![next_id,project_id,task_id,fen,ply,child_id,priority,remaining-1])
                        .map_err(|e|e.to_string())?;
                    // A chained prolong is part of its parent's original request, not a new global-settings request.
                    tx.execute(
                        "UPDATE deep_tasks SET
                        execution_engine=(SELECT execution_engine FROM deep_tasks WHERE id=?2),
                        engine_profile=(SELECT engine_profile FROM deep_tasks WHERE id=?2),
                        requested_multipv=(SELECT requested_multipv FROM deep_tasks WHERE id=?2),
                        requested_time_ms=(SELECT requested_time_ms FROM deep_tasks WHERE id=?2),
                        requested_depth=(SELECT requested_depth FROM deep_tasks WHERE id=?2),
                        operation_mode=(SELECT operation_mode FROM deep_tasks WHERE id=?2),
                        cache_policy=(SELECT cache_policy FROM deep_tasks WHERE id=?2)
                        WHERE id=?1",
                        params![next_id, task_id],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        tx.execute(
            "UPDATE deep_tasks SET status='completed' WHERE id=?1",
            [task_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())
    }
    fn progress(
        &self,
        project_id: &str,
        task_id: Option<String>,
        error: Option<String>,
    ) -> Result<DeepAnalysisProgress, String> {
        let tasks = self.list_tasks(project_id)?;
        Ok(DeepAnalysisProgress {
            project_id: project_id.into(),
            task_id,
            status: self.status(project_id)?,
            completed: tasks.iter().filter(|t| t.status == "completed").count() as u32,
            total: tasks.len() as u32,
            error,
        })
    }
}

// Validate the entire PV against the board; never persist a truncated/illegal continuation.
fn parse_line(text: &str, fen: &str, history: &[String]) -> Option<DeepAnalysisLine> {
    let mut board = position_after(fen, history).ok()?;
    let turn = board.turn();
    let mut multipv = 1;
    let mut depth = 0;
    let mut score = None;
    let mut moves = None;
    if let UciMessage::Info(attrs) = parse_one(text) {
        for attr in attrs {
            match attr {
                UciInfoAttribute::MultiPv(v) => multipv = v,
                UciInfoAttribute::Depth(v) => depth = v,
                UciInfoAttribute::Score(v) => score = Some(v.value),
                UciInfoAttribute::Pv(v) => moves = Some(v),
                _ => {}
            }
        }
    }
    let (score_kind, score_value) = match score? {
        ScoreValue::Cp(v) => ("cp", v),
        ScoreValue::Mate(v) => ("mate", v),
    };
    let mut uci_moves = vec![];
    for m in moves? {
        let uci = UciMove::from_ascii(m.to_string().as_bytes()).ok()?;
        let mv = uci.to_move(&board).ok()?;
        board.play_unchecked(&mv);
        uci_moves.push(uci.to_string());
    }
    if depth == 0 || multipv == 0 || uci_moves.is_empty() {
        return None;
    }
    Some(DeepAnalysisLine {
        multipv,
        depth,
        score_kind: score_kind.into(),
        score_value: if turn == Color::Black {
            -score_value
        } else {
            score_value
        },
        score_perspective: "white".into(),
        uci_moves,
    })
}

#[derive(Default)]
pub struct DeepAnalysisState {
    active: Arc<Mutex<Option<ActiveWorker>>>,
}
struct ActiveWorker {
    project_id: String,
    cancelled: Arc<AtomicBool>,
}
struct WorkerLease {
    active: Arc<Mutex<Option<ActiveWorker>>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for WorkerLease {
    fn drop(&mut self) {
        *self.active.lock().unwrap() = None;
    }
}
impl DeepAnalysisState {
    fn pause_after_current(&self, store: &Store, project_id: &str) -> Result<(), String> {
        let active = self.active.lock().map_err(|e| e.to_string())?;
        if active
            .as_ref()
            .is_some_and(|worker| worker.project_id == project_id)
        {
            store.pause_after_current(project_id)
        } else {
            // A missing worker cannot finish an orphaned running task.
            store.pause(project_id)
        }
    }
    fn pause(&self, store: &Store, project_id: &str) -> Result<(), String> {
        let active = self.active.lock().map_err(|e| e.to_string())?;
        store.pause(project_id)?;
        if let Some(worker) = active.as_ref().filter(|w| w.project_id == project_id) {
            worker.cancelled.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn stop(&self, store: &Store, project_id: &str, clear_pending: bool) -> Result<bool, String> {
        let active = self.active.lock().map_err(|e| e.to_string())?;
        store.stop(project_id, clear_pending)?;
        if let Some(worker) = active.as_ref().filter(|w| w.project_id == project_id) {
            worker.cancelled.store(true, Ordering::Release);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn cancel(&self, store: &Store, project_id: &str) -> Result<(), String> {
        let active = self.active.lock().map_err(|e| e.to_string())?;
        store.cancel(project_id)?;
        if let Some(worker) = active.as_ref().filter(|w| w.project_id == project_id) {
            worker.cancelled.store(true, Ordering::Release);
        }
        Ok(())
    }
    async fn acquire_after_drain(&self, project_id: &str) -> Result<WorkerLease, String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                {
                    let active = self.active.lock().map_err(|e| e.to_string())?;
                    if active.as_ref().is_some_and(|w| w.project_id != project_id) {
                        return Err("another deep analysis project is active".into());
                    }
                }
                if let Ok(lease) = self.acquire(project_id) {
                    return Ok(lease);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| "previous worker did not stop in time".to_owned())?
    }
    fn acquire(&self, project_id: &str) -> Result<WorkerLease, String> {
        let mut active = self.active.lock().map_err(|e| e.to_string())?;
        if active.is_some() {
            return Err("deep analysis worker already active".into());
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        *active = Some(ActiveWorker {
            project_id: project_id.to_owned(),
            cancelled: cancelled.clone(),
        });
        Ok(WorkerLease {
            active: self.active.clone(),
            cancelled,
        })
    }
}
static APP_STORE: Mutex<Option<Store>> = Mutex::new(None);

fn cached_store(path: &Path, cache: &Mutex<Option<Store>>) -> Result<Store, String> {
    let mut guard = cache.lock().map_err(|e| e.to_string())?;
    if let Some(store) = guard.as_ref() {
        if store.path != path {
            return Err("analysis database path changed within one app process".into());
        }
        return Ok(store.clone());
    }
    let store = Store::open(path)?;
    *guard = Some(store.clone());
    Ok(store)
}

fn store(app: &tauri::AppHandle) -> Result<Store, String> {
    cached_store(&database_path(app)?, &APP_STORE)
}
pub fn recover_on_startup(app: &tauri::AppHandle) -> Result<(), String> {
    store(app)?.recover()
}

#[tauri::command]
#[specta::specta]
pub fn create_deep_analysis_project(
    app: tauri::AppHandle,
    title: String,
    engine: String,
    time_ms: u32,
    multipv: u16,
    fen: String,
    moves: Vec<String>,
) -> Result<DeepAnalysisProject, String> {
    let registration = app
        .path()
        .resolve("engines/engines.json", BaseDirectory::AppData)
        .map_err(|e| e.to_string())?;
    registered_engine(&engine, &registration)?;
    store(&app)?.create_project(&title, &engine, time_ms, multipv, &fen, &moves)
}
#[tauri::command]
#[specta::specta]
pub fn list_deep_analysis_summaries(
    app: tauri::AppHandle,
) -> Result<Vec<read::AnalysisProjectSummary>, String> {
    let s = store(&app)?;
    s.list_projects()?
        .iter()
        .map(|p| s.read_project_summary(&p.id))
        .collect()
}
#[tauri::command]
#[specta::specta]
pub fn get_deep_analysis_summary(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<read::AnalysisProjectSummary, String> {
    store(&app)?.read_project_summary(&project_id)
}
#[tauri::command]
#[specta::specta]
pub fn get_deep_analysis_tree(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<Vec<read::AnalysisNodeView>, String> {
    store(&app)?.read_tree(&project_id)
}
#[tauri::command]
#[specta::specta]
pub fn get_deep_analysis_node(
    app: tauri::AppHandle,
    project_id: String,
    node_id: String,
) -> Result<read::AnalysisNodeView, String> {
    node_id
        .parse::<i64>()
        .map_err(|_| "invalid analysis node id")?;
    store(&app)?
        .read_tree(&project_id)?
        .into_iter()
        .find(|node| node.id == node_id)
        .ok_or("analysis node not found".into())
}
#[tauri::command]
#[specta::specta]
pub fn get_deep_analysis_task_views(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<Vec<read::AnalysisTaskView>, String> {
    store(&app)?.read_tasks(&project_id)
}
#[tauri::command]
#[specta::specta]
pub fn get_deep_analysis_scheduler(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<read::SchedulerView, String> {
    store(&app)?.read_scheduler(&project_id)
}
#[tauri::command]
#[specta::specta]
pub async fn start_automatic_deep_analysis(
    app: tauri::AppHandle,
    project_id: String,
    state: tauri::State<'_, DeepAnalysisState>,
) -> Result<(), String> {
    store(&app)?.prepare_automatic_project(&project_id)?;
    launch(app, project_id, &state).await
}
#[tauri::command]
#[specta::specta]
pub fn manual_prolong_deep_analysis(
    app: tauri::AppHandle,
    project_id: String,
    node_id: String,
    count: u8,
) -> Result<DeepAnalysisTask, String> {
    let node_id = node_id
        .parse::<i64>()
        .map_err(|_| "invalid analysis node id")?;
    let s = store(&app)?;
    let task = s.manual_prolong(&project_id, node_id, count)?;
    emit_update(
        &app,
        &project_id,
        AnalysisChange::TaskQueued,
        Some(&task.id),
    );
    let _ =
        DeepAnalysisProgress::emit(&s.progress(&project_id, Some(task.id.clone()), None)?, &app);
    Ok(task)
}

#[tauri::command]
#[specta::specta]
pub fn list_deep_analysis_projects(
    app: tauri::AppHandle,
) -> Result<Vec<DeepAnalysisProject>, String> {
    store(&app)?.list_projects()
}
#[tauri::command]
#[specta::specta]
pub fn list_deep_analysis_tasks(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<Vec<DeepAnalysisTask>, String> {
    store(&app)?.list_tasks(&project_id)
}
#[tauri::command]
#[specta::specta]
pub fn list_deep_analysis_results(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<Vec<DeepAnalysisResult>, String> {
    store(&app)?.results(&project_id)
}
#[tauri::command]
#[specta::specta]
pub fn queue_deep_analysis_root(
    app: tauri::AppHandle,
    project_id: String,
    fen: String,
    ply: u32,
) -> Result<DeepAnalysisTask, String> {
    let task = store(&app)?.queue_root(&project_id, &fen, ply)?;
    emit_update(
        &app,
        &project_id,
        AnalysisChange::TaskQueued,
        Some(&task.id),
    );
    Ok(task)
}
#[tauri::command]
#[specta::specta]
pub fn queue_deep_analysis_continuation(
    app: tauri::AppHandle,
    project_id: String,
    parent_task_id: String,
    fen: String,
    ply: u32,
    occurrence_id: Option<i64>,
) -> Result<DeepAnalysisTask, String> {
    let s = store(&app)?;
    let task = match occurrence_id {
        Some(id) => s.queue_at_occurrence(&project_id, &parent_task_id, &fen, ply, id),
        None => s.queue(&project_id, Some(&parent_task_id), &fen, ply),
    }?;
    emit_update(
        &app,
        &project_id,
        AnalysisChange::TaskQueued,
        Some(&task.id),
    );
    Ok(task)
}
#[tauri::command]
#[specta::specta]
pub fn pause_after_current_deep_analysis(
    app: tauri::AppHandle,
    project_id: String,
) -> Result<(), String> {
    let s = store(&app)?;
    app.state::<DeepAnalysisState>()
        .pause_after_current(&s, &project_id)?;
    emit_update(&app, &project_id, AnalysisChange::ProjectStateChanged, None);
    let _ = DeepAnalysisProgress::emit(&s.progress(&project_id, None, None)?, &app);
    Ok(())
}
#[tauri::command]
#[specta::specta]
pub fn pause_deep_analysis(app: tauri::AppHandle, project_id: String) -> Result<(), String> {
    let s = store(&app)?;
    app.state::<DeepAnalysisState>().pause(&s, &project_id)?;
    emit_update(&app, &project_id, AnalysisChange::ProjectStateChanged, None);
    emit_update(&app, &project_id, AnalysisChange::EngineStateChanged, None);
    let _ = DeepAnalysisProgress::emit(&s.progress(&project_id, None, None)?, &app);
    Ok(())
}
#[tauri::command]
#[specta::specta]
pub async fn stop_deep_analysis(
    app: tauri::AppHandle,
    project_id: String,
    clear_pending: bool,
    state: tauri::State<'_, DeepAnalysisState>,
) -> Result<(), String> {
    let s = store(&app)?;
    let active = state.stop(&s, &project_id, clear_pending)?;
    if active {
        drop(state.acquire_after_drain(&project_id).await?);
    }
    emit_update(&app, &project_id, AnalysisChange::ProjectStateChanged, None);
    emit_update(&app, &project_id, AnalysisChange::EngineStateChanged, None);
    let _ = DeepAnalysisProgress::emit(&s.progress(&project_id, None, None)?, &app);
    Ok(())
}
#[tauri::command]
#[specta::specta]
pub fn cancel_deep_analysis(app: tauri::AppHandle, project_id: String) -> Result<(), String> {
    let s = store(&app)?;
    app.state::<DeepAnalysisState>().cancel(&s, &project_id)?;
    emit_update(&app, &project_id, AnalysisChange::ProjectStateChanged, None);
    let _ = DeepAnalysisProgress::emit(&s.progress(&project_id, None, None)?, &app);
    Ok(())
}
#[tauri::command]
#[specta::specta]
pub async fn resume_deep_analysis(
    app: tauri::AppHandle,
    project_id: String,
    state: tauri::State<'_, DeepAnalysisState>,
) -> Result<(), String> {
    launch(app, project_id, &state).await
}
#[tauri::command]
#[specta::specta]
pub async fn start_deep_analysis(
    app: tauri::AppHandle,
    project_id: String,
    state: tauri::State<'_, DeepAnalysisState>,
) -> Result<(), String> {
    launch(app, project_id, &state).await
}
async fn launch(
    app: tauri::AppHandle,
    project_id: String,
    state: &DeepAnalysisState,
) -> Result<(), String> {
    let s = store(&app)?;
    let initial_status = s.status(&project_id)?;
    if initial_status == "cancelled" {
        return Err("cancelled project cannot resume".into());
    }
    if initial_status == "running" || initial_status == "pausing" {
        return Err("project already running".into());
    }
    let project = s.get_project(&project_id)?.ok_or("project not found")?;
    let registration = app
        .path()
        .resolve("engines/engines.json", BaseDirectory::AppData)
        .map_err(|e| e.to_string())?;
    registered_engine(&project.engine, &registration)?;
    // Hold the single gate through engine termination and worker teardown.
    let lease = state.acquire_after_drain(&project_id).await?;
    let current_status = s.status(&project_id)?;
    if current_status != initial_status {
        return Err(format!(
            "project changed state while waiting for worker: {current_status}"
        ));
    }
    s.begin_running(&project_id, &initial_status)?;
    emit_update(&app, &project_id, AnalysisChange::ProjectStateChanged, None);
    emit_update(&app, &project_id, AnalysisChange::EngineStateChanged, None);
    tauri::async_runtime::spawn(async move {
        let cancelled = lease.cancelled.clone();
        let _lease = lease;
        if let Err(e) = run_worker(&app, &project_id, &cancelled).await {
            log::error!("Deep analysis worker {}: {}", project_id, e);
            if let Ok(s) = store(&app) {
                let _ = s.fail_worker(&project_id, &e);
                emit_update(&app, &project_id, AnalysisChange::ProjectFailed, None);
                emit_update(&app, &project_id, AnalysisChange::EngineStateChanged, None);
                if let Ok(progress) = s.progress(&project_id, None, Some(e)) {
                    let _ = progress.emit(&app);
                }
            }
        }
    });
    Ok(())
}
async fn run_worker(
    app: &tauri::AppHandle,
    project_id: &str,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let s = store(app)?;
    loop {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        let status = s.status(project_id)?;
        if status == "pausing" {
            if s.complete_pause_after_current(project_id)? {
                emit_update(app, project_id, AnalysisChange::ProjectStateChanged, None);
                emit_update(app, project_id, AnalysisChange::EngineStateChanged, None);
                let _ = DeepAnalysisProgress::emit(&s.progress(project_id, None, None)?, app);
            }
            break;
        }
        if status != "running" {
            break;
        }
        let Some(task) = s.claim(project_id)? else {
            if cancelled.load(Ordering::Acquire) || s.status(project_id)? != "running" {
                break;
            }
            if s.generate_next(project_id)? {
                emit_update(app, project_id, AnalysisChange::SchedulerStageChanged, None);
                let _ = DeepAnalysisProgress::emit(&s.progress(project_id, None, None)?, app);
                continue;
            }
            if !s.finish(project_id)? {
                continue; // Queue won the race after claim returned None.
            }
            emit_update(app, project_id, AnalysisChange::ProjectExhausted, None);
            emit_update(app, project_id, AnalysisChange::EngineStateChanged, None);
            let _ = DeepAnalysisProgress::emit(&s.progress(project_id, None, None)?, app);
            break;
        };
        emit_update(app, project_id, AnalysisChange::TaskStarted, Some(&task.id));
        let _ =
            DeepAnalysisProgress::emit(&s.progress(project_id, Some(task.id.clone()), None)?, app);
        let project = s.get_project(project_id)?.ok_or("project not found")?;
        let outcome = match cached_for_project(&s, &task) {
            Ok(Some(lines)) => Ok((lines, None)),
            Ok(None) => {
                let registration = app
                    .path()
                    .resolve("engines/engines.json", BaseDirectory::AppData)
                    .map_err(|e| e.to_string())?;
                registered_engine(&s.task_execution(&task.id)?.engine, &registration)?;
                analyze_position_timed(&project, &task, &s, cancelled, Some(app))
                    .await
                    .map(|(lines, ms)| (lines, Some(ms)))
            }
            Err(e) => Err(e),
        };
        if cancelled.load(Ordering::Acquire)
            || !matches!(s.status(project_id)?.as_str(), "running" | "pausing")
        {
            s.requeue(&task.id)?;
            break;
        }
        match outcome {
            Ok((lines, elapsed)) => {
                let saved = if let Some(ms) = elapsed {
                    s.save_cached(&task.id, &lines, ms, &s.task_execution(&task.id)?.engine)
                } else {
                    s.save(&task.id, &lines)
                };
                if let Err(e) = saved {
                    if cancelled.load(Ordering::Acquire)
                        || !matches!(s.status(project_id)?.as_str(), "running" | "pausing")
                    {
                        s.requeue(&task.id)?;
                        break;
                    }
                    s.fail(&task.id, &e)?;
                    let _ = DeepAnalysisProgress::emit(
                        &s.progress(project_id, Some(task.id.clone()), Some(e))?,
                        app,
                    );
                    continue;
                }
            }
            Err(e) => {
                s.fail(&task.id, &e)?;
                let _ = DeepAnalysisProgress::emit(
                    &s.progress(project_id, Some(task.id.clone()), Some(e))?,
                    app,
                );
                continue;
            }
        }
        emit_update(
            app,
            project_id,
            AnalysisChange::TaskCompleted,
            Some(&task.id),
        );
        emit_update(
            app,
            project_id,
            AnalysisChange::TreeNodeUpdated,
            Some(&task.id),
        );
        emit_update(
            app,
            project_id,
            AnalysisChange::MinimaxUpdated,
            Some(&task.id),
        );
        let _ = DeepAnalysisProgress::emit(&s.progress(project_id, Some(task.id), None)?, app);
    }
    Ok(())
}
fn cached_for_project(
    s: &Store,
    task: &DeepAnalysisTask,
) -> Result<Option<Vec<DeepAnalysisLine>>, String> {
    let spec = s.task_execution(&task.id)?;
    s.cached(task, &spec.engine, spec.multipv, spec.time_ms, spec.depth)
}

#[cfg(test)]
async fn analyze_position(
    project: &DeepAnalysisProject,
    task: &DeepAnalysisTask,
    s: &Store,
    cancelled: &AtomicBool,
) -> Result<Vec<DeepAnalysisLine>, String> {
    Ok(analyze_position_timed(project, task, s, cancelled, None)
        .await?
        .0)
}
async fn analyze_position_timed(
    project: &DeepAnalysisProject,
    task: &DeepAnalysisTask,
    s: &Store,
    cancelled: &AtomicBool,
    event_app: Option<&tauri::AppHandle>,
) -> Result<(Vec<DeepAnalysisLine>, u64), String> {
    let spec = s.task_execution(&task.id)?;
    let requested_time = spec.time_ms;
    let requested_depth = spec.depth;
    let operation = spec.operation;
    // Dropping a pending handshake kills its child (UciCommunicator uses kill_on_drop).
    let handshake = EngineProcess::new(PathBuf::from(&spec.engine));
    tokio::pin!(handshake);
    let (mut engine, mut reader) = loop {
        tokio::select! {
            result = &mut handshake => break result.map_err(|e| e.to_string())?,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if cancelled.load(Ordering::Acquire)
                    || !matches!(s.status(&project.id)?.as_str(), "running" | "pausing") {
                    return Err("interrupted".into());
                }
            }
        }
    };
    if cancelled.load(Ordering::Acquire)
        || !matches!(s.status(&project.id)?.as_str(), "running" | "pausing")
    {
        let _ = engine.kill().await;
        return Err("interrupted".into());
    }
    let outcome = async {
        engine
            .set_options(EngineOptions {
                fen: task.fen.clone(),
                moves: task.moves.clone(),
                extra_options: vec![EngineOption {
                    name: "MultiPV".into(),
                    value: spec.multipv.to_string(),
                }],
            })
            .await
            .map_err(|e| e.to_string())?;
        engine
            .go(&GoMode::Infinite)
            .await
            .map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        let mut stop_at = None::<std::time::Instant>;
        let mut best = std::collections::BTreeMap::<u16, DeepAnalysisLine>::new();
        let mut last_update = std::time::Instant::now() - Duration::from_secs(1);
        loop {
            let next = tokio::time::timeout(Duration::from_millis(50), reader.next_line()).await;
            if cancelled.load(Ordering::Acquire)
                || !matches!(s.status(&project.id)?.as_str(), "running" | "pausing")
            {
                let _ = engine.stop().await;
                return Err("interrupted".into());
            }
            match next {
                Err(_) => {}
                Ok(Err(e)) => return Err(e.to_string()),
                Ok(Ok(None)) => return Err("engine exited before bestmove".into()),
                Ok(Ok(Some(line))) => {
                    if matches!(parse_one(&line), UciMessage::BestMove { .. }) {
                        break;
                    }
                    if let Some(parsed) = parse_line(&line, &task.fen, &task.moves) {
                        if parsed.multipv <= spec.multipv
                            && best
                                .get(&parsed.multipv)
                                .is_none_or(|old| old.depth <= parsed.depth)
                        {
                            s.snapshot(&task.id, &parsed)?;
                            best.insert(parsed.multipv, parsed);
                            if let Some(app) = event_app {
                                if last_update.elapsed() >= Duration::from_millis(800) {
                                    let _ = DeepAnalysisProgress::emit(
                                        &s.progress(&project.id, Some(task.id.clone()), None)?,
                                        app,
                                    );
                                    last_update = std::time::Instant::now();
                                }
                            }
                        }
                    }
                }
            }
            if let Some(at) = stop_at {
                if at.elapsed() >= Duration::from_secs(2) {
                    return Err("engine did not respond to stop".into());
                }
            } else {
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let depth = best.get(&1).map_or(0, |line| line.depth);
                match operation.decide(
                    elapsed,
                    depth,
                    requested_time,
                    requested_depth,
                    best.contains_key(&1),
                ) {
                    Completion::Waiting => {}
                    Completion::Success => {
                        engine.stop().await.map_err(|e| e.to_string())?;
                        stop_at = Some(std::time::Instant::now());
                    }
                    Completion::Timeout => {
                        engine.stop().await.map_err(|e| e.to_string())?;
                        return Err("engine produced no valid scored PV before hard timeout".into());
                    }
                }
            }
        }
        if best.is_empty() {
            return Err("engine produced no valid scored PV".into());
        }
        let measured_ms = u64::try_from(
            stop_at
                .unwrap_or_else(std::time::Instant::now)
                .duration_since(started)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        Ok((best.into_values().collect(), measured_ms))
    };
    let result = tokio::time::timeout(
        Duration::from_millis(
            u64::from(requested_time)
                .saturating_mul(10)
                .saturating_add(3_000),
        ),
        outcome,
    )
    .await
    .map_err(|_| "engine search timed out".to_owned())
    .and_then(|v| v);
    let _ = engine.kill().await;
    result
}

/// Standalone, deliberately bounded smoke-test entry point; does not touch the app-data database.
pub async fn run_debug_analysis(
    db: &Path,
    engine: &str,
    fen: &str,
    max_results: u32,
    time_ms: u32,
) -> Result<String, String> {
    if max_results == 0 {
        return Err("max_results must be positive".into());
    }
    let s = Store::open(db)?;
    let project = s.create_project("Debug analysis", engine, time_ms, 2, fen, &[])?;
    let settings = scheduler::AnalysisSettings {
        max_positions: 1,
        max_ply: 3,
        analysis_time_ms: time_ms,
        analysis_depth: 10,
        analysis_operation: AnalysisOperation::TimeOrDepth,
        engine_id: engine.to_owned(),
        ..Default::default()
    };
    s.enable_automatic(&project.id, &settings)?;
    drop(s);
    drive_debug_analysis(db, &project.id, max_results).await
}

/// Engines are restarted only after an explicit resume command; stage and queue are retained.
pub async fn resume_debug_analysis(
    db: &Path,
    id: &str,
    max_results: u32,
) -> Result<String, String> {
    drive_debug_analysis(db, id, max_results).await
}

async fn drive_debug_analysis(db: &Path, id: &str, max_results: u32) -> Result<String, String> {
    if max_results == 0 {
        return Err("max_results must be positive".into());
    }
    let s = Store::open(db)?;
    let project = s.get_project(id)?.ok_or("project not found")?;
    s.begin_running(id, "paused")?;
    let mut completed = 0;
    loop {
        let task = match s.claim(&project.id)? {
            Some(task) => task,
            None if s.generate_next(&project.id)? => continue,
            None => {
                s.finish(&project.id)?;
                break;
            }
        };
        let outcome = match cached_for_project(&s, &task) {
            Ok(Some(lines)) => Ok((lines, None)),
            Ok(None) => analyze_position_timed(&project, &task, &s, &AtomicBool::new(false), None)
                .await
                .map(|(lines, ms)| (lines, Some(ms))),
            Err(e) => Err(e),
        };
        match outcome {
            Ok((lines, Some(ms))) => {
                s.save_cached(&task.id, &lines, ms, &s.task_execution(&task.id)?.engine)?;
                completed += 1;
            }
            Ok((lines, None)) => {
                s.save(&task.id, &lines)?;
                completed += 1;
            }
            Err(e) => {
                s.fail(&task.id, &e)?;
                s.fail_worker(&project.id, &e)?;
                return Err(format!(
                    "project {} task {} failed: {e}",
                    project.id, task.id
                ));
            }
        }
        if completed >= max_results {
            s.pause(&project.id)?;
            break;
        }
    }
    drop(s);
    show_debug_analysis(db, &project.id)
}

/// Explicitly stop generation, preserve results and choose the queued-task policy.
pub fn stop_debug_analysis(
    db: &Path,
    project_id: &str,
    clear_pending: bool,
) -> Result<String, String> {
    let s = Store::open(db)?;
    s.stop(project_id, clear_pending)?;
    show_debug_analysis(db, project_id)
}

pub fn show_debug_analysis(db: &Path, id: &str) -> Result<String, String> {
    let s = Store::open(db)?;
    let p = s.get_project(id)?.ok_or("project not found")?;
    let tasks = s.list_tasks(id)?;
    let mut output = format!(
        "project={} status={} cycle={} completed={} tasks={}\n",
        id,
        p.status,
        s.scheduler_cycle(id)?,
        tasks.iter().filter(|t| t.status == "completed").count(),
        tasks.len()
    );
    let conn = s.conn()?;
    let mut statement=conn.prepare("SELECT id,parent_id,move_uci,fen,direct_score_kind,direct_score_value,minimax_score_kind,minimax_score_value,direct_depth FROM deep_tree_nodes WHERE project_id=?1 ORDER BY ply,id").map_err(|e|e.to_string())?;
    let nodes: Vec<_> = statement
        .query_map([id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<i32>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<i32>>(7)?,
                r.get::<_, Option<u32>>(8)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    output.push_str(&format!("nodes={}\n", nodes.len()));
    for (node, parent, mv, fen, kind, value, min_kind, min_value, depth) in nodes {
        output.push_str(&format!("node={node} parent={parent:?} move={mv:?} direct={kind:?}:{value:?} minimax={min_kind:?}:{min_value:?} depth={depth:?} fen={fen}\n"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn independent_root_fen() -> String {
        Fen::from_position(
            position_after(&start_fen(), &["d2d4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string()
    }

    #[test]
    fn analysis_store_uses_wal_for_concurrent_ui_reads_and_worker_writes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("analysis.sqlite")).unwrap();
        let journal: String = store
            .conn()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
    }

    #[test]
    fn app_store_does_not_rerun_migrations_for_each_ui_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.sqlite");
        let cached = std::sync::Mutex::new(None);
        let first = cached_store(&path, &cached).unwrap();
        first
            .create_project(
                "Local",
                "/opt/homebrew/bin/stockfish",
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        // A second Store::open would retry schema writes and eventually fail under this lock.
        let second = cached_store(&path, &cached).unwrap();
        assert_eq!(second.path, path);
        lock.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn analysis_update_event_kinds_have_stable_ui_names() {
        assert_eq!(
            serde_json::to_string(&AnalysisChange::TreeNodeUpdated).unwrap(),
            "\"tree_node_updated\""
        );
        assert_eq!(
            serde_json::to_string(&AnalysisChange::ProjectExhausted).unwrap(),
            "\"project_exhausted\""
        );
    }

    #[test]
    fn automatic_project_start_uses_its_persisted_engine_and_time_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("analysis.sqlite")).unwrap();
        let engine = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let project = store
            .create_project("UI study", &engine, 750, 2, &start_fen(), &[])
            .unwrap();
        store.prepare_automatic_project(&project.id).unwrap();
        let (settings, cycle, _) = store.automatic(&project.id).unwrap().unwrap();
        assert_eq!(settings.analysis_time_ms, 750);
        assert_eq!(settings.engine_id, engine);
        assert_eq!(cycle, 0);
        store.set_status(&project.id, "running").unwrap();
        assert!(store.prepare_automatic_project(&project.id).is_err());
        store.set_status(&project.id, "pausing").unwrap();
        assert!(store.prepare_automatic_project(&project.id).is_err());
        assert_eq!(store.automatic(&project.id).unwrap().unwrap().1, 0);
        assert!(store.prepare_automatic_project("missing-project").is_err());
    }

    #[test]
    fn creation_is_atomic_and_preserves_position_history_and_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let fen = start_fen();
        assert!(s
            .create_project(
                "invalid",
                engine.to_str().unwrap(),
                1000,
                2,
                &fen,
                &["e2e5".into()]
            )
            .is_err());
        assert!(s.list_projects().unwrap().is_empty());
        let p = s
            .create_project(
                "valid",
                engine.to_str().unwrap(),
                1000,
                2,
                &fen,
                &["e2e4".into()],
            )
            .unwrap();
        assert_eq!(p.time_ms, 1000);
        let tasks = s.list_tasks(&p.id).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].fen, fen);
        assert_eq!(tasks[0].moves, vec!["e2e4"]);
        assert!(s
            .create_project("bad", engine.to_str().unwrap(), 0, 1, &fen, &[])
            .is_err());
        assert!(s
            .create_project("bad", engine.to_str().unwrap(), u32::MAX, 1, &fen, &[])
            .is_err());
        assert!(s
            .create_project("bad", "/definitely/not/an/engine", 1000, 1, &fen, &[])
            .is_err());
    }
    #[test]
    fn pause_requeues_running_task_and_failure_never_overwrites_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project("x", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        let task = s.list_tasks(&p.id).unwrap().remove(0);
        s.set_status(&p.id, "running").unwrap();
        s.claim(&p.id).unwrap();
        s.pause(&p.id).unwrap();
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "queued");
        s.cancel(&p.id).unwrap();
        s.fail(&task.id, "late failure").unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "cancelled");
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "cancelled");
    }
    #[test]
    fn global_worker_gate_rejects_other_project_until_release() {
        let state = DeepAnalysisState::default();
        let first = state.acquire("first").unwrap();
        assert!(state.acquire("other").is_err());
        assert!(state.acquire("first").is_err());
        drop(first);
        assert!(state.acquire("other").is_ok());
    }
    #[test]
    fn failed_task_cannot_make_project_completed_when_queue_drains() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project(
                "failure",
                engine.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        s.fail(&task.id, "engine exited").unwrap();
        assert!(s.claim(&p.id).unwrap().is_none());
        s.finish(&p.id).unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "failed");
        assert_eq!(s.progress(&p.id, None, None).unwrap().completed, 0);
    }
    #[test]
    fn worker_error_marks_running_task_failed_without_touching_paused_project() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project("x", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        s.set_status(&p.id, "running").unwrap();
        s.claim(&p.id).unwrap();
        s.fail_worker(&p.id, "save failed").unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "failed");
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "failed");
        s.set_status(&p.id, "running").unwrap();
        s.pause(&p.id).unwrap();
        s.fail_worker(&p.id, "late failure").unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "paused");
    }
    #[test]
    fn registered_engine_rejects_unregistered_executables() {
        let dir = tempfile::tempdir().unwrap();
        let registration = dir.path().join("engines.json");
        let executable = std::env::current_exe().unwrap();
        std::fs::write(&registration, "[]").unwrap();
        assert!(registered_engine(executable.to_str().unwrap(), &registration).is_err());
        std::fs::write(
            &registration,
            serde_json::json!([{"type":"local", "path":executable}]).to_string(),
        )
        .unwrap();
        registered_engine(executable.to_str().unwrap(), &registration).unwrap();
    }
    #[test]
    fn cancelled_projects_reject_new_roots_and_continuations() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project("x", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        let parent = s.list_tasks(&p.id).unwrap().remove(0);
        s.cancel(&p.id).unwrap();
        assert!(s.queue_root(&p.id, "startpos", 0).is_err());
        assert!(s.queue(&p.id, Some(&parent.id), "startpos", 1).is_err());
        assert_eq!(s.list_tasks(&p.id).unwrap().len(), 1);
    }
    #[test]
    fn continuation_requires_completed_parent_and_a_position_on_its_pv() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project("x", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        let parent = s.list_tasks(&p.id).unwrap().remove(0);
        let after_e4 = Fen::from_position(
            position_after(&parent.fen, &["e2e4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let unrelated = Fen::from_position(
            position_after(&parent.fen, &["d2d4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        assert!(s.queue(&p.id, Some(&parent.id), &after_e4, 1).is_err());
        s.begin_running(&p.id, "paused").unwrap();
        s.claim(&p.id).unwrap();
        s.save(
            &parent.id,
            &[DeepAnalysisLine {
                multipv: 1,
                depth: 8,
                score_kind: "cp".into(),
                score_value: 22,
                score_perspective: "white".into(),
                uci_moves: vec!["e2e4".into(), "e7e5".into()],
            }],
        )
        .unwrap();
        assert!(s.queue(&p.id, Some(&parent.id), &unrelated, 1).is_err());
        assert!(s.queue(&p.id, Some(&parent.id), &parent.fen, 0).is_err());
        let child = s.queue(&p.id, Some(&parent.id), &after_e4, 1).unwrap();
        assert_eq!(child.parent_task_id.as_deref(), Some(parent.id.as_str()));
    }
    #[test]
    fn finish_reports_newly_queued_task_for_worker_retry() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project("race", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let first = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &first.id,
            &[parse_line("info depth 8 score cp 10 pv e2e4", &start_fen(), &[]).unwrap()],
        )
        .unwrap();
        assert!(s.claim(&p.id).unwrap().is_none());
        s.queue_root(&p.id, "startpos", 0).unwrap();
        assert!(!s.finish(&p.id).unwrap());
        assert!(s.claim(&p.id).unwrap().is_some());
    }
    #[test]
    fn paused_project_cannot_claim_queued_task() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let executable = std::env::current_exe().unwrap();
        let p = s
            .create_project(
                "x",
                executable.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        assert!(s.claim(&p.id).unwrap().is_none());
    }
    #[test]
    fn pv_is_validated_from_position_after_history() {
        let fen = start_fen();
        let parsed =
            parse_line("info depth 8 score cp 14 pv e7e5", &fen, &["e2e4".into()]).unwrap();
        assert_eq!(parsed.uci_moves, vec!["e7e5"]);
        assert_eq!(parsed.score_perspective, "white");
        assert!(parse_line("info depth 8 score cp 14 pv e2e4", &fen, &["e2e4".into()]).is_none());
    }
    #[test]
    fn existing_result_table_gains_explicit_white_score_perspective() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE deep_results (task_id TEXT NOT NULL, multipv INTEGER NOT NULL, depth INTEGER NOT NULL, score_kind TEXT NOT NULL, score_value INTEGER NOT NULL, uci_moves TEXT NOT NULL, PRIMARY KEY(task_id,multipv)); INSERT INTO deep_results VALUES ('old',1,8,'cp',17,'[\"e2e4\"]');").unwrap();
        drop(conn);
        let s = Store::open(&path).unwrap();
        let perspective: String = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT score_perspective FROM deep_results WHERE task_id='old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(perspective, "white");
    }
    #[test]
    fn score_perspective_survives_running_snapshot_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "score",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let task = s.list_tasks(&p.id).unwrap().remove(0);
        s.begin_running(&p.id, "paused").unwrap();
        s.claim(&p.id).unwrap();
        let line = parse_line("info depth 9 score cp 18 pv e2e4", &task.fen, &task.moves).unwrap();
        s.snapshot(&task.id, &line).unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.list_tasks(&p.id).unwrap()[0].status, "running");
        assert_eq!(
            reopened.results(&p.id).unwrap()[0].lines[0].score_perspective,
            "white"
        );
    }
    #[tokio::test]
    async fn resume_waits_for_paused_worker_to_release_single_gate() {
        let state = DeepAnalysisState::default();
        let old = state.acquire("first").unwrap();
        let copy = state.active.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            drop(old);
        });
        let next = state.acquire_after_drain("first").await.unwrap();
        assert_eq!(
            copy.lock().unwrap().as_ref().map(|w| w.project_id.as_str()),
            Some("first")
        );
        drop(next);
        assert!(state.acquire("other").is_ok());
    }
    #[test]
    fn launch_transition_cannot_revive_cancelled_project() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let executable = std::env::current_exe().unwrap();
        let p = s
            .create_project(
                "x",
                executable.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.cancel(&p.id).unwrap();
        assert!(s.begin_running(&p.id, "paused").is_err());
        assert_eq!(s.status(&p.id).unwrap(), "cancelled");
    }
    #[test]
    fn project_and_task_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.sqlite");
        let store = Store::open(&path).unwrap();
        let project = store
            .create_project(
                "Study",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        let task = store.list_tasks(&project.id).unwrap().remove(0);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_projects().unwrap()[0].id, project.id);
        assert_eq!(store.list_tasks(&project.id).unwrap()[0].id, task.id);
    }
    #[test]
    fn existing_transposed_paths_gain_one_project_local_position_without_losing_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transpositions.sqlite");
        let store = Store::open(&path).unwrap();
        let project = store
            .create_project(
                "Two paths",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let task = store.list_tasks(&project.id).unwrap().remove(0);
        let mut conn = store.conn().unwrap();
        let tx = conn.transaction().unwrap();
        let root: i64 = tx
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE source_task_id=?1 AND parent_id IS NULL",
                [&task.id],
                |r| r.get(0),
            )
            .unwrap();
        let mut leaves = Vec::new();
        for order in [
            ["g1f3", "g8f6", "b1c3", "b8c6"],
            ["b1c3", "b8c6", "g1f3", "g8f6"],
        ] {
            let mut history = Vec::new();
            let mut parent = root;
            for mv in order {
                history.push(mv.to_string());
                let board = position_after(&start_fen(), &history).unwrap();
                let fen = Fen::from_position(board, EnPassantMode::Legal).to_string();
                tx.execute(
                    "INSERT INTO deep_tree_nodes(project_id,source_task_id,parent_id,move_uci,fen,history,ply)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![project.id, task.id, parent, mv, fen,
                        serde_json::to_string(&history).unwrap(), history.len()],
                )
                .unwrap();
                parent = tx.last_insert_rowid();
            }
            leaves.push(parent);
        }
        tx.commit().unwrap();
        assert_ne!(leaves[0], leaves[1]);
        drop(conn);
        drop(store);

        for _ in 0..2 {
            let reopened = Store::open(&path).unwrap();
            let conn = reopened.conn().unwrap();
            let (first, second): (i64, i64) = (
                conn.query_row(
                    "SELECT position_id FROM deep_tree_nodes WHERE id=?1",
                    [leaves[0]],
                    |r| r.get(0),
                )
                .unwrap(),
                conn.query_row(
                    "SELECT position_id FROM deep_tree_nodes WHERE id=?1",
                    [leaves[1]],
                    |r| r.get(0),
                )
                .unwrap(),
            );
            assert_eq!(first, second);
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM deep_tree_nodes WHERE project_id=?1",
                    [&project.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                9
            );
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM deep_position_records WHERE project_id=?1",
                    [&project.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                8
            );
            assert_eq!(
                conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
            assert_eq!(reopened.list_tasks(&project.id).unwrap().len(), 1);
        }
    }

    #[test]
    fn newly_created_root_and_pv_nodes_have_canonical_positions_before_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("fresh.sqlite")).unwrap();
        let project = store
            .create_project(
                "Fresh positions",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let task = store.list_tasks(&project.id).unwrap().remove(0);
        store.begin_running(&project.id, "paused").unwrap();
        store.claim(&project.id).unwrap();
        let line = parse_line(
            "info depth 8 score cp 23 pv e2e4 e7e5",
            &task.fen,
            &task.moves,
        )
        .unwrap();
        store.save(&task.id, &[line]).unwrap();
        let conn = store.conn().unwrap();
        let (total, linked): (i64, i64) = conn
            .query_row(
                "SELECT count(*),count(position_id) FROM deep_tree_nodes WHERE project_id=?1",
                [&project.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((total, linked), (3, 3));
        let (target, root_position): (Option<i64>, i64) = conn
            .query_row(
                "SELECT t.position_id,n.position_id FROM deep_tasks t JOIN deep_tree_nodes n
             ON n.source_task_id=t.id AND n.parent_id IS NULL WHERE t.id=?1",
                [&task.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(target, Some(root_position));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM deep_task_interests WHERE task_id=?1",
                [&task.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            6
        );
    }

    #[test]
    fn legacy_pending_task_migrates_execution_requirements_once_without_losing_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE deep_projects(id TEXT PRIMARY KEY,title TEXT NOT NULL,engine TEXT NOT NULL,depth INTEGER NOT NULL DEFAULT 0,time_ms INTEGER NOT NULL,multipv INTEGER NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (datetime('now')));
            CREATE TABLE deep_tasks(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,parent_task_id TEXT,fen TEXT NOT NULL,moves TEXT NOT NULL DEFAULT '[]',ply INTEGER NOT NULL,status TEXT NOT NULL DEFAULT 'queued',error TEXT);
            CREATE TABLE deep_results(task_id TEXT NOT NULL,multipv INTEGER NOT NULL,depth INTEGER NOT NULL,score_kind TEXT NOT NULL,score_value INTEGER NOT NULL,score_perspective TEXT NOT NULL DEFAULT 'white',uci_moves TEXT NOT NULL,PRIMARY KEY(task_id,multipv));").unwrap();
        let engine = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        conn.execute("INSERT INTO deep_projects(id,title,engine,time_ms,multipv,status) VALUES('legacy','old',?1,650,2,'paused')", [&engine]).unwrap();
        conn.execute("INSERT INTO deep_tasks(id,project_id,fen,moves,ply) VALUES('legacy-task','legacy',?1,'[]',0)", [start_fen()]).unwrap();
        drop(conn);
        let migrated = Store::open(&path).unwrap();
        let spec = migrated.task_execution("legacy-task").unwrap();
        assert_eq!(
            (spec.time_ms, spec.depth, spec.multipv, spec.operation),
            (650, 12, 2, AnalysisOperation::TimeAndDepth)
        );
        assert_eq!(spec.engine, engine);
        drop(migrated);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.list_tasks("legacy").unwrap().len(), 1);
        assert_eq!(reopened.task_execution("legacy-task").unwrap().time_ms, 650);
        assert_eq!(
            reopened
                .conn()
                .unwrap()
                .pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
                .unwrap(),
            6
        );
    }

    #[test]
    fn fresh_task_schema_and_repeated_open_preserve_execution_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.sqlite");
        let store = Store::open(&path).unwrap();
        let project = store
            .create_project(
                "fresh",
                std::env::current_exe().unwrap().to_str().unwrap(),
                550,
                3,
                &start_fen(),
                &[],
            )
            .unwrap();
        let task = store.list_tasks(&project.id).unwrap().remove(0);
        let original = store.task_execution(&task.id).unwrap();
        assert_eq!((original.time_ms, original.multipv), (550, 3));
        let conn = store.conn().unwrap();
        let required: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('deep_tasks') WHERE name IN ('execution_engine','engine_profile','requested_multipv','requested_time_ms','requested_depth','operation_mode','cache_policy') AND \"notnull\"=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(required, 7);
        let triggers: i64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='deep_task_execution_snapshot'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(triggers, 1);
        drop(conn);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.list_tasks(&project.id).unwrap().len(), 1);
        let restored = reopened.task_execution(&task.id).unwrap();
        assert_eq!(
            (restored.time_ms, restored.multipv, restored.engine),
            (original.time_ms, original.multipv, original.engine)
        );
        assert_eq!(
            reopened
                .conn()
                .unwrap()
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn queued_task_keeps_its_engine_profile_limits_operation_and_cache_policy_after_settings_change(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let engine = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let p = s
            .create_project("settings", &engine, 750, 2, &start_fen(), &[])
            .unwrap();
        let first = s.list_tasks(&p.id).unwrap().remove(0);
        let original = s.task_execution(&first.id).unwrap();
        assert_eq!(
            (
                original.engine.as_str(),
                original.engine_profile.as_str(),
                original.time_ms,
                original.depth,
                original.multipv
            ),
            (engine.as_str(), "default", 750, 12, 2)
        );
        assert_eq!(original.operation, AnalysisOperation::TimeAndDepth);
        assert_eq!(original.cache_policy, "compatible_only");

        let initial = scheduler::AnalysisSettings {
            analysis_time_ms: 900,
            analysis_depth: 19,
            analysis_operation: AnalysisOperation::TimeOrDepth,
            engine_id: engine.clone(),
            ..Default::default()
        };
        s.enable_automatic(&p.id, &initial).unwrap();
        let pending = s.queue_root(&p.id, "startpos", 0).unwrap();
        let snapshot = s.task_execution(&pending.id).unwrap();
        assert_eq!(
            (
                snapshot.time_ms,
                snapshot.depth,
                snapshot.operation,
                snapshot.multipv
            ),
            (900, 19, AnalysisOperation::TimeOrDepth, 2)
        );
        assert_eq!(snapshot.engine, engine);
        assert_eq!(snapshot.cache_policy, "compatible_only");
        let changed = scheduler::AnalysisSettings {
            analysis_time_ms: 250,
            analysis_depth: 7,
            analysis_operation: AnalysisOperation::Ungated,
            engine_id: "another-engine".into(),
            ..Default::default()
        };
        s.enable_automatic(&p.id, &changed).unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        let restored = reopened.task_execution(&pending.id).unwrap();
        assert_eq!(
            (
                restored.time_ms,
                restored.depth,
                restored.operation,
                restored.multipv
            ),
            (900, 19, AnalysisOperation::TimeOrDepth, 2)
        );
        assert_eq!(restored.engine, engine);
        let view = reopened
            .read_tasks(&p.id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == pending.id)
            .unwrap();
        assert_eq!(
            (
                view.requested_time_ms,
                view.requested_depth,
                view.operation_mode.as_deref()
            ),
            (Some(900), Some(19), Some("time_or_depth"))
        );
        assert_eq!(
            (
                view.priority,
                view.remaining_prolong_count,
                view.source_node_id,
                view.strategy.as_str(),
                view.position_fen.as_str(),
                view.engine_profile.as_str(),
                view.requested_multipv,
                view.cache_policy.as_str(),
            ),
            (
                1,
                0,
                None,
                "manual",
                start_fen().as_str(),
                "default",
                2,
                "compatible_only"
            )
        );
    }

    #[test]
    fn finish_current_pause_preserves_running_task_queue_and_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "finish",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let next = s.queue_root(&p.id, &independent_root_fen(), 1).unwrap();
        s.conn()
            .unwrap()
            .execute("UPDATE deep_tasks SET priority=0 WHERE id=?1", [&next.id])
            .unwrap();
        let settings = scheduler::AnalysisSettings::default();
        s.enable_automatic(&p.id, &settings).unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let current = s.claim(&p.id).unwrap().unwrap();
        let before = s.automatic(&p.id).unwrap().unwrap();
        let state = DeepAnalysisState::default();
        let lease = state.acquire(&p.id).unwrap();
        state.pause_after_current(&s, &p.id).unwrap();
        state.pause_after_current(&s, &p.id).unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "pausing");
        assert!(!lease.cancelled.load(Ordering::Acquire));
        assert_eq!(
            s.list_tasks(&p.id)
                .unwrap()
                .iter()
                .find(|t| t.id == current.id)
                .unwrap()
                .status,
            "running"
        );
        assert!(s.claim(&p.id).unwrap().is_none());
        let line = DeepAnalysisLine {
            multipv: 1,
            depth: 12,
            score_kind: "cp".into(),
            score_value: 21,
            score_perspective: "white".into(),
            uci_moves: vec!["e2e4".into()],
        };
        s.snapshot(&current.id, &line).unwrap();
        s.save(&current.id, &[line.clone()]).unwrap();
        assert!(s.complete_pause_after_current(&p.id).unwrap());
        assert_eq!(s.status(&p.id).unwrap(), "paused");
        assert_eq!(
            s.list_tasks(&p.id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "queued")
                .count(),
            1
        );
        assert_eq!(
            s.list_tasks(&p.id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "completed")
                .count(),
            1
        );
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0], line);
        assert_eq!(s.automatic(&p.id).unwrap().unwrap().1, before.1);
        drop(lease);
    }

    #[test]
    fn recovery_of_pausing_project_is_safe_and_explicitly_resumable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "pausing",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let _next = s.queue_root(&p.id, &independent_root_fen(), 1).unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let state = DeepAnalysisState::default();
        let lease = state.acquire(&p.id).unwrap();
        state.pause_after_current(&s, &p.id).unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "pausing");
        drop(lease);
        drop(s);
        let reopened = Store::open(&path).unwrap();
        reopened.recover().unwrap();
        assert_eq!(reopened.status(&p.id).unwrap(), "paused");
        assert_eq!(
            reopened
                .list_tasks(&p.id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "queued")
                .count(),
            2
        );
        assert_eq!(reopened.read_scheduler(&p.id).unwrap().active_task_id, None);
        assert!(reopened.claim(&p.id).unwrap().is_none());
        reopened.begin_running(&p.id, "paused").unwrap();
        assert_eq!(reopened.claim(&p.id).unwrap().unwrap().id, task.id);
        let tasks = reopened.list_tasks(&p.id).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_ne!(tasks[0].id, tasks[1].id);
    }

    #[test]
    fn recovery_requeues_inflight_and_pauses_project() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "x",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let t = s.list_tasks(&p.id).unwrap().remove(0);
        s.set_status(&p.id, "running").unwrap();
        s.claim(&p.id).unwrap();
        let line = DeepAnalysisLine {
            multipv: 1,
            depth: 8,
            score_kind: "cp".into(),
            score_value: 22,
            score_perspective: "white".into(),
            uci_moves: vec!["e2e4".into()],
        };
        s.snapshot(&t.id, &line).unwrap();
        s.recover().unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "paused");
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "queued");
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].id, t.id);
        assert_eq!(s.progress(&p.id, None, None).unwrap().completed, 0);
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0], line);
        assert!(s.claim(&p.id).unwrap().is_none());
        s.begin_running(&p.id, "paused").unwrap();
        s.claim(&p.id).unwrap();
        let shallower = DeepAnalysisLine {
            depth: 4,
            ..line.clone()
        };
        s.snapshot(&t.id, &shallower).unwrap();
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0], line);
        s.pause(&p.id).unwrap();
        s.snapshot(
            &t.id,
            &DeepAnalysisLine {
                depth: 12,
                ..line.clone()
            },
        )
        .unwrap();
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0], line);
    }
    #[test]
    fn cancelled_task_cannot_accept_late_engine_output() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "x",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let t = s.list_tasks(&p.id).unwrap().remove(0);
        s.set_status(&p.id, "running").unwrap();
        s.claim(&p.id).unwrap();
        s.cancel(&p.id).unwrap();
        assert!(s
            .save(
                &t.id,
                &[DeepAnalysisLine {
                    multipv: 1,
                    depth: 10,
                    score_kind: "cp".into(),
                    score_value: 10,
                    score_perspective: "white".into(),
                    uci_moves: vec!["e2e4".into()]
                }]
            )
            .is_err());
        assert!(s.results(&p.id).unwrap().is_empty());
    }
    #[test]
    fn cancellation_marks_unfinished_tasks_and_keeps_completed_results() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "x",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let first = s.list_tasks(&p.id).unwrap().remove(0);
        s.set_status(&p.id, "running").unwrap();
        assert_eq!(s.claim(&p.id).unwrap().unwrap().id, first.id);
        let second = s.queue_root(&p.id, &independent_root_fen(), 1).unwrap();
        s.save(
            &first.id,
            &[DeepAnalysisLine {
                multipv: 1,
                depth: 10,
                score_kind: "cp".into(),
                score_value: 12,
                score_perspective: "white".into(),
                uci_moves: vec!["e2e4".into()],
            }],
        )
        .unwrap();
        s.cancel(&p.id).unwrap();
        assert_eq!(
            s.list_tasks(&p.id)
                .unwrap()
                .iter()
                .find(|t| t.id == second.id)
                .unwrap()
                .status,
            "cancelled"
        );
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0].score_value, 12);
    }
    #[test]
    fn black_score_is_white_relative_without_flattening_mate() {
        let fen = "rnbqkbnr/pppppppp/8/8/8/4P3/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
        let line = parse_line("info depth 8 multipv 1 score mate -3 pv e7e5", fen, &[]).unwrap();
        assert_eq!(line.score_kind, "mate");
        assert_eq!(line.score_value, 3);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_interrupts_uci_handshake_without_waiting_for_timeout() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("silent.sh");
        let received = dir.path().join("uci-received");
        std::fs::write(&engine, format!("#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) touch '{}';;\n esac\ndone\n", received.display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "handshake",
                engine.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let path = s.path.clone();
        let project = p.clone();
        let search = tokio::spawn(async move {
            analyze_position(
                &project,
                &task,
                &Store::open(&path).unwrap(),
                &AtomicBool::new(false),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !received.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        s.cancel(&p.id).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), search)
            .await
            .expect("cancellation must interrupt handshake")
            .unwrap();
        assert_eq!(outcome.unwrap_err(), "interrupted");
        assert_eq!(s.status(&p.id).unwrap(), "cancelled");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn streaming_pv_is_visible_while_running_and_only_bestmove_completes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let release = dir.path().join("release");
        let engine = dir.path().join("stream.sh");
        std::fs::write(&engine, format!("#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go*) echo 'info depth 8 multipv 1 score cp 34 pv e2e4 e7e5'; echo 'info depth 12 score cp 99 pv e2e5'; while [ ! -f '{}' ]; do sleep 0.02; done; echo 'bestmove e2e4';;\n quit) exit 0;;\n esac\ndone\n", release.display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "stream",
                engine.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let other = Store::open(&path).unwrap();
        let project = p.clone();
        let running_task = task.clone();
        let search = tokio::spawn(async move {
            analyze_position(&project, &running_task, &other, &AtomicBool::new(false)).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if !s.results(&p.id).unwrap().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "running");
        assert_eq!(s.progress(&p.id, None, None).unwrap().completed, 0);
        let snapshot = &s.results(&p.id).unwrap()[0].lines[0];
        assert_eq!(
            (
                snapshot.depth,
                snapshot.score_value,
                snapshot.uci_moves.as_slice()
            ),
            (8, 34, &["e2e4".to_string(), "e7e5".to_string()][..])
        );
        std::fs::write(&release, "release").unwrap();
        let lines = search.await.unwrap().unwrap();
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "running");
        s.save(&task.id, &lines).unwrap();
        assert_eq!(s.progress(&p.id, None, None).unwrap().completed, 1);
        let reopened = Store::open(&path).unwrap();
        reopened.recover().unwrap();
        assert_eq!(reopened.list_tasks(&p.id).unwrap()[0].status, "completed");
        assert_eq!(reopened.results(&p.id).unwrap()[0].lines[0].depth, 8);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn finish_current_pause_does_not_cancel_active_uci_search() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stopped = dir.path().join("stopped");
        let engine = dir.path().join("finish.sh");
        std::fs::write(&engine, format!("#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go*) echo 'info depth 12 score cp 34 pv e2e4 e7e5';;\n stop) touch '{}'; echo 'bestmove e2e4';;\n quit) exit 0;;\n esac\ndone\n", stopped.display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "finish current",
                engine.to_str().unwrap(),
                400,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let next = s.queue_root(&p.id, &independent_root_fen(), 1).unwrap();
        s.conn()
            .unwrap()
            .execute("UPDATE deep_tasks SET priority=0 WHERE id=?1", [&next.id])
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let current = s.claim(&p.id).unwrap().unwrap();
        let state = DeepAnalysisState::default();
        let lease = state.acquire(&p.id).unwrap();
        let other = Store::open(&path).unwrap();
        let project = p.clone();
        let task = current.clone();
        let cancelled = lease.cancelled.clone();
        let search =
            tokio::spawn(
                async move { analyze_position(&project, &task, &other, &cancelled).await },
            );
        tokio::time::timeout(Duration::from_secs(3), async {
            while s.results(&p.id).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        state.pause_after_current(&s, &p.id).unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "pausing");
        assert!(
            !stopped.exists(),
            "pause must not stop the current search early"
        );
        let lines = search.await.unwrap().unwrap();
        assert!(stopped.exists(), "normal completion should stop the engine");
        s.save(&current.id, &lines).unwrap();
        s.complete_pause_after_current(&p.id).unwrap();
        assert_eq!(s.status(&p.id).unwrap(), "paused");
        assert_eq!(
            s.list_tasks(&p.id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "completed")
                .count(),
            1
        );
        assert_eq!(
            s.list_tasks(&p.id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "queued")
                .count(),
            1
        );
        assert!(s.claim(&p.id).unwrap().is_none());
        drop(lease);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pause_stops_search_preserves_snapshot_and_blocks_next_task() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stopped = dir.path().join("stopped");
        let engine = dir.path().join("pause.sh");
        std::fs::write(&engine, format!("#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go*) echo 'info depth 8 score mate 3 pv e2e4';;\n stop) touch '{}'; echo 'bestmove e2e4';;\n quit) exit 0;;\n esac\ndone\n", stopped.display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "pause",
                engine.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let next = s.queue_root(&p.id, &independent_root_fen(), 1).unwrap();
        s.conn()
            .unwrap()
            .execute("UPDATE deep_tasks SET priority=0 WHERE id=?1", [&next.id])
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let other = Store::open(&path).unwrap();
        let project = p.clone();
        let search = tokio::spawn(async move {
            analyze_position(&project, &task, &other, &AtomicBool::new(false)).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while s.results(&p.id).unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        s.pause(&p.id).unwrap();
        assert!(search.await.unwrap().is_err());
        assert!(stopped.exists());
        assert_eq!(s.status(&p.id).unwrap(), "paused");
        assert!(s
            .list_tasks(&p.id)
            .unwrap()
            .iter()
            .all(|t| t.status == "queued"));
        assert!(s.claim(&p.id).unwrap().is_none());
        assert_eq!(s.progress(&p.id, None, None).unwrap().completed, 0);
        let reopened = Store::open(&path).unwrap();
        reopened.recover().unwrap();
        assert_eq!(
            reopened.results(&p.id).unwrap()[0].lines[0].score_kind,
            "mate"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn immediate_resume_does_not_let_old_search_continue() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("waiting.sh");
        let started = dir.path().join("started");
        let stopped = dir.path().join("stopped");
        std::fs::write(
            &engine,
            format!(
                "#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go*) touch '{}';;\n stop) touch '{}'; echo 'bestmove e2e4';;\n quit) exit 0;;\n esac\ndone\n",
                started.display(), stopped.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project("race", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let state = DeepAnalysisState::default();
        let lease = state.acquire(&p.id).unwrap();
        let other = Store::open(&s.path).unwrap();
        let project = p.clone();
        let cancelled = lease.cancelled.clone();
        let search =
            tokio::spawn(
                async move { analyze_position(&project, &task, &other, &cancelled).await },
            );
        tokio::time::timeout(Duration::from_secs(3), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        state.pause(&s, &p.id).unwrap();
        // Recreate the status transition before the old worker's next poll.
        s.begin_running(&p.id, "paused").unwrap();
        assert!(
            state.acquire(&p.id).is_err(),
            "new worker must wait for old lease"
        );
        assert!(tokio::time::timeout(Duration::from_secs(2), search)
            .await
            .expect("old search must stop despite running status")
            .unwrap()
            .is_err());
        assert!(stopped.exists(), "old UCI search must receive stop");
        drop(lease);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn engine_exit_after_scored_pv_leaves_task_failed_not_completed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("exit.sh");
        std::fs::write(&engine, "#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go*) echo 'info depth 8 score cp 34 pv e2e4'; exit 0;;\n esac\ndone\n").unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "failure",
                engine.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let err = analyze_position(&p, &task, &s, &AtomicBool::new(false))
            .await
            .unwrap_err();
        s.fail(&task.id, &err).unwrap();
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "failed");
        assert_eq!(s.progress(&p.id, None, None).unwrap().completed, 0);
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0].score_value, 34);
    }
    #[test]
    fn stop_retains_or_clears_pending_without_losing_accepted_tree() {
        for clear_pending in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let s = Store::open(&dir.path().join("db")).unwrap();
            let p = s
                .create_project(
                    "stop",
                    std::env::current_exe().unwrap().to_str().unwrap(),
                    100,
                    1,
                    &start_fen(),
                    &[],
                )
                .unwrap();
            s.begin_running(&p.id, "paused").unwrap();
            let root = s.claim(&p.id).unwrap().unwrap();
            s.save(
                &root.id,
                &[parse_line("info depth 8 score cp 0 pv e2e4", &root.fen, &[]).unwrap()],
            )
            .unwrap();
            let pending = s.queue_root(&p.id, "startpos", 0).unwrap();
            s.stop(&p.id, clear_pending).unwrap();
            assert_eq!(s.status(&p.id).unwrap(), "stopped");
            assert_eq!(s.results(&p.id).unwrap().len(), 1);
            let task = s
                .list_tasks(&p.id)
                .unwrap()
                .into_iter()
                .find(|t| t.id == pending.id)
                .unwrap();
            assert_eq!(
                task.status,
                if clear_pending { "cancelled" } else { "queued" }
            );
            assert!(s.claim(&p.id).unwrap().is_none());
        }
    }

    #[test]
    fn cache_is_separate_from_tree_and_requires_measured_time_depth_and_context() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let engine = std::env::current_exe().unwrap();
        let engine = engine.to_str().unwrap();
        let p = s
            .create_project("cache", engine, 100, 1, &start_fen(), &[])
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        assert!(s.cached(&task, engine, 1, 100, 8).unwrap().is_none());
        let line = parse_line("info depth 8 score cp 0 pv e2e4", &task.fen, &[]).unwrap();
        s.save_cached(&task.id, &[line.clone()], 100, engine)
            .unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.cached(&task, engine, 1, 100, 8).unwrap().unwrap()[0],
            line
        );
        assert!(reopened.cached(&task, engine, 1, 101, 8).unwrap().is_none());
        assert!(reopened.cached(&task, engine, 1, 100, 9).unwrap().is_none());
        assert!(reopened
            .cached(&task, "/different/engine", 1, 100, 8)
            .unwrap()
            .is_none());
        let mut different_history = task.clone();
        different_history.moves.push("g1f3".into());
        assert!(reopened
            .cached(&different_history, engine, 1, 100, 8)
            .unwrap()
            .is_none());
    }

    #[test]
    fn project_creation_persists_unanalysed_root_before_engine_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "root",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        let (nodes, analyzed): (i64, i64) = reopened
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*),count(direct_score_kind) FROM deep_tree_nodes WHERE project_id=?1",
                [&p.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((nodes, analyzed), (1, 0));
        reopened.begin_running(&p.id, "paused").unwrap();
        let task = reopened.claim(&p.id).unwrap().unwrap();
        reopened
            .save(
                &task.id,
                &[parse_line("info depth 8 score cp 0 pv e2e4", &task.fen, &[]).unwrap()],
            )
            .unwrap();
        let roots: i64 = reopened
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM deep_tree_nodes WHERE project_id=?1 AND parent_id IS NULL",
                [&p.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(roots, 1);
    }

    #[test]
    fn manual_prolong_three_chains_at_most_three_positions_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "prolong",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line("info depth 12 score cp 0 pv e2e4 e7e5 g1f3", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        let first: i64 = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND ply=1",
                [&p.id],
                |r| r.get(0),
            )
            .unwrap();
        let manual = s.manual_prolong(&p.id, first, 3).unwrap();
        assert_eq!(manual.status, "queued");
        let original_spec = s.task_execution(&manual.id).unwrap();
        s.enable_automatic(
            &p.id,
            &scheduler::AnalysisSettings {
                analysis_time_ms: 250,
                analysis_depth: 19,
                analysis_operation: AnalysisOperation::TimeOrDepth,
                ..Default::default()
            },
        )
        .unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.claim(&p.id).unwrap().unwrap().id, manual.id);
        let one = s
            .list_tasks(&p.id)
            .unwrap()
            .into_iter()
            .find(|t| t.id == manual.id)
            .unwrap();
        s.save(
            &one.id,
            &[parse_line(
                "info depth 12 score cp 3 pv e7e5 g1f3",
                &one.fen,
                &one.moves,
            )
            .unwrap()],
        )
        .unwrap();
        let two = s.claim(&p.id).unwrap().unwrap();
        let child_spec = s.task_execution(&two.id).unwrap();
        assert_eq!(
            (
                child_spec.engine,
                child_spec.engine_profile,
                child_spec.multipv,
                child_spec.time_ms,
                child_spec.depth,
                child_spec.operation,
                child_spec.cache_policy
            ),
            (
                original_spec.engine.clone(),
                original_spec.engine_profile.clone(),
                original_spec.multipv,
                original_spec.time_ms,
                original_spec.depth,
                original_spec.operation,
                original_spec.cache_policy.clone()
            )
        );
        s.save(
            &two.id,
            &[parse_line("info depth 12 score cp 2 pv g1f3", &two.fen, &two.moves).unwrap()],
        )
        .unwrap();
        let three = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &three.id,
            &[parse_line("info depth 12 score cp 1 pv b8c6", &three.fen, &three.moves).unwrap()],
        )
        .unwrap();
        assert!(s.claim(&p.id).unwrap().is_none());
        assert_eq!(s.list_tasks(&p.id).unwrap().len(), 4);
        let remaining: Vec<i64> = s.conn().unwrap().prepare("SELECT remaining_prolong FROM deep_tasks WHERE project_id=?1 AND strategy='Prolong' ORDER BY rowid").unwrap().query_map([&p.id],|r|r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(remaining, vec![3, 2, 1]);
    }

    #[test]
    fn manual_queue_takes_priority_without_interrupting_running_task() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "priority",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.enable_automatic(&p.id, &scheduler::AnalysisSettings::default())
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        assert_eq!(s.list_tasks(&p.id).unwrap()[0].status, "running");
        s.save(
            &root.id,
            &[parse_line("info depth 8 score cp 0 pv e2e4", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        s.generate_next(&p.id).unwrap();
        let manual = s.queue_root(&p.id, "startpos", 0).unwrap();
        assert_eq!(s.claim(&p.id).unwrap().unwrap().id, manual.id);
    }

    #[test]
    fn exhausted_only_after_stage_cycle_and_no_active_or_pending_task() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "bounded",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let settings = scheduler::AnalysisSettings {
            max_ply: 1,
            ..Default::default()
        };
        s.enable_automatic(&p.id, &settings).unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line("info depth 8 score cp 0 pv e2e4", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        assert!(s.generate_next(&p.id).unwrap());
        assert!(!s.finish(&p.id).unwrap());
        let child = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &child.id,
            &[parse_line("info depth 8 score cp 3 pv e7e5", &child.fen, &child.moves).unwrap()],
        )
        .unwrap();
        assert!(!s.generate_next(&p.id).unwrap());
        assert!(s.scheduler_cycle(&p.id).unwrap() >= 6);
        assert!(s.finish(&p.id).unwrap());
        assert_eq!(s.status(&p.id).unwrap(), "exhausted");
    }

    #[test]
    fn automatic_candidates_and_stage_survive_reopen_without_duplicate_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let engine = std::env::current_exe().unwrap();
        let p = s
            .create_project("auto", engine.to_str().unwrap(), 100, 2, &start_fen(), &[])
            .unwrap();
        let mut settings = scheduler::AnalysisSettings::default();
        settings.max_positions = 1;
        s.enable_automatic(&p.id, &settings).unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[
                parse_line(
                    "info depth 12 multipv 1 score cp 0 pv e2e4 e7e5",
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    "info depth 12 multipv 2 score cp -8 pv d2d4 d7d5",
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        assert!(s.generate_next(&p.id).unwrap());
        let queued = s.list_tasks(&p.id).unwrap();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[1].status, "queued");
        let cycle = s.scheduler_cycle(&p.id).unwrap();
        assert_eq!(cycle, 1);
        drop(s);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.scheduler_cycle(&p.id).unwrap(), cycle);
        assert_eq!(reopened.list_tasks(&p.id).unwrap().len(), 2);
        assert!(reopened.generate_next(&p.id).unwrap());
        assert_eq!(reopened.list_tasks(&p.id).unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_line_slice_reopens_tree_after_three_uci_positions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("fake-uci.sh");
        std::fs::write(&engine,"#!/bin/sh\nturn=white\nwhile read line; do case \"$line\" in\n uci) echo uciok;; isready) echo readyok;;\n position*' b '*) turn=black;; position*' w KQkq - 0 1') turn=white;; position*) turn=whiteother;;\n go\\ infinite) if [ \"$turn\" = black ]; then echo 'info depth 12 multipv 1 score cp 2 pv e7e5'; elif [ \"$turn\" = whiteother ]; then echo 'info depth 12 multipv 1 score cp 1 pv b1c3'; else echo 'info depth 12 multipv 1 score cp 0 pv g1f3'; echo 'info depth 12 multipv 2 score cp -8 pv d2d4'; fi;;\n stop) echo 'bestmove e7e5';; quit) exit 0;; esac; done\n").unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let db = dir.path().join("analysis.sqlite");
        let output = run_debug_analysis(&db, engine.to_str().unwrap(), &start_fen(), 3, 50)
            .await
            .unwrap();
        assert!(output.contains("completed=3"), "{output}");
        assert!(output.contains("nodes="), "{output}");
        let reopened = Store::open(&db).unwrap();
        assert_eq!(reopened.list_projects().unwrap().len(), 1);
        assert_eq!(
            reopened
                .list_tasks(&reopened.list_projects().unwrap()[0].id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "completed")
                .count(),
            3
        );
        let project_id = reopened.list_projects().unwrap()[0].id.clone();
        drop(reopened);
        let resumed = resume_debug_analysis(&db, &project_id, 1).await.unwrap();
        assert!(resumed.contains("completed=4"), "{resumed}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_time_or_depth_stops_on_depth_without_waiting_for_time() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("depth.sh");
        std::fs::write(&engine, "#!/bin/sh\nwhile read line; do case \"$line\" in\n uci) echo uciok;; isready) echo readyok;;\n go\\ infinite) echo 'info depth 8 score cp 12 pv e2e4';;\n stop) echo 'bestmove e2e4';; quit) exit 0;; esac; done\n").unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "mode0",
                engine.to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let settings = scheduler::AnalysisSettings {
            analysis_operation: AnalysisOperation::TimeOrDepth,
            analysis_depth: 8,
            analysis_time_ms: 1000,
            ..Default::default()
        };
        s.enable_automatic(&p.id, &settings).unwrap();
        let requested = s.queue_root(&p.id, "startpos", 0).unwrap();
        s.enable_automatic(
            &p.id,
            &scheduler::AnalysisSettings {
                analysis_operation: AnalysisOperation::TimeAndDepth,
                analysis_time_ms: 1000,
                analysis_depth: 20,
                ..Default::default()
            },
        )
        .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        assert_eq!(task.id, requested.id);
        let result = tokio::time::timeout(
            Duration::from_millis(700),
            analyze_position(&p, &task, &s, &AtomicBool::new(false)),
        )
        .await;
        assert!(
            result.unwrap().is_ok(),
            "depth gate should complete before one second"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scheduler_controls_infinite_search_and_stops_after_scored_minimum() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("infinite.sh");
        let stopped = dir.path().join("stopped");
        std::fs::write(&engine, format!("#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go\\ infinite) echo 'info depth 12 score cp 0 pv e2e4 e7e5';;\n stop) touch '{}'; echo 'bestmove e2e4';;\n quit) exit 0;;\n esac\ndone\n", stopped.display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "infinite",
                engine.to_str().unwrap(),
                50,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let lines = tokio::time::timeout(
            Duration::from_secs(3),
            analyze_position(&p, &task, &s, &AtomicBool::new(false)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(lines[0].score_value, 0);
        assert!(
            stopped.exists(),
            "scheduler must issue stop before accepting a result"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_persists_real_uci_output_and_finishes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let engine = dir.path().join("fake-engine.sh");
        std::fs::write(&engine,"#!/bin/sh\nwhile read line; do\n case \"$line\" in\n uci) echo uciok;;\n isready) echo readyok;;\n go\\ infinite) echo 'info depth 8 multipv 1 score cp 34 pv e2e4 e7e5'; echo 'bestmove e2e4';;\n quit) exit 0;;\n esac\ndone\n").unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project("x", engine.to_str().unwrap(), 1000, 1, &start_fen(), &[])
            .unwrap();
        let task = s.list_tasks(&p.id).unwrap().remove(0);
        s.set_status(&p.id, "running").unwrap();
        s.claim(&p.id).unwrap();
        let lines = analyze_position(&p, &task, &s, &AtomicBool::new(false))
            .await
            .unwrap();
        s.save(&task.id, &lines).unwrap();
        assert_eq!(
            s.results(&p.id).unwrap()[0].lines[0].uci_moves,
            vec!["e2e4", "e7e5"]
        );
    }
    #[test]
    fn rejects_invalid_pv_and_preserves_mate_score_kind() {
        let fen = start_fen();
        assert!(parse_line("info depth 12 score cp 20 pv e2e5", &fen, &[]).is_none());
        assert!(parse_line("info depth 12 pv e2e4", &fen, &[]).is_none());
        let line = parse_line(
            "info depth 12 multipv 1 score mate 3 pv e2e4 e7e5",
            &fen,
            &[],
        )
        .unwrap();
        assert_eq!(line.score_kind, "mate");
        assert_eq!(line.score_value, 3);
        assert_eq!(line.uci_moves, vec!["e2e4", "e7e5"]);
    }
    #[test]
    fn completed_pv_persists_a_legal_position_path_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "tree",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let task = s.claim(&p.id).unwrap().unwrap();
        let line = parse_line(
            "info depth 8 score cp 34 pv e2e4 e7e5",
            &task.fen,
            &task.moves,
        )
        .unwrap();
        s.save(&task.id, &[line]).unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        let conn = reopened.conn().unwrap();
        let mut statement = conn.prepare("SELECT id,parent_id,move_uci,fen,history,direct_score_kind,direct_score_value FROM deep_tree_nodes WHERE project_id=?1 ORDER BY ply,id").unwrap();
        let nodes: Vec<_> = statement
            .query_map([&p.id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i32>>(6)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].1, None);
        assert_eq!(nodes[0].2, None);
        assert_eq!(nodes[0].3, start_fen());
        assert_eq!(nodes[0].4, "[]");
        assert_eq!(nodes[0].5.as_deref(), Some("cp"));
        assert_eq!(nodes[0].6, Some(34));
        assert_eq!(nodes[1].1, Some(nodes[0].0));
        assert_eq!(nodes[1].2.as_deref(), Some("e2e4"));
        assert_eq!(nodes[1].4, "[\"e2e4\"]");
        assert_eq!(nodes[2].1, Some(nodes[1].0));
        assert_eq!(nodes[2].2.as_deref(), Some("e7e5"));
        assert_eq!(nodes[2].4, "[\"e2e4\",\"e7e5\"]");
        assert_eq!(
            nodes[2].3,
            Fen::from_position(
                position_after(&start_fen(), &["e2e4".into(), "e7e5".into()]).unwrap(),
                EnPassantMode::Legal
            )
            .to_string()
        );
    }
    #[test]
    fn minimax_prefers_the_higher_scored_child_for_white_and_keeps_direct_score() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "white",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[
                parse_line(
                    "info depth 8 multipv 1 score cp 30 pv e2e4 e7e5",
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    "info depth 8 multipv 2 score cp 28 pv d2d4 d7d5",
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        for (first_move, response, black_score) in [("e2e4", "e7e5", -10), ("d2d4", "d7d5", -15)] {
            let fen = Fen::from_position(
                position_after(&start_fen(), &[first_move.into()]).unwrap(),
                EnPassantMode::Legal,
            )
            .to_string();
            s.queue(&p.id, Some(&root.id), &fen, 1).unwrap();
            let child = s.claim(&p.id).unwrap().unwrap();
            assert_eq!(child.fen, fen);
            let line = parse_line(
                &format!("info depth 8 score cp {black_score} pv {response}"),
                &child.fen,
                &[],
            )
            .unwrap();
            s.save(&child.id, &[line]).unwrap();
        }
        drop(s);
        let s = Store::open(&path).unwrap();
        let scores: (i32, i32) = s.conn().unwrap().query_row(
            "SELECT direct_score_value,minimax_score_value FROM deep_tree_nodes WHERE analyzed_task_id=?1",
            [&root.id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(scores, (30, 15));
    }
    #[test]
    fn black_minimax_backs_up_through_a_reused_pv_position() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "black",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[
                parse_line(
                    "info depth 8 multipv 1 score cp 30 pv e2e4 e7e5",
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    "info depth 8 multipv 2 score cp 28 pv d2d4 d7d5",
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let e4_fen = Fen::from_position(
            position_after(&start_fen(), &["e2e4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        s.queue(&p.id, Some(&root.id), &e4_fen, 1).unwrap();
        let e4_task = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &e4_task.id,
            &[
                parse_line("info depth 8 multipv 1 score cp -20 pv e7e5", &e4_fen, &[]).unwrap(),
                parse_line("info depth 8 multipv 2 score cp -25 pv e7e6", &e4_fen, &[]).unwrap(),
            ],
        )
        .unwrap();
        for (reply, white_score) in [("e7e5", 5), ("e7e6", 25)] {
            let fen = Fen::from_position(
                position_after(&start_fen(), &["e2e4".into(), reply.into()]).unwrap(),
                EnPassantMode::Legal,
            )
            .to_string();
            s.queue(&p.id, Some(&e4_task.id), &fen, 2).unwrap();
            let task = s.claim(&p.id).unwrap().unwrap();
            let line = parse_line(
                &format!("info depth 8 score cp {white_score} pv g1f3"),
                &fen,
                &[],
            )
            .unwrap();
            s.save(&task.id, &[line]).unwrap();
        }
        let conn = s.conn().unwrap();
        let e4: (i32, i32) = conn.query_row("SELECT direct_score_value,minimax_score_value FROM deep_tree_nodes WHERE analyzed_task_id=?1", [&e4_task.id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        let root_score: i32 = conn
            .query_row(
                "SELECT minimax_score_value FROM deep_tree_nodes WHERE analyzed_task_id=?1",
                [&root.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(e4, (20, 5));
        assert_eq!(root_score, 5);
    }
    #[test]
    fn accepted_position_score_updates_both_transposed_paths_and_their_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("shared.sqlite")).unwrap();
        let project = store
            .create_project(
                "shared",
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
        let lines = paths
            .iter()
            .enumerate()
            .map(|(index, moves)| {
                parse_line(
                    &format!("info depth 8 multipv {} score cp 20 pv {moves}", index + 1),
                    &root.fen,
                    &[],
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        store.save(&root.id, &lines).unwrap();
        let histories: Vec<Vec<String>> = paths
            .iter()
            .map(|path| path.split_whitespace().map(str::to_owned).collect())
            .collect();
        let conn = store.conn().unwrap();
        let leaves: Vec<i64> = histories
            .iter()
            .map(|history| {
                conn.query_row(
                    "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                    params![project.id, serde_json::to_string(history).unwrap()],
                    |r| r.get(0),
                )
                .unwrap()
            })
            .collect();
        let shared_position: i64 = conn
            .query_row(
                "SELECT position_id FROM deep_tree_nodes WHERE id=?1",
                [leaves[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            shared_position,
            conn.query_row(
                "SELECT position_id FROM deep_tree_nodes WHERE id=?1",
                [leaves[1]],
                |r| r.get::<_, i64>(0)
            )
            .unwrap()
        );
        drop(conn);
        let fen = Fen::from_position(
            position_after(&root.fen, &histories[0]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let task = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, leaves[0])
            .unwrap();
        store.claim(&project.id).unwrap();
        store
            .save(
                &task.id,
                &[parse_line("info depth 12 score cp 37 pv e2e4", &fen, &[]).unwrap()],
            )
            .unwrap();
        let views = store.read_tree(&project.id).unwrap();
        assert_eq!(
            store
                .read_project_summary(&project.id)
                .unwrap()
                .analysed_node_count,
            3
        );
        let conn = store.conn().unwrap();
        for leaf in leaves {
            let view = views
                .iter()
                .find(|node| node.id == leaf.to_string())
                .unwrap();
            assert_eq!(
                view.raw_evaluation,
                read::EvaluationView::Centipawns {
                    value: 37,
                    display: "+0.37".into()
                }
            );
            assert_eq!(
                view.minimax_evaluation,
                read::EvaluationView::Centipawns {
                    value: 37,
                    display: "+0.37".into()
                }
            );
            let parent_minimax: Option<i32> = conn
                .query_row(
                    "SELECT p.minimax_score_value FROM deep_tree_nodes n
                 JOIN deep_tree_nodes p ON p.id=n.parent_id WHERE n.id=?1",
                    [leaf],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(parent_minimax, Some(37));
        }
    }

    #[test]
    fn equivalent_pending_transposed_requests_share_one_task_and_retain_both_interests() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("coalesced.sqlite")).unwrap();
        let project = store
            .create_project(
                "coalesce",
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
        let lines: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                parse_line(
                    &format!("info depth 8 multipv {} score cp 20 pv {path}", i + 1),
                    &root.fen,
                    &[],
                )
                .unwrap()
            })
            .collect();
        store.save(&root.id, &lines).unwrap();
        let conn = store.conn().unwrap();
        let ids: Vec<i64> = paths
            .iter()
            .map(|path| {
                conn.query_row(
                    "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                    params![
                        project.id,
                        serde_json::to_string(&path.split_whitespace().collect::<Vec<_>>())
                            .unwrap()
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
        let second = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[1])
            .unwrap();
        assert_eq!(first.id, second.id);
        let conn = store.conn().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM deep_tasks WHERE project_id=?1 AND status='queued'",
                [&project.id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let interests: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT node_id FROM deep_task_interests WHERE task_id=?1 ORDER BY node_id",
                )
                .unwrap();
            stmt.query_map([&first.id], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(interests, ids);
    }

    #[test]
    fn stronger_pending_transposed_request_upgrades_search_without_mutating_older_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("stronger.sqlite")).unwrap();
        let project = store
            .create_project(
                "upgrade",
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
                            &format!("info depth 8 multipv {} score cp 20 pv {p}", i + 1),
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
        let mut settings = scheduler::AnalysisSettings {
            analysis_time_ms: 100,
            analysis_depth: 8,
            engine_id: project.engine.clone(),
            ..Default::default()
        };
        store.enable_automatic(&project.id, &settings).unwrap();
        let first = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[0])
            .unwrap();
        let initial = store.task_execution(&first.id).unwrap();
        assert_eq!((initial.time_ms, initial.depth), (100, 8));
        settings.analysis_time_ms = 250;
        settings.analysis_depth = 14;
        store.enable_automatic(&project.id, &settings).unwrap();
        let second = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[1])
            .unwrap();
        assert_eq!(first.id, second.id);
        let upgraded = store.task_execution(&first.id).unwrap();
        assert_eq!((upgraded.time_ms, upgraded.depth), (250, 14));
        assert_eq!(
            store
                .list_tasks(&project.id)
                .unwrap()
                .iter()
                .filter(|t| t.status == "queued")
                .count(),
            1
        );
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM deep_task_interests WHERE task_id=?1",
                    [&first.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn accepted_quality_reuses_one_position_search_without_queueing_a_second_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("reused.sqlite")).unwrap();
        let project = store
            .create_project(
                "reuse",
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
                            &format!("info depth 8 multipv {} score cp 20 pv {p}", i + 1),
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
        let lines = [
            parse_line("info depth 16 multipv 1 score cp 51 pv e2e4", &fen, &[]).unwrap(),
            parse_line("info depth 16 multipv 2 score cp 42 pv d2d4", &fen, &[]).unwrap(),
        ];
        store
            .save_cached(&first.id, &lines, 200, &project.engine)
            .unwrap();
        let reused = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[1])
            .unwrap();
        assert_eq!(reused.id, first.id);
        assert_eq!(reused.status, "completed");
        assert_eq!(store.list_tasks(&project.id).unwrap().len(), 2);
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM deep_task_interests WHERE task_id=?1",
                    [&first.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        let views = store.read_tree(&project.id).unwrap();
        for id in ids {
            assert_eq!(
                views
                    .iter()
                    .find(|n| n.id == id.to_string())
                    .unwrap()
                    .raw_evaluation,
                read::EvaluationView::Centipawns {
                    value: 51,
                    display: "+0.51".into()
                }
            );
        }
    }

    #[test]
    fn running_position_search_accepts_a_second_path_and_finishes_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("running-share.sqlite")).unwrap();
        let project = store
            .create_project(
                "running",
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
                            &format!("info depth 8 multipv {} score cp 20 pv {p}", i + 1),
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
        assert_eq!(store.claim(&project.id).unwrap().unwrap().id, first.id);
        let second = store
            .queue_at_occurrence(&project.id, &root.id, &fen, 4, ids[1])
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.status, "running");
        assert_eq!(store.list_tasks(&project.id).unwrap().len(), 2);
        store
            .save(
                &first.id,
                &[parse_line("info depth 12 score mate 3 pv e2e4", &fen, &[]).unwrap()],
            )
            .unwrap();
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM deep_position_evaluations WHERE task_id=?1",
                    [&first.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM deep_task_interests WHERE task_id=?1",
                    [&first.id],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
        let views = store.read_tree(&project.id).unwrap();
        for id in ids {
            assert_eq!(
                views
                    .iter()
                    .find(|n| n.id == id.to_string())
                    .unwrap()
                    .raw_evaluation,
                read::EvaluationView::Mate {
                    distance: 3,
                    display: "M+3".into()
                }
            );
        }
    }

    #[test]
    fn transposed_pvs_need_an_exact_occurrence_to_queue_a_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "transpose",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        let a = "g1f3 g8f6 b1c3 b8c6";
        let b = "b1c3 b8c6 g1f3 g8f6";
        s.save(
            &root.id,
            &[
                parse_line(
                    &format!("info depth 8 multipv 1 score cp 20 pv {a}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    &format!("info depth 8 multipv 2 score cp 30 pv {b}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let path_a: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
        let path_b: Vec<String> = b.split_whitespace().map(str::to_owned).collect();
        let fen_a = Fen::from_position(
            position_after(&root.fen, &path_a).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let fen_b = Fen::from_position(
            position_after(&root.fen, &path_b).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        assert_eq!(fen_a, fen_b);
        let err = s.queue(&p.id, Some(&root.id), &fen_a, 4).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert_eq!(s.list_tasks(&p.id).unwrap().len(), 1);
    }
    #[test]
    fn explicit_transposed_occurrence_attaches_only_to_the_chosen_move_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "transpose",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        let a = "g1f3 g8f6 b1c3 b8c6";
        let b = "b1c3 b8c6 g1f3 g8f6";
        s.save(
            &root.id,
            &[
                parse_line(
                    &format!("info depth 8 multipv 1 score cp 20 pv {a}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    &format!("info depth 8 multipv 2 score cp 30 pv {b}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let path_a: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
        let path_b: Vec<String> = b.split_whitespace().map(str::to_owned).collect();
        let fen = Fen::from_position(
            position_after(&root.fen, &path_b).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let conn = s.conn().unwrap();
        let occurrence_b: i64 = conn
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                params![p.id, serde_json::to_string(&path_b).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        let occurrence_a: i64 = conn
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                params![p.id, serde_json::to_string(&path_a).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let child = s
            .queue_at_occurrence(&p.id, &root.id, &fen, 4, occurrence_b)
            .unwrap();
        let claimed = s.claim(&p.id).unwrap().unwrap();
        assert_eq!(claimed.id, child.id);
        let line = parse_line("info depth 8 score cp 55 pv e2e4", &claimed.fen, &[]).unwrap();
        s.save(&child.id, &[line]).unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        let conn = reopened.conn().unwrap();
        let task_occurrence: i64 = conn
            .query_row(
                "SELECT occurrence_id FROM deep_tasks WHERE id=?1",
                [&child.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(task_occurrence, occurrence_b);
        let scored: i64 = conn
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE analyzed_task_id=?1",
                [&child.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scored, occurrence_b);
        let wrong_branch_score: Option<i32> = conn
            .query_row(
                "SELECT direct_score_value FROM deep_tree_nodes WHERE id=?1",
                [occurrence_a],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(wrong_branch_score, None);
        assert_eq!(reopened.results(&p.id).unwrap().len(), 2);
    }
    #[test]
    fn duplicate_analysis_keeps_both_task_links_and_stable_latest_score_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "duplicates",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line("info depth 8 score cp 30 pv e2e4", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        let fen = Fen::from_position(
            position_after(&root.fen, &["e2e4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let mut task_ids = vec![];
        for (score, reply) in [(-10, "e7e5"), (-20, "e7e6")] {
            let task = s.queue(&p.id, Some(&root.id), &fen, 1).unwrap();
            s.claim(&p.id).unwrap();
            s.save(
                &task.id,
                &[parse_line(
                    &format!("info depth 8 score cp {score} pv {reply}"),
                    &fen,
                    &[],
                )
                .unwrap()],
            )
            .unwrap();
            task_ids.push(task.id);
        }
        drop(s);
        for _ in 0..2 {
            let reopened = Store::open(&path).unwrap();
            let conn = reopened.conn().unwrap();
            let links: Vec<(String, i64)> = {
                let mut stmt = conn.prepare("SELECT task_id,node_id FROM deep_tree_analyses WHERE task_id IN (?1,?2) ORDER BY task_id").unwrap();
                stmt.query_map(params![task_ids[0], task_ids[1]], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
            };
            assert_eq!(links.len(), 2);
            assert_eq!(links[0].1, links[1].1);
            let score: i32 = conn
                .query_row(
                    "SELECT direct_score_value FROM deep_tree_nodes WHERE id=?1",
                    [links[0].1],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(score, 20);
            let score_source: String = conn
                .query_row(
                    "SELECT analyzed_task_id FROM deep_tree_nodes WHERE id=?1",
                    [links[0].1],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(score_source, task_ids[1]);
            let results = reopened.results(&p.id).unwrap();
            assert_eq!(results.len(), 3);
            assert_eq!(results[1].lines[0].score_value, 10);
            assert_eq!(results[2].lines[0].score_value, 20);
        }
    }
    #[test]
    fn ambiguous_legacy_continuation_is_deferred_without_losing_results_or_blocking_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "legacy",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        let a = "g1f3 g8f6 b1c3 b8c6";
        let b = "b1c3 b8c6 g1f3 g8f6";
        s.save(
            &root.id,
            &[
                parse_line(
                    &format!("info depth 8 multipv 1 score cp 20 pv {a}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    &format!("info depth 8 multipv 2 score cp 30 pv {b}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let path_a: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
        let fen = Fen::from_position(
            position_after(&root.fen, &path_a).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let child_id = uuid::Uuid::new_v4().to_string();
        let conn = s.conn().unwrap();
        conn.execute(
            "INSERT INTO deep_tasks(id,project_id,parent_task_id,fen,moves,ply,status)
            VALUES(?1,?2,?3,?4,'[]',4,'completed')",
            params![child_id, p.id, root.id, fen],
        )
        .unwrap();
        conn.execute("INSERT INTO deep_results(task_id,multipv,depth,score_kind,score_value,score_perspective,uci_moves)
            VALUES(?1,1,8,'cp',77,'white','[\"e2e4\"]')", [&child_id]).unwrap();
        conn.execute(
            "UPDATE deep_tasks SET occurrence_id=NULL WHERE id=?1",
            [&root.id],
        )
        .unwrap();
        conn.execute_batch("DROP TABLE deep_tree_analyses; DROP TABLE deep_tree_nodes;")
            .unwrap();
        drop(conn);
        drop(s);
        for _ in 0..2 {
            let reopened =
                Store::open(&path).expect("ambiguous legacy data must not block project open");
            let conn = reopened.conn().unwrap();
            let reason: String = conn
                .query_row(
                    "SELECT reason FROM deep_tree_backfill_deferred WHERE task_id=?1",
                    [&child_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(reason.contains("ambiguous"), "{reason}");
            let links: i64 = conn
                .query_row(
                    "SELECT count(*) FROM deep_tree_analyses WHERE task_id=?1",
                    [&child_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(links, 0);
            let nodes: i64 = conn
                .query_row(
                    "SELECT count(*) FROM deep_tree_nodes WHERE project_id=?1",
                    [&p.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(nodes, 9);
            let results = reopened.results(&p.id).unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[1].lines[0].score_value, 77);
            assert_eq!(reopened.list_tasks(&p.id).unwrap()[1].status, "completed");
        }
    }
    #[test]
    fn migration_does_not_keep_an_old_guessed_transposition_link_or_score() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "old-tree",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        let a = "g1f3 g8f6 b1c3 b8c6";
        let b = "b1c3 b8c6 g1f3 g8f6";
        s.save(
            &root.id,
            &[
                parse_line(
                    &format!("info depth 8 multipv 1 score cp 20 pv {a}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
                parse_line(
                    &format!("info depth 8 multipv 2 score cp 30 pv {b}"),
                    &root.fen,
                    &[],
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let path_a: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
        let fen = Fen::from_position(
            position_after(&root.fen, &path_a).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let child_id = uuid::Uuid::new_v4().to_string();
        let conn = s.conn().unwrap();
        conn.execute(
            "INSERT INTO deep_tasks(id,project_id,parent_task_id,fen,moves,ply,status)
            VALUES(?1,?2,?3,?4,'[]',4,'completed')",
            params![child_id, p.id, root.id, fen],
        )
        .unwrap();
        conn.execute("INSERT INTO deep_results(task_id,multipv,depth,score_kind,score_value,score_perspective,uci_moves)
            VALUES(?1,1,8,'cp',77,'white','[\"e2e4\"]')", [&child_id]).unwrap();
        let a_id: i64 = conn
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                params![p.id, serde_json::to_string(&path_a).unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute("UPDATE deep_tree_nodes SET analyzed_task_id=?1,direct_score_kind='cp',direct_score_value=77,
            minimax_score_kind='cp',minimax_score_value=77 WHERE id=?2", params![child_id,a_id]).unwrap();
        conn.execute_batch(
            "DROP TABLE deep_tree_analyses; DROP TABLE deep_tree_backfill_deferred;",
        )
        .unwrap();
        drop(conn);
        drop(s);
        let reopened = Store::open(&path).unwrap();
        let conn = reopened.conn().unwrap();
        let (old_pointer, old_score): (Option<String>, Option<i32>) = conn
            .query_row(
                "SELECT analyzed_task_id,direct_score_value FROM deep_tree_nodes WHERE id=?1",
                [a_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((old_pointer, old_score), (None, None));
        let root_minimax: i32 = conn
            .query_row(
                "SELECT minimax_score_value FROM deep_tree_nodes WHERE analyzed_task_id=?1",
                [&root.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(root_minimax, 20);
        assert_eq!(reopened.results(&p.id).unwrap()[1].lines[0].score_value, 77);
    }
    #[test]
    fn legacy_duplicate_results_relink_to_one_occurrence_once_without_oscillating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "legacy-duplicates",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line("info depth 8 score cp 30 pv e2e4", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        let fen = Fen::from_position(
            position_after(&root.fen, &["e2e4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let mut task_ids = Vec::new();
        for (score, move_uci) in [(-10, "e7e5"), (-20, "e7e6")] {
            let child = s.queue(&p.id, Some(&root.id), &fen, 1).unwrap();
            s.claim(&p.id).unwrap();
            s.save(
                &child.id,
                &[parse_line(
                    &format!("info depth 8 score cp {score} pv {move_uci}"),
                    &fen,
                    &[],
                )
                .unwrap()],
            )
            .unwrap();
            task_ids.push(child.id);
        }
        let conn = s.conn().unwrap();
        let node_id: i64 = conn
            .query_row(
                "SELECT node_id FROM deep_tree_analyses WHERE task_id=?1",
                [&task_ids[0]],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE deep_tree_nodes SET analyzed_task_id=?1 WHERE id=?2",
            params![task_ids[1], node_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE deep_tasks SET occurrence_id=NULL WHERE id IN (?1,?2)",
            params![task_ids[0], task_ids[1]],
        )
        .unwrap();
        conn.execute_batch("DROP TABLE deep_tree_analyses;")
            .unwrap();
        drop(conn);
        drop(s);
        for _ in 0..2 {
            let reopened = Store::open(&path).unwrap();
            let conn = reopened.conn().unwrap();
            for task_id in &task_ids {
                let (mapped_node, task_occurrence): (i64, i64) = conn
                    .query_row(
                        "SELECT a.node_id,t.occurrence_id FROM deep_tree_analyses a
                     JOIN deep_tasks t ON t.id=a.task_id WHERE a.task_id=?1",
                        [task_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap();
                assert_eq!((mapped_node, task_occurrence), (node_id, node_id));
            }
            let score: i32 = conn
                .query_row(
                    "SELECT direct_score_value FROM deep_tree_nodes WHERE id=?1",
                    [node_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(score, 20);
            assert_eq!(reopened.results(&p.id).unwrap().len(), 3);
        }
    }
    #[test]
    fn explicit_occurrence_must_follow_the_named_parent_tasks_own_pv() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "branch",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line(
                "info depth 8 score cp 20 pv g1f3 g8f6 b1c3 b8c6",
                &root.fen,
                &[],
            )
            .unwrap()],
        )
        .unwrap();
        let first_fen = Fen::from_position(
            position_after(&root.fen, &["g1f3".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let child = s.queue(&p.id, Some(&root.id), &first_fen, 1).unwrap();
        s.claim(&p.id).unwrap();
        s.save(
            &child.id,
            &[parse_line(
                "info depth 8 score cp -10 pv b8c6 b1c3 g8f6",
                &first_fen,
                &[],
            )
            .unwrap()],
        )
        .unwrap();
        let other_history = vec!["g1f3", "b8c6", "b1c3", "g8f6"];
        let fen = Fen::from_position(
            position_after(
                &root.fen,
                &other_history
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let node: i64 = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history=?2",
                params![p.id, serde_json::to_string(&other_history).unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let err = s
            .queue_at_occurrence(&p.id, &root.id, &fen, 4, node)
            .unwrap_err();
        assert!(err.contains("parent PV"), "{err}");
        assert_eq!(s.list_tasks(&p.id).unwrap().len(), 2);
    }
    #[test]
    fn relinking_an_older_orphan_does_not_replace_a_newer_direct_score() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "partial",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line("info depth 8 score cp 30 pv e2e4", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        let fen = Fen::from_position(
            position_after(&root.fen, &["e2e4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let older = s.queue(&p.id, Some(&root.id), &fen, 1).unwrap();
        s.claim(&p.id).unwrap();
        s.save(
            &older.id,
            &[parse_line("info depth 8 score cp -10 pv e7e5", &fen, &[]).unwrap()],
        )
        .unwrap();
        let newer = s.queue(&p.id, Some(&root.id), &fen, 1).unwrap();
        s.claim(&p.id).unwrap();
        s.save(
            &newer.id,
            &[parse_line("info depth 8 score cp -20 pv e7e6", &fen, &[]).unwrap()],
        )
        .unwrap();
        s.conn()
            .unwrap()
            .execute(
                "DELETE FROM deep_tree_analyses WHERE task_id=?1",
                [&older.id],
            )
            .unwrap();
        drop(s);
        let reopened = Store::open(&path).unwrap();
        let conn = reopened.conn().unwrap();
        let node_id: i64 = conn
            .query_row(
                "SELECT node_id FROM deep_tree_analyses WHERE task_id=?1",
                [&newer.id],
                |r| r.get(0),
            )
            .unwrap();
        let (links, score): (i64,i32) = conn.query_row(
            "SELECT (SELECT count(*) FROM deep_tree_analyses WHERE node_id=?1),direct_score_value
             FROM deep_tree_nodes WHERE id=?1", [node_id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((links, score), (2, 20));
        assert_eq!(reopened.results(&p.id).unwrap().len(), 3);
    }
    #[test]
    fn opening_a_legacy_project_backfills_tree_without_changing_saved_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "legacy",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let task = s.list_tasks(&p.id).unwrap().remove(0);
        let conn = s.conn().unwrap();
        conn.execute(
            "UPDATE deep_tasks SET status='completed' WHERE id=?1",
            [&task.id],
        )
        .unwrap();
        conn.execute("INSERT INTO deep_results(task_id,multipv,depth,score_kind,score_value,score_perspective,uci_moves) VALUES(?1,1,8,'cp',42,'white','[\"e2e4\"]')", [&task.id]).unwrap();
        conn.execute_batch("DROP TABLE deep_tree_nodes").unwrap();
        drop(conn);
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(s.results(&p.id).unwrap()[0].lines[0].score_value, 42);
        let count: i32 = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM deep_tree_nodes WHERE project_id=?1",
                [&p.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }
    #[test]
    fn unambiguous_legacy_child_replaces_a_wrong_cached_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.sqlite");
        let s = Store::open(&path).unwrap();
        let p = s
            .create_project(
                "legacy",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                2,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[
                parse_line("info depth 8 multipv 1 score cp 20 pv e2e4", &root.fen, &[]).unwrap(),
                parse_line("info depth 8 multipv 2 score cp 10 pv d2d4", &root.fen, &[]).unwrap(),
            ],
        )
        .unwrap();
        let fen = Fen::from_position(
            position_after(&root.fen, &["e2e4".into()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let child_id = uuid::Uuid::new_v4().to_string();
        let conn = s.conn().unwrap();
        let intended: i64 = conn
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history='[\"e2e4\"]'",
                [&p.id],
                |r| r.get(0),
            )
            .unwrap();
        let wrong: i64 = conn
            .query_row(
                "SELECT id FROM deep_tree_nodes WHERE project_id=?1 AND history='[\"d2d4\"]'",
                [&p.id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute("INSERT INTO deep_tasks(id,project_id,parent_task_id,fen,moves,ply,status) VALUES(?1,?2,?3,?4,'[]',1,'completed')", params![child_id, p.id, root.id, fen]).unwrap();
        conn.execute("INSERT INTO deep_results(task_id,multipv,depth,score_kind,score_value,score_perspective,uci_moves) VALUES(?1,1,8,'cp',77,'white','[\"e7e5\"]')", [&child_id]).unwrap();
        conn.execute("UPDATE deep_tree_nodes SET analyzed_task_id=?1,direct_score_kind='cp',direct_score_value=77,minimax_score_kind='cp',minimax_score_value=77 WHERE id=?2", params![child_id, wrong]).unwrap();
        drop(conn);
        drop(s);
        for _ in 0..2 {
            let reopened = Store::open(&path).unwrap();
            let conn = reopened.conn().unwrap();
            let linked: i64 = conn
                .query_row(
                    "SELECT node_id FROM deep_tree_analyses WHERE task_id=?1",
                    [&child_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(linked, intended);
            let stale: (Option<String>, Option<i32>) = conn
                .query_row(
                    "SELECT analyzed_task_id,direct_score_value FROM deep_tree_nodes WHERE id=?1",
                    [wrong],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(stale, (None, None));
            let deferred: i64 = conn
                .query_row(
                    "SELECT count(*) FROM deep_tree_backfill_deferred WHERE task_id=?1",
                    [&child_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(deferred, 0);
            assert_eq!(reopened.results(&p.id).unwrap()[1].lines[0].score_value, 77);
        }
    }
    #[test]
    fn saving_a_pv_near_max_ply_returns_an_error_without_partial_tree_rows() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("db")).unwrap();
        let p = s
            .create_project(
                "overflow",
                std::env::current_exe().unwrap().to_str().unwrap(),
                1000,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        s.begin_running(&p.id, "paused").unwrap();
        let root = s.claim(&p.id).unwrap().unwrap();
        s.save(
            &root.id,
            &[parse_line("info depth 8 score cp 20 pv e2e4", &root.fen, &[]).unwrap()],
        )
        .unwrap();
        let extreme = s.queue_root(&p.id, &start_fen(), u32::MAX).unwrap();
        assert_eq!(s.claim(&p.id).unwrap().unwrap().id, extreme.id);
        let line = parse_line("info depth 8 score cp 25 pv e2e4", &extreme.fen, &[]).unwrap();
        let err = s.save(&extreme.id, &[line]).unwrap_err();
        assert!(err.contains("ply"), "{err}");
        assert_eq!(
            s.list_tasks(&p.id).unwrap().last().unwrap().status,
            "running"
        );
        assert_eq!(s.results(&p.id).unwrap().len(), 1);
        let count: i64 = s
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM deep_tree_nodes WHERE analyzed_task_id=?1",
                [&extreme.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
