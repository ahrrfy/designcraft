//! The production host keeps print approval separate from generated commands.
//! The submitted artifact is the engine's frozen PDF, never a fresh export.
use super::*;
use sha2::{Digest, Sha256};

impl ProductionRunner {
    pub(super) fn refresh_local_server(&mut self) {
        let state = self.local_server.status();
        self.snapshot.local_server =
            Some(LocalServerStatus { owned: state.owned, running: state.running, pid: state.pid, endpoint: state.endpoint, message: state.message });
    }

    pub(super) fn prepare_print(&mut self, session: &mut Session, printer: Option<String>, grayscale: bool, duplex: String) -> Result<(), String> {
        self.snapshot.print_proof = None;
        if let Some(contract) = self.print_contracts.get(&session.doc().map_err(|e| e.to_string())?.uid) {
            contract.validate_existing(session.doc().map_err(|e| e.to_string())?)?;
            contract.validate_final_export()?;
            let checked = ensure_preflight_at(session, contract.min_image_ppi().unwrap_or(150.0))?;
            if checked
                .get("issues")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|issue| issue.get("kind").and_then(Value::as_str) == Some("lowResolution"))
            {
                return Err("Resolve low-resolution artwork before preparing final print".into());
            }
        }
        let prepared = session.execute("file.preparePrint", &print_params(printer, grayscale, duplex)).map_err(|e| e.to_string())?;
        let token = prepared.get("token").and_then(Value::as_str).ok_or("Missing frozen print token")?.to_string();
        valid_token(&token)?;
        let sha256 = prepared.get("sha256").and_then(Value::as_str).ok_or("Missing frozen PDF hash")?.to_string();
        valid_token(&sha256)?;
        let directory = self.root.join("print-proofs");
        self.profile_access.check(&directory, file_access::Access::Write)?;
        std::fs::create_dir_all(&directory).map_err(|e| format!("Create print proof directory: {e}"))?;
        self.profile_access.check(&directory, file_access::Access::Write)?;
        let path = directory.join(format!("{token}.pdf"));
        self.profile_access.check(&path, file_access::Access::Write)?;
        file_access::confine(Some(&self.profile_access), || {
            session.execute("file.savePreparedPrint", &json!({"token":token,"path":path})).map_err(|e| e.to_string())
        })?;
        let proof = PrintProof {
            token,
            sha256,
            bytes: prepared.get("bytes").and_then(Value::as_u64).ok_or("Missing frozen PDF byte count")?,
            pages: prepared.get("pages").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).ok_or("Invalid frozen PDF page count")?,
            manifest: prepared.get("manifest").cloned().ok_or("Missing frozen print manifest")?,
            pdf_path: Some(path.to_string_lossy().into_owned()),
        };
        write_json(
            &self.profile_access,
            &directory.join(format!("{}.json", proof.token)),
            &json!({"status":"awaiting_human_print_review","proof":proof}),
        )?;
        self.snapshot.print_proof = Some(proof);
        self.snapshot.message = "Frozen PDF prepared. Review all pages, embedded fonts, colors and the selected queue before printing at 100%".into();
        Ok(())
    }

    fn verified_print_proof(&self, session: &mut Session, token: &str) -> Result<PrintProof, String> {
        valid_token(token)?;
        let proof = self.snapshot.print_proof.as_ref().filter(|proof| proof.token == token).ok_or("Prepare a fresh frozen print proof first")?;
        session.execute("file.printPrepared", &json!({"token":token,"dryRun":true})).map_err(|e| e.to_string())?;
        let path = proof.pdf_path.as_ref().ok_or("Missing frozen proof PDF")?;
        self.profile_access.check(path, file_access::Access::Read)?;
        let bytes = file_access::confine(Some(&self.profile_access), || designcraft_engine::io::read_capped(path, 128 * 1024 * 1024))
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 != proof.bytes || format!("{:x}", Sha256::digest(&bytes)) != proof.sha256 {
            return Err("The PDF proof changed after preparation. Prepare and review a fresh print artifact".into());
        }
        Ok(proof.clone())
    }

    pub(super) fn save_prepared_print(&mut self, session: &mut Session, token: &str, path: &str) -> Result<(), String> {
        self.verified_print_proof(session, token)?;
        file_access::check_write(path)?;
        session.execute("file.savePreparedPrint", &json!({"token":token,"path":path})).map_err(|e| e.to_string())?;
        self.snapshot.message = "Saved the exact frozen print PDF; fonts and printer settings were not regenerated".into();
        Ok(())
    }

    pub(super) fn submit_prepared_print(&mut self, session: &mut Session, token: &str, request_id: String) -> Result<(), String> {
        if request_id.is_empty() || request_id.len() > 128 || request_id.chars().any(char::is_control) {
            return Err("Print request needs a bounded operation ID".into());
        }
        if self.print_attempts.contains(&request_id) {
            return Err("This print operation was already attempted; check the print queue before creating a new operation".into());
        }
        if self.print_attempts.len() >= 4096 {
            return Err("Print operation history reached its safety limit; archive it before further printing".into());
        }
        let proof = self.verified_print_proof(session, token)?;
        let trace = self.root.join("print-proofs").join(format!("{token}.json"));
        // Write-ahead prevents automatic retries after a crash or ambiguous spooler response.
        self.print_attempts.insert(request_id.clone());
        write_json(&self.profile_access, &self.root.join("print-attempts.json"), &self.print_attempts)?;
        write_json(&self.profile_access, &trace, &json!({"status":"submission_attempted","operationId":request_id,"proof":proof}))?;
        self.snapshot.print_proof = None;
        let result = session.execute("file.printPrepared", &json!({"token":token})).map_err(|e| e.to_string());
        match result {
            Ok(submission) => {
                write_json(
                    &self.profile_access,
                    &trace,
                    &json!({"status":"spooler_accepted","operationId":request_id,"proof":proof,"submission":submission}),
                )?;
                self.snapshot.message = "The system spooler accepted the frozen PDF. Verify the physical paper at 100%; the driver and printer still control final color reproduction".into();
                Ok(())
            }
            Err(error) => {
                let _ = write_json(
                    &self.profile_access,
                    &trace,
                    &json!({"status":"submission_uncertain_or_failed","operationId":request_id,"proof":proof,"error":error}),
                );
                Err(format!("Print submission was attempted and will not be retried automatically. Check the print queue: {error}"))
            }
        }
    }

    pub(super) fn open_print_proof(&mut self, session: &mut Session, token: &str) -> Result<(), String> {
        let proof = self.verified_print_proof(session, token)?;
        let path = proof.pdf_path.ok_or("Missing frozen PDF proof")?;
        open_pdf(Path::new(&path))?;
        self.snapshot.message = "Opened the verified frozen PDF in your PDF viewer; review every page before approving print".into();
        Ok(())
    }
}

fn valid_token(token: &str) -> Result<(), String> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid frozen artifact token".into());
    }
    Ok(())
}

fn open_pdf(path: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let executable = PathBuf::from(std::env::var_os("SystemRoot").ok_or("Windows directory unavailable")?).join("explorer.exe");
        let mut command = Command::new(executable);
        command.creation_flags(0x0800_0000);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = Command::new("/usr/bin/open");
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut command = Command::new("/usr/bin/xdg-open");
    command.arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("Open frozen PDF proof: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_tokens_and_unprepared_submissions_fail_before_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let mut host = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        assert!(valid_token("../outside").is_err());
        assert!(host.submit_prepared_print(&mut session, &"a".repeat(64), "new-operation".into()).is_err());
        assert!(host.print_attempts.is_empty());
        assert!(!directory.path().join("print-attempts.json").exists());
    }

    #[test]
    fn print_proof_bytes_cannot_change_between_review_and_submission() {
        let directory = tempfile::tempdir().unwrap();
        let mut host = ProductionRunner::open(directory.path()).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &new_a4(1)).unwrap();
        host.prepare_print(&mut session, Some("Synthetic test queue".into()), false, "off".into()).unwrap();
        let proof = host.snapshot.print_proof.clone().unwrap();
        session.execute("frame.create", &json!({"rect":[60,60,120,120]})).unwrap();
        assert!(host.verified_print_proof(&mut session, &proof.token).is_ok());
        std::fs::write(proof.pdf_path.as_ref().unwrap(), b"changed").unwrap();
        assert!(host.submit_prepared_print(&mut session, &proof.token, "tampered-operation".into()).is_err());
        assert!(host.print_attempts.is_empty());
        assert!(!directory.path().join("print-attempts.json").exists());
    }
}
