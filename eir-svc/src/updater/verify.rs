//! Post-update verification: confirm an app now reports the version we expected to
//! install. Reads the installed version from winget (by id or by name) or, as a
//! second signal for native installs, an installed exe's ProductVersion, and maps
//! it to a [`Verification`] verdict via [`super::version::classify_version`]. The
//! comparison itself is pure and unit-tested in `version`; this module is the I/O.

use super::domain::Verification;
use super::version::classify_version;
use super::winget_parse::{column, winget_table};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What to read the installed version from.
pub enum VerifyTarget {
    Winget {
        id: String,
    },
    ByName {
        name: String,
        verify_exe: Option<String>,
    },
}

/// A binary's ProductVersion is often a 4-part FILEVERSION that trails the
/// marketing/release version, so don't hard-fail a native install on an exe-fallback
/// "mismatch" — soften it to Unverified. Pure.
fn soften_exe_fallback(verdict: Verification, from_exe: bool) -> Verification {
    if from_exe && verdict == Verification::Mismatch {
        Verification::Unverified
    } else {
        verdict
    }
}

/// Confirm an app now reports `expected` (or newer). Returns (verdict, found
/// version). One short retry absorbs the ARP-registration lag right after a fresh
/// install before declaring a version unverifiable.
pub async fn verify_app(target: &VerifyTarget, expected: &str) -> (Verification, String) {
    for attempt in 0..2 {
        if attempt == 1 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let (found, from_exe) = match target {
            VerifyTarget::Winget { id } => (winget_installed_version(id).await, false),
            VerifyTarget::ByName { name, verify_exe } => {
                let v = winget_installed_version_by_name(name).await;
                match (v, verify_exe) {
                    (Some(v), _) => (Some(v), false),
                    (None, Some(exe)) => (exe_file_version(exe).await, true),
                    (None, None) => (None, false),
                }
            }
        };
        if let Some(found) = found {
            let verdict = soften_exe_fallback(classify_version(&found, expected), from_exe);
            return (verdict, found);
        }
    }
    (Verification::Unverified, String::new())
}

/// Read an installed app's version from `winget list --id <id> --exact`.
async fn winget_installed_version(id: &str) -> Option<String> {
    let id = id.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("winget")
            .args([
                "list",
                "--id",
                &id,
                "--exact",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (offsets, rows) = winget_table(&text);
    rows.first()
        .map(|r| column(&offsets, r, "Version"))
        .filter(|v| !v.is_empty())
}

/// Read an installed app's version from `winget list --name <name>`, matching the
/// row whose Name overlaps the queried name (display names are fuzzy).
async fn winget_installed_version_by_name(name: &str) -> Option<String> {
    let q = name.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("winget")
            .args([
                "list",
                "--name",
                &q,
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (offsets, rows) = winget_table(&text);
    let lname = name.to_lowercase();
    for r in &rows {
        let n = column(&offsets, r, "Name").to_lowercase();
        if !n.is_empty() && (n.contains(&lname) || lname.contains(&n)) {
            let v = column(&offsets, r, "Version");
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Read FileVersion/ProductVersion of an absolute exe path (a second confirmation
/// signal for native installs). Returns None for relative paths or missing files.
async fn exe_file_version(path: &str) -> Option<String> {
    if !Path::new(path).is_absolute() {
        return None;
    }
    let script = format!(
        "try {{ (Get-Item -LiteralPath '{}').VersionInfo.ProductVersion }} catch {{ '' }}",
        path.replace('\'', "''")
    );
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_fallback_softens_only_a_real_mismatch() {
        // An exe-fallback mismatch (4-part FILEVERSION below marketing) is softened…
        assert_eq!(
            soften_exe_fallback(Verification::Mismatch, true),
            Verification::Unverified
        );
        // …but a winget (non-exe) mismatch stays a mismatch (the update didn't take).
        assert_eq!(
            soften_exe_fallback(Verification::Mismatch, false),
            Verification::Mismatch
        );
        // Verified/Unverified are untouched regardless of source.
        assert_eq!(
            soften_exe_fallback(Verification::Verified, true),
            Verification::Verified
        );
        assert_eq!(
            soften_exe_fallback(Verification::Unverified, true),
            Verification::Unverified
        );
    }
}
