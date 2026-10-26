//! Measurable style checks. These do not claim educational or aesthetic certification.
use designcraft_ai_types::DesignProfile;
use designcraft_engine::DocState;
use std::sync::Arc;

pub(crate) fn document_params(profile: Option<&DesignProfile>, pages: u32) -> serde_json::Value {
    document_params_with_binding(profile, pages, false)
}

pub(crate) fn document_params_with_binding(profile: Option<&DesignProfile>, pages: u32, right_to_left_binding: bool) -> serde_json::Value {
    let mut params = match profile {
        Some(p) => {
            serde_json::json!({"width":p.page_mm[0]*72.0/25.4,"height":p.page_mm[1]*72.0/25.4,"pages":pages,"facingPages":false,"margins":p.margin_mm*72.0/25.4,"bleed":0})
        }
        None => serde_json::json!({"preset":"A4","pages":pages,"facingPages":false,"margins":15.0*72.0/25.4,"bleed":0}),
    };
    if right_to_left_binding {
        params["binding"] = serde_json::json!("rightToLeft");
    }
    params
}

pub(crate) fn check_geometry(before: Option<&DocState>, after: &DocState, profile: &DesignProfile) -> Result<(), String> {
    profile.validate()?;
    let factor = 72.0 / 25.4;
    let margin = profile.margin_mm * factor;
    for spread in after.doc.spreads.iter().chain(after.doc.parents.iter()) {
        for page in &spread.pages {
            if (page.width - profile.page_mm[0] * factor).abs() > 0.1 || (page.height - profile.page_mm[1] * factor).abs() > 0.1 {
                return Err(format!("Design style '{}': page size must be {} × {} mm", profile.name, profile.page_mm[0], profile.page_mm[1]));
            }
        }
        for item in &spread.items {
            if before
                .and_then(|base| base.doc.spreads.iter().chain(base.doc.parents.iter()).find(|s| s.id == spread.id))
                .is_some_and(|old| old.items.iter().any(|other| Arc::ptr_eq(item, other)))
            {
                continue;
            }
            // Group outline bounds omit descendant strokes. Check the actual tree too.
            let mut pending = vec![(item.as_ref(), designcraft_geom::Affine::IDENTITY, 0usize)];
            let mut visited = 0usize;
            while let Some((node, outer, depth)) = pending.pop() {
                visited = visited.saturating_add(1);
                if depth > 64 || visited > 10_000 {
                    return Err("Design style geometry exceeds its bounded item-tree limit".into());
                }
                let bounds = outer.transform_rect_bbox(node.visible_bounds());
                let valid = [bounds.x0, bounds.y0, bounds.x1, bounds.y1].iter().all(|n| n.is_finite())
                    && spread.pages.iter().any(|page| {
                        bounds.x0 >= page.x + margin - 0.1
                            && bounds.x1 <= page.x + page.width - margin + 0.1
                            && bounds.y0 >= margin - 0.1
                            && bounds.y1 <= page.height - margin + 0.1
                    });
                if !valid {
                    return Err(format!(
                        "Design style '{}': item {:?} crosses the {} mm safe margin; reposition or resize it",
                        profile.name, node.id, profile.margin_mm
                    ));
                }
                if pending.len().saturating_add(node.children().len()) > 10_000 {
                    return Err("Design style geometry exceeds its pending item-tree limit".into());
                }
                for child in node.children() {
                    pending.push((child.as_ref(), outer * node.xf, depth.saturating_add(1)));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn check_image_slots(
    base: &DocState,
    images: &[designcraft_ai_types::protocol::ImagePlacement],
    profile: &DesignProfile,
) -> Result<(), String> {
    let margin = profile.margin_mm * 72.0 / 25.4;
    for image in images {
        let spread = base.doc.spreads.get(image.page).ok_or("Design image refers to a missing page")?;
        for [left, top, right, bottom] in image.all_rects() {
            if !spread.pages.iter().any(|p| left >= p.x + margin && right <= p.x + p.width - margin && top >= margin && bottom <= p.height - margin) {
                return Err("Design image slot crosses the pinned safe margin; correct it before requesting an illustration".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn check_document_image_slots(base: &DocState, images: &[designcraft_ai_types::protocol::ImagePlacement]) -> Result<(), String> {
    for image in images {
        let spread = base.doc.spreads.get(image.page).ok_or("Illustration refers to a missing native page")?;
        for r in image.all_rects() {
            if r.iter().any(|v| !v.is_finite())
                || r[2] <= r[0]
                || r[3] <= r[1]
                || !spread.pages.iter().any(|p| {
                    let safe = p.margin_rect();
                    r[0] >= safe.x0 - 0.01 && r[1] >= safe.y0 - 0.01 && r[2] <= safe.x1 + 0.01 && r[3] <= safe.y1 + 0.01
                })
            {
                return Err("Illustration cell crosses the opened document safe margin; no images were requested".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_engine::Session;
    use serde_json::json;

    fn profile() -> DesignProfile {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = designcraft_ai_skills::SkillRegistry::open(dir.path()).unwrap();
        registry.install_builtin_drafts().unwrap().into_iter().find_map(|r| r.manifest.design_profile).unwrap()
    }
    #[test]
    fn geometry_checks_actual_page_size_and_grouped_child_strokes() {
        let profile = profile();
        let mut session = Session::new();
        session.execute("file.new", &document_params(Some(&profile), 1)).unwrap();
        let a = session.execute("frame.create", &json!({"rect":[44,100,100,150]})).unwrap()["id"].clone();
        let b = session.execute("frame.create", &json!({"rect":[150,100,200,150]})).unwrap()["id"].clone();
        check_geometry(None, session.doc().unwrap(), &profile).unwrap();
        session.execute("object.stroke", &json!({"ids":[a.clone()],"swatch":"[Black]","weight":8})).unwrap();
        session.execute("object.group", &json!({"ids":[a,b]})).unwrap();
        assert!(check_geometry(None, session.doc().unwrap(), &profile).unwrap_err().contains("safe margin"));
        let mut wrong = profile;
        wrong.page_mm = [148.0, 210.0];
        for family in &mut wrong.page_families {
            family.image_slots.clear();
        }
        assert!(check_geometry(None, session.doc().unwrap(), &wrong).unwrap_err().contains("page size"));
    }
    #[test]
    fn image_slots_are_checked_before_generation_and_old_untouched_items_are_preserved() {
        let profile = profile();
        let mut session = Session::new();
        session.execute("file.new", &document_params(Some(&profile), 1)).unwrap();
        session.execute("frame.create", &json!({"rect":[1,1,20,20]})).unwrap();
        let base = session.doc().unwrap().clone();
        session.execute("frame.create", &json!({"rect":[60,60,150,150]})).unwrap();
        check_geometry(Some(&base), session.doc().unwrap(), &profile).unwrap();
        assert!(check_geometry(None, session.doc().unwrap(), &profile).is_err());
        let mut image = designcraft_ai_types::protocol::ImagePlacement {
            family_id: None,
            slot_id: None,
            repeat_rects: Vec::new(),
            repeat_groups: Vec::new(),
            prompt: "Tree".into(),
            page: 0,
            rect: [1.0, 60.0, 100.0, 160.0],
            label: "tree".into(),
        };
        assert!(check_image_slots(&base, &[image.clone()], &profile).is_err());
        image.rect = [60.0, 60.0, 160.0, 160.0];
        check_image_slots(&base, &[image], &profile).unwrap();
    }
}
