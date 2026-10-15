//! Independent CLI smoke test for the existing Rust analysis core.
use std::path::Path;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.as_slice() {
        [_, command, db, project, count] if command == "resume" =>
            match count.parse::<u32>() {
                Ok(count) => pawn_appetit_lib::resume_debug_analysis(Path::new(db), project, count).await,
                _ => Err("count must be a positive integer".into()),
            },
        [_, command, db, project] if command == "show" =>
            pawn_appetit_lib::show_debug_analysis(Path::new(db), project),
        [_, command, db, engine, fen, count, time_ms] if command == "run" => {
            match (count.parse::<u32>(), time_ms.parse::<u32>()) {
                (Ok(count), Ok(time_ms)) => pawn_appetit_lib::run_debug_analysis(
                    Path::new(db), engine, fen, count, time_ms).await,
                _ => Err("count and time_ms must be positive integers".into()),
            }
        }
        _ => Err("usage: analysis_debug run DB ENGINE 'FEN' COUNT TIME_MS | resume DB PROJECT_ID COUNT | show DB PROJECT_ID".into()),
    };
    match result {
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
