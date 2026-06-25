//! The "Check" step: build the list of update candidates from every source. winget
//! `upgrade` gives the package-manager candidates; `winget list` + an AI web-search
//! pass covers the apps no package manager can update (correlated-standalone and
//! ARP/unmanaged), which become native candidates. Results are de-duplicated and
//! filtered against the user's ignore list and notes.

use crate::ai::client::{extract_json, AiClient};
use crate::updater::config::UpdaterConfig;
use crate::updater::domain::{Method, UpdateCandidate};
use crate::updater::methods::winget;
use crate::updater::names::match_installed;
use crate::updater::version::is_newer;
use crate::updater::winget_parse::parse_unmanaged;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Cap on apps sent to the AI in one batch, to bound cost/latency.
const AI_CHECK_CAP: usize = 20;

/// The result of a full check.
pub struct CheckResult {
    pub candidates: Vec<UpdateCandidate>,
    pub cost_usd: f64,
    /// Human-readable notes (truncation, AI-check failures) for the UI.
    pub notes: Vec<String>,
}

fn ignored(cfg: &UpdaterConfig, id: &str) -> bool {
    cfg.ignored.iter().any(|ig| ig.eq_ignore_ascii_case(id))
}

/// Collect every update candidate across the enabled methods.
pub async fn collect(
    ai: Option<&AiClient>,
    cfg: &UpdaterConfig,
    model_override: &str,
) -> CheckResult {
    let enabled: Vec<Method> = cfg
        .methods
        .iter()
        .filter_map(|m| Method::from_token(m))
        .collect();
    let mut candidates: Vec<UpdateCandidate> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut cost = 0.0;

    // 1. winget upgrade candidates (winget first, native as a fallback).
    let winget_ups = if enabled.contains(&Method::Winget) {
        winget::list_updates().await
    } else {
        Vec::new()
    };
    let managed: HashSet<String> = winget_ups.iter().map(|u| u.name.to_lowercase()).collect();
    for u in &winget_ups {
        let id = crate::updater::names::clean_app_name(&u.name).to_lowercase();
        if ignored(cfg, &id) {
            continue;
        }
        let mut methods = vec![Method::Winget];
        if cfg.native_enabled && ai.is_some() {
            methods.push(Method::Native);
        }
        candidates.push(UpdateCandidate {
            id,
            name: u.name.clone(),
            current: u.current.clone(),
            available: u.available.clone(),
            package_id: Some(u.id.clone()),
            methods,
        });
    }

    // 2. AI check for apps winget can't manage -> native candidates.
    if cfg.native_enabled {
        if let Some(ai) = ai {
            match check_unmanaged(ai, cfg, model_override, &managed).await {
                Ok((mut native_cands, c, note)) => {
                    cost += c;
                    if let Some(n) = note {
                        notes.push(n);
                    }
                    candidates.append(&mut native_cands);
                }
                Err(e) => notes.push(format!("couldn't check non-winget apps: {e}")),
            }
        }
    }

    CheckResult {
        candidates,
        cost_usd: cost,
        notes,
    }
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

/// Ask the AI which unmanaged apps have a newer version, and turn the verified ones
/// into native candidates.
async fn check_unmanaged(
    ai: &AiClient,
    cfg: &UpdaterConfig,
    model_override: &str,
    managed: &HashSet<String>,
) -> Result<(Vec<UpdateCandidate>, f64, Option<String>), String> {
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

    let mut apps = parse_unmanaged(&String::from_utf8_lossy(&list_out.stdout), managed);
    apps.retain(|(n, _)| !ignored(cfg, &n.to_lowercase()));
    let total = apps.len();
    let mut note = None;
    if apps.len() > AI_CHECK_CAP {
        apps.truncate(AI_CHECK_CAP);
        note = Some(format!(
            "Checked the first {AI_CHECK_CAP} of {total} apps winget doesn't manage."
        ));
    }
    if apps.is_empty() {
        return Ok((vec![], 0.0, None));
    }

    let app_lines = apps
        .iter()
        .map(|(n, v)| match cfg.notes.get(&n.to_lowercase()) {
            Some(note) if !note.is_empty() => format!("- {n} ({v}) [user note: {note}]"),
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

    let (content, usage) = ai
        .complete(&prompt, model_override)
        .await
        .map_err(|e| e.to_string())?;
    let cost = usage.map(|u| u.cost_usd).unwrap_or(0.0);
    let resp: AiResp = serde_json::from_str(extract_json(&content))
        .map_err(|e| format!("could not parse update list: {e}"))?;

    let installed: HashMap<String, String> = apps
        .iter()
        .map(|(n, v)| (n.to_lowercase(), v.clone()))
        .collect();
    let candidates = native_candidates_from(&resp.updates, &installed, cfg);
    Ok((candidates, cost, note))
}

/// Pure: turn the AI's reported updates into native candidates, keeping only those
/// strictly newer than what is actually installed and not on the ignore list, and
/// stamping each with the authoritative installed version. Split out so the
/// filtering is unit-testable without a live provider.
fn native_candidates_from(
    updates: &[AiUpdateRaw],
    installed: &HashMap<String, String>,
    cfg: &UpdaterConfig,
) -> Vec<UpdateCandidate> {
    let mut out = Vec::new();
    for u in updates {
        if u.name.trim().is_empty() || u.latest.trim().is_empty() {
            continue;
        }
        let cur = match_installed(installed, &u.name)
            .cloned()
            .unwrap_or_else(|| u.current.clone());
        if !is_newer(&u.latest, &cur) {
            continue;
        }
        let id = u.name.to_lowercase();
        if ignored(cfg, &id) {
            continue;
        }
        out.push(UpdateCandidate {
            id,
            name: u.name.clone(),
            current: cur,
            available: u.latest.clone(),
            package_id: None,
            methods: vec![Method::Native],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(name: &str, current: &str, latest: &str) -> AiUpdateRaw {
        AiUpdateRaw {
            name: name.to_string(),
            current: current.to_string(),
            latest: latest.to_string(),
            url: String::new(),
        }
    }

    #[test]
    fn native_candidates_keep_only_strictly_newer_and_respect_ignore() {
        let installed: HashMap<String, String> = [
            ("obsidian".to_string(), "1.5.0".to_string()),
            ("krita".to_string(), "5.2.0".to_string()),
            ("oldtool".to_string(), "2.9.0".to_string()),
        ]
        .into_iter()
        .collect();
        let cfg = UpdaterConfig {
            ignored: vec!["krita".to_string()],
            ..UpdaterConfig::default()
        };
        let updates = vec![
            upd("Obsidian", "1.5.0", "1.6.0"), // newer -> kept
            upd("Krita", "5.2.0", "5.3.0"),    // newer but ignored -> dropped
            upd("OldTool", "2.9.0", "2.7.5"),  // AI hallucinated an older "update" -> dropped
            upd("Empty", "1.0", ""),           // no latest -> dropped
        ];
        let cands = native_candidates_from(&updates, &installed, &cfg);
        let names: Vec<&str> = cands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Obsidian"]);
        // The kept candidate carries the authoritative installed version + target.
        assert_eq!(cands[0].current, "1.5.0");
        assert_eq!(cands[0].available, "1.6.0");
        assert_eq!(cands[0].methods, vec![Method::Native]);
    }
}
