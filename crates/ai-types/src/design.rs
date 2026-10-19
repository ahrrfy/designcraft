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
    pub max_chars: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesignImageSlot {
    pub id: String,
    pub purpose: String,
    pub rect: [f64; 4],
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
            (&self.keywords, 24, 80),
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
            if family.text_slots.len() > 12 || family.image_slots.len() > 4 {
                return Err("A design page family supports at most 12 text slots and 4 image slots".into());
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
                if !text(&slot.id, 64)
                    || !slot.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    || !names.insert(&slot.id)
                    || !text(&slot.purpose, 256)
                    || slot.rect.iter().any(|n| !n.is_finite())
                    || slot.rect[0] < margin
                    || slot.rect[1] < margin
                    || slot.rect[2] <= slot.rect[0]
                    || slot.rect[3] <= slot.rect[1]
                    || slot.rect[2] > (self.page_mm[0] * 72.0 / 25.4 - margin)
                    || slot.rect[3] > (self.page_mm[1] * 72.0 / 25.4 - margin)
                {
                    return Err("Invalid design image slot or unsafe geometry".into());
                }
            }
        }
        if self.corrections.iter().any(|c| !text(&c.before, 512) || !text(&c.after, 512) || !text(&c.reason, 512)) {
            return Err("Design correction requires bounded before, after and reason".into());
        }
        if serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > 24 * 1024 {
            return Err("Design profile exceeds 24 KiB".into());
        }
        Ok(())
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
}
