---
name: svg-skill
description: >-
  Agent Skill for writing production-ready SVG markup — icons, logos,
  favicons, illustrations, flowcharts, charts, progress rings, sprites, CSS
  masks, and animated UI graphics. Use when the user asks for SVG, vector
  graphics, inline graphics, icon sets, data viz, or scalable web assets, or
  mentions this skill. Prefer over image generation when scaling, theming, CSS
  control, or code integration matters.
---

# svg-skill

Write SVG source directly. Do not use image generation tools.

## Scenario index

Match user intent, then pick a path. Details in [scenarios.md](scenarios.md), examples in [examples.md](examples.md).

| Scenario | viewBox | Key technique | Deliverable |
|----------|---------|---------------|-------------|
| UI icons (outline/filled) | 0 0 24 24 | `currentColor`, uniform stroke-width | file / React component |
| Multi-color icons | 0 0 24 24 | separate paths + fixed fill | file |
| Logo / brand mark | brand-specific | path outlines, safe margin | SVG + optional favicon |
| Favicon / app icon | 0 0 32 32 | minimal, high contrast | file |
| Illustration / empty state | 0 0 100 100+ | flat shapes | SVG / inline |
| Flowchart / architecture | content-based | marker arrows, grid alignment | UTF-8 file |
| Charts (bar/line/pie) | data-based | coords from scale | title + desc |
| Progress ring / bar | 0 0 24 24 or bar | `stroke-dasharray` or rect width | animatable |
| Avatar placeholder | 0 0 40 40 | circle + letter / person path | currentColor |
| Badge / label | text-based | rounded rect + text | brand colors |
| Wave divider | 0 0 1200 120 | bezier path, `preserveAspectRatio="none"` | full-width |
| Map pin | 0 0 24 24 | teardrop path + center dot | theme or fixed color |
| Sprite sheet | per symbol | `<symbol>` + `<use href="#id">` | single sprite.svg |
| CSS background / mask | same as icons | external file or data URI | CSS snippet |
| Loading animation | 0 0 24 24 | CSS `@keyframes` or stroke-dash | inline SVG |
| Hover interaction | any | CSS `:hover` on fill/stroke/transform | inline + style |
| Clip shapes | container-based | `<clipPath>` / `<mask>` | defs reference |
| Shadow / blur | any | `<filter>` feDropShadow / feGaussianBlur | defs reference |
| Pattern texture | small unit | `<pattern>` tile | defs reference |
| React / Vue component | 0 0 24 24 | spread props, camelCase attrs | .tsx / .vue |
| Email inline | minimal | no style/animation/external refs | compact snippet |

**Poor fit for SVG**: photorealism, complex bitmap textures, overly detailed illustration → explain limits, suggest raster or simplified style.

## Workflow

```
- [ ] 1. Pick scenario and deliverable from index
- [ ] 2. Set viewBox, style, colors (currentColor vs brand)
- [ ] 3. Draw: compose shapes → paths; shared assets in <defs>
- [ ] 4. A11y: decorative aria-hidden / informative title+desc
- [ ] 5. Minimize; non-ASCII text → UTF-8 + XML declaration
- [ ] 6. Write file; run validate.sh
```

### Skeleton templates

**Icon only (no text)**:

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" aria-hidden="true">
</svg>
```

**Non-ASCII text (CJK, etc.)**:

```svg
<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 120" role="img" aria-labelledby="title">
  <title id="title">Diagram title</title>
</svg>
```

**Full-width decor (waves, etc.)**:

```svg
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 120" preserveAspectRatio="none" aria-hidden="true">
</svg>
```

### Drawing rules

1. **viewBox defines logic coords**; display size via CSS `width`/`height`
2. **Monochrome UI assets** default to `currentColor`
3. **Outline icons** — `stroke-width="2"` `stroke-linecap="round"` `stroke-linejoin="round"`
4. **Shared defs** — gradients, arrows, patterns, filters in `<defs>`; short unique `id`s
5. **Charts** — compute margins and scale first; summarize data in title/desc
6. **Animation** — prefer CSS; SMIL only for simple cases
7. **Flat by default** — gradients/shadows only when needed

### Quality checklist

- [ ] `xmlns` + `viewBox`
- [ ] Non-ASCII text → UTF-8 + XML declaration
- [ ] No placeholder paths or editor metadata
- [ ] `id`s unique within file (sprites especially)
- [ ] Charts have title; decorative icons use aria-hidden
- [ ] File opens and previews standalone

```bash
bash scripts/validate.sh path/to/dir/
xmllint --noout file.svg   # required for non-ASCII text
```

### Deliverables

| Form | Approach |
|------|----------|
| Standalone `.svg` | write to `assets/` etc. |
| Sprite | multiple `<symbol>` in one file; document `<use>` usage |
| React | `export function XIcon(props: SVGProps<SVGSVGElement>)` + spread |
| Vue | `<template><svg ...></svg></template>` |
| CSS background | `background: url(...)` or inline data URI |
| CSS mask | `-webkit-mask: url(...)`; shape needs solid fill area |
| Inline HTML | raw `<svg>` fragment, no wrapper div (unless requested) |

## Do not

- Use `<image>` to fake vectors with bitmaps (unless explicitly requested)
- Use image generation for simple vectorizable graphics
- Leave empty paths / TODO placeholders
- Omit UTF-8 declaration for non-ASCII text
- Add complex animation or external deps for email use cases

## Tools & docs

| Resource | Description |
|----------|-------------|
| [scenarios.md](scenarios.md) | Per-scenario step-by-step |
| [reference.md](reference.md) | Path syntax, defs, frameworks |
| [examples.md](examples.md) | Full code samples |
| [scripts/validate.sh](scripts/validate.sh) | Batch validation |
