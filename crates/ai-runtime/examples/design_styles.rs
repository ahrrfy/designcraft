//! Offline native template proof. No provider, key, reference book or model request is used.
use designcraft_ai_skills::SkillRegistry;
use designcraft_engine::Session;
use serde_json::json;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(std::env::args().nth(1).ok_or("Pass a new output directory")?);
    std::fs::create_dir_all(&directory)?;
    let mut registry = SkillRegistry::open(directory.join("sample-skills"))?;
    let record = registry.install_builtin_drafts()?.into_iter().find(|r| r.skill_id == "early-years-clear-style").ok_or("Missing original style")?;
    let profile = record.manifest.design_profile.as_ref().ok_or("Missing design profile")?;
    let templates = registry.review_templates(&record.skill_id, &record.revision)?;
    let mut session = Session::new();
    session
        .execute("file.new", &json!({"preset":"A4","pages":profile.page_families.len(),"facingPages":false,"margins":15.0*72.0/25.4,"bleed":0}))?;
    let mut action_count = 0usize;
    for (page, family) in profile.page_families.iter().enumerate() {
        let mut actions = templates.get(&family.template).ok_or("Missing page-family template")?.clone();
        for action in &mut actions {
            if let Some(params) = action.params.as_object_mut() {
                if params.contains_key("spread") {
                    params.insert("spread".into(), json!(page));
                }
                if params.get("text").and_then(serde_json::Value::as_str).is_some_and(|text| text.contains("{{page}}")) {
                    params.insert("text".into(), json!(format!("اكتشاف وتعلم | {}", page.saturating_add(1))));
                }
            }
        }
        action_count = action_count.saturating_add(actions.len());
        session
            .execute("production.apply", &json!({"commands":actions,"outputMode":"native_only","requirePreflight":true,"label":family.purpose}))?;
    }
    let preflight = session.execute("preflight.run", &json!({}))?;
    let native = directory.join("original-style-samples.designcraft");
    let pdf = directory.join("original-style-samples.pdf");
    session.execute("file.saveAs", &json!({"path":native}))?;
    session.execute("file.exportPdf", &json!({"path":pdf,"spreads":false,"bleed":false,"marks":false}))?;
    let mut renderer = designcraft_render::Renderer::new();
    for page in 0..profile.page_families.len() {
        let image = renderer
            .render_page(&session.doc()?.doc, &session.cache, page, 1.2, false, &designcraft_render::RenderOptions::default())
            .ok_or("Cannot render native page template")?;
        std::fs::write(directory.join(format!("style-page-{}.png", page.saturating_add(1))), image.to_png())?;
    }
    registry.export_zip(&record.skill_id, &record.revision, directory.join("early-years-clear-style.zip"))?;
    let evidence = json!({"source":"Original deterministic native templates; not AI-generated or publisher-derived","provider_requests":0,"estimated_api_cost_usd":0,"style":record.skill_id,"revision":record.revision,"pages":profile.page_families.len(),"native_actions":action_count,"preflight":preflight,"native":native,"pdf":pdf});
    std::fs::write(directory.join("offline-verification.json"), serde_json::to_vec_pretty(&evidence)?)?;
    println!("{}", serde_json::to_string_pretty(&evidence)?);
    Ok(())
}
