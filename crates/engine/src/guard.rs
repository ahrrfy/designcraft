//! Last-resort guard (craftrules `standards/never-crash.md`): a panic that escapes a command —
//! any edit, import or export, from the UI, the CLI, MCP or the control channel — becomes an
//! error, and the document is kept as it was before the command (and saved for crash recovery
//! when a recovery directory is set). It is a safety net: commands still must not panic.

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use crate::{EngineError, Result, Session};

/// Text of a panic payload (`panic!` with a literal or a formatted message).
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Log every panic (message and location) before it unwinds, then run the previous hook.
/// Installed once per process, when the first [`Session`] is made.
pub fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
            log::error!("panic{at}: {}", panic_message(info.payload()));
            previous(info);
        }));
    });
}

impl Session {
    /// Run command `id` (`f`) inside the guard.
    pub(crate) fn guarded(&mut self, id: &str, f: impl FnOnce(&mut Session) -> Result<Value>) -> Result<Value> {
        // Most commands only append to `history.undo`: moving `undo` out while cloning `self.docs`
        // avoids cloning up to 1,000 `HistoryEntry` items per open document on every command and
        // 60 Hz tool preview. Temporarily lifting `history.limit` prevents `push_undo` from
        // shifting index 0 during `f`, so panic rollback is an O(1) `truncate` and success trims
        // back to the saved limit afterwards.
        let borrow_undo = self.new_document_preview.is_none()
            && !matches!(
                id,
                "edit.undo" | "edit.redo" | "file.revert" | "file.close" | "file.new" | "script.run" | "production.apply" | "production.applyGroup"
            )
            && crate::cmd::find_command(id).is_some();
        let mut saved_undo: Vec<(u64, usize, usize)> = Vec::new();
        let mut taken_undo: Vec<Vec<crate::HistoryEntry>> = Vec::new();
        if borrow_undo {
            saved_undo.reserve(self.docs.len());
            taken_undo.reserve(self.docs.len());
            for d in &mut self.docs {
                saved_undo.push((d.uid, d.history.undo.len(), d.history.limit));
                taken_undo.push(std::mem::take(&mut d.history.undo));
            }
        }
        let mut docs = self.docs.clone();
        if borrow_undo {
            for (d, undo) in self.docs.iter_mut().zip(taken_undo) {
                d.history.undo = undo;
                d.history.limit = usize::MAX;
            }
        }
        let active = self.active;
        let journal = self.journal.len();
        let ui_requests = self.ui_requests.len();
        let in_undo_step = self.in_undo_step;
        let prepared_print = self.prepared_print.clone();
        let prefs = self.prefs.clone();
        let last_selection_tool = self.last_selection_tool;
        let clipboard = self.clipboard.clone();
        let clipboard_origin = self.clipboard_origin;
        let text_clipboard = self.text_clipboard.clone();
        let loaded = self.loaded;
        let untitled = self.untitled;
        let transforms = self.transforms.clone();
        let text_drag = self.text_drag;
        let text_unit = self.text_unit;
        let typing_format = self.typing_format.clone();
        let library = self.library.clone();
        let book = self.book.clone();
        let conveyor = self.conveyor.clone();
        let new_document_preview = self.new_document_preview;
        let text_link_checks = self.text_link_checks.clone();
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(r) => {
                for (uid, _, limit) in saved_undo {
                    if let Some(d) = self.docs.iter_mut().find(|d| d.uid == uid) {
                        d.history.limit = limit;
                        let max = limit.max(1);
                        if d.history.undo.len() > max {
                            let excess = d.history.undo.len() - max;
                            d.history.undo.drain(..excess);
                        }
                    }
                }
                r
            }
            Err(payload) => {
                let msg = panic_message(payload.as_ref());
                // Keep the documents and session state as they were before the command: a nested
                // edit that succeeded before the panic is rolled back with its history entry, a
                // document opened or switched by the command is gone, and redo it cleared is back.
                for (uid, orig_len, limit) in saved_undo {
                    if let (Some(src), Some(dst)) = (self.docs.iter_mut().find(|d| d.uid == uid), docs.iter_mut().find(|d| d.uid == uid)) {
                        let mut undo = std::mem::take(&mut src.history.undo);
                        undo.truncate(orig_len);
                        dst.history.undo = undo;
                        dst.history.limit = limit;
                    }
                }
                self.docs = docs;
                self.active = active;
                self.journal.truncate(journal);
                self.ui_requests.truncate(ui_requests);
                self.prepared_print = prepared_print;
                self.prefs = prefs;
                self.last_selection_tool = last_selection_tool;
                self.clipboard = clipboard;
                self.clipboard_origin = clipboard_origin;
                self.text_clipboard = text_clipboard;
                self.loaded = loaded;
                self.untitled = untitled;
                self.transforms = transforms;
                self.text_drag = text_drag;
                self.text_unit = text_unit;
                self.typing_format = typing_format;
                self.library = library;
                self.book = book;
                self.conveyor = conveyor;
                self.new_document_preview = new_document_preview;
                self.text_link_checks = text_link_checks;
                self.reset_find_scope();
                if let Some(st) = self.active_mut() {
                    st.interaction = None;
                    st.bump_revision();
                }
                if let Some(dir) = self.recovery_dir.clone()
                    && let Err(e) = crate::recovery::save(self, &dir)
                {
                    log::error!("crash recovery save after `{id}` failed: {e}");
                }
                // The command never got to say that it is done; a command that ran it goes on.
                self.in_undo_step = in_undo_step;
                log::error!("command `{id}` panicked: {msg}");
                Err(EngineError::Internal(id.to_string(), msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn a_panicking_command_is_an_error_and_keeps_the_document() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [36, 36, 200, 200], "content": "text", "text": "keep me"})).unwrap();
        let before = s.active().unwrap().doc.clone();
        // A command that edits the document and then panics half way.
        let err = s
            .guarded("test.panic", |s| {
                s.execute("frame.create", &json!({"rect": [36, 236, 200, 400]}))?;
                panic!("boom");
            })
            .unwrap_err();
        assert!(err.to_string().contains("test.panic") && err.to_string().contains("boom"), "{err}");
        let st = s.active().unwrap();
        assert!(std::sync::Arc::ptr_eq(&st.doc, &before), "the document is the one from before the command");
        // The session keeps working.
        s.execute("frame.create", &json!({"rect": [36, 236, 200, 400]})).unwrap();
    }

    /// An undoable command that panics is no longer running: the next command records its own
    /// undo step.
    #[test]
    fn a_panicking_command_does_not_swallow_later_undo_steps() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let panicked = s.guarded("test.panic", |s| {
            s.in_undo_step = true;
            panic!("boom");
        });
        assert!(panicked.is_err());
        s.execute("frame.create", &json!({"rect": [36, 36, 200, 200]})).unwrap();
        assert_eq!(s.active().unwrap().history.undo.len(), 1);
    }

    /// A command that panics inside an undoable one leaves that one running: what it runs next
    /// is still part of its undo step.
    #[test]
    fn a_panic_in_a_command_run_by_another_leaves_the_outer_undo_step() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        // An undoable command that runs one that panics, carries on and runs another.
        s.guarded("test.outer", |s| {
            s.in_undo_step = true;
            assert!(s.guarded("test.panic", |_| panic!("boom")).is_err());
            let made = s.execute("frame.create", &json!({"rect": [36, 36, 200, 200]}));
            s.in_undo_step = false;
            made
        })
        .unwrap();
        let st = s.active().unwrap();
        assert_eq!(st.doc.spreads[0].items.len(), 1);
        assert_eq!(st.history.undo.len(), 0, "the frame is left to the outer command's step");
    }

    /// A nested edit that succeeded before the panic is rolled back with its history entry, and
    /// the redo it cleared is back (#742).
    #[test]
    fn a_panicking_command_restores_history_with_the_document() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [36, 36, 200, 200]})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        let (undo, redo) = {
            let h = &s.active().unwrap().history;
            (h.undo.len(), h.redo.len())
        };
        assert_eq!(redo, 1, "there is redo to lose");
        let err = s
            .guarded("test.panic", |s| {
                s.execute("frame.create", &json!({"rect": [36, 236, 200, 400]}))?;
                panic!("boom");
            })
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        let st = s.active().unwrap();
        assert_eq!(st.history.undo.len(), undo, "the nested edit's undo entry is gone with the edit");
        assert_eq!(st.history.redo.len(), redo, "the redo the nested edit cleared is back");
        assert_eq!(st.doc.spreads[0].items.len(), 0, "the document is the one from before the command");
    }

    /// A command that opens another document and then panics leaves none of it behind (#742).
    #[test]
    fn a_panicking_command_restores_the_document_set() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let err = s
            .guarded("test.panic", |s| {
                s.execute("file.new", &json!({}))?;
                assert_eq!(s.docs.len(), 2);
                panic!("boom");
            })
            .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        assert_eq!(s.docs.len(), 1, "the document the command opened is gone");
        assert_eq!(s.active, Some(0), "the document from before the command is active");
    }

    #[test]
    fn guarded_avoids_cloning_undo_history_and_rolls_back_session_fields_on_panic() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [36, 36, 120, 120]})).unwrap();
        let id = s.execute("frame.create", &json!({"rect": [140, 36, 220, 120]})).unwrap()["id"].as_u64().unwrap();
        assert_eq!(s.active().unwrap().history.undo.len(), 2);

        // `undo[1].doc` is only held by `history.undo[1]` (`undo[0].doc` is also `saved_doc`): if
        // `guarded` cloned `history.undo`, its `Arc` strong count inside a command/preview would be > 2.
        let entry_doc = s.active().unwrap().history.undo[1].doc.clone();
        assert_eq!(std::sync::Arc::strong_count(&entry_doc), 2);
        s.guarded("transform.move", |_| {
            assert_eq!(std::sync::Arc::strong_count(&entry_doc), 2);
            Ok(json!(null))
        })
        .unwrap();
        s.begin_interaction("Move").unwrap();
        s.preview("transform.move", json!({"ids": [id], "dx": 10, "dy": 5})).unwrap();
        assert_eq!(std::sync::Arc::strong_count(&entry_doc), 2);
        s.cancel_interaction();

        // Snapshot session-level fields and verify a panicking command rolls every one of them back.
        let orig_smart = s.prefs.smart_text_reflow;
        let orig_origin = s.clipboard_origin;
        let orig_untitled = s.untitled;
        let err = s
            .guarded("frame.create", |s| {
                s.prefs.smart_text_reflow = !orig_smart;
                s.clipboard_origin = 999.0;
                s.text_clipboard = Some((std::sync::Arc::new(designcraft_doc::Story::new(designcraft_doc::StoryId(1))), "leaked".into()));
                s.loaded = Some(crate::LoadedGraphic { asset: designcraft_doc::AssetId(77), size: (10.0, 10.0) });
                s.book = Some(crate::cmd::book::Book { path: "panic.dcbook".into(), documents: vec![], style_source: 0 });
                s.library = Some(crate::cmd::library::Library { path: Some("panic.dclib".into()), items: vec![] });
                s.conveyor.push(("leaked".into(), vec![1, 2, 3]));
                s.untitled = 42;
                s.transforms.0.push(("transform.move".into(), json!({"dx": 5})));
                s.ui_requests.push(crate::UiRequest::View { params: json!({"fit": true}) });
                s.execute("frame.create", &json!({"rect": [36, 200, 120, 280]}))?;
                panic!("rollback everything");
            })
            .unwrap_err();
        assert!(err.to_string().contains("rollback everything"), "{err}");
        assert_eq!(s.active().unwrap().history.undo.len(), 2, "borrowed undo restored and truncated");
        assert_eq!(std::sync::Arc::strong_count(&entry_doc), 2);
        assert_eq!(s.prefs.smart_text_reflow, orig_smart);
        assert_eq!(s.clipboard_origin, orig_origin);
        assert!(s.text_clipboard.is_none());
        assert!(s.loaded.is_none());
        assert!(s.book.is_none());
        assert!(s.library.is_none());
        assert!(s.conveyor.is_empty());
        assert_eq!(s.untitled, orig_untitled);
        assert!(s.transforms.0.is_empty());
        assert!(s.ui_requests.is_empty());
    }
}
