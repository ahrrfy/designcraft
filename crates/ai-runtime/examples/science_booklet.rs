//! Generate and inspect the native production path without making a provider call.
fn main() -> Result<(), String> {
    let destination = std::env::args().nth(1).ok_or("usage: science_booklet OUTPUT_DIRECTORY")?;
    designcraft_fonts::FontDb::global().load_system_fonts();
    let mut session = designcraft_engine::Session::new();
    for artifact in designcraft_ai_runtime::science::build_science_sample(&mut session, std::path::Path::new(&destination))? {
        println!("{artifact}");
    }
    if std::env::args().any(|arg| arg == "--ui-fixture") {
        let mut runner = designcraft_ai_runtime::ProductionRunner::open(std::path::Path::new(&destination).join("ai-profile"))?;
        let snapshot = runner.handle(&mut session, designcraft_ai_types::protocol::AiRequest::Poll);
        let bytes = serde_json::to_vec_pretty(&snapshot).map_err(|e| e.to_string())?;
        std::fs::write(std::path::Path::new(&destination).join("ai-snapshot.json"), bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}
