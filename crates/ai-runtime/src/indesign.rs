//! Optional Windows bridge to a normally installed InDesign application, using only its
//! documented COM document API. No Adobe installation files are inspected or copied.
//! Native-only exports are deliberately required to prevent linked-resource IO outside roots.
#[cfg(target_os = "windows")]
use base64::Engine;
use designcraft_engine::{Session, file_access};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

const MAX_FILE: u64 = 128 * 1024 * 1024;
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndesignExport {
    pub path: String,
    pub audit_path: String,
    pub version: String,
    pub pages: u64,
    pub stories: u64,
    pub text_frames: u64,
    pub page_items: u64,
    pub shapes: u64,
    pub overset_text_frames: u64,
    pub missing_fonts: u64,
    pub text_editable: bool,
    pub shape_editable: bool,
    pub sha256: String,
    pub content_sha256: String,
    pub page_geometry: Vec<PageGeometry>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Page-local native object bounds in points, excluding strokes; not a glyph-rendering proof.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageGeometry {
    pub width: f64,
    pub height: f64,
    pub items: Vec<[f64; 4]>,
}

/// Prepared on the session thread; `run` belongs on a background worker.
pub struct PreparedIndesignExport {
    source: PathBuf,
    intermediate: PathBuf,
    output: PathBuf,
    audit_path: PathBuf,
    audit: File,
    source_hash: String,
    pages: u64,
    stories: u64,
    text_frames: u64,
    page_items: u64,
    content_sha256: String,
    page_geometry: Vec<PageGeometry>,
    warnings: Vec<String>,
    roots: Option<Arc<file_access::AutomationRoots>>,
}

/// Preflight and stage an engine-produced IDML beside the destination. All paths are checked
/// before effects; existing output and audit files are refused. Does not launch InDesign.
pub fn prepare(session: &mut Session, output: &Path) -> Result<PreparedIndesignExport, String> {
    if !cfg!(target_os = "windows") {
        return Err("INDD export requires a working, licensed Windows InDesign installation".into());
    }
    if output.extension().and_then(|s| s.to_str()).is_none_or(|s| !s.eq_ignore_ascii_case("indd")) {
        return Err("InDesign export destination must end in .indd".into());
    }
    let output = std::path::absolute(output).map_err(|e| e.to_string())?;
    let parent = output.parent().ok_or("INDD destination has no parent folder")?;
    let audit_path = output.with_extension("indd.audit.json");
    file_access::check_read(parent)?;
    file_access::check_write(parent)?;
    for path in [&output, &audit_path] {
        file_access::check_write(path)?;
        if path.try_exists().map_err(|e| e.to_string())? {
            return Err(format!("Refusing to replace existing export: {}", path.display()));
        }
    }
    if !parent.is_dir() {
        return Err("Create the INDD output folder before exporting".into());
    }
    let document = &session.doc().map_err(|e| e.to_string())?.doc;
    if !document.assets.is_empty() || document.stories.values().any(|s| s.file_link.is_some()) || !document.data_merge.is_empty() {
        return Err("Optional INDD bridge currently accepts native-only documents without assets, linked text or data-merge sources".into());
    }
    let pages = document.page_count() as u64;
    let stories = document.stories.len() as u64;
    let page_items = document.spreads.iter().map(|spread| spread.items.len() as u64).sum::<u64>();
    let text_frames = document.spreads.iter().flat_map(|spread| &spread.items).filter(|item| item.is_text_frame()).count() as u64;
    if pages > 1000 || stories > 4096 || page_items > 4096 || document.spreads.iter().flat_map(|spread| &spread.items).any(|item| item.is_group()) {
        return Err("Optional INDD bridge supports at most 1000 pages and 4096 ungrouped native objects/stories".into());
    }
    let content_sha256 = content_hash(document.stories.values().map(|story| story.text.as_str()));
    let page_geometry: Vec<PageGeometry> = document
        .spreads
        .iter()
        .flat_map(|spread| {
            spread.pages.iter().map(|page| PageGeometry {
                width: page.width,
                height: page.height,
                items: spread
                    .items
                    .iter()
                    .filter_map(|item| {
                        let bounds = item.bounds();
                        let center = bounds.center();
                        (center.x >= page.x && center.x < page.x + page.width && center.y >= 0.0 && center.y < page.height).then_some([
                            bounds.x0 - page.x,
                            bounds.y0,
                            bounds.x1 - page.x,
                            bounds.y1,
                        ])
                    })
                    .collect(),
            })
        })
        .collect();
    if page_geometry.iter().map(|page| page.items.len() as u64).sum::<u64>() != page_items {
        return Err("Optional INDD bridge requires every native object's center to lie on a page".into());
    }
    let preflight = session.execute("preflight.run", &json!({})).map_err(|e| e.to_string())?;
    if preflight.get("errors").and_then(|v| v.as_u64()) != Some(0) {
        return Err(format!("INDD export refused: native preflight errors: {preflight}"));
    }
    let job = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
    let stage = parent.join(format!(".designcraft-indesign-{}-{job}", std::process::id()));
    let source = stage.join("source.idml");
    let intermediate = stage.join("converted.indd");
    for path in [&stage, &source, &intermediate] {
        file_access::check_write(path)?;
        file_access::check_read(path)?;
    }
    std::fs::create_dir(&stage).map_err(|e| format!("Create isolated INDD staging folder: {e}"))?;
    // Re-resolve after creating the directory: an externally changed parent must not move
    // subsequent exports outside the original automation roots.
    for path in [&stage, &source, &intermediate] {
        file_access::check_write(path)?;
        file_access::check_read(path)?;
    }
    file_access::check_write(&audit_path)?;
    let audit = OpenOptions::new().write(true).create_new(true).open(&audit_path).map_err(|e| format!("Create INDD audit: {e}"))?;
    let mut prepared = PreparedIndesignExport {
        source,
        intermediate,
        output,
        audit_path,
        audit,
        source_hash: String::new(),
        pages,
        stories,
        text_frames,
        page_items,
        content_sha256,
        page_geometry,
        warnings: Vec::new(),
        roots: file_access::current(),
    };
    prepared.record(json!({"status":"prepared","pages":pages,"stories":stories,"textFrames":text_frames,"pageItems":page_items}))?;
    let exported = match session.execute("file.exportIdml", &json!({"path":prepared.source,"embedImages":true})) {
        Ok(exported) => exported,
        Err(error) => {
            let message = error.to_string();
            prepared.record(json!({"status":"failed","error":message}))?;
            return Err(message);
        }
    };
    prepared.warnings = exported
        .get("warnings")
        .and_then(|value| value.as_array())
        .map(|warnings| warnings.iter().filter_map(|warning| warning.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let source_bytes = match read_bounded(&prepared.source) {
        Ok(bytes) => bytes,
        Err(error) => {
            prepared.record(json!({"status":"failed","error":error}))?;
            return Err(error);
        }
    };
    prepared.source_hash = format!("{:x}", Sha256::digest(source_bytes));
    Ok(prepared)
}

impl PreparedIndesignExport {
    fn record(&mut self, mut value: serde_json::Value) -> Result<(), String> {
        if let Some(fields) = value.as_object_mut() {
            fields.insert("sourceIdml".into(), json!(self.source));
            fields.insert("stagedIndd".into(), json!(self.intermediate));
            fields.insert("requestedOutput".into(), json!(self.output));
        }
        file_access::check_write(&self.audit_path)?;
        self.audit.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        self.audit.set_len(0).map_err(|e| e.to_string())?;
        self.audit.write_all(&serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        self.audit.flush().map_err(|e| e.to_string())
    }

    /// Timeout terminates only our PowerShell helper. InDesign itself and user documents are
    /// never terminated; a timed-out document may remain open there for manual inspection.
    pub fn run(self, timeout: Duration) -> Result<IndesignExport, String> {
        self.run_with_cancel(timeout, &AtomicBool::new(false))
    }

    /// Cancellation stops only the helper, before the final destination is published.
    pub fn run_with_cancel(mut self, timeout: Duration, cancel: &AtomicBool) -> Result<IndesignExport, String> {
        let roots = self.roots.clone();
        file_access::confine(roots.as_ref(), || {
            let result = self.convert(timeout, cancel);
            match &result {
                Ok(export) => self.record(json!({"status":"verified","export":export,"sourceIdmlSha256":self.source_hash}))?,
                Err(error) => self.record(json!({"status":"failed","error":error,"sourceIdmlSha256":self.source_hash,
                    "note":"Only the helper is stopped on timeout. The external application may retain its export document."}))?,
            }
            result
        })
    }

    fn convert(&self, timeout: Duration, cancel: &AtomicBool) -> Result<IndesignExport, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("InDesign export cancelled before conversion".into());
        }
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err("InDesign helper timeout must be greater than zero and at most 300 seconds".into());
        }
        for path in [&self.source, &self.intermediate] {
            file_access::check_read(path)?;
        }
        for path in [&self.intermediate, &self.output] {
            file_access::check_write(path)?;
        }
        if self.output.try_exists().map_err(|e| e.to_string())? || self.intermediate.try_exists().map_err(|e| e.to_string())? {
            return Err("Refusing existing INDD destination or intermediate file".into());
        }
        let source_hash = format!("{:x}", Sha256::digest(read_bounded(&self.source)?));
        if source_hash != self.source_hash {
            return Err("Staged IDML changed after preparation".into());
        }
        let response = run_helper(&self.source, &self.intermediate, timeout, cancel)?;
        if format!("{:x}", Sha256::digest(read_bounded(&self.source)?)) != self.source_hash {
            return Err("Staged IDML changed during conversion".into());
        }
        if response.content_sha256 != self.content_sha256 {
            return Err("InDesign roundtrip changed native Unicode story content (including Arabic)".into());
        }
        if !same_geometry(&self.page_geometry, &response.page_geometry) {
            return Err("InDesign roundtrip changed page dimensions or native object bounds".into());
        }
        if response.pages != self.pages
            || response.stories != self.stories
            || response.text_frames != self.text_frames
            || response.page_items != self.page_items
        {
            return Err(format!("InDesign roundtrip changed document structure: {}", serde_json::to_string(&response).map_err(|e| e.to_string())?));
        }
        if response.overset_text_frames != 0 || response.missing_fonts != 0 || !response.text_editable || !response.shape_editable {
            return Err("InDesign roundtrip failed text, geometry or font validation; inspect staged INDD".into());
        }
        let bytes = read_bounded(&self.intermediate)?;
        // Known INDD signature; validation alone is never used to manufacture an INDD.
        const SIGNATURE: [u8; 16] = [0x06, 0x06, 0xed, 0xf5, 0xd8, 0x1d, 0x46, 0xe5, 0xbd, 0x31, 0xef, 0xe7, 0xfe, 0x74, 0xb7, 0x1d];
        if !bytes.starts_with(&SIGNATURE) {
            return Err("InDesign did not produce a native INDD file".into());
        }
        file_access::check_write(&self.output)?;
        if cancel.load(Ordering::Relaxed) {
            return Err("InDesign export cancelled before publishing output".into());
        }
        let mut destination =
            OpenOptions::new().write(true).create_new(true).open(&self.output).map_err(|e| format!("Create new INDD destination: {e}"))?;
        destination.write_all(&bytes).and_then(|()| destination.flush()).map_err(|e| format!("Write verified INDD: {e}"))?;
        Ok(IndesignExport {
            path: self.output.to_string_lossy().into_owned(),
            audit_path: self.audit_path.to_string_lossy().into_owned(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            warnings: self.warnings.clone(),
            ..response
        })
    }
}

fn content_hash<'a>(texts: impl Iterator<Item = &'a str>) -> String {
    let mut hashes: Vec<String> =
        texts.map(|text| format!("{:x}", Sha256::digest(text.replace("\r\n", "\n").replace(['\r', '\u{2029}'], "\n").as_bytes()))).collect();
    hashes.sort_unstable();
    format!("{:x}", Sha256::digest(hashes.join("\n").as_bytes()))
}

fn same_geometry(expected: &[PageGeometry], actual: &[PageGeometry]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected.iter().zip(actual).all(|(a, b)| {
        if ![a.width, a.height, b.width, b.height].iter().all(|value| value.is_finite())
            || (a.width - b.width).abs() > 0.02
            || (a.height - b.height).abs() > 0.02
            || a.items.len() != b.items.len()
        {
            return false;
        }
        let mut matched = vec![false; b.items.len()];
        a.items.iter().all(|bounds| {
            let index = b.items.iter().enumerate().position(|(index, other)| {
                matched.get(index).copied() == Some(false)
                    && bounds.iter().zip(other).all(|(x, y)| x.is_finite() && y.is_finite() && (x - y).abs() <= 0.02)
            });
            if let Some(mark) = index.and_then(|index| matched.get_mut(index)) {
                *mark = true;
                true
            } else {
                false
            }
        })
    })
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    file_access::check_read(path)?;
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("InDesign bridge file exceeds 128 MiB".into());
    }
    Ok(bytes)
}

#[cfg(target_os = "windows")]
fn run_helper(source: &Path, destination: &Path, timeout: Duration, cancel: &AtomicBool) -> Result<IndesignExport, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let executable = PathBuf::from(std::env::var_os("SystemRoot").ok_or("Windows system folder unavailable")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let encoded = base64::engine::general_purpose::STANDARD.encode(SCRIPT.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>());
    let mut child = Command::new(executable)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .env("DESIGNCRAFT_IDML_SOURCE", source)
        .env("DESIGNCRAFT_INDD_STAGE", destination)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|e| format!("Start InDesign COM helper: {e}"))?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Observe InDesign helper: {error}"));
            }
        }
        if start.elapsed() >= timeout || cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(if cancel.load(Ordering::Relaxed) {
                "InDesign conversion cancelled; only the helper was terminated"
            } else {
                "InDesign conversion timed out; only the helper was terminated"
            }
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("InDesign COM conversion failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("Invalid InDesign conversion evidence: {e}"))
}

#[cfg(not(target_os = "windows"))]
fn run_helper(_source: &Path, _destination: &Path, _timeout: Duration, _cancel: &AtomicBool) -> Result<IndesignExport, String> {
    Err("InDesign COM conversion is available only on Windows".into())
}

// Static code only: selected paths arrive as environment data, never interpolated script text.
// Adobe API references: Application.open; Document.save/close/undo; PathPoint.anchor;
// SaveOptions.NO=1852776480; FontStatus.INSTALLED=1718831470. See developer.adobe.com/indesign/uxp/dom/api/.
#[cfg(target_os = "windows")]
const SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$document=$null
$application=$null
try {
    $source=$env:DESIGNCRAFT_IDML_SOURCE
    $destination=$env:DESIGNCRAFT_INDD_STAGE
    if ([IO.File]::Exists($destination)) { throw 'Refusing existing intermediate destination' }
    $application=New-Object -ComObject InDesign.Application
    $document=$application.Open($source,$false)
    [void]$document.Save($destination)
    [void]$document.Close(1852776480)
    $document=$null
    $document=$application.Open($destination,$false)
    if($document.Pages.Count -gt 1000 -or $document.Stories.Count -gt 4096 -or $document.PageItems.Count -gt 4096){throw 'Reopened INDD exceeds bridge limits'}
    function HashText([string]$text){
        $hasher=[Security.Cryptography.SHA256]::Create()
        try{return [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($text))).Replace('-','').ToLowerInvariant()}
        finally{$hasher.Dispose()}
    }
    $storyHashes=New-Object 'System.Collections.Generic.List[string]'
    for($i=1;$i -le $document.Stories.Count;$i++){
        $text=([string]$document.Stories.Item($i).Contents).Replace("`r`n","`n").Replace("`r","`n").Replace([char]0x2029,[char]10)
        $storyHashes.Add((HashText $text))
    }
    $storyHashes.Sort([StringComparer]::Ordinal)
    $contentHash=HashText ([string]::Join("`n",$storyHashes))
    # Local, unsaved measurement preferences only; the saved INDD and app defaults are unchanged.
    $document.ViewPreferences.HorizontalMeasurementUnits=2054188905
    $document.ViewPreferences.VerticalMeasurementUnits=2054188905
    $geometry=New-Object 'System.Collections.Generic.List[object]'
    for($i=1;$i -le $document.Pages.Count;$i++){
        $page=$document.Pages.Item($i)
        $bounds=@($page.Bounds)
        $items=New-Object 'System.Collections.Generic.List[object]'
        for($j=1;$j -le $page.PageItems.Count;$j++){
            $itemBounds=@($page.PageItems.Item($j).GeometricBounds)
            $items.Add(@(([double]$itemBounds[1]-[double]$bounds[1]),([double]$itemBounds[0]-[double]$bounds[0]),([double]$itemBounds[3]-[double]$bounds[1]),([double]$itemBounds[2]-[double]$bounds[0])))
        }
        $geometry.Add([ordered]@{width=([double]$bounds[3]-[double]$bounds[1]);height=([double]$bounds[2]-[double]$bounds[0]);items=@($items.ToArray())})
    }
    $overset=0
    for($i=1;$i -le $document.TextFrames.Count;$i++){ if($document.TextFrames.Item($i).Overflows){$overset++} }
    $missingFonts=0
    for($i=1;$i -le $document.Fonts.Count;$i++){ if([int]$document.Fonts.Item($i).Status -ne 1718831470){$missingFonts++} }
    $textEditable=$true
    if($document.Stories.Count -gt 0){
        $story=$document.Stories.Item(1)
        $original=[string]$story.Contents
        $story.InsertionPoints.Item(1).Contents=' '
        $textEditable=([string]$story.Contents -eq (' '+$original))
        [void]$document.Undo()
        $textEditable=$textEditable -and ([string]$story.Contents -eq $original)
    }
    $shapeEditable=$true
    $shape=$null
    foreach($collection in @($document.Polygons,$document.Ovals,$document.Rectangles,$document.GraphicLines)){
        if($collection.Count -gt 0){$shape=$collection.Item(1);break}
    }
    if($null -ne $shape){
        $point=$shape.Paths.Item(1).PathPoints.Item(1)
        $original=@($point.Anchor)
        $point.Anchor=@(([double]$original[0]+1.0),([double]$original[1]+1.0))
        $moved=@($point.Anchor)
        $shapeEditable=([math]::Abs([double]$moved[0]-[double]$original[0]-1.0) -lt 0.001)
        [void]$document.Undo()
        $restored=@($point.Anchor)
        $shapeEditable=$shapeEditable -and ([math]::Abs([double]$restored[0]-[double]$original[0]) -lt 0.001)
    }
    [ordered]@{
        path='';auditPath='';sha256='';contentSha256=$contentHash;pageGeometry=@($geometry.ToArray());version=[string]$application.Version
        pages=[long]$document.Pages.Count;stories=[long]$document.Stories.Count
        textFrames=[long]$document.TextFrames.Count;pageItems=[long]$document.PageItems.Count
        shapes=[long]($document.Polygons.Count+$document.Ovals.Count+$document.Rectangles.Count+$document.GraphicLines.Count)
        oversetTextFrames=$overset;missingFonts=$missingFonts;textEditable=$textEditable;shapeEditable=$shapeEditable
    } | ConvertTo-Json -Compress -Depth 8
} catch {
    [Console]::Error.WriteLine(([string]$_.Exception.Message).Substring(0,[math]::Min(1024,([string]$_.Exception.Message).Length)))
    exit 1
} finally {
    if($null -ne $document){try{[void]$document.Close(1852776480)}catch{}}
    if($null -ne $application){[void][Runtime.InteropServices.Marshal]::ReleaseComObject($application)}
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_errors_precede_staging_or_application_launch() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        assert!(prepare(&mut session, &directory.path().join("not-indd.pdf")).is_err());
        let existing = directory.path().join("existing.indd");
        std::fs::write(&existing, b"preserved").unwrap();
        assert!(prepare(&mut session, &existing).is_err());
        assert_eq!(std::fs::read(&existing).unwrap(), b"preserved");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn forbidden_output_root_is_rejected_before_audit_or_staging() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = file_access::AutomationRoots::new(&[allowed.path()], &[allowed.path()]).unwrap();
        let mut session = Session::new();
        let result = file_access::confine(roots.as_ref(), || prepare(&mut session, &outside.path().join("forbidden.indd")));
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(allowed.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn unicode_story_proof_normalizes_paragraphs_without_losing_arabic_letters() {
        assert_eq!(content_hash(["أرى\nبعيني", "Sight"].into_iter()), content_hash(["Sight", "أرى\rبعيني"].into_iter()));
        assert_ne!(content_hash(["أرى\nبعيني"].into_iter()), content_hash(["أسمع\nبأذني"].into_iter()));
        assert_ne!(content_hash(["أرى\nبعيني"].into_iter()), content_hash(["أرىبعيني"].into_iter()));
    }

    #[test]
    fn page_and_object_geometry_proof_catches_shifts_and_size_changes() {
        let expected = PageGeometry { width: 595.2756, height: 841.8898, items: vec![[10.0, 20.0, 30.0, 40.0], [50.0, 60.0, 70.0, 80.0]] };
        let mut actual = expected.clone();
        actual.items.reverse();
        assert!(same_geometry(std::slice::from_ref(&expected), std::slice::from_ref(&actual)));
        actual.items[0][0] += 0.1;
        assert!(!same_geometry(std::slice::from_ref(&expected), std::slice::from_ref(&actual)));
        actual = expected.clone();
        actual.width += 1.0;
        assert!(!same_geometry(std::slice::from_ref(&expected), std::slice::from_ref(&actual)));
        actual = expected.clone();
        actual.height = f64::NAN;
        assert!(!same_geometry(std::slice::from_ref(&expected), std::slice::from_ref(&actual)));
    }
}
