//! App-update monitoring via winget. Listing runs unelevated; applying an update
//! runs winget elevated through `Start-Process -Verb RunAs` (one UAC prompt) so
//! machine-scope packages can be installed.

use serde::Serialize;
use std::os::windows::process::CommandExt;

/// CREATE_NO_WINDOW — keep the console-based winget/powershell hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize, Clone, Debug)]
pub struct AppUpdate {
    pub id: String,
    pub name: String,
    pub current: String,
    pub available: String,
}

/// Split a winget table row on runs of 2+ spaces, preserving single spaces that
/// occur inside a field (e.g. "7-Zip 25.01 (x64)").
fn split_columns(line: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut cur = String::new();
    let mut spaces = 0usize;
    for ch in line.chars() {
        if ch == ' ' {
            spaces += 1;
        } else {
            if spaces >= 2 && !cur.is_empty() {
                cols.push(cur.trim().to_string());
                cur.clear();
            } else if spaces == 1 && !cur.is_empty() {
                cur.push(' ');
            }
            spaces = 0;
            cur.push(ch);
        }
    }
    if !cur.trim().is_empty() {
        cols.push(cur.trim().to_string());
    }
    cols
}

/// Parse `winget upgrade` output. Skips the progress-noise and header by starting
/// after the dashes separator, and stops at the "N upgrades available" footer.
fn parse_upgrades(text: &str) -> Vec<AppUpdate> {
    let mut updates = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table {
            // The separator line is a run of dashes (possibly after stray noise).
            if trimmed.starts_with("---") || trimmed.ends_with("---") || trimmed.contains("-----")
            {
                in_table = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        // Footer, e.g. "21 upgrades available." — also stops at the "explicit
        // targeting" sub-table that some winget versions append.
        let lower = trimmed.to_lowercase();
        if lower.contains("upgrade") && lower.contains("available")
            || lower.starts_with("the following packages")
        {
            break;
        }
        let cols = split_columns(trimmed);
        if cols.len() < 4 {
            continue;
        }
        let name = cols[0].clone();
        // Strip the truncation ellipsis winget adds to long ids.
        let id = cols[1]
            .trim_end_matches('…')
            .trim_end_matches('.')
            .to_string();
        let current = cols[2].clone();
        let available = cols[3].clone();
        if id.is_empty() || available.is_empty() {
            continue;
        }
        updates.push(AppUpdate {
            id,
            name,
            current,
            available,
        });
    }
    updates
}

/// List apps with an available update. Runs unelevated — listing needs no admin.
#[tauri::command]
pub async fn list_app_updates() -> Result<Vec<AppUpdate>, String> {
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new("winget")
            .args([
                "upgrade",
                "--include-unknown",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("winget is not available: {e}"))?;

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_upgrades(&text))
}

/// Run an elevated winget command via UAC, waiting for it to finish.
async fn run_winget_elevated(args: Vec<String>) -> Result<String, String> {
    // Build a single-quoted PowerShell argument list; '' escapes a quote.
    let arg_list = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "$p = Start-Process winget -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList {arg_list}; exit $p.ExitCode"
    );
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    if status.success() {
        Ok("ok".to_string())
    } else {
        Err(format!(
            "winget exited with code {}",
            status.code().unwrap_or(-1)
        ))
    }
}

#[tauri::command]
pub async fn update_app(id: String) -> Result<String, String> {
    run_winget_elevated(vec![
        "upgrade".into(),
        "--id".into(),
        id,
        "--silent".into(),
        "--accept-package-agreements".into(),
        "--accept-source-agreements".into(),
        "--disable-interactivity".into(),
    ])
    .await
}

#[tauri::command]
pub async fn update_all_apps() -> Result<String, String> {
    run_winget_elevated(vec![
        "upgrade".into(),
        "--all".into(),
        "--silent".into(),
        "--accept-package-agreements".into(),
        "--accept-source-agreements".into(),
        "--disable-interactivity".into(),
    ])
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_winget_table_with_spaces_and_truncation() {
        let sample = "\
   -    progress noise    Name                Id                  Version    Available    Source
-----------------------------------------------------------------------------------------------
7-Zip 25.01 (x64)         7zip.7zip           25.01      26.01        winget
Visual Studio Build Tools 2022    Microsoft.VisualStudio.2022.Buil…   17.14.25 (January 2026)    17.14.34    winget
21 upgrades available.";
        let ups = parse_upgrades(sample);
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].id, "7zip.7zip");
        assert_eq!(ups[0].name, "7-Zip 25.01 (x64)");
        assert_eq!(ups[0].available, "26.01");
        // Ellipsis stripped from the truncated id.
        assert_eq!(ups[1].id, "Microsoft.VisualStudio.2022.Buil");
        assert_eq!(ups[1].current, "17.14.25 (January 2026)");
    }
}
