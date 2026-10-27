//! Bounded, provider-independent educational design profiles. These are data, not permissions.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignProfile {
    pub name: String,
    pub keywords: Vec<String>,
    pub age_range: [u8; 2],
    pub page_mm: [f64; 2],
    pub margin_mm: f64,
    pub palette: Vec<String>,
    pub typography: Vec<String>,
    pub layout: Vec<String>,
    pub pedagogy: Vec<String>,
    pub illustration: Vec<String>,
    pub page_families: Vec<PageFamily>,
    #[serde(default)]
    pub corrections: Vec<DesignCorrection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageFamily {
    pub id: String,
    pub purpose: String,
    pub template: String,
    #[serde(default)]
    pub text_slots: Vec<DesignTextSlot>,
    #[serde(default)]
    pub image_slots: Vec<DesignImageSlot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignTextSlot {
    pub id: String,
    pub purpose: String,
    pub command: usize,
    /// Copy-length target for the planner. Publication also requires real native
    /// composition at the authored type size; character counts cannot prove fit.
    pub max_chars: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignImageSlot {
    pub id: String,
    pub purpose: String,
    pub rect: [f64; 4],
    /// Normalized source rectangle [left, top, right, bottom]; pixels stay unmodified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<[f64; 4]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_aspect_ratio: Option<f64>,
    /// Additional native frames sharing this one generated source image.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<DesignImageView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignImageView {
    pub rect: [f64; 4],
    pub crop: [f64; 4],
}

impl DesignImageSlot {
    pub fn placement_count(&self) -> usize {
        self.views.len().saturating_add(1)
    }
}

/// Explicitly recorded before/after guidance; never inferred approval or weight training.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignCorrection {
    pub before: String,
    pub after: String,
    pub reason: String,
}

impl DesignProfile {
    pub fn validate(&self) -> Result<(), String> {
        let text = |s: &str, max: usize| !s.trim().is_empty() && s.len() <= max && !s.contains('\0');
        if !text(&self.name, 160)
            || self.age_range[0] > self.age_range[1]
            || self.age_range[1] > 99
            || self.page_mm.iter().any(|n| !n.is_finite() || !(40.0..=1000.0).contains(n))
            || !self.margin_mm.is_finite()
            || !(0.0..=100.0).contains(&self.margin_mm)
            || self.page_mm.iter().any(|n| self.margin_mm * 2.0 >= *n)
        {
            return Err("Design profile name, age or print geometry is invalid".into());
        }
        for (values, max_count, max_text) in [
            (&self.keywords, 48, 80),
            (&self.palette, 12, 80),
            (&self.typography, 16, 512),
            (&self.layout, 16, 512),
            (&self.pedagogy, 16, 512),
            (&self.illustration, 16, 512),
        ] {
            if values.is_empty() || values.len() > max_count || values.iter().any(|s| !text(s, max_text)) {
                return Err("Design profile rules exceed their limits or contain empty values".into());
            }
        }
        if self.page_families.is_empty() || self.page_families.len() > 12 || self.corrections.len() > 24 {
            return Err("Design profile needs 1–12 page families and at most 24 corrections".into());
        }
        let mut ids = BTreeSet::new();
        let mut image_slots: Vec<&DesignImageSlot> = Vec::new();
        for family in &self.page_families {
            if !text(&family.id, 64)
                || !family.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || !ids.insert(&family.id)
                || !text(&family.purpose, 512)
                || !family.template.starts_with("templates/")
                || !family.template.ends_with(".json")
                || family.template.len() > 120
                || family.template.contains(['\\', ':'])
                || family.template.split('/').any(|part| part.is_empty() || part == "." || part == "..")
            {
                return Err("Design profile page family or template path is invalid".into());
            }
            if family.text_slots.len() > 24 || family.image_slots.len() > 6 {
                return Err("A design page family supports at most 24 text slots and 6 image slots".into());
            }
            let mut names = BTreeSet::new();
            let mut commands = BTreeSet::new();
            for slot in &family.text_slots {
                if !text(&slot.id, 64)
                    || !slot.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    || !names.insert(&slot.id)
                    || !commands.insert(slot.command)
                    || !text(&slot.purpose, 256)
                    || !(1..=256).contains(&slot.max_chars)
                    || slot.command >= 512
                {
                    return Err("Invalid or duplicate native text slot".into());
                }
            }
            for slot in &family.image_slots {
                let margin = self.margin_mm * 72.0 / 25.4;
                let safe_rect = |rect: &[f64; 4]| {
                    rect.iter().all(|n| n.is_finite())
                        && rect[0] >= margin
                        && rect[1] >= margin
                        && rect[2] > rect[0]
                        && rect[3] > rect[1]
                        && rect[2] <= self.page_mm[0] * 72.0 / 25.4 - margin
                        && rect[3] <= self.page_mm[1] * 72.0 / 25.4 - margin
                };
                let safe_crop = |crop: &[f64; 4]| {
                    crop.iter().all(|n| n.is_finite() && (0.0..=1.0).contains(n)) && crop[2] - crop[0] >= 0.01 && crop[3] - crop[1] >= 0.01
                };
                if !text(&slot.id, 64)
                    || !slot.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    || !names.insert(&slot.id)
                    || !text(&slot.purpose, 256)
                    || !safe_rect(&slot.rect)
                    || slot.crop.as_ref().is_some_and(|crop| !safe_crop(crop))
                    || slot.source_aspect_ratio.is_some_and(|ratio| !ratio.is_finite() || !(0.25..=4.0).contains(&ratio))
                    || slot.views.len() > 7
                    || slot.views.iter().any(|view| !safe_rect(&view.rect) || !safe_crop(&view.crop))
                {
                    return Err("Invalid design image slot or unsafe geometry".into());
                }
                // Serialized placements carry purpose and rect, without a family
                // id. Any slots that could match one placement must agree on its
                // source and views, including a slot with default fitting.
                if image_slots.iter().any(|other| {
                    other.purpose == slot.purpose
                        && other.rect.iter().zip(slot.rect).all(|(a, b)| (a - b).abs() <= 0.02)
                        && (other.crop != slot.crop || other.source_aspect_ratio != slot.source_aspect_ratio || other.views != slot.views)
                }) {
                    return Err("Ambiguous design image slots share a purpose and rectangle but disagree on their source or views".into());
                }
                image_slots.push(slot);
            }
        }
        if self.corrections.iter().any(|c| !text(&c.before, 512) || !text(&c.after, 512) || !text(&c.reason, 512)) {
            return Err("Design correction requires bounded before, after and reason".into());
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > 32 * 1024 {
            return Err("Design profile exceeds 32 KiB".into());
        }
        Ok(())
    }

    /// Resolves a declared `DesignImageSlot` by explicit `(family_id, slot_id)` first,
    /// then unambiguous `slot_id`, and finally legacy `(purpose == label, rect)` fallback.
    pub fn resolve_image_slot(&self, family_id: Option<&str>, slot_id: Option<&str>, label: &str, rect: [f64; 4]) -> Option<&DesignImageSlot> {
        if let (Some(fid), Some(sid)) = (family_id, slot_id) {
            return self
                .page_families
                .iter()
                .find(|family| family.id == fid)
                .and_then(|family| family.image_slots.iter().find(|slot| slot.id == sid));
        }
        if let Some(sid) = slot_id {
            let mut matches = self.page_families.iter().flat_map(|family| &family.image_slots).filter(|slot| slot.id == sid);
            let first = matches.next()?;
            return matches.next().is_none().then_some(first);
        }
        if let Some(fid) = family_id {
            return self.page_families.iter().find(|family| family.id == fid).and_then(|family| {
                family.image_slots.iter().find(|slot| slot.purpose == label && slot.rect.iter().zip(rect).all(|(a, b)| (a - b).abs() <= 0.01))
            });
        }
        self.page_families
            .iter()
            .flat_map(|family| &family.image_slots)
            .find(|slot| slot.purpose == label && slot.rect.iter().zip(rect).all(|(a, b)| (a - b).abs() <= 0.01))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DesignStyleUse {
    pub skill_id: String,
    pub revision: String,
    pub name: String,
    #[serde(default)]
    pub execution_mode: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_slot_lookup_rejects_ambiguous_family_source_metadata() {
        let mut p = profile();
        let slot: DesignImageSlot = serde_json::from_value(serde_json::json!({
            "id":"atlas", "purpose":"Shared source", "rect":[54,180,154,280],
            "crop":[0,0,0.5,1], "source_aspect_ratio":2,
            "views":[{"rect":[180,180,280,280],"crop":[0.5,0,1,1]}]
        }))
        .unwrap();
        p.page_families[0].image_slots = vec![slot.clone()];
        let mut other = p.page_families[0].clone();
        other.id = "comparison".into();
        other.template = "templates/comparison.json".into();
        p.page_families.push(other);
        p.validate().unwrap();
        for changed in [
            DesignImageSlot { crop: Some([0.0, 0.0, 1.0, 1.0]), ..slot.clone() },
            DesignImageSlot { source_aspect_ratio: Some(1.5), ..slot.clone() },
            DesignImageSlot { views: vec![], ..slot.clone() },
            DesignImageSlot { crop: None, source_aspect_ratio: None, views: vec![], ..slot.clone() },
        ] {
            p.page_families[1].image_slots[0] = changed;
            assert!(p.validate().unwrap_err().contains("Ambiguous"));
        }
        // One placement can match rectangles up to 0.02 points apart because
        // each is compared with the same 0.01-point tolerance.
        p.page_families[1].image_slots[0].rect[0] += 0.015;
        assert!(p.validate().unwrap_err().contains("Ambiguous"));
        p.page_families[1].image_slots[0].purpose = "Different illustration".into();
        p.validate().unwrap();
    }

    #[test]
    fn shared_illustration_views_are_bounded_and_stay_inside_print_margins() {
        let mut p = profile();
        let slot = serde_json::json!({"id":"atlas","purpose":"Four individual objects",
            "rect":[54,180,154,280],"crop":[0,0,0.25,1],"source_aspect_ratio":4,
            "views":[{"rect":[180,180,280,280],"crop":[0.25,0,0.5,1]}]});
        p.page_families[0].image_slots = vec![serde_json::from_value(slot.clone()).unwrap()];
        p.validate().unwrap();
        for (key, bad) in [
            ("crop", serde_json::json!([0, 0, 0, 1])),
            ("crop", serde_json::json!([0, 0, 1.1, 1])),
            ("source_aspect_ratio", serde_json::json!(10000)),
            ("views", serde_json::json!([{"rect":[0,0,100,100],"crop":[0,0,1,1]}])),
            ("views", serde_json::json!(vec![serde_json::json!({"rect":[54,180,154,280],"crop":[0,0,1,1]}); 8])),
        ] {
            let mut changed = slot.clone();
            changed[key] = bad;
            p.page_families[0].image_slots = vec![serde_json::from_value(changed).unwrap()];
            assert!(p.validate().is_err(), "{key}");
        }
    }
    fn profile() -> DesignProfile {
        DesignProfile {
            name: "School".into(),
            keywords: vec!["kindergarten".into()],
            age_range: [4, 6],
            page_mm: [210.0, 297.0],
            margin_mm: 15.0,
            palette: vec!["[Black]".into()],
            typography: vec!["Readable native type".into()],
            layout: vec!["Aligned tasks".into()],
            pedagogy: vec!["Guided practice".into()],
            illustration: vec!["Colored drawings".into()],
            page_families: vec![PageFamily {
                id: "activity".into(),
                purpose: "Independent practice".into(),
                template: "templates/activity.json".into(),
                text_slots: vec![],
                image_slots: vec![],
            }],
            corrections: vec![],
        }
    }
    #[test]
    fn design_profile_rejects_invalid_print_geometry_paths_duplicates_and_unbounded_rules() {
        let valid = profile();
        valid.validate().unwrap();
        for margin in [f64::NAN, f64::INFINITY, -1.0, 106.0] {
            let mut p = valid.clone();
            p.margin_mm = margin;
            assert!(p.validate().is_err());
        }
        let mut p = valid.clone();
        p.age_range = [7, 4];
        assert!(p.validate().is_err());
        let mut p = valid.clone();
        p.page_families.push(p.page_families[0].clone());
        assert!(p.validate().is_err());
        for path in ["templates/../../secret.json", "templates/C:secret.json", "templates\\file.json"] {
            let mut p = valid.clone();
            p.page_families[0].template = path.into();
            assert!(p.validate().is_err());
        }
        let mut p = valid;
        p.layout = vec!["x".repeat(513)];
        assert!(p.validate().is_err());
    }
    #[test]
    fn legacy_settings_default_to_automatic_style_selection() {
        let settings: crate::protocol::AiSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.design_style.is_none());
        assert!(!settings.allow_paid_repairs);
        assert_eq!(settings.pages_per_batch, 2);
    }

    #[test]
    fn wave3_resolve_image_slot_prefers_explicit_family_and_slot_ids_before_geometric_fallback() {
        let mut p = profile();
        let slot_a: DesignImageSlot = serde_json::from_value(serde_json::json!({
            "id": "hero-art",
            "purpose": "Lesson scene",
            "rect": [54.0, 180.0, 214.0, 280.0],
            "crop": [0.0, 0.0, 0.5, 1.0],
            "source_aspect_ratio": 2.0,
            "views": [{"rect": [230.0, 180.0, 390.0, 280.0], "crop": [0.5, 0.0, 1.0, 1.0]}]
        }))
        .unwrap();
        p.page_families[0].image_slots = vec![slot_a.clone()];
        let page_w = p.page_mm[0] * 72.0 / 25.4;
        let mirrored_rect = [page_w - slot_a.rect[2], slot_a.rect[1], page_w - slot_a.rect[0], slot_a.rect[3]];
        // Geometric fallback misses mirrored RTL rect, whereas explicit (family_id, slot_id) resolves cleanly.
        assert!(p.resolve_image_slot(None, None, "Lesson scene", mirrored_rect).is_none());
        let resolved = p.resolve_image_slot(Some("activity"), Some("hero-art"), "Renamed label", mirrored_rect).unwrap();
        assert_eq!(resolved, &slot_a);
        // Legacy fallback without IDs still works when purpose and rect match.
        assert_eq!(p.resolve_image_slot(None, None, "Lesson scene", slot_a.rect), Some(&slot_a));
    }
}
