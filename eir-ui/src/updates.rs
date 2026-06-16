//! App-update monitoring via winget. Listing runs unelevated; applying an update
//! runs winget elevated through `Start-Process -Verb RunAs` (one UAC prompt) so
//! machine-scope packages can be installed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use tokio::io::AsyncWriteExt;

/// CREATE_NO_WINDOW — keep the console-based winget/powershell hidden.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── Per-app notes / ignore list (persisted to %APPDATA%\Eir\app-notes.json) ─

#[derive(Serialize, Deserialize, Clone, Default)]
struct AppNote {
    #[serde(default)]
    ignore: bool,
    #[serde(default)]
    note: String,
}

fn notes_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("APPDATA").ok()?;
    let dir = std::path::Path::new(&base).join("Eir");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join("app-notes.json"))
}

fn load_notes() -> HashMap<String, AppNote> {
    notes_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_notes(notes: &HashMap<String, AppNote>) -> Result<(), String> {
    let path = notes_path().ok_or("cannot resolve notes path")?;
    let json = serde_json::to_string_pretty(notes).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

/// Record a note and/or ignore flag for an app, used in future AI checks.
/// An empty note with ignore=false removes the entry.
#[tauri::command]
pub async fn set_app_note(name: String, note: String, ignore: bool) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let mut notes = load_notes();
        let key = name.to_lowercase();
        if note.trim().is_empty() && !ignore {
            notes.remove(&key);
        } else {
            notes.insert(
                key,
                AppNote {
                    ignore,
                    note: note.trim().to_string(),
                },
            );
        }
        save_notes(&notes)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone, Debug)]
pub struct AppUpdate {
    pub id: String,
    pub name: String,
    pub current: String,
    pub available: String,
}

/// The columns a winget table can have, in display order.
const WINGET_COLUMNS: [&str; 5] = ["Name", "Id", "Version", "Available", "Source"];

/// Locate each column's start (char offset) from the table header. winget aligns
/// columns to fixed positions under their header labels, so this is far more
/// robust than splitting on whitespace: winget separates columns with a *single*
/// space (the wide gaps are just padding to the widest cell), and it truncates
/// long fields with '…'. Both break a space-run splitter but leave column
/// positions intact. The header is often prefixed with progress-spinner output
/// terminated by '\r', so only the text after the last '\r' is considered.
fn header_offsets(text: &str) -> Vec<(&'static str, usize)> {
    let header = text
        .lines()
        .map(|l| l.rsplit('\r').next().unwrap_or(l))
        .find(|l| l.contains("Id") && l.contains("Version"));
    let mut offsets = Vec::new();
    if let Some(h) = header {
        for label in WINGET_COLUMNS {
            if let Some(byte) = h.find(label) {
                offsets.push((label, h[..byte].chars().count()));
            }
        }
        offsets.sort_by_key(|&(_, start)| start);
    }
    offsets
}

/// Read one column's trimmed value from a row. A column spans from its own start
/// to the next column's start (the last runs to end of line). Returns "" when the
/// column is absent or starts past the row's end.
fn column(offsets: &[(&'static str, usize)], row: &[char], label: &str) -> String {
    let Some(idx) = offsets.iter().position(|&(l, _)| l == label) else {
        return String::new();
    };
    let start = offsets[idx].1;
    if start >= row.len() {
        return String::new();
    }
    let end = offsets
        .get(idx + 1)
        .map(|&(_, s)| s.min(row.len()))
        .unwrap_or(row.len());
    row[start..end]
        .iter()
        .collect::<String>()
        .trim()
        .to_string()
}

/// Split a winget table into its column offsets and data rows (as char vectors).
/// Skips the progress-noise and header above the dashed separator, and stops at
/// the "N upgrades available" footer (and the "explicit targeting" sub-table some
/// winget versions append).
fn winget_table(text: &str) -> (Vec<(&'static str, usize)>, Vec<Vec<char>>) {
    let offsets = header_offsets(text);
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_table {
            if trimmed.contains("-----") {
                in_table = true;
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if lower.contains("upgrade") && lower.contains("available")
            || lower.starts_with("the following packages")
        {
            break;
        }
        rows.push(line.chars().collect());
    }
    (offsets, rows)
}

/// Parse `winget upgrade` output into the apps with an available update.
fn parse_upgrades(text: &str) -> Vec<AppUpdate> {
    let (offsets, rows) = winget_table(text);
    let mut updates = Vec::new();
    for row in &rows {
        let name = column(&offsets, row, "Name");
        // Strip the truncation ellipsis winget adds to long ids; winget's `--id`
        // does substring matching, so the un-truncated prefix still resolves.
        let id = column(&offsets, row, "Id")
            .trim_end_matches('…')
            .trim_end_matches('.')
            .to_string();
        let current = column(&offsets, row, "Version");
        let available = column(&offsets, row, "Available");
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

// ── AI update check (apps winget can't manage) ───────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct AiUpdate {
    pub name: String,
    pub current: String,
    pub latest: String,
    pub url: String,
}

#[derive(Serialize)]
pub struct AiCheckResult {
    pub updates: Vec<AiUpdate>,
    pub checked: usize,
    pub cost_usd: f64,
    pub note: Option<String>,
}

/// Cap on apps sent to the AI in one batch, to bound cost/latency.
const AI_CHECK_CAP: usize = 20;

/// Names we never AI-check: drivers, runtimes, redistributables, self-updating
/// suites, and Eir itself. Keeps the batch to real, user-updatable apps.
fn is_noise(name: &str) -> bool {
    let n = name.to_lowercase();
    const SKIP: &[&str] = &[
        "driver",
        "redistributable",
        "runtime",
        "microsoft visual c++",
        "windows sdk",
        "update for",
        "security update",
        "hotfix",
        "maintenance service",
        "microsoft .net",
        "directx",
        "realtek",
        "intel(r)",
        "host app",
        "web experience",
        "microsoft 365",
        "microsoft office",
        "visual studio installer",
        "onedrive",
        "teams machine-wide",
        "eir",
    ];
    SKIP.iter().any(|s| n.contains(s))
}

/// Parse `winget list` for apps winget can't manage — rows whose Source is not
/// "winget"/"msstore". Tolerates winget's '…' column truncation, and skips
/// MSIX/Store-delivered packages (those update through the Store, not a download).
/// Returns (name, version).
fn parse_unmanaged(text: &str) -> Vec<(String, String)> {
    let (offsets, rows) = winget_table(text);
    let mut apps = Vec::new();
    for row in &rows {
        // Source column: winget/msstore rows are handled by the winget panel.
        // winget can truncate even the source (e.g. "mssto…"), so compare the
        // ellipsis-stripped token as a prefix of a real source name. The length
        // guard keeps a blank or short version string from matching.
        let source = column(&offsets, row, "Source")
            .trim_end_matches('…')
            .to_lowercase();
        if source.len() >= 5 && ["winget", "msstore"].iter().any(|s| s.starts_with(&source)) {
            continue;
        }
        // MSIX/Store-delivered packages (Windows components, Store apps) update
        // through the Store, not by downloading an installer — skip them so the
        // AI check stays focused on real third-party desktop apps.
        if column(&offsets, row, "Id").starts_with("MSIX\\") {
            continue;
        }
        let name = column(&offsets, row, "Name");
        let version = column(&offsets, row, "Version");
        if name.is_empty() || version.is_empty() || is_noise(&name) {
            continue;
        }
        apps.push((name, version));
    }
    apps
}

fn claude_binary() -> String {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let candidate = format!("{profile}\\.local\\bin\\claude.exe");
        if std::path::Path::new(&candidate).is_file() {
            return candidate;
        }
    }
    "claude".into()
}

fn strip_fences(s: &str) -> &str {
    let t = s.trim();
    for (open, close) in [("```json", "```"), ("```", "```")] {
        if let Some(i) = t.find(open) {
            let after = &t[i + open.len()..];
            return after
                .find(close)
                .map(|e| &after[..e])
                .unwrap_or(after)
                .trim();
        }
    }
    t
}

#[derive(Deserialize)]
struct CliEnvelope {
    result: Option<String>,
    total_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
struct AiResp {
    updates: Vec<AiUpdateRaw>,
}

#[derive(Deserialize)]
struct AiUpdateRaw {
    name: String,
    #[serde(default)]
    current: String,
    #[serde(default)]
    latest: String,
    #[serde(default)]
    url: String,
}

/// On-demand: ask the configured AI provider (with live web search) for updates
/// to apps winget can't manage. Uses OpenRouter's web plugin when the provider
/// is OpenRouter, otherwise the Claude CLI — see `run_ai_check`.
#[tauri::command]
pub async fn check_ai_updates() -> Result<AiCheckResult, String> {
    // 1. Apps winget can't manage.
    let list_out = tokio::task::spawn_blocking(|| {
        std::process::Command::new("winget")
            .args([
                "list",
                "--accept-source-agreements",
                "--disable-interactivity",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("winget is not available: {e}"))?;

    let notes = load_notes();
    let mut apps = parse_unmanaged(&String::from_utf8_lossy(&list_out.stdout));
    // Drop apps the user has chosen to ignore.
    apps.retain(|(n, _)| {
        !notes
            .get(&n.to_lowercase())
            .map(|x| x.ignore)
            .unwrap_or(false)
    });
    let total = apps.len();
    let mut note = None;
    if apps.len() > AI_CHECK_CAP {
        apps.truncate(AI_CHECK_CAP);
        note = Some(format!(
            "Checked the first {AI_CHECK_CAP} of {total} apps winget doesn't manage."
        ));
    }
    if apps.is_empty() {
        return Ok(AiCheckResult {
            updates: vec![],
            checked: 0,
            cost_usd: 0.0,
            note: Some("No non-winget apps to check.".into()),
        });
    }

    // 2. Ask Claude to look up the latest versions, honouring any user notes.
    let app_lines = apps
        .iter()
        .map(|(n, v)| match notes.get(&n.to_lowercase()) {
            Some(x) if !x.note.is_empty() => format!("- {n} ({v}) [user note: {}]", x.note),
            _ => format!("- {n} ({v})"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "You are an application update checker. Below are installed Windows applications with their \
current versions. Use web search to find each one's latest STABLE release from its official source. \
Return ONLY the apps that have a NEWER version available.\n\n\
Respect any [user note]: it may say an app is custom/self-built or give its real release source — \
follow that guidance and do NOT report an update that contradicts the note.\n\n\
Respond ONLY with JSON, no markdown:\n\
{{\"updates\":[{{\"name\":\"<app>\",\"current\":\"<installed>\",\"latest\":\"<newer version>\",\"url\":\"<official download or releases page URL>\"}}]}}\n\
Omit apps that are already current or that you cannot verify. Only include real, verified versions.\n\n\
INSTALLED APPS:\n{app_lines}"
    );

    let (updates, cost) = run_ai_check(prompt).await?;
    Ok(AiCheckResult {
        updates,
        checked: apps.len(),
        cost_usd: cost,
        note,
    })
}

/// Re-check a single app (using its stored note) — cheaper than a full sweep.
#[tauri::command]
pub async fn check_app_update(name: String, current: String) -> Result<AiCheckResult, String> {
    let notes = load_notes();
    let key = name.to_lowercase();
    if notes.get(&key).map(|x| x.ignore).unwrap_or(false) {
        return Ok(AiCheckResult {
            updates: vec![],
            checked: 1,
            cost_usd: 0.0,
            note: Some(format!("{name} is on your ignore list.")),
        });
    }
    let note_line = match notes.get(&key) {
        Some(x) if !x.note.is_empty() => format!(" [user note: {}]", x.note),
        _ => String::new(),
    };
    let prompt = format!(
        "You are an application update checker. Use web search to find the latest STABLE release of \
the Windows app below from its official source, and report whether a newer version exists.\n\n\
Respect any [user note]: it may say the app is custom/self-built or give its real release source — \
follow it and do NOT report an update that contradicts the note.\n\n\
Respond ONLY with JSON, no markdown:\n\
{{\"updates\":[{{\"name\":\"{name}\",\"current\":\"<installed>\",\"latest\":\"<newer version>\",\"url\":\"<official download or releases page URL>\"}}]}}\n\
Return an empty updates array if it is already current or you cannot verify a newer version.\n\n\
APP: {name} ({current}){note_line}"
    );
    let (updates, cost) = run_ai_check(prompt).await?;
    Ok(AiCheckResult {
        updates,
        checked: 1,
        cost_usd: cost,
        note: None,
    })
}

/// Route an update-check prompt through whichever provider the service is
/// configured for (read from config.toml next to the executable):
/// OpenRouter free model + web plugin, or the Claude CLI (default fallback).
async fn run_ai_check(prompt: String) -> Result<(Vec<AiUpdate>, f64), String> {
    let cfg = resolve_ai_cfg();
    if cfg.provider == "openrouter" {
        let key = cfg.openrouter_api_key.unwrap_or_default();
        if key.trim().is_empty() {
            return Err(
                "OpenRouter is selected but no API key is set — add it in Settings.".into(),
            );
        }
        let model = if cfg.model.trim().is_empty() {
            "openrouter/free".to_string()
        } else {
            cfg.model.trim().to_string()
        };
        run_openrouter_check(prompt, &model, key.trim()).await
    } else {
        // Claude CLI (also the fallback for anthropic / openai_compatible, which
        // don't have a built-in web path here). update_check_model -> haiku.
        run_claude_check(prompt, &cfg.update_check_model).await
    }
}

/// The AI settings the update check needs, read from the on-disk config.toml
/// (the service keeps the API key out of the pipe payload, so read it directly).
#[derive(Deserialize, Default)]
struct FileApiCfg {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    openrouter_api_key: Option<String>,
    #[serde(default)]
    update_check_model: String,
}

#[derive(Deserialize, Default)]
struct FileCfg {
    #[serde(default)]
    api: FileApiCfg,
}

fn resolve_ai_cfg() -> FileApiCfg {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.toml")))
        .unwrap_or_else(|| std::path::PathBuf::from("config.toml"));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<FileCfg>(&s).ok())
        .map(|c| c.api)
        .unwrap_or_default()
}

/// Ask OpenRouter (with its web-search plugin) for app updates. Works with free
/// models — OpenRouter performs the search and feeds results to the model, so it
/// is model-agnostic. Returns the parsed updates plus the call's USD cost.
async fn run_openrouter_check(
    prompt: String,
    model: &str,
    key: &str,
) -> Result<(Vec<AiUpdate>, f64), String> {
    let body = serde_json::json!({
        "model": model,
        "plugins": [{ "id": "web", "max_results": 5 }],
        "messages": [{ "role": "user", "content": prompt }],
    });
    let resp = reqwest::Client::new()
        .post("https://openrouter.ai/api/v1/chat/completions")
        .bearer_auth(key)
        .header("HTTP-Referer", "https://github.com/Swatto86/eir")
        .header("X-Title", "Eir")
        .json(&body)
        .timeout(std::time::Duration::from_secs(420))
        .send()
        .await
        .map_err(|e| format!("OpenRouter request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail: String = text.chars().take(400).collect();
        return Err(format!("OpenRouter error ({status}): {detail}"));
    }

    let parsed: OrResp =
        serde_json::from_str(&text).map_err(|e| format!("bad OpenRouter response: {e}"))?;
    if let Some(err) = parsed.error {
        return Err(format!("OpenRouter error: {}", err.message));
    }
    let content = parsed
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .unwrap_or_default();
    let cost = parsed.usage.and_then(|u| u.cost).unwrap_or(0.0);

    let json = extract_json(&content);
    let resp_updates: AiResp =
        serde_json::from_str(json).map_err(|e| format!("could not parse update list: {e}"))?;
    let updates = resp_updates
        .updates
        .into_iter()
        .filter(|u| !u.name.is_empty() && !u.latest.is_empty())
        .map(|u| AiUpdate {
            name: u.name,
            current: u.current,
            latest: u.latest,
            url: u.url,
        })
        .collect();
    Ok((updates, cost))
}

#[derive(Deserialize)]
struct OrResp {
    #[serde(default)]
    choices: Vec<OrChoice>,
    #[serde(default)]
    usage: Option<OrUsage>,
    #[serde(default)]
    error: Option<OrError>,
}

#[derive(Deserialize)]
struct OrChoice {
    message: OrMsg,
}

#[derive(Deserialize)]
struct OrMsg {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct OrUsage {
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Deserialize)]
struct OrError {
    #[serde(default)]
    message: String,
}

/// Pull the JSON object out of a model response that may wrap it in prose or
/// code fences (reasoning models often add commentary around the JSON).
fn extract_json(s: &str) -> &str {
    let t = strip_fences(s);
    if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
        if b > a {
            return &t[a..=b];
        }
    }
    t
}

/// Spawn `claude --print --output-format json` with the given prompt and model,
/// parse the result, and return the found updates plus the call's USD cost.
/// Map a requested model to a Claude model the CLI will accept. Claude aliases
/// (`haiku`/`sonnet`/`opus`) and any `claude*` id pass through; everything else
/// — blank, or a non-Claude id such as an OpenRouter model — becomes `haiku`.
fn claude_model_or_haiku(model: &str) -> String {
    let m = model.trim();
    let lower = m.to_lowercase();
    let is_claude =
        matches!(lower.as_str(), "haiku" | "sonnet" | "opus") || lower.starts_with("claude");
    if is_claude {
        m.to_string()
    } else {
        "haiku".to_string()
    }
}

async fn run_claude_check(prompt: String, model: &str) -> Result<(Vec<AiUpdate>, f64), String> {
    let binary = claude_binary();
    let mut std_cmd = std::process::Command::new(&binary);
    std_cmd.args(["--print", "--output-format", "json"]);
    // The app-update check needs live web search, so it must run on a Claude
    // model. Blank — or anything that isn't a Claude model (e.g. an OpenRouter
    // id left over from the decision-loop model) — falls back to Haiku, the
    // cheapest Claude model, so the check can never be handed a model the CLI
    // would reject.
    let model = claude_model_or_haiku(model);
    std_cmd.args(["--model", model.as_str()]);
    std_cmd.creation_flags(CREATE_NO_WINDOW);
    let mut cmd = tokio::process::Command::from(std_cmd);
    cmd.kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run claude: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(420),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| "AI check timed out after 7 minutes".to_string())?
    .map_err(|e| e.to_string())?;

    if !output.status.success() {
        // `claude --output-format json` writes its error to stdout, not stderr,
        // so check both. Prefer whichever has content.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let raw = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        let detail: String = if raw.is_empty() {
            "no output from claude — check the logged-in claude session".to_string()
        } else {
            raw.chars().take(400).collect()
        };
        return Err(format!(
            "claude exited (code {}) using model '{model}': {detail}",
            output.status.code().unwrap_or(-1)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let env: CliEnvelope =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("bad claude output: {e}"))?;
    let cost = env.total_cost_usd.unwrap_or(0.0);
    let result_text = env.result.unwrap_or_default();
    let json = strip_fences(&result_text);
    let resp: AiResp =
        serde_json::from_str(json).map_err(|e| format!("could not parse update list: {e}"))?;
    let updates = resp
        .updates
        .into_iter()
        .filter(|u| !u.name.is_empty() && !u.latest.is_empty())
        .map(|u| AiUpdate {
            name: u.name,
            current: u.current,
            latest: u.latest,
            url: u.url,
        })
        .collect();
    Ok((updates, cost))
}

/// Current USD→GBP rate for displaying costs in pounds. Fetched (cached by the
/// caller) with a sensible fallback if offline.
#[tauri::command]
pub async fn gbp_per_usd() -> Result<f64, String> {
    let rate = tokio::task::spawn_blocking(|| {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "try { (Invoke-RestMethod -Uri 'https://open.er-api.com/v6/latest/USD' -TimeoutSec 8).rates.GBP } catch { '' }",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        out.ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|r| *r > 0.1 && *r < 5.0)
            .unwrap_or(0.79)
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(rate)
}

/// Open an http(s) URL in the user's default browser.
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("refusing to open a non-http URL".into());
    }
    let script = format!("Start-Process '{}'", url.replace('\'', "''"));
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a winget-style fixed-width table from explicit column widths, a
    /// header, and data rows. Mirrors winget exactly: columns are left-aligned and
    /// joined by a single space (so when a field fills its column the gap is one
    /// space — the layout that defeats whitespace splitting), and any field longer
    /// than its column is truncated with a trailing '…'.
    fn render(widths: &[usize], header: &[&str], rows: &[&[&str]]) -> String {
        fn line(widths: &[usize], fields: &[&str]) -> String {
            let cells: Vec<String> = fields
                .iter()
                .enumerate()
                .map(|(c, &f)| {
                    let w = widths[c];
                    if f.chars().count() > w {
                        f.chars().take(w - 1).collect::<String>() + "…"
                    } else {
                        format!("{f:<w$}")
                    }
                })
                .collect();
            cells.join(" ").trim_end().to_string()
        }
        let total: usize = widths.iter().sum::<usize>() + widths.len().saturating_sub(1);
        let mut out = line(widths, header);
        out.push('\n');
        out.push_str(&"-".repeat(total));
        for r in rows {
            out.push('\n');
            out.push_str(&line(widths, r));
        }
        out
    }

    #[test]
    fn header_offsets_ignore_progress_spinner_noise() {
        // winget prefixes the header with carriage-return-overwritten spinner text;
        // only the text after the last '\r' is the real header.
        let noisy = "  -  \r  \\  \rName    Id      Version  Source";
        let clean = "Name    Id      Version  Source";
        assert_eq!(header_offsets(noisy), header_offsets(clean));
        assert_eq!(header_offsets(clean).first(), Some(&("Name", 0)));
    }

    #[test]
    fn parse_upgrades_handles_single_space_columns() {
        // Every field fills its column, so winget separates them with a single
        // space — the narrow layout that previously parsed to zero upgrades.
        let widths = [11, 14, 7, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &[
                    "Copilot CLI",
                    "GitHub.Copilot",
                    "v1.0.44",
                    "v1.0.63",
                    "winget",
                ],
                &["7-Zip", "7zip.7zip", "25.01", "26.01", "winget"],
            ],
        );
        let table = format!("{table}\n2 upgrades available.");
        let ups = parse_upgrades(&table);
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].name, "Copilot CLI");
        assert_eq!(ups[0].id, "GitHub.Copilot");
        assert_eq!(ups[0].current, "v1.0.44");
        assert_eq!(ups[0].available, "v1.0.63");
    }

    #[test]
    fn parse_upgrades_strips_truncated_id() {
        // A long id is truncated with '…'; winget's `--id` substring match still
        // resolves it, so we keep the prefix and drop the ellipsis.
        let widths = [30, 33, 23, 9, 6];
        let header = ["Name", "Id", "Version", "Available", "Source"];
        let table = render(
            &widths,
            &header,
            &[&[
                "Visual Studio Build Tools 2022",
                "Microsoft.VisualStudio.2022.BuildTools",
                "17.14.25 (January 2026)",
                "17.14.34",
                "winget",
            ]],
        );
        let ups = parse_upgrades(&table);
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].name, "Visual Studio Build Tools 2022");
        assert_eq!(ups[0].id, "Microsoft.VisualStudio.2022.Buil");
        assert_eq!(ups[0].current, "17.14.25 (January 2026)");
        assert_eq!(ups[0].available, "17.14.34");
    }

    #[test]
    fn unmanaged_classifies_winget_list_rows() {
        // Id width forces '…' truncation on long ARP/MSIX ids; Source width forces
        // 'msstore' to truncate to 'mssto…' — both are real winget behaviours.
        let widths = [22, 30, 18, 6];
        let header = ["Name", "Id", "Version", "Source"];
        let table = render(
            &widths,
            &header,
            &[
                &["7-Zip", "7zip.7zip", "25.01", "winget"], // winget-managed → skip
                &["iCloud", "9PKTQ5699M62", "15.8.127.0", "msstore"], // store (mssto…) → skip
                &["Git", "ARP\\Machine\\X64\\Git_is1", "2.52.0", ""], // unmanaged → keep
                &[
                    "PSForge",
                    "ARP\\Machine\\X64\\{2B48800A-5551-4F2F-97F0-2B5B1234ABCD}",
                    "1.2.18",
                    "",
                ], // truncated id, unmanaged → keep (was dropped before)
                &["Battle.net", "ARP\\Machine\\X86\\Battle.net", "Unknown", ""], // ".net" must not filter it
                &[
                    "NVIDIA Graphics Driver",
                    "ARP\\Machine\\X64\\{B2FE1952-0186-46C3}",
                    "596.49",
                    "",
                ], // driver → noise
                &[
                    "AV1 Video Extension",
                    "MSIX\\Microsoft.AV1VideoExtension_2.0.7.0_x64",
                    "2.0.7.0",
                    "",
                ], // MSIX/Store → skip
                &[
                    "Microsoft .NET Runtime",
                    "ARP\\Machine\\X64\\{DOTNET8}",
                    "8.0.11",
                    "",
                ], // runtime → noise
            ],
        );
        let apps = parse_unmanaged(&table);
        let names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        // Kept: real third-party apps, including one whose long id was truncated.
        assert!(names.contains(&"Git"));
        assert!(names.contains(&"PSForge"));
        assert!(names.contains(&"Battle.net"));
        let psforge = apps.iter().find(|(n, _)| n == "PSForge").unwrap();
        assert_eq!(psforge.1, "1.2.18");
        // Excluded: winget source, truncated msstore source, driver/runtime noise,
        // and MSIX/Store-delivered packages.
        assert!(!names.contains(&"7-Zip"));
        assert!(!names.contains(&"iCloud"));
        assert!(!names.contains(&"NVIDIA Graphics Driver"));
        assert!(!names.contains(&"AV1 Video Extension"));
        assert!(!names.contains(&"Microsoft .NET Runtime"));
    }

    #[test]
    fn non_claude_model_falls_back_to_haiku() {
        // Blank and non-Claude ids become haiku; Claude aliases/ids pass through.
        assert_eq!(claude_model_or_haiku(""), "haiku");
        assert_eq!(claude_model_or_haiku("  "), "haiku");
        assert_eq!(claude_model_or_haiku("nex-agi/nex-n2-pro:free"), "haiku");
        assert_eq!(
            claude_model_or_haiku("nvidia/nemotron-3-super-120b-a12b:free"),
            "haiku"
        );
        assert_eq!(claude_model_or_haiku("haiku"), "haiku");
        assert_eq!(claude_model_or_haiku("Sonnet"), "Sonnet");
        assert_eq!(claude_model_or_haiku("opus"), "opus");
        assert_eq!(
            claude_model_or_haiku("claude-haiku-4-5"),
            "claude-haiku-4-5"
        );
    }
}
