# Upstream integration review

CONFLICTS: automatic merge stopped safely to preserve Smart Design features. Resolve conflicts manually.

- Fork main base: c8a7d3f9d3c7c4dc1de950dd8b43ef7edd4073c8
- Upstream main: ed8aabd1dc5a88652a076631c2d46886ca60c031
- Source: https://github.com/storytold/designcraft

## Conflicting paths

~~~text
.github/workflows/ci.yml
.github/workflows/release.yml
Cargo.lock
ROADMAP.md
apps/designcraft/Cargo.toml
apps/designcraft/src/main.rs
crates/engine/src/cmd/export.rs
crates/engine/src/cmd/interchange.rs
crates/engine/src/cmd/mod.rs
crates/engine/src/cmd/package.rs
crates/engine/src/cmd/preflight.rs
crates/engine/src/cmd/print.rs
crates/idml/src/tests.rs
crates/pdf/src/export.rs
crates/render/src/lib.rs
crates/ui-egui/Cargo.toml
crates/ui-egui/src/about.rs
crates/ui-egui/src/chrome.rs
crates/ui-egui/src/dialogs.rs
crates/ui-egui/src/effects_dialog.rs
crates/ui-egui/src/lib.rs
crates/ui-egui/src/panels/datamerge.rs
crates/ui-egui/src/panels/notes.rs
crates/ui-egui/src/panels/pages.rs
crates/ui-egui/src/panels/swatches.rs
crates/ui-egui/src/rtl.rs
crates/ui-egui/src/story_editor.rs
crates/ui-egui/src/widgets.rs
docs/control-protocol.md
~~~

## Upstream changed paths

~~~text
M	.github/workflows/ci.yml
M	.github/workflows/freebsd.yml
M	.github/workflows/release.yml
M	.github/workflows/sonarcloud.yml
M	ASSETS.md
M	Cargo.lock
M	Cargo.toml
M	NOTICE
M	ROADMAP.md
M	apps/designcraft-cli/src/automation_args.rs
M	apps/designcraft-cli/src/main.rs
M	apps/designcraft-cli/src/perf.rs
M	apps/designcraft-cli/tests/it/cli.rs
M	apps/designcraft-web/src/web.rs
M	apps/designcraft/Cargo.toml
M	apps/designcraft/src/gpu/backend.rs
D	apps/designcraft/src/mac_window.rs
M	apps/designcraft/src/main.rs
M	apps/designcraft/src/native_menu.rs
M	contributors/people.toml
M	crates/color/src/ase.rs
M	crates/color/src/cms/mod.rs
M	crates/color/src/gradient.rs
M	crates/compose/src/breaker.rs
M	crates/compose/src/cache.rs
M	crates/compose/src/grep.rs
M	crates/compose/src/lib.rs
M	crates/compose/src/notes.rs
M	crates/compose/src/overlay.rs
M	crates/compose/src/parabox.rs
A	crates/compose/src/placed.rs
M	crates/compose/src/ruby.rs
M	crates/compose/src/shape.rs
M	crates/compose/src/table.rs
M	crates/compose/src/tests.rs
M	crates/compose/src/warichu.rs
M	crates/compose/src/wrap.rs
M	crates/doc/src/anchored.rs
M	crates/doc/src/assets.rs
M	crates/doc/src/attrs.rs
M	crates/doc/src/build.rs
M	crates/doc/src/edit.rs
M	crates/doc/src/endnotes.rs
A	crates/doc/src/file_info.rs
M	crates/doc/src/index.rs
M	crates/doc/src/item.rs
M	crates/doc/src/lib.rs
M	crates/doc/src/notes.rs
M	crates/doc/src/otf.rs
M	crates/doc/src/story.rs
M	crates/doc/src/styles.rs
M	crates/doc/src/swatch_refs.rs
M	crates/doc/src/table.rs
M	crates/engine/Cargo.toml
M	crates/engine/src/cmd/anchored.rs
M	crates/engine/src/cmd/anchors.rs
M	crates/engine/src/cmd/book.rs
M	crates/engine/src/cmd/datamerge/fill.rs
M	crates/engine/src/cmd/datamerge/mod.rs
M	crates/engine/src/cmd/datamerge/parse.rs
M	crates/engine/src/cmd/datamerge/tests.rs
M	crates/engine/src/cmd/edit.rs
M	crates/engine/src/cmd/editorial.rs
M	crates/engine/src/cmd/endnotes.rs
M	crates/engine/src/cmd/export.rs
M	crates/engine/src/cmd/file.rs
M	crates/engine/src/cmd/file/eps_place_tests.rs
A	crates/engine/src/cmd/file_info.rs
M	crates/engine/src/cmd/find.rs
M	crates/engine/src/cmd/flex.rs
M	crates/engine/src/cmd/fonts.rs
M	crates/engine/src/cmd/index.rs
M	crates/engine/src/cmd/interchange.rs
M	crates/engine/src/cmd/layout.rs
M	crates/engine/src/cmd/links.rs
M	crates/engine/src/cmd/lists.rs
M	crates/engine/src/cmd/load_styles.rs
M	crates/engine/src/cmd/mathexpr.rs
M	crates/engine/src/cmd/media.rs
M	crates/engine/src/cmd/mod.rs
M	crates/engine/src/cmd/notes.rs
M	crates/engine/src/cmd/object.rs
M	crates/engine/src/cmd/package.rs
M	crates/engine/src/cmd/paste_svg.rs
M	crates/engine/src/cmd/pdflayers.rs
M	crates/engine/src/cmd/place_text.rs
M	crates/engine/src/cmd/preflight.rs
A	crates/engine/src/cmd/prepress.rs
M	crates/engine/src/cmd/print.rs
M	crates/engine/src/cmd/style.rs
M	crates/engine/src/cmd/table.rs
M	crates/engine/src/cmd/text.rs
M	crates/engine/src/cmd/thread.rs
M	crates/engine/src/cmd/variables.rs
M	crates/engine/src/cmd/xref.rs
M	crates/engine/src/file_access.rs
M	crates/engine/src/guard.rs
M	crates/engine/src/io.rs
M	crates/engine/src/lib.rs
M	crates/engine/src/math.rs
M	crates/engine/src/recovery.rs
M	crates/engine/src/sample.rs
A	crates/engine/src/separations.rs
M	crates/engine/src/tests.rs
M	crates/engine/src/tests_ime.rs
M	crates/engine/src/tests_linked_paths.rs
M	crates/engine/src/tests_roots.rs
M	crates/engine/src/tests_table.rs
M	crates/engine/src/tooling.rs
A	crates/engine/tests/graphic_layer_options.rs
M	crates/engine/tests/it/pdf_backend.rs
A	crates/engine/tests/pdf_optional_content.rs
A	crates/engine/tests/support/optional_content.rs
A	crates/engine/tests/support/optional_content_fallback.rs
A	crates/engine/tests/support/optional_content_xref.rs
M	crates/epub/src/lib.rs
M	crates/epub/tests/it/export_tag_styles.rs
M	crates/epub/tests/it/grouped_reading_order.rs
M	crates/fonts/build.rs
M	crates/fonts/src/fontdb.rs
M	crates/fonts/src/tests_docfonts.rs
M	crates/fonts/src/tests_fallback.rs
M	crates/fonts/src/tests_vertical.rs
M	crates/format/src/lib.rs
M	crates/geom/src/path.rs
M	crates/geom/src/trace.rs
M	crates/geom/src/units.rs
M	crates/idml/src/export.rs
M	crates/idml/src/import.rs
M	crates/idml/src/names.rs
M	crates/idml/src/ruby.rs
M	crates/idml/src/tests.rs
M	crates/idml/src/tests/assets.rs
A	crates/idml/src/tests/facing_ltr.rs
A	crates/idml/src/tests/frame_grid.rs
M	crates/idml/src/xml.rs
M	crates/images/Cargo.toml
M	crates/images/src/lib.rs
M	crates/images/src/psd.rs
M	crates/mcp/src/server.rs
M	crates/mcp/src/tests.rs
M	crates/mcp/tests/it/automation_roots.rs
M	crates/pdf/Cargo.toml
M	crates/pdf/src/export.rs
M	crates/pdf/src/forms.rs
A	crates/pdf/src/gradient_inks.rs
A	crates/pdf/src/inks.rs
M	crates/pdf/src/lib.rs
A	crates/pdf/src/optional_content.rs
M	crates/pdf/src/pdfx.rs
A	crates/pdf/src/pdfx/fonts.rs
A	crates/pdf/src/pdfx/fonts/tests.rs
A	crates/pdf/src/pdfx1a.rs
A	crates/pdf/src/print_tests.rs
A	crates/pdf/src/registration_tests.rs
M	crates/pdf/src/rtl_tests.rs
M	crates/pdf/src/tests.rs
M	crates/pdf/src/text.rs
M	crates/pdf/src/transitions.rs
A	crates/pdf/src/x1a_tests.rs
M	crates/pdf/tests/it/gradient_color_spaces.rs
M	crates/pdf/tests/it/main.rs
A	crates/pdf/tests/it/spot_gradients.rs
M	crates/pdf/tests/support/image_content_cache_contract.rs
M	crates/pptx/src/tests.rs
M	crates/render/src/blendspace.rs
M	crates/render/src/images.rs
M	crates/render/src/lib.rs
M	crates/render/src/pdf_bounds.rs
M	crates/render/src/pdf_layers.rs
M	crates/render/src/text.rs
M	crates/render/tests/it/colorize.rs
M	crates/render/tests/it/gradient_inks.rs
M	crates/render/tests/it/gradients.rs
M	crates/render/tests/it/placed_pdf.rs
M	crates/textimport/src/docx.rs
M	crates/textimport/src/docx_numbering.rs
M	crates/textimport/src/lib.rs
M	crates/textimport/src/rtf.rs
M	crates/textimport/src/tagged.rs
M	crates/textimport/src/xlsx.rs
M	crates/tools/src/catalog.rs
M	crates/tools/src/gradient.rs
M	crates/tools/src/lib.rs
M	crates/tools/src/select.rs
M	crates/tools/src/snap.rs
M	crates/tools/src/tests.rs
M	crates/tools/src/text.rs
M	crates/ui-egui/Cargo.toml
M	crates/ui-egui/examples/ui_shot.rs
M	crates/ui-egui/src/about.rs
M	crates/ui-egui/src/canvas.rs
M	crates/ui-egui/src/chrome.rs
M	crates/ui-egui/src/control.rs
M	crates/ui-egui/src/credits.rs
M	crates/ui-egui/src/cursors.rs
M	crates/ui-egui/src/dialogs.rs
M	crates/ui-egui/src/dialogs/options.rs
M	crates/ui-egui/src/dock.rs
M	crates/ui-egui/src/effects_dialog.rs
M	crates/ui-egui/src/guide_selection_tests.rs
M	crates/ui-egui/src/i18n.rs
M	crates/ui-egui/src/i18n/ar.rs
M	crates/ui-egui/src/i18n/cs.rs
M	crates/ui-egui/src/i18n/de.rs
M	crates/ui-egui/src/i18n/fr.rs
M	crates/ui-egui/src/i18n/it.rs
M	crates/ui-egui/src/i18n/ja.rs
M	crates/ui-egui/src/i18n/ka.rs
M	crates/ui-egui/src/i18n/pl.rs
M	crates/ui-egui/src/i18n/pt_br.rs
M	crates/ui-egui/src/i18n/ru.rs
M	crates/ui-egui/src/i18n/uk.rs
M	crates/ui-egui/src/icons.rs
A	crates/ui-egui/src/keys.rs
M	crates/ui-egui/src/lib.rs
M	crates/ui-egui/src/menus.rs
M	crates/ui-egui/src/new_document.rs
M	crates/ui-egui/src/object_control.rs
A	crates/ui-egui/src/page_history.rs
M	crates/ui-egui/src/panel_docking.rs
M	crates/ui-egui/src/panel_docking_tests.rs
M	crates/ui-egui/src/panels/cell_stroke.rs
M	crates/ui-egui/src/panels/conditions.rs
M	crates/ui-egui/src/panels/datamerge.rs
M	crates/ui-egui/src/panels/font_menu.rs
M	crates/ui-egui/src/panels/glyphs.rs
M	crates/ui-egui/src/panels/interactive.rs
M	crates/ui-egui/src/panels/layers.rs
M	crates/ui-egui/src/panels/library.rs
M	crates/ui-egui/src/panels/mod.rs
M	crates/ui-egui/src/panels/notes.rs
M	crates/ui-egui/src/panels/pages.rs
M	crates/ui-egui/src/panels/properties.rs
A	crates/ui-egui/src/panels/separations.rs
A	crates/ui-egui/src/panels/story.rs
M	crates/ui-egui/src/panels/styles.rs
M	crates/ui-egui/src/panels/swatches.rs
M	crates/ui-egui/src/panels/table.rs
M	crates/ui-egui/src/panels/tabs.rs
M	crates/ui-egui/src/panels/typography.rs
M	crates/ui-egui/src/parent_canvas_tests.rs
M	crates/ui-egui/src/prefs.rs
M	crates/ui-egui/src/rtl.rs
A	crates/ui-egui/src/separations.rs
M	crates/ui-egui/src/shortcut_import.rs
A	crates/ui-egui/src/shortcuts_dialog.rs
M	crates/ui-egui/src/story_editor.rs
M	crates/ui-egui/src/taskbar.rs
M	crates/ui-egui/src/theme.rs
D	crates/ui-egui/src/title_bar.rs
M	crates/ui-egui/src/toolbar.rs
A	crates/ui-egui/src/updates_ui.rs
M	crates/ui-egui/src/widgets.rs
A	crates/ui-egui/tests/it/color_profiles_restart.rs
M	crates/ui-egui/tests/it/main.rs
M	docs/agents.md
M	docs/control-protocol.md
A	docs/egui-037-migration-plan.md
M	docs/file-format-parity.md
M	docs/gaps.md
A	docs/graphic-layer-options.md
M	docs/mcp.md
M	docs/pdf-backend.md
A	docs/pdf-layer-visibility.md
M	docs/roadmap.md
M	docs/target-app-parity.md
M	docs/typography-parity.md
M	docs/ui-parity.md
M	docs/user/guides/export-a-pdf.md
M	packaging/env.sh
M	packaging/linux/flatpak/ai.storyteller.designcraft.yml
M	packaging/linux/package.sh
M	packaging/macos/package.sh
M	packaging/windows/designcraft.wxs
M	packaging/windows/package.ps1
M	xtask/src/assets.rs
M	xtask/src/layers.rs
M	xtask/src/pdf_deps.rs
~~~
