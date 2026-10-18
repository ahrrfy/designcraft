//! Optional black-box IDML → real INDD conversion through installed Windows InDesign.
fn main() -> Result<(), String> {
    let mut arguments = std::env::args().skip(1);
    let source = arguments.next().ok_or("usage: indesign_export SOURCE.designcraft DESTINATION.indd")?;
    let destination = arguments.next().ok_or("usage: indesign_export SOURCE.designcraft DESTINATION.indd")?;
    if arguments.next().is_some() {
        return Err("usage: indesign_export SOURCE.designcraft DESTINATION.indd".into());
    }
    designcraft_fonts::FontDb::global().load_system_fonts();
    let mut session = designcraft_engine::Session::new();
    session.execute("file.open", &serde_json::json!({"path":source})).map_err(|e| e.to_string())?;
    let job = designcraft_ai_runtime::indesign::prepare(&mut session, std::path::Path::new(&destination))?;
    let export = job.run(std::time::Duration::from_secs(180))?;
    println!("{}", serde_json::to_string_pretty(&export).map_err(|e| e.to_string())?);
    Ok(())
}
