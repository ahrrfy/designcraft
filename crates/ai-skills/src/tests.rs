#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::io::Write;

fn registry() -> (tempfile::TempDir, SkillRegistry) {
    let temporary = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::open(temporary.path().join("catalog")).unwrap();
    (temporary, registry)
}

fn approve(registry: &mut SkillRegistry, temp: &Path, record: &SkillRevision) {
    let sample = temp.join("sample.png");
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 4, 4);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255u8; 48]).unwrap();
    }
    fs::write(&sample, bytes).unwrap();
    let actions = registry.sample_actions(&record.skill_id, &record.revision).unwrap();
    registry
        .approve(
            &record.skill_id,
            &record.revision,
            ApprovalEvidence {
                reviewer: "Human reviewer".into(),
                sample_artifact: sample,
                executed_actions: actions,
                supported_commands: BTreeSet::from(["frame.create".into(), "type.char".into(), "type.para".into()]),
                notes: "Reviewed scratch sample spacing and typography.".into(),
            },
        )
        .unwrap();
}

#[test]
fn drafts_require_review_and_preserve_active_revision_on_edit() {
    let (temp, mut registry) = registry();
    let records = registry.install_builtin_drafts().unwrap();
    assert_eq!(records.len(), 8);
    let first = records.first().unwrap();
    assert_eq!(first.status, RevisionStatus::Draft);
    assert!(registry.snapshot(&first.skill_id, &[]).is_err());
    approve(&mut registry, temp.path(), first);
    let pinned = registry.snapshot(&first.skill_id, &["references/review.md".into()]).unwrap();
    assert_eq!(pinned.references.len(), 1);
    let directory = temp.path().join("edited");
    registry.export_folder(&first.skill_id, &first.revision, &directory).unwrap();
    let skill = directory.join("SKILL.md");
    let mut instructions = fs::read_to_string(&skill).unwrap();
    instructions.push_str("\nNew draft instruction.\n");
    fs::write(&skill, instructions).unwrap();
    let edited = registry.import_folder(&directory, Provenance::default()).unwrap();
    assert_eq!(edited.status, RevisionStatus::Draft);
    assert_ne!(first.revision, edited.revision);
    assert_eq!(registry.snapshot(&first.skill_id, &[]).unwrap().revision, first.revision);
    approve(&mut registry, temp.path(), &edited);
    assert_eq!(registry.snapshot(&first.skill_id, &[]).unwrap().revision, edited.revision);
    assert_eq!(pinned.revision, first.revision);
    assert!(!pinned.instructions.contains("New draft instruction"));
    registry.rollback(&first.skill_id, &first.revision).unwrap();
    assert_eq!(registry.snapshot(&first.skill_id, &[]).unwrap().revision, first.revision);
    registry.set_enabled(&first.skill_id, false).unwrap();
    assert!(registry.snapshot(&first.skill_id, &[]).is_err());
    assert!(!pinned.instructions.is_empty());
    let reopened = SkillRegistry::open(temp.path().join("catalog")).unwrap();
    assert!(!reopened.catalog(&first.skill_id).first().unwrap().enabled);
}

#[test]
fn native_adaptations_preserve_licenses_without_loading_them_as_task_guidance() {
    let (_temp, mut registry) = registry();
    let records = registry.install_builtin_drafts().unwrap();
    for (id, license, needle) in [("native-design-composition", "Apache-2.0", "Apache License"), ("native-design-quality", "MIT", "MIT License")] {
        let record = records.iter().find(|record| record.skill_id == id).unwrap();
        assert_eq!(record.status, RevisionStatus::Draft);
        assert_eq!(record.provenance.license.as_deref(), Some(license));
        assert!(record.manifest.external_tools.is_empty());
        assert_eq!(record.manifest.references, ["references/layout-contract.md"]);
        let files = registry.review_files(id, &record.revision).unwrap();
        let license = std::str::from_utf8(files.get("references/LICENSE.txt").unwrap()).unwrap();
        assert!(license.contains(needle));
        let notice = std::str::from_utf8(files.get("references/NOTICE.md").unwrap()).unwrap();
        assert!(notice.contains("Pinned commit:") && notice.contains("Source SHA-256:"));
        assert!(registry.snapshot(id, &[]).is_err(), "import must not approve the adaptation");
    }
}

#[test]
fn approval_rejects_missing_capabilities_wrong_sample_and_external_tools() {
    let (temp, mut registry) = registry();
    let first = registry.install_builtin_drafts().unwrap().remove(0);
    let sample = temp.path().join("sample.png");
    fs::write(&sample, b"not a render").unwrap();
    assert!(
        registry
            .approve(
                &first.skill_id,
                &first.revision,
                ApprovalEvidence {
                    reviewer: "Human".into(),
                    sample_artifact: sample.clone(),
                    executed_actions: Vec::new(),
                    supported_commands: BTreeSet::new(),
                    notes: "review".into(),
                }
            )
            .is_err()
    );
    assert!(
        registry
            .approve(
                &first.skill_id,
                &first.revision,
                ApprovalEvidence {
                    reviewer: "Human".into(),
                    sample_artifact: sample,
                    executed_actions: registry.sample_actions(&first.skill_id, &first.revision).unwrap(),
                    supported_commands: BTreeSet::from(["frame.create".into()]),
                    notes: "review".into(),
                }
            )
            .is_err()
    );
    let imported = registry
        .import_files(
            PackageFiles::from([(
                "SKILL.md".into(),
                b"---\nname: upstream-skill\ndescription: Example external skill\nallowed-tools: Bash\n---\nRun an external tool.\n".to_vec(),
            )]),
            Provenance::default(),
        )
        .unwrap();
    assert_eq!(imported.status, RevisionStatus::Draft);
    assert!(!imported.manifest.external_tools.is_empty());
    assert!(registry.sample_actions(&imported.skill_id, &imported.revision).is_err());
}

#[test]
fn tampering_never_enters_task_snapshot_or_export() {
    let (temp, mut registry) = registry();
    let first = registry.install_builtin_drafts().unwrap().remove(0);
    approve(&mut registry, temp.path(), &first);
    assert!(registry.review_files(&first.skill_id, &first.revision).is_ok());
    assert!(!registry.review_templates(&first.skill_id, &first.revision).unwrap().is_empty());
    let skill = registry.revision_dir(&first.skill_id, &first.revision).unwrap().join("SKILL.md");
    fs::write(skill, b"changed").unwrap();
    assert!(matches!(registry.review_files(&first.skill_id, &first.revision), Err(SkillError::Integrity(_))));
    assert!(matches!(registry.review_templates(&first.skill_id, &first.revision), Err(SkillError::Integrity(_))));
    assert!(matches!(registry.snapshot(&first.skill_id, &[]), Err(SkillError::Integrity(_))));
    assert!(registry.export_zip(&first.skill_id, &first.revision, temp.path().join("out.zip")).is_err());
    assert!(registry.rollback(&first.skill_id, &first.revision).is_err());
}

#[test]
fn zip_roundtrip_and_unsafe_zip_rejection() {
    let (temp, mut registry) = registry();
    let first = registry.install_builtin_drafts().unwrap().remove(0);
    let zip = temp.path().join("safe.zip");
    registry.export_zip(&first.skill_id, &first.revision, &zip).unwrap();
    let (_other, mut destination) = self::registry();
    let copied = destination.import_zip(zip, Provenance::default()).unwrap();
    assert_eq!(copied.revision, first.revision);
    assert_eq!(copied.status, RevisionStatus::Draft);
    for (number, name) in
        ["../outside.md", "references/../../outside.md", "C:/outside.md", "references\\bad.md", "references/CON.txt"].iter().enumerate()
    {
        let path = temp.path().join(format!("unsafe-{number}.zip"));
        let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        archive.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        archive.write_all(b"unsafe").unwrap();
        archive.finish().unwrap();
        assert!(destination.import_zip(path, Provenance::default()).is_err(), "{name}");
    }
    assert!(!temp.path().join("outside.md").exists());
    let path = temp.path().join("symlink.zip");
    let mut archive = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    archive.add_symlink("references/link.md", "../../outside.md", zip::write::SimpleFileOptions::default()).unwrap();
    archive.finish().unwrap();
    assert!(destination.import_zip(path, Provenance::default()).is_err());
}

#[test]
fn case_collision_and_over_limit_packages_are_rejected() {
    let (_temp, mut registry) = registry();
    let mut files = builtin::packages().unwrap().remove(0);
    files.insert("references/REVIEW.md".into(), b"collision".to_vec());
    assert!(registry.import_files(files, Provenance::default()).is_err());
    let mut files = builtin::packages().unwrap().remove(0);
    files.insert("assets/large.txt".into(), vec![0; package::MAX_FILE as usize + 1]);
    assert!(registry.import_files(files, Provenance::default()).is_err());
}

#[test]
fn cache_is_content_and_analyzer_keyed_and_persistent() {
    let (temp, mut registry) = registry();
    let hash = sha256(b"reference source");
    registry.cache_analysis(&hash, "local-v1", "Extracted design requirements.").unwrap();
    assert!(registry.cached_analysis(&hash, "local-v2").is_none());
    let reopened = SkillRegistry::open(temp.path().join("catalog")).unwrap();
    assert_eq!(reopened.cached_analysis(&hash, "local-v1").unwrap().text, "Extracted design requirements.");
}

#[test]
fn arabic_catalog_queries_find_the_respective_builtin_drafts() {
    let (_temp, mut registry) = registry();
    let drafts = registry.install_builtin_drafts().unwrap();
    for draft in &drafts {
        assert!(draft.description.chars().count() < 1024);
    }
    for (query, expected_id) in [
        ("كتيب", "early-years-booklet-planning"),
        ("منهج", "early-years-booklet-planning"),
        ("رياض الأطفال", "early-years-booklet-planning"),
        ("علوم", "early-years-booklet-planning"),
        ("رياضيات", "early-years-booklet-planning"),
        ("حاسوب", "early-years-booklet-planning"),
        ("أنشطة", "early-years-activity-layout"),
        ("رسومات", "early-years-activity-layout"),
        ("توزيع", "early-years-activity-layout"),
        ("العربية", "arabic-english-typography"),
        ("الإنجليزية", "arabic-english-typography"),
        ("تنسيق", "arabic-english-typography"),
        ("طباعة", "school-print-preflight"),
        ("هوامش", "school-print-preflight"),
        ("A4", "school-print-preflight"),
    ] {
        let entries = registry.catalog(query);
        let entry = entries.iter().find(|entry| entry.skill_id == expected_id).unwrap_or_else(|| panic!("query: {query}"));
        assert_eq!(entry.skill_id, expected_id, "query: {query}");
        assert_eq!(entry.revisions.first().unwrap().status, RevisionStatus::Draft);
        assert!(entry.active_revision.is_none());
    }
}

#[test]
fn template_review_rejects_excessive_variant_and_total_action_counts() {
    let (_temp, mut registry) = registry();
    for (variant_count, actions_per_variant) in [(17usize, 1usize), (2, 2049)] {
        let mut files = builtin::packages().unwrap().remove(0);
        let mut manifest: NativeManifest = serde_json::from_slice(files.get("designcraft.json").unwrap()).unwrap();
        let action = EngineAction { command: "frame.create".into(), params: serde_json::json!({"rect":[54,54,100,100],"content":"unassigned"}) };
        let bytes = serde_json::to_vec(&vec![action; actions_per_variant]).unwrap();
        manifest.templates.clear();
        for index in 0..variant_count {
            let name = format!("templates/variant-{index}.json");
            files.insert(name.clone(), bytes.clone());
            manifest.templates.push(name);
        }
        manifest.sample_template = manifest.templates.first().cloned();
        files.insert("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap());
        let draft = registry.import_files(files, Provenance::default()).unwrap();
        assert!(matches!(registry.review_templates(&draft.skill_id, &draft.revision), Err(SkillError::Unsupported(_))));
        assert_eq!(draft.status, RevisionStatus::Draft);
    }
}

#[test]
fn snapshot_text_formats_are_shared_and_binary_references_are_never_read_as_text() {
    let (temp, mut registry) = registry();
    let mut files = builtin::packages().unwrap().remove(0);
    let mut manifest: NativeManifest = serde_json::from_slice(files.get("designcraft.json").unwrap()).unwrap();
    manifest.references = ["references/a.md", "references/b.txt", "references/c.JSON", "references/d.csv", "references/source.pdf"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for name in &manifest.references {
        files.insert(name.clone(), format!("Content for {name}").into_bytes());
    }
    files.insert("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap());
    let revision = registry.import_files(files, Provenance::default()).unwrap();
    approve(&mut registry, temp.path(), &revision);
    let root = registry.revision_dir(&revision.skill_id, &revision.revision).unwrap();
    fs::remove_file(root.join("references/source.pdf")).unwrap();
    let selected: Vec<_> = manifest.references.iter().filter(|name| is_supported_text_reference(name)).cloned().collect();
    let snapshot = registry.snapshot(&revision.skill_id, &selected).unwrap();
    assert_eq!(snapshot.references.len(), 4);
    assert_eq!(snapshot.references.get("references/c.JSON").unwrap(), "Content for references/c.JSON");
    assert!(matches!(registry.snapshot(&revision.skill_id, &["references/source.pdf".into()]), Err(SkillError::Unsupported(_))));
    assert!(registry.snapshot(&revision.skill_id, &["references/undeclared.txt".into()]).is_err());
}

#[test]
fn snapshot_rejects_large_text_context_instead_of_silently_dropping_references() {
    let (temp, mut registry) = registry();
    let mut files = builtin::packages().unwrap().remove(0);
    let mut manifest: NativeManifest = serde_json::from_slice(files.get("designcraft.json").unwrap()).unwrap();
    manifest.references = vec!["references/large.json".into()];
    files.insert("references/large.json".into(), vec![b' '; MAX_SNAPSHOT_TEXT_BYTES + 1]);
    files.insert("designcraft.json".into(), serde_json::to_vec(&manifest).unwrap());
    let revision = registry.import_files(files, Provenance::default()).unwrap();
    approve(&mut registry, temp.path(), &revision);
    let error = registry.snapshot(&revision.skill_id, &["references/large.json".into()]).unwrap_err();
    assert!(error.to_string().contains("64-KiB text context limit"));
    assert_eq!(registry.snapshot(&revision.skill_id, &[]).unwrap().references.len(), 0);
}

#[test]
#[cfg(any(windows, unix))]
fn bounded_text_reference_loading_still_rejects_directory_redirection() {
    let (temp, mut registry) = registry();
    let revision = registry.install_builtin_drafts().unwrap().remove(0);
    approve(&mut registry, temp.path(), &revision);
    let root = registry.revision_dir(&revision.skill_id, &revision.revision).unwrap();
    let reference_dir = root.join("references");
    let original = root.join("original-references");
    fs::rename(&reference_dir, &original).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("review.md"), b"Outside content must not be read").unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("cmd")
            .creation_flags(0x08000000)
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&reference_dir)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "junction fixture failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &reference_dir).unwrap();
    let error = registry.snapshot(&revision.skill_id, &["references/review.md".into()]).unwrap_err();
    assert!(matches!(&error, SkillError::Invalid(_)));
    assert!(error.to_string().contains("links are not allowed") || error.to_string().contains("reparse points are not allowed"));
    #[cfg(windows)]
    fs::remove_dir(&reference_dir).unwrap();
    #[cfg(unix)]
    fs::remove_file(&reference_dir).unwrap();
    fs::rename(&original, &reference_dir).unwrap();
}

#[cfg(unix)]
#[test]
fn folder_symlink_is_rejected() {
    let (temp, mut registry) = registry();
    let root = temp.path().join("import");
    fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink(temp.path(), root.join("references")).unwrap();
    assert!(registry.import_folder(root, Provenance::default()).is_err());
}
