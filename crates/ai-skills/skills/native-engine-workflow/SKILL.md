---
name: native-engine-workflow
description: Use when an agent executes drawing, text, styles, tables, lists, threading, QR codes, layout and export through native engine commands, محرك البرنامج أوامر رسم وكتابة وأنماط وجداول وقوائم وتنظيم تنفيذ سكربت وتدقيق ميزانية ومهارات.
---
# Native engine execution

Treat the current native command catalog as the executable contract. Translate the requested product and approved visual requirements into bounded commands and image placements. Do not invent command names, JSON fields, font availability or tool capabilities. Never emit `type.set`; use `story.setText` or `frame.create` `text`.

For each page, plan semantic sections, reading order, native frame bounds, reusable style systems (`style.paragraph.create`/`apply`, `style.character.create`/`apply`, `style.object.create`/`apply`), visual assets, structured tables (`table.insert`, `table.setCell`, `table.insertRow`, `table.insertColumn`, `table.mergeCells`, `table.setRowHeight`, `table.setColumnWidth`, `table.setHeaderFooterRows`, `table.setCellStyle`, `table.setTableStyle`), lists (`type.bulletedList`, `type.numberedList`), multi-frame text threading (`text.thread`, `text.unthread`), vector QR codes (`object.qrCode`), and response areas. Multiple activities, pictures and related subtopics are supported. Use points for native coordinates (mm × 72 / 25.4). Frame rectangles are [left, top, right, bottom]; spreads are zero-based. Resolve object, story and table identifiers from prior results such as `$0.id`, `$0.story` and `$0.table` (or target numeric IDs from `document.objects` when editing an opened document). Formatting acts on the current native selection: select/create the intended frame before `type.char` and `type.para`. Never hard-code document IDs in new-page batches.

Within reviewed batches create only the declared target spreads. Preserve previous pages, document fonts, styles and swatches. Make all text, captions, headers, footers, rules, diagrams and activity boxes native editable elements. Raster photographs remain independent image assets within native frames. The host inserts requested images; planning output must not supply file.place or execute external scripts.

Validate the text draft before requesting images, and validate the complete native draft before publishing it. On overflow, repair only the affected geometry or instruction within the correction allowance. Do not cut unseen characters, silently lower readability, skip preflight, reset the ledger or move to a paid provider. Respect request, token, cost and resource limits. Report actual usage separately from estimates.

Pin approved skill revisions for the task. Present each completed batch with native/PDF/PNG proofs and wait for the human’s exact checkpoint approval. Importing instructions grants no approval or executable permission. Save and export only when requested; printing requires the frozen reviewed artifact. See references/layout-contract.md.
