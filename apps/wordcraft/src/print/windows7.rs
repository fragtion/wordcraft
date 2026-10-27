//! The experimental Windows 7 build's spooler (docs/windows7.md), as in VectorCraft's Windows 7
//! build. craft-print draws jobs with `Windows.Data.Pdf`, a Windows Runtime API Windows 7 doesn't
//! have, so this build lists printers through PowerShell's WMI (`Win32_Printer`) and hands the
//! print-ready PDF to the installed PDF viewer's Print / PrintTo verb. The viewer prints with its
//! own settings: copies, two-sided and grayscale chosen in WordCraft don't reach it, and the
//! result says so.

/// Where a job's PDF waits for the viewer: a fresh file in `dir`. The viewer reads it after the
/// verb returns, so it can't be removed then; files of jobs more than an hour old are removed
/// first instead. Never truncates an existing file, and removes one it couldn't finish.
pub(crate) fn job_file(dir: &std::path::Path, pdf: &[u8]) -> Result<std::path::PathBuf, String> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    // A folder of a few files; the cap only bounds a runaway listing.
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten().take(4096) {
        let old = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs() > 3600);
        if old {
            // Another job's viewer may still be reading it: it goes next time.
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let path = dir.join(format!("job-{}-{stamp}.pdf", std::process::id()));
    let mut file =
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| format!("can't create {}: {e}", path.display()))?;
    if let Err(e) = file.write_all(pdf).and_then(|()| file.flush()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("can't write {}: {e}", path.display()));
    }
    Ok(path)
}

/// `s` as a PowerShell single-quoted string.
pub(crate) fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// PowerShell's `<Default>\t<Name>` lines → the printers, the default first.
pub(crate) fn parse_printers(text: &str) -> Vec<wordcraft_ui_egui::printing::Printer> {
    let mut printers: Vec<wordcraft_ui_egui::printing::Printer> = text
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').split_once('\t'))
        .filter(|(_, name)| !name.trim().is_empty())
        .map(|(default, name)| wordcraft_ui_egui::printing::Printer {
            name: name.trim().to_string(),
            label: String::new(),
            default: default.trim().eq_ignore_ascii_case("true"),
        })
        .collect();
    printers.sort_by_key(|p| !p.default);
    printers
}

/// The PowerShell that prints `file` on `printer` (the default printer when empty) through the
/// PDF viewer's verb. PrintTo takes the printer name in double quotes, which a Windows printer
/// name can't contain.
pub(crate) fn print_script(file: &str, printer: &str) -> String {
    let path = ps_quote(file);
    match printer.trim() {
        "" => format!("Start-Process -FilePath {path} -Verb Print -WindowStyle Hidden"),
        p => format!(
            "Start-Process -FilePath {path} -Verb PrintTo -ArgumentList {} -WindowStyle Hidden",
            ps_quote(&format!("\"{}\"", p.replace('"', "")))
        ),
    }
}

/// What the status bar is told once the viewer has the job: the settings the viewer decides.
pub(crate) fn handed_over(copies: u32) -> String {
    let mut note = String::from("handed to the PDF viewer's print command, which prints with its own settings");
    if copies > 1 {
        note.push_str(&format!(" (one copy, not {copies})"));
    }
    note
}

#[cfg(target_vendor = "win7")]
fn powershell(script: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()));
    let mut c = std::process::Command::new(root.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"));
    c.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); {script}"),
    ])
    .creation_flags(CREATE_NO_WINDOW);
    c
}

/// The spooler's printers; empty when they can't be listed.
#[cfg(target_vendor = "win7")]
pub fn printers() -> Vec<wordcraft_ui_egui::printing::Printer> {
    let script = "Get-WmiObject Win32_Printer | ForEach-Object { \"$($_.Default)`t$($_.Name)\" }";
    match powershell(script).output() {
        Ok(out) if out.status.success() => parse_printers(&String::from_utf8_lossy(&out.stdout)),
        Ok(out) => {
            log::warn!("listing printers: PowerShell failed ({})", out.status);
            Vec::new()
        }
        Err(e) => {
            log::warn!("listing printers: can't run PowerShell: {e}");
            Vec::new()
        }
    }
}

/// Print `pdf` on `printer` through the PDF viewer's verb.
#[cfg(target_vendor = "win7")]
pub fn submit(pdf: &[u8], printer: &str, copies: u32) -> Result<String, String> {
    let file = job_file(&std::env::temp_dir().join("wordcraft-print"), pdf)?;
    let out = powershell(&print_script(&file.to_string_lossy(), printer)).output();
    let failed = match out {
        Ok(o) if o.status.success() => None,
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Some(if err.is_empty() { format!("PowerShell failed ({})", o.status) } else { err })
        }
        Err(e) => Some(format!("can't run PowerShell: {e}")),
    };
    if let Some(e) = failed {
        // No viewer took the job, so nothing will read the file.
        let _ = std::fs::remove_file(&file);
        return Err(format!("no app prints PDF files here; install a PDF viewer that can print, or export a PDF ({e})"));
    }
    Ok(handed_over(copies))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_printer_lines_are_read_default_first() {
        let printers = parse_printers("False\tOffice Laser\r\nTrue\tHome Inkjet\r\nFalse\t \r\nno tab\r\n");
        let names: Vec<(&str, bool)> = printers.iter().map(|p| (p.name.as_str(), p.default)).collect();
        assert_eq!(names, [("Home Inkjet", true), ("Office Laser", false)]);
        assert!(parse_printers("").is_empty());
    }

    #[test]
    fn printer_names_and_paths_are_quoted_for_powershell() {
        assert_eq!(ps_quote("it's.pdf"), "'it''s.pdf'");
        assert_eq!(print_script(r"C:\T\job.pdf", ""), r"Start-Process -FilePath 'C:\T\job.pdf' -Verb Print -WindowStyle Hidden");
        assert_eq!(
            print_script(r"C:\Bob's\job.pdf", "Office \"Laser\" 'B'"),
            r#"Start-Process -FilePath 'C:\Bob''s\job.pdf' -Verb PrintTo -ArgumentList '"Office Laser ''B''"' -WindowStyle Hidden"#
        );
    }

    #[test]
    fn job_files_hold_the_pdf_for_the_viewer_and_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("wordcraft-print-test-{}", std::process::id()));
        let a = job_file(&dir, b"%PDF-1.7 a").unwrap();
        let b = job_file(&dir, b"%PDF-1.7 b").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"%PDF-1.7 a");
        assert_eq!(std::fs::read(&b).unwrap(), b"%PDF-1.7 b");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_result_says_the_viewer_decides_and_copies_are_not_applied() {
        assert!(!handed_over(1).contains("copy"));
        assert!(handed_over(3).ends_with("(one copy, not 3)"));
    }
}
