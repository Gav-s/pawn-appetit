//! Project-local canonical positions. Occurrences keep their own histories and minimax values.
use super::position;
use rusqlite::{params, Connection, Transaction};
use shakmaty::{
    fen::Epd,
    zobrist::{Zobrist128, ZobristHash},
    EnPassantMode, Position,
};

struct Identity {
    key: String,
    verification: String,
    halfmove_clock: u32,
}

fn identity(fen: &str) -> Result<Identity, String> {
    let board = position(fen)?;
    let halfmove_clock = board.halfmoves();
    let hash: Zobrist128 = board.zobrist_hash(EnPassantMode::Legal);
    Ok(Identity {
        key: format!("{:032x}:{halfmove_clock}", hash.0),
        verification: Epd::from_position(board, EnPassantMode::Legal).to_string(),
        halfmove_clock,
    })
}

pub(super) fn get_or_create(
    tx: &Transaction<'_>,
    project_id: &str,
    fen: &str,
) -> Result<i64, String> {
    let identity = identity(fen)?;
    let mut stmt = tx
        .prepare("SELECT id,canonical_epd,halfmove_clock FROM deep_position_records WHERE project_id=?1 AND position_key=?2")
        .map_err(|e| e.to_string())?;
    let matches = stmt
        .query_map(params![project_id, identity.key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u32>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(stmt);
    if let Some((id, _, _)) = matches
        .iter()
        .find(|(_, epd, clock)| epd == &identity.verification && *clock == identity.halfmove_clock)
    {
        return Ok(*id);
    }
    // A hash collision is not equality: the full canonical representation is also unique.
    tx.execute(
        "INSERT INTO deep_position_records(project_id,position_key,canonical_epd,halfmove_clock)
         VALUES(?1,?2,?3,?4)",
        params![
            project_id,
            identity.key,
            identity.verification,
            identity.halfmove_clock
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(tx.last_insert_rowid())
}

pub(super) fn link_node(tx: &Transaction<'_>, node_id: i64) -> Result<i64, String> {
    let (project_id, fen, existing): (String, String, Option<i64>) = tx
        .query_row(
            "SELECT project_id,fen,position_id FROM deep_tree_nodes WHERE id=?1",
            [node_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|e| e.to_string())?;
    let position_id = get_or_create(tx, &project_id, &fen)?;
    if existing.is_some_and(|id| id != position_id) {
        return Err(format!(
            "analysis node {node_id} has an inconsistent position link"
        ));
    }
    tx.execute(
        "UPDATE deep_tree_nodes SET position_id=?2 WHERE id=?1",
        params![node_id, position_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(position_id)
}

/// Add only the position link; preserve every occurrence, result, queue entry and scheduler row.
/// The schema and backfill commit together, including the version marker.
pub(super) fn migrate(conn: &mut Connection) -> Result<(), String> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS deep_position_records (
            id INTEGER PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES deep_projects(id) ON DELETE CASCADE,
            position_key TEXT NOT NULL,
            canonical_epd TEXT NOT NULL,
            halfmove_clock INTEGER NOT NULL CHECK(halfmove_clock>=0),
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(project_id,canonical_epd,halfmove_clock)
         );
         CREATE INDEX IF NOT EXISTS deep_position_lookup ON deep_position_records(project_id,position_key);",
    )
    .map_err(|e| e.to_string())?;
    let has_link = tx
        .prepare("PRAGMA table_info(deep_tree_nodes)")
        .map_err(|e| e.to_string())?
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|column| column == "position_id");
    if !has_link {
        tx.execute_batch("ALTER TABLE deep_tree_nodes ADD COLUMN position_id INTEGER REFERENCES deep_position_records(id)")
            .map_err(|e| e.to_string())?;
    }
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS deep_tree_nodes_position ON deep_tree_nodes(position_id)",
    )
    .map_err(|e| e.to_string())?;
    let missing = {
        let mut query = tx
            .prepare("SELECT id FROM deep_tree_nodes WHERE position_id IS NULL ORDER BY id")
            .map_err(|e| e.to_string())?;
        let ids = query
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        ids
    };
    for id in missing {
        link_node(&tx, id)?;
    }
    tx.execute_batch("PRAGMA user_version=4")
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deep_analysis::{position_after, start_fen, Store};
    use shakmaty::{fen::Fen, EnPassantMode};

    #[test]
    fn canonical_identity_distinguishes_chess_state_and_fifty_move_clock() {
        let start = start_fen();
        let alternate_order = [
            ["g1f3", "g8f6", "b1c3", "b8c6"],
            ["b1c3", "b8c6", "g1f3", "g8f6"],
        ];
        let identities: Vec<_> = alternate_order
            .into_iter()
            .map(|sequence| {
                let board = position_after(&start, &sequence.map(str::to_string)).unwrap();
                identity(&Fen::from_position(board, EnPassantMode::Legal).to_string()).unwrap()
            })
            .collect();
        assert_eq!(identities[0].key, identities[1].key);
        assert_eq!(identities[0].verification, identities[1].verification);
        assert_eq!(identities[0].halfmove_clock, identities[1].halfmove_clock);

        let opposite_turn = identity(&start.replace(" w ", " b ")).unwrap();
        let different_castles = identity(&start.replace(" KQkq ", " - ")).unwrap();
        let normal = identity(&start).unwrap();
        assert_ne!(normal.key, opposite_turn.key);
        assert_ne!(normal.key, different_castles.key);
        let advanced_clock = identity(&start.replace(" - 0 1", " - 20 8")).unwrap();
        assert_ne!(normal.key, advanced_clock.key);
        let later_move_number = identity(&start.replace(" - 0 1", " - 0 8")).unwrap();
        assert_eq!(normal.key, later_move_number.key);

        let legal_ep = "4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1";
        let no_ep = "4k3/8/8/3pP3/8/8/8/4K3 w - - 0 1";
        assert_ne!(
            identity(legal_ep).unwrap().key,
            identity(no_ep).unwrap().key
        );
    }

    #[test]
    fn hash_collision_does_not_merge_different_canonical_positions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("collision.sqlite")).unwrap();
        let project = store
            .create_project(
                "collision",
                std::env::current_exe().unwrap().to_str().unwrap(),
                100,
                1,
                &start_fen(),
                &[],
            )
            .unwrap();
        let target_fen = Fen::from_position(
            position_after(&start_fen(), &["e2e4".to_string()]).unwrap(),
            EnPassantMode::Legal,
        )
        .to_string();
        let target = identity(&target_fen).unwrap();
        let mut conn = store.conn().unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute("INSERT INTO deep_position_records(project_id,position_key,canonical_epd,halfmove_clock)
            VALUES(?1,?2,'unrelated',?3)", params![project.id,target.key,target.halfmove_clock]).unwrap();
        let collision = tx.last_insert_rowid();
        let actual = get_or_create(&tx, &project.id, &target_fen).unwrap();
        assert_ne!(collision, actual);
        assert_eq!(
            actual,
            get_or_create(&tx, &project.id, &target_fen).unwrap()
        );
        tx.commit().unwrap();
    }
}
