//! Reading and writing the files a command names by path.
//!
//! Every such read and write goes through this module, so the automation roots
//! ([`crate::file_access`]) are checked in one place. A command module that called `std::fs`
//! itself would bypass them: a test in `file_access` keeps that from happening.
//!
//! A path the roots refuse reads as a file that isn't there where the caller only asks whether
//! it is ([`is_file`], [`is_dir`], [`exists`], [`metadata`]), and as the roots' own message where
//! it asks for the bytes.

pub use craft_docpath::Refused;
pub use std::fs::Metadata;
use std::path::{Path, PathBuf};

/// Maximum number of bytes any unbounded file-reading helper in [`crate::io`] will read (512 MiB).
pub const MAX_READ_BYTES: u64 = 512 * 1024 * 1024;

/// The path to open for a path stored in a document (a link, a data source, a book chapter),
/// resolved against `document`, the path of the file it came from, when there is one.
///
/// Paths to other computers (other than the document's own network share), devices and URLs are
/// refused, and so is a result that isn't absolute: without a document, a relative path would
/// depend on the working directory, and a Windows path names no file on other platforms. A
/// refused path is never handed to the file system; callers treat it as a missing file.
pub fn document_path(stored: &str, document: Option<&Path>) -> Result<PathBuf, Refused> {
    let document = document.map(|d| std::path::absolute(d).unwrap_or_else(|_| d.to_path_buf()));
    let path = craft_docpath::resolve(stored, document.as_deref())?;
    if path.is_absolute() { Ok(path) } else { Err(Refused::Invalid) }
}

/// [`document_path`] for a path stored in a file inside `folder` (a data file's image names).
pub fn folder_path(stored: &str, folder: &Path) -> Result<PathBuf, Refused> {
    // Resolved against a document in the folder: only the document's folder is used.
    document_path(stored, Some(&folder.join("-")))
}

#[cfg(test)]
thread_local! {
    static TOUCHED: std::cell::RefCell<Option<Vec<PathBuf>>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` and list every path it handed to this module, in order (on this thread).
#[cfg(test)]
pub(crate) fn touched<T>(f: impl FnOnce() -> T) -> (T, Vec<PathBuf>) {
    let previous = TOUCHED.with(|t| t.replace(Some(Vec::new())));
    let out = f();
    let seen = TOUCHED.with(|t| t.replace(previous)).unwrap_or_default();
    (out, seen)
}

fn touch(path: &Path) {
    #[cfg(test)]
    TOUCHED.with(|t| {
        if let Some(seen) = t.borrow_mut().as_mut() {
            seen.push(path.to_path_buf());
        }
    });
    #[cfg(not(test))]
    let _ = path;
}

fn open_regular(path: &Path) -> std::io::Result<(std::fs::File, Metadata)> {
    // Check `stat` before `open` so a Unix FIFO without a writer cannot block in `open(2)`, then
    // verify `fstat` on the opened handle so a concurrent swap to a directory/FIFO/device is caught.
    let pre = std::fs::metadata(path)?;
    if !pre.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let file = std::fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok((file, meta))
}

fn read_regular_capped(path: &Path, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let (file, meta) = open_regular(path)?;
    let too_large =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, format!("file exceeds maximum read size of {} MiB", max_bytes / (1024 * 1024)));
    if meta.len() > max_bytes {
        return Err(too_large());
    }
    let mut buf = Vec::with_capacity(usize::try_from(meta.len().min(64 * 1024 * 1024)).unwrap_or(0));
    file.take(max_bytes.saturating_add(1)).read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(too_large());
    }
    Ok(buf)
}

/// The bytes of the regular file at `path` (capped at [`MAX_READ_BYTES`]), inside the automation
/// read root when there is one.
pub fn read_file(path: impl AsRef<Path>) -> Result<Vec<u8>, String> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path)?;
    read_regular_capped(path, MAX_READ_BYTES).map_err(|e| format!("{}: {e}", path.display()))
}

/// [`read_file`] with the file system's own error, for callers that tell "missing" from other
/// failures. A refused path is a `PermissionDenied` error carrying the roots' message.
pub fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    read_regular_capped(path, MAX_READ_BYTES)
}

/// [`read`] that stops past `cap` bytes (clamped to [`MAX_READ_BYTES`]): at most `cap + 1` bytes
/// are read from a verified regular file, so a device or a file that grows while it is read can't
/// exhaust memory. The caller compares the length with `cap`.
pub fn read_capped(path: impl AsRef<Path>, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    let (file, _) = open_regular(path)?;
    let limit = cap.min(MAX_READ_BYTES);
    let mut b = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut b)?;
    Ok(b)
}

/// The regular file at `path` opened for reading, inside the automation read root when there is
/// one, for callers that read only part of it. A refused path is a `PermissionDenied` error
/// carrying the roots' message.
pub fn open(path: impl AsRef<Path>) -> std::io::Result<std::fs::File> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    open_regular(path).map(|(file, _)| file)
}

/// Open the regular file at `path` for reading together with its `fstat` byte length, inside the
/// automation read root when there is one.
pub fn open_with_len(path: impl AsRef<Path>) -> Result<(std::fs::File, u64), String> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path)?;
    open_regular(path).map(|(file, meta)| (file, meta.len())).map_err(|e| format!("{}: {e}", path.display()))
}

/// The text of the regular file at `path` (capped at [`MAX_READ_BYTES`]), inside the automation
/// read root when there is one.
pub fn read_to_string(path: impl AsRef<Path>) -> Result<String, String> {
    let path = path.as_ref();
    let bytes = read_file(path)?;
    String::from_utf8(bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write `bytes` to the file at `path`, inside the automation write root when there is one.
pub fn write_file(path: impl AsRef<Path>, bytes: &[u8]) -> Result<(), String> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_write(path)?;
    // Replaced atomically where there is a real file system (see `atomic`).
    #[cfg(not(target_arch = "wasm32"))]
    let written = crate::atomic::write_atomic(path, bytes);
    #[cfg(target_arch = "wasm32")]
    let written = std::fs::write(path, bytes);
    written.map_err(|e| format!("{}: {e}", path.display()))
}

/// Atomically replace the file at `path` with the contents of `staged`, a private temporary file
/// the engine made itself (so it is not checked), inside the automation write root when there is
/// one.
#[cfg(not(target_arch = "wasm32"))]
pub fn replace_with(path: impl AsRef<Path>, staged: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_write(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    let mut input = std::fs::File::open(staged)?;
    crate::atomic::write_atomic_with(path, |file| std::io::copy(&mut input, file).map(|_| ()))
}

/// Could the file at `path` be opened for writing (without truncating it)? Inside the automation
/// write root when there is one.
pub fn writable(path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_write(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    std::fs::OpenOptions::new().write(true).open(path).map(drop)
}

/// The canonical form of `path` (links resolved), inside the automation read root when there is
/// one.
pub fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<std::path::PathBuf> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    std::fs::canonicalize(path)
}

/// What the file system says about `path` (links followed), inside the automation read root when
/// there is one. A refused path is a `PermissionDenied` error carrying the roots' message.
pub fn metadata(path: impl AsRef<Path>) -> std::io::Result<Metadata> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    std::fs::metadata(path)
}

/// Is there a file at `path` that automation may read?
pub fn is_file(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|m| m.is_file())
}

/// Is there a folder at `path` that automation may read?
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok_and(|m| m.is_dir())
}

/// Is there anything at `path` that automation may read?
pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

/// The entries of the folder at `path`, inside the automation read root when there is one.
pub fn read_dir(path: impl AsRef<Path>) -> std::io::Result<std::fs::ReadDir> {
    let path = path.as_ref();
    touch(path);
    crate::file_access::check_read(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    std::fs::read_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_helpers_reject_directories_and_files_above_max_read_bytes() {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("dc-io-cap-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Non-regular file (directory) is rejected before/after open.
        let dir_err = read_file(&dir).unwrap_err();
        assert!(dir_err.contains("not a regular file"), "{dir_err}");
        assert!(read(&dir).is_err());
        assert!(read_capped(&dir, 64).is_err());
        assert!(open(&dir).is_err());

        // Regular file within cap reads cleanly.
        let small = dir.join("small.txt");
        std::fs::write(&small, b"hello world").unwrap();
        assert_eq!(read_file(&small).unwrap(), b"hello world");
        assert_eq!(read_to_string(&small).unwrap(), "hello world");

        // Sparse file reporting `MAX_READ_BYTES + 1` is rejected without reading 512 MiB into RAM.
        let huge = dir.join("huge.bin");
        if std::fs::File::create(&huge).and_then(|f| f.set_len(MAX_READ_BYTES + 1)).is_ok() {
            let err = read_file(&huge).unwrap_err();
            assert!(err.contains("exceeds maximum read size"), "{err}");
            assert!(read(&huge).is_err());
            assert!(read_to_string(&huge).is_err());
        }

        // `read_regular_capped` also catches a file whose bytes exceed a smaller cap via `Read::take`.
        let over_custom = read_regular_capped(&small, 5).unwrap_err();
        assert_eq!(over_custom.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
