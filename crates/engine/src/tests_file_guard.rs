//! The guarantee behind `--automation-read-root` and `--automation-write-root`: every file a
//! command reads or writes by path goes through a checked reader, in every crate a command can
//! reach, not only this one.
//!
//! In the engine that reader is the [`crate::io`] module. The lower crates cannot depend on the
//! engine (`cargo xtask layers`), so they either have no file access at all or take a reader or
//! an access check from the caller (`designcraft_idml::import_idml_with`,
//! `designcraft_fonts::FontDb::load_document_fonts`).
//!
//! The scan reads the source of every crate under `crates/` and flags what touches the file
//! system, however it is imported or spelled: `std::fs`, a bare `fs::` after `use std::fs` or
//! `use std::{fs, ...}`, `File::open`/`File::create`, `OpenOptions`, and the `Path` probes
//! (`.metadata(`, `.exists(`, `.canonicalize(`, `.read_dir(`...), as methods or as
//! `Path::exists`. The host applications under `apps/` are out of scope: they run with the
//! operator's own authority, and the roots are applied inside the process they start.

use std::path::{Path, PathBuf};

/// Files that may touch the file system, with why. Paths are relative to `crates/`. A file not
/// listed here, and not a test file or test module, must not.
const ALLOWED: &[(&str, &str)] = &[
    (
        "ui-egui/src/preview_update.rs",
        "The desktop launcher selects its manifest through the host environment; bounded manifest reads use engine::io. The update command refuses confined automation before any polling or process launch. Direct writes create only a new fixed-prefix temporary copy of the binary's embedded updater (never downloaded or command-selected code) and remove that exact owned temporary file. The external updater runs with explicit unrestricted desktop authority and preserves the existing profile and documents.",
    ),
    (
        "ai-runtime/src/vault.rs",
        "The native host supplies its already-validated private profile directory. Canonicalization establishes a hashed credential namespace only; this module reads no profile file contents and creates no credential files. Secret persistence uses the OS credential store, isolated by profile, provider and normalized endpoint, with no command-selected filesystem destination.",
    ),
    (
        "ai-runtime/src/batch.rs",
        "Batch proof directories and proof reads are checked against the host's pinned private profile authority; fixed checkpoint/history names and validated artifact names are written through checked profile helpers. Requested final export directories are checked before creation, and each final file goes through an engine command under the originating job's captured automation roots. Recovered checkpoint files confer no approval or continuation authority.",
    ),
    (
        "ai-runtime/src/indesign.rs",
        "The optional installed-InDesign COM bridge stages only engine-produced, native-only IDML. Every requested output, staging, audit and bounded-read path is checked against captured AutomationRoots before filesystem effects, including asynchronous completion; final and audit files use create_new and existing outputs are refused. The external helper runs fixed code with paths passed as environment data; it does not inspect installation internals, accept arbitrary scripts, or terminate the user's application.",
    ),
    (
        "ai-runtime/src/localserver.rs",
        "Only the explicit desktop executable selection reaches this module. The selected local ollama.exe path is checked against automation read roots before canonicalization or metadata probes, and the resolved path is checked again. Launch uses fixed serve arguments and a restricted environment; lifecycle operations address only the child this host started. This does not sandbox that external program's own filesystem access.",
    ),
    (
        "ai-runtime/src/output.rs",
        "The host's fixed print-proofs directory and bounded hash-token filenames are checked against pinned private profile authority before creation and engine writes. Proof reads use capped engine I/O under that same authority and verify the frozen PDF hash before review or submission. Requested external saves retain caller automation roots and use the engine's checked frozen-PDF save command; durable attempt records precede submission of the existing immutable engine artifact.",
    ),
    (
        "ai-runtime/src/runtime/state.rs",
        "The desktop host chooses the private profile root; direct profile initialization, mtime checks, and portable skills catalog/member link validation read only fixed or validated descendants.",
    ),
    (
        "ai-runtime/src/runtime/dispatch.rs",
        "Every command-named skill import/export or run output path is checked against the effective automation roots before probing or creating it, and asynchronous publication retains the originating roots.",
    ),
    (
        "ai-runtime/src/runtime/proofs.rs",
        "Batch proof refresh directories live under the pinned private profile root and are checked against profile authority before creation.",
    ),
    (
        "ai-runtime/src/runtime/persistence.rs",
        "Settings, ledger, and draft persistence write atomically only to fixed descendants inside the host's pinned private profile root.",
    ),
    (
        "ai-runtime/src/references.rs",
        "Selected reference paths are checked against the originating automation read roots before metadata probes and each bounded open; worker execution retains those roots. Reference decoding does not open linked resources.",
    ),
    (
        "ai-runtime/src/production_cache.rs",
        "The native host supplies a fixed private-profile cache directory, never a provider-selected path. Captured automation roots are checked before directory probes, scans and creation; ancestor metadata rejects links/reparse points without reading ancestor contents. Entry names are fixed SHA-256 hex digests and kinds, reads use capped engine I/O, and atomic writes use the engine gateway. Quotas, corruption and link tests bound reuse; cached data grants no execution or approval authority.",
    ),
    (
        "ai-runtime/src/science.rs",
        "The requested output directory is checked against automation write roots before creation; every generated file is written through engine::io or an engine export command.",
    ),
    (
        "ai-skills/src/lib.rs",
        "The registry root is chosen by the desktop host. Command-named import/export paths are checked by ai-runtime before entry; internal revision/evidence/catalog paths use fixed names or validated identifiers and package link checks.",
    ),
    (
        "ai-skills/src/package.rs",
        "ai-runtime checks command-named package sources and destinations against automation roots before entry. Folder traversal rejects symbolic links and reparse points; package members are bounded validated relative names, and exports create new destinations without following existing files.",
    ),
    ("engine/src/io.rs", "the checked gateway: every read and write a command makes"),
    ("engine/src/file_access.rs", "the root check itself (resolves links with the file system)"),
    ("engine/src/atomic.rs", "how a write reaches the disk; commands call it only through io::write_file and io::replace_with"),
    (
        "engine/src/recovery.rs",
        "the unsaved documents live in a folder the host application chooses (a command's own `dir` is checked in cmd/file.rs before it gets here)",
    ),
    (
        "engine/src/cmd/package.rs",
        "`OutputRoot` checks the folder against the write root before it creates anything and then only writes inside it. Its reads are of the font files that draw the document's text (installed fonts and the `Document Fonts` folder), never of a path a command names",
    ),
    (
        "ai-runtime/src/resources.rs",
        "Read-only Linux /sys/class/drm sysfs probes for local GPU VRAM telemetry using fixed kernel device entries; never reads or writes user, document, or command-named paths.",
    ),
    (
        "fonts/src/fontdb.rs",
        "the installed-font scan reads the platform's fixed font folders (`system_font_dirs`: no command parameter or document field names one), and `load_document_fonts` asks the caller's `allow` check about its folder and each font file before opening either",
    ),
];

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

/// Test files (`tests.rs`, `tests_*.rs`, `*_tests.rs`, anything under a `tests/` folder) build
/// their own fixtures on disk.
fn is_test_file(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with("tests") || name.ends_with("_tests.rs") || name == "testing.rs" || rel.split('/').any(|part| part == "tests")
}

/// The file's lines but its test modules: a module is skipped from its `#[cfg(test)]` (or
/// `#[cfg(all(test, ...))]`) to the closing brace at the same indent. A `mod x;` declaration
/// has no body to skip.
fn code_outside_tests(text: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut code = vec![];
    let mut i = 0;
    while i < lines.len() {
        let attr = lines[i].trim();
        let module = lines.get(i + 1).map(|l| l.trim_start().trim_start_matches("pub "));
        if attr.starts_with("#[cfg(") && attr.contains("test") && module.is_some_and(|m| m.starts_with("mod ")) {
            let header = lines[i + 1];
            i += 2;
            if header.trim_end().ends_with('{') {
                let indent = &header[..header.len() - header.trim_start().len()];
                let close = format!("{indent}}}");
                while i < lines.len() && lines[i].trim_end() != close {
                    i += 1;
                }
                i += 1;
            }
            continue;
        }
        code.push((i + 1, lines[i]));
        i += 1;
    }
    code
}

/// `line` without its `//` comment (a `//` inside a string or char is rare enough in this code
/// to leave alone: it only hides the rest of that line from the scan).
fn without_comment(line: &str) -> &str {
    line.find("//").map_or(line, |at| &line[..at])
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Does `line` contain `word` not glued to a longer identifier on its left (`std::fs::` and
/// ` fs::` match `fs::`; `ofs::` does not)?
fn has_word(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(at, _)| !line[..at].chars().next_back().is_some_and(is_ident))
}

/// What in `line` reaches the file system, or `None`.
fn file_system_use(line: &str) -> Option<String> {
    let line = without_comment(line);
    // `std::fs::read`, `fs::read` after `use std::fs;` or `use std::{fs, io};`, `std::os::unix::fs::...`.
    for word in ["fs::", "File::open", "File::create", "File::options", "OpenOptions", "atomic::write_atomic"] {
        if has_word(line, word) {
            return Some(word.into());
        }
    }
    // The probes on a path, as `p.exists()` or `Path::exists`.
    for method in ["metadata", "symlink_metadata", "canonicalize", "read_dir", "try_exists", "exists", "is_file", "is_dir", "read_link"] {
        if line.contains(&format!(".{method}(")) || line.contains(&format!("Path::{method}")) {
            return Some(method.to_string());
        }
    }
    None
}

/// `use` statements (which can span lines) that bring the file system module in under any name:
/// `use std::{fs as files, io};` leaves no `fs::` to find at the call.
fn imports_fs(text: &str) -> Vec<usize> {
    let mut hits = vec![];
    let mut statement = String::new();
    let mut start = 0;
    for (n, line) in code_outside_tests(text) {
        let line = without_comment(line);
        if statement.is_empty() {
            if !line.trim_start().starts_with("use ") && !line.trim_start().starts_with("pub use ") {
                continue;
            }
            start = n;
        }
        statement.push_str(line);
        statement.push(' ');
        if line.contains(';') {
            let words: Vec<&str> = statement.split(|c: char| !is_ident(c)).collect();
            if words.contains(&"fs") && (words.contains(&"std") || words.contains(&"tokio") || words.contains(&"async_std")) {
                hits.push(start);
            }
            statement.clear();
        }
    }
    hits
}

/// Every offending line of the crate sources under `crates_dir`, minus [`ALLOWED`] and test code.
fn offenders(crates_dir: &Path) -> (Vec<String>, usize) {
    let mut files = vec![];
    rust_files(crates_dir, &mut files);
    let mut scanned = 0;
    let mut found = vec![];
    for path in files {
        let rel = path.strip_prefix(crates_dir).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        // `crates/<name>/src/...` only: build scripts, benches and `tests/` are not shipped code.
        if rel.split('/').nth(1).is_none_or(|d| d != "src") || is_test_file(&rel) || ALLOWED.iter().any(|(allowed, _)| *allowed == rel) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        scanned += 1;
        for (n, line) in code_outside_tests(&text) {
            if let Some(what) = file_system_use(line) {
                found.push(format!("crates/{rel}:{n}: `{what}`: {}", line.trim()));
            }
        }
        for n in imports_fs(&text) {
            found.push(format!("crates/{rel}:{n}: imports the file system module"));
        }
    }
    (found, scanned)
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Every command reads and writes files through [`crate::io`], in the engine and in every crate
/// below it: none may reach the file system itself, so the automation roots cannot be walked
/// around by a path that one of them opens. The exceptions are [`ALLOWED`], each with its reason.
#[test]
fn no_crate_touches_the_file_system_outside_the_checked_readers() {
    let (found, scanned) = offenders(&crates_dir());
    assert!(scanned > 100, "the scan found only {scanned} source files: has the layout moved?");
    assert!(
        found.is_empty(),
        "route these through crate::io (engine) or a reader the caller injects (lower crates), or add the file to ALLOWED with the reason it is safe: {found:#?}"
    );
}

/// The allow list names files that exist, so a rename cannot leave a stale exception behind (or
/// quietly exempt a new file of the same name).
#[test]
fn every_allowed_file_exists_and_still_needs_its_exception() {
    for (rel, reason) in ALLOWED {
        let path = crates_dir().join(rel);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(!reason.is_empty());
        let touches = code_outside_tests(&text).iter().any(|(_, line)| file_system_use(line).is_some()) || !imports_fs(&text).is_empty();
        assert!(touches, "{rel} no longer touches the file system: drop it from ALLOWED");
    }
}

/// The scan must not be foolable by how a call is spelled.
#[test]
fn the_scan_sees_every_spelling() {
    let flagged = [
        "let b = std::fs::read(p)?;",
        "let b = fs::read(p)?;",
        "    fs::write(p, b)",
        "let f = File::open(p)?;",
        "let f = std::fs::File::create(p)?;",
        "let f = OpenOptions::new().write(true).open(p)?;",
        "let m = path.metadata()?;",
        "let m = path.symlink_metadata()?;",
        "let c = path.canonicalize()?;",
        "for e in path.read_dir()? {}",
        "if path.try_exists()? {}",
        "if p.exists() {}",
        "if p.is_file() {}",
        "if p.is_dir() {}",
        "let l = p.read_link()?;",
        "files.retain(Path::exists);",
        "crate::atomic::write_atomic(p, b)?;",
        "(a, fs::read(x))",
    ];
    for line in flagged {
        assert!(file_system_use(line).is_some(), "not flagged: {line}");
    }
    let fine = [
        "let b = crate::io::read_file(p)?;",
        "if crate::io::is_dir(p) {}",
        "let m = crate::io::metadata(p)?;",
        "// std::fs::read(p) is what io wraps",
        "let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);",
        "let ofs::x = 1;",
    ];
    for line in fine {
        assert!(file_system_use(line).is_none(), "wrongly flagged: {line}");
    }
    for import in ["use std::fs;", "use std::{fs, io};", "use std::{\n    io,\n    fs as files,\n};", "use std::fs::File;"] {
        assert!(!imports_fs(import).is_empty(), "import not flagged: {import}");
    }
    assert!(imports_fs("use std::io::Read;\nuse std::path::Path;").is_empty());
    assert!(imports_fs("#[cfg(test)]\nmod tests {\n    use std::fs;\n}\n").is_empty(), "test modules are skipped");
}
