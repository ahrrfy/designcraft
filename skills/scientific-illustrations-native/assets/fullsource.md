---
name: scientific-illustrations
description: Design publication-ready scientific diagrams and data figures through generated-image iteration, critical review, and faithful native Penpot redraw. Use for methodology diagrams, framework figures, statistical plots, composites, or converting an approved visual reference into an editable vector figure.
---

# Scientific Illustrations

Produce a scientifically correct visual design, select a reviewed raster reference,
then redraw it as editable native vector content in Penpot. Use the role briefs in [agent-prompts.md](references/agent-prompts.md). One agent may perform several roles.

## Establish the brief

Record the source material, figure purpose, audience, target dimensions, output
location, and user constraints in `brief.md`. Include an exact-label manifest,
required legends, every meaningful node and connection, and the source for each
scientific claim or example. Treat the user's supplied reference, caption, labels,
palette, and venue rules as authoritative. Resolve ambiguities that would change
scientific meaning before generation.

Choose the mode:

- **Diagram:** run the image-design loop, then redraw the selected design in Penpot.
- **Quantitative plot:** render supplied data with executable plotting code, export
  native SVG, and retain the code and data mapping. Do not ask an image model to
  invent quantitative marks.
- **Composite:** generate and refine the surrounding design, but import plots from
  their data-generated native SVG.
- **Native edit or redraw:** when the user supplies a fixed reference or existing
  Penpot figure and requests only native editing, treat it as the selected design
  and start at the Penpot phase.

Do not omit required labels, legends, connections, data, or scientific qualifiers
to make the figure cleaner. Keep the caption outside the artwork unless requested.

## Route image generation by capability

Honor an explicit user choice of image provider or tool first. Otherwise:

1. Use GPT image generation built into Codex when the current runtime exposes it.
   This path needs no API key. If the runtime does not expose a model name or
   version, record only the tool and information it reports. Never guess the model.
2. If built-in generation is unavailable, inspect the tools and configured
   providers that are actually available. Choose any image-capable option that
   meets the task's data, budget, and tool permissions. Existing Gemini or OpenRouter
   configurations are valid examples,
   but do not hardcode a provider or model version.
3. If a chosen provider is unavailable or out of quota, move to another configured,
   permitted image-capable option without asking the user to choose a model again.
   Ask only when the switch requires permission beyond the task's existing data,
   spending, credential, or other user constraints.

Install dependencies only within the task's existing authorization. Do not add
unprovided credentials, enable billing, exceed the generation budget, or claim
calls that did not occur. If no configured image-capable route works, report the
design phase as blocked; do not hand-draw an initial SVG in its place.

## Develop and select the design

Use a maximum of three generated candidates by default, including the initial
candidate. The user may set another finite budget. Generate at least one actual
candidate and continue while the critic requests changes and the budget remains.

For each round:

1. Save the exact generation or edit prompt as `prompts/round-NN.md` and the
   returned image as `design-round-NN.png`.
2. Open and inspect the actual image. Check every item in the brief, including label
   spelling, example validity, arrows, legends, grouping, visual hierarchy, and
   readability at the target size.
3. Save the findings and accept/revise verdict as `critiques/round-NN.md`.
4. When the tool supports reference-image editing, make targeted edits to the
   current best candidate. Otherwise regenerate with explicit retained features and
   correction constraints. Count either operation as the next candidate round.

The next prompt must preserve correct content instead of rewriting the design from
scratch. Use [style-guide.md](references/style-guide.md) only when it fits the
user's venue or requested style. Its NeurIPS palette is an option, not a default.

After the loop, compare all candidates and select the best scientifically correct
one. Do not select the last candidate by default. A critic's acceptance is
sufficient unless the user requested sign-off. Do not freeze a design with
unresolved scientific errors. A documented glyph or spelling artifact may remain
only when its intended text is unambiguous in the exact-label manifest and will be
corrected during native text reconstruction.

Copy the accepted candidate to `design-selected.png` without further image edits.
Record its source round, pixel dimensions, SHA-256 checksum, generation provenance,
and selection reason in `provenance.md`. Keep the exact-label and source brief fixed
for the redraw.

## Redraw in Penpot

Read [penpot-redraw.md](references/penpot-redraw.md) before this phase. Reproduce
the fixed reference as closely as possible with native Penpot objects: canvas and
layout, icon geometry, colors, gradients, typography, connectors, line weights,
spacing, and visual hierarchy.

Do not silently redesign the selected image. Do not use Fablevect automatic tracing,
an embedded PNG, or another raster layer as a substitute for editable vectors. A
quantitative plot may enter a composite as its native plot SVG.

Export the Penpot drawing, compare it with `design-selected.png` at matching
dimensions, and inspect both images. Correct mismatches with targeted native edits.
Use up to three native refinement passes after the first export by default, or the
user's finite budget. Stop when the reference fidelity and QA checks pass. If the
budget is reached, record the remaining differences and the checks that failed.
Correct known raster glyph errors from the exact-label manifest and document each
such exception in `qa.md`.

If Penpot is unavailable, preserve the accepted design artifacts and report
`vector-blocked: Penpot unavailable`. Never fabricate Penpot operations or call the
result vector-ready.

## Deliverables and completion

Store the job under the user-selected output directory, or
`projects/output/<job-name>/` by default. For diagrams and composites, deliver:

- `brief.md`, with sources, communicative intent, user constraints, and exact labels
- versioned round prompts, candidate images, and critiques
- `design-selected.png`
- `figure.penpot`, `figure.svg`, and `figure.png`
- plotting code and native plot SVG for quantitative figures
- `provenance.md`, with actual tools and exposed model information, source assets,
  dimensions, checksum, and documented substitutions
- `qa.md`, with scientific, label-manifest, visual, export, and
  reference-comparison results
- redistributable font files needed for exact rendering, with their family,
  version, and source recorded in provenance

Report `vector-ready` only when the native Penpot file and both exports are verified,
practical editing checks pass, and scientific, visual, and reference-fidelity QA
all pass. Record any accepted minor differences. A completed comparison with failed
fidelity checks is still incomplete; name the blocking phase and preserve the
reviewable artifacts already produced.

For a standalone quantitative plot, deliver its code, data mapping, native SVG,
PNG, provenance, and data/visual QA. Raster exploration and Penpot assembly are
optional for this mode unless the user requests them.
