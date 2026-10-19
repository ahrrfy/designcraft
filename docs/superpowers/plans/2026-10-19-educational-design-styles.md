# Educational design styles implementation plan

> **For agentic workers:** Execute inline using the executing-plans workflow. The user approved the reference → rules → native templates → sample → reusable style design on 2026-10-19.

**Goal:** Make reusable educational design styles part of the existing native skill and production workflow, with automatic selection and an explicit style override.

**Architecture:** Store a bounded, typed design profile in a skill's immutable native manifest. Reuse the registry, cache, sample validation, task snapshots and human batch gates. Generate style profiles and several native page-family templates from references or a corrected document, and carry the chosen profile into text planning, illustration prompts and native geometry checks.

**Tech stack:** Rust, serde, the existing ai-types/ai-skills/ai-runtime crates and thin egui UI.

## Global constraints

- All document edits use Session::execute. No new external scripts, tools or dependencies.
- Imported references remain untrusted data. Model output cannot grant permissions or mark itself human-approved.
- Existing settings/packages deserialize without a profile. Style revisions are pinned across batches.
- A single design style applies per task. Disabled/incompatible/unvalidated styles cannot run.
- A4 sample: 210 × 297 mm, 15 mm margins. Native text, shapes and layout remain editable.
- Automatic technical validation does not certify educational or aesthetic quality. Human batch approval stays mandatory.
- No paid requests or continuation of the user's pending book during implementation.

## Tasks

- [x] Add bounded DesignProfile/PageFamily/DesignCorrection contracts; reject invalid dimensions, duplicate families, invalid template links and excessive context.
- [x] Extend skill manifests/snapshots with optional profiles; authenticate the manifest before loading its profile. Add an original early-years style with several executable native templates.
- [x] Select one style by explicit ID or local keyword match, validate it with existing sample machinery, and pin it with other skills. Record the chosen name and revision in reports.
- [x] Extend reference/document skill generation with an explicit design-style mode, structured rules, page-family templates and correction examples. Validate all templates before activation.
- [x] Include the pinned profile in planning and illustration prompts, and verify actual generated page dimensions and safe content bounds before publication. Do not claim automated aesthetic matching.
- [x] Add a compact style selector to production, a style-library view and style-building controls to Skills. Keep the main prompt/model/produce flow simple.
- [x] Test malformed profiles, legacy compatibility, manifest integrity, profile pinning, conflicting styles, style routing, geometry rejection, generator packaging and illustration context. Run cargo xtask ci, build, inspect the actual UI, and save a local synthetic sample without provider calls.
- [x] Update Arabic usage docs and ROADMAP, then commit only after the quality gates pass.

## Verification commands

```powershell
cargo test -p designcraft-ai-types -p designcraft-ai-skills -p designcraft-ai-runtime
cargo xtask ci
cargo build -p designcraft -p designcraft-cli
```

Acceptance evidence must distinguish synthetic local examples from model generation. Existing PDF references are not bundled, copied or described as newly analyzed without running analysis.

## User steering: eliminate expensive command-repair loops

The user reported provider timeouts and paying to repair long native command plans. New educational styles therefore bind text slots to exact template creation commands and define image slots with fixed geometry. A content-only model response selects page families and fills bounded strings; the host compiles commands and rebases result references locally. Compiler compatibility and image command capacity are checked before provider charges. Exactly one content planning request is used per template batch. Raw planning retries now require an explicit allow_paid_repairs setting, false for new and legacy settings. New profiles default to two pages per batch. Existing human proof gates are retained.

Live-profile validation also exposed an imported illustration draft containing an unsupported crayon glyph. Automatic preparation now excludes invalid drafts, reports bounded reasons, retains a valid active revision when present and continues with sound skills. Native compatibility errors never become model repair requests. Content mode receives the pinned instructions and references of selected skills without their command templates.
