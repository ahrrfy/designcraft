//! Deterministic single-source counted artwork. No model, I/O or live document edits.
use designcraft_ai_types::protocol::{ImagePlacement, ProductionPlan};

pub(crate) const POLICY: &str = "DETERMINISTIC MATH SUBJECT COUNTS: when a lesson requires counting illustrated subjects (especially N >= 6 or 14 = 10 + 4), never ask diffusion to draw N objects in one raster. Emit one ImagePlacement describing ONE single isolated subject on a plain background. Use repeat_groups:[10,4] inside the overall bounding rect, or repeat_rects (additional cells, at most 23). The host generates one unit image and places it in exactly N native frames sharing one asset. Do not precreate the repeated graphic frames. Keep every cell inside safeRect, clear of text and other images. At most four groups, 1..20 subjects per group, 2..24 total. Group declarations and explicit repeat_rects are mutually exclusive. Human visual proof must verify that the source depicts exactly one subject; native cardinality does not certify raster semantics.";

pub(crate) fn unit_prompt(image: &ImagePlacement) -> String {
    if image.instance_count() == 1 || image.prompt.contains("NATIVE REPEATED COUNT UNIT") {
        return image.prompt.clone();
    }
    format!(
        "NATIVE REPEATED COUNT UNIT (1 OF {} NATIVE FRAMES). Render EXACTLY ONE single isolated subject, with generous plain background margin and no text or numerals. The host repeats this unit in {} native frames. Context below establishes subject identity and style only; do not depict its total group quantities. Human visual proof remains required.\n{}",
        image.instance_count(),
        image.instance_count(),
        image.prompt
    )
}

pub(crate) fn normalize(plan: &mut ProductionPlan) -> Result<(), String> {
    // Invalid input must not leave half of the plan expanded.
    let mut images = plan.images.clone();
    for image in &mut images {
        if !image.repeat_groups.is_empty() {
            if !image.repeat_rects.is_empty() {
                return Err("Use repeat_groups OR repeat_rects, not both".into());
            }
            let cells = group_cells(image)?;
            image.rect = *cells.first().ok_or("Repeated subject group has no cells")?;
            image.repeat_rects = cells.get(1..).ok_or("Invalid repeated cell range")?.to_vec();
            image.repeat_groups.clear();
        }
        validate(image)?;
    }
    plan.images = images;
    Ok(())
}

pub(crate) fn validate(image: &ImagePlacement) -> Result<(), String> {
    if !image.repeat_groups.is_empty() || image.repeat_rects.len() > 23 {
        return Err("Counted artwork must be normalized and contain at most 24 native frames".into());
    }
    if image.repeat_rects.is_empty() {
        return Ok(());
    }
    let ratio = (image.rect[2] - image.rect[0]) / (image.rect[3] - image.rect[1]);
    let rects: Vec<_> = image.all_rects().collect();
    for (index, rect) in rects.iter().enumerate() {
        valid_rect(*rect)?;
        let aspect = (rect[2] - rect[0]) / (rect[3] - rect[1]);
        if !ratio.is_finite() || ((aspect / ratio) - 1.0).abs() > 0.08 {
            return Err("Repeated subject cells must share their source aspect ratio within 8%".into());
        }
        if rects.iter().take(index).any(|other| overlaps(*rect, *other)) {
            return Err("Repeated subject cells overlap; no images were requested".into());
        }
    }
    Ok(())
}

pub(crate) fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] - 0.01 && a[2] > b[0] + 0.01 && a[1] < b[3] - 0.01 && a[3] > b[1] + 0.01
}
pub(crate) fn same_cell_size(a: [f64; 4], b: [f64; 4]) -> bool {
    ((a[2] - a[0]) - (b[2] - b[0])).abs() <= 0.000001 && ((a[3] - a[1]) - (b[3] - b[1])).abs() <= 0.000001
}
fn valid_rect(r: [f64; 4]) -> Result<(), String> {
    if r.iter().any(|n| !n.is_finite() || !(0.0..=15552.0).contains(n)) || r[2] <= r[0] || r[3] <= r[1] {
        return Err("Counted artwork needs finite positive rectangles within 0..15552 points".into());
    }
    Ok(())
}
fn group_cells(image: &ImagePlacement) -> Result<Vec<[f64; 4]>, String> {
    valid_rect(image.rect)?;
    let groups = &image.repeat_groups;
    let total: usize = groups.iter().map(|n| usize::from(*n)).sum();
    if groups.len() > 4 || groups.iter().any(|n| !(1..=20).contains(n)) || !(2..=24).contains(&total) {
        return Err("repeat_groups requires 1..4 groups of 1..20 and 2..24 total subjects".into());
    }
    let [left, top, right, bottom] = image.rect;
    let (width, height) = (right - left, bottom - top);
    let horizontal = width >= height;
    let grids: Vec<_> = groups
        .iter()
        .map(|n| {
            let cols = if *n >= 5 { 5usize } else { (f64::from(*n).sqrt().ceil()) as usize };
            (cols, usize::from(*n).div_ceil(cols))
        })
        .collect();
    let gap = 4.0;
    let group_gap = 12.0;
    let bands = (groups.len().saturating_sub(1)) as f64;
    let sum_cols: usize = grids.iter().map(|g| g.0).sum();
    let sum_rows: usize = grids.iter().map(|g| g.1).sum();
    let max_cols = grids.iter().map(|g| g.0).max().ok_or("Missing count group")?;
    let max_rows = grids.iter().map(|g| g.1).max().ok_or("Missing count group")?;
    let (cols, rows) = if horizontal { (sum_cols, max_rows) } else { (max_cols, sum_rows) };
    let cell =
        ((width - if horizontal { bands * group_gap } else { 0.0 } - (cols.saturating_sub(if horizontal { groups.len() } else { 1 })) as f64 * gap)
            / cols as f64)
            .min(
                (height
                    - if horizontal { 0.0 } else { bands * group_gap }
                    - (rows.saturating_sub(if horizontal { 1 } else { groups.len() })) as f64 * gap)
                    / rows as f64,
            );
    if !cell.is_finite() || cell < 1.0 {
        return Err("Counted subject region is too small for its cells and group gaps".into());
    }
    let extent = |n: usize| n as f64 * cell + n.saturating_sub(1) as f64 * gap;
    let used = grids.iter().map(|g| extent(if horizontal { g.0 } else { g.1 })).sum::<f64>() + bands * group_gap;
    let mut cursor = if horizontal { left + (width - used) / 2.0 } else { top + (height - used) / 2.0 };
    let mut cells = Vec::with_capacity(total);
    for (quantity, (cols, rows)) in groups.iter().zip(grids) {
        let x = if horizontal { cursor } else { left + (width - extent(cols)) / 2.0 };
        let y = if horizontal { top + (height - extent(rows)) / 2.0 } else { cursor };
        for n in 0..usize::from(*quantity) {
            let a = x + (n % cols) as f64 * (cell + gap);
            let b = y + (n / cols) as f64 * (cell + gap);
            cells.push([a, b, a + cell, b + cell]);
        }
        cursor += extent(if horizontal { cols } else { rows }) + group_gap;
    }
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn plan(groups: serde_json::Value) -> ProductionPlan {
        serde_json::from_value(json!({"title":"Count","commands":[],"images":[{"prompt":"One fish","page":0,"rect":[60,100,540,360],"label":"Fish","repeat_groups":groups}]})).unwrap()
    }
    #[test]
    fn ai24_groups_are_exact_bounded_idempotent_and_distinct() {
        for groups in [json!([10, 4]), json!([20, 4]), json!([6, 3]), json!([14]), json!([1, 1, 1, 1])] {
            for vertical in [false, true] {
                let mut p = plan(groups.clone());
                if vertical {
                    p.images[0].rect = [60.0, 100.0, 320.0, 580.0];
                }
                let bounds = p.images[0].rect;
                normalize(&mut p).unwrap();
                assert_eq!(p.images[0].instance_count(), groups.as_array().unwrap().iter().map(|n| n.as_u64().unwrap() as usize).sum::<usize>());
                assert!(
                    p.images[0]
                        .all_rects()
                        .all(|r| r[0] >= bounds[0] - 0.001 && r[1] >= bounds[1] - 0.001 && r[2] <= bounds[2] + 0.001 && r[3] <= bounds[3] + 0.001)
                );
                let first = p.images.clone();
                normalize(&mut p).unwrap();
                assert_eq!(p.images, first);
            }
        }
    }
    #[test]
    fn ai24_bad_groups_rectangles_and_overlaps_are_rejected_atomically() {
        for groups in [json!([0, 4]), json!([21]), json!([20, 5]), json!([1]), json!([1, 1, 1, 1, 1])] {
            let mut p = plan(groups);
            let before = p.images.clone();
            assert!(normalize(&mut p).is_err());
            assert_eq!(p.images, before);
        }
        let mut p = plan(json!([]));
        for rects in [
            vec![[60.0, 100.0, 540.0, 360.0]],
            vec![[60.0, 400.0, 80.0, 600.0]],
            vec![[f64::NAN, 0.0, 1.0, 1.0]],
            vec![[0.0, 500.0, 480.0, 760.0]; 24],
        ] {
            p.images[0].repeat_rects = rects;
            assert!(normalize(&mut p).is_err());
        }
    }
}
