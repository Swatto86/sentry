//! On-disk configuration for the update subsystem (the `[updater]` section of
//! `config.toml`). The whole struct is `#[serde(default)]`, and the parent
//! `Config` carries it as `#[serde(default)]`, so an existing `config.toml` with
//! no `[updater]` section still loads — every field falls back to a sane default.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How strictly a downloaded AI-found native installer must be signed before it
/// is allowed to run. Decided in Rust *before* the installer is staged, never by
/// the AI. Defaults to the safe choice for an unattended SYSTEM install.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePolicy {
    /// Installer must carry a valid Authenticode signature (any trusted signer).
    #[default]
    RequireValid,
    /// Valid signature AND the signer subject must contain the expected publisher.
    RequirePublisherMatch,
    /// Run regardless of signature — least safe, explicit opt-in only.
    AllowUnsigned,
}

/// Configuration for the autonomous updater. Field order matters for TOML
/// serialization: all scalars/arrays come before the `notes` sub-table so the
/// emitted `[updater]` section is valid (a sub-table must follow its parent's
/// own keys).
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct UpdaterConfig {
    /// Master switch. Off by default — running installers unattended as SYSTEM is
    /// opt-in; the user turns it on in Settings.
    pub enabled: bool,
    /// How often an unattended update cycle runs, in seconds.
    pub schedule_interval_secs: u64,
    /// Package-manager methods to use, in preference order. Unknown names are
    /// ignored at use time. `native` (AI-found installers) is gated separately by
    /// `native_enabled`, not listed here.
    pub methods: Vec<String>,
    /// Whether AI-found native installers may be downloaded and run at all.
    pub native_enabled: bool,
    /// Signature gate applied to native installs.
    pub native_signature_policy: SignaturePolicy,
    /// Max methods tried for one app before giving up.
    pub max_attempts_per_app: u32,
    /// Max apps acted on in a single cycle (bounds cost and blast radius).
    pub max_apps_per_run: u32,
    /// AI spend ceiling (USD) per cycle; 0 = no explicit ceiling (still bounded by
    /// the attempt caps).
    pub budget_usd_per_run: f64,
    /// Largest native installer to download, in MiB.
    pub max_installer_mb: u64,
    /// Auto-install a missing package manager (Chocolatey/Scoop) when a method
    /// needs it.
    pub bootstrap_managers: bool,
    /// App identities to skip entirely. Keyed by the stable, version-stripped,
    /// lowercased display name (the same key used for notes).
    pub ignored: Vec<String>,
    /// Per-app freeform hints for the AI, keyed by the stable app identity. A
    /// `BTreeMap` so serialization is deterministic (stable diffs/tests).
    pub notes: BTreeMap<String, String>,
}

impl Default for UpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule_interval_secs: 24 * 3600,
            methods: vec![
                "winget".to_string(),
                "choco".to_string(),
                "scoop".to_string(),
                "msstore".to_string(),
            ],
            native_enabled: true,
            native_signature_policy: SignaturePolicy::default(),
            max_attempts_per_app: 3,
            max_apps_per_run: 20,
            budget_usd_per_run: 0.50,
            max_installer_mb: 256,
            bootstrap_managers: true,
            ignored: Vec::new(),
            notes: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_policy_round_trips_as_snake_case_string() {
        // TOML stores the enum as a bare string; the snake_case names must survive.
        for (policy, token) in [
            (SignaturePolicy::RequireValid, "require_valid"),
            (
                SignaturePolicy::RequirePublisherMatch,
                "require_publisher_match",
            ),
            (SignaturePolicy::AllowUnsigned, "allow_unsigned"),
        ] {
            #[derive(Serialize, Deserialize)]
            struct Wrap {
                p: SignaturePolicy,
            }
            let s = toml::to_string(&Wrap { p: policy }).unwrap();
            assert!(s.contains(token), "{s} should contain {token}");
            let back: Wrap = toml::from_str(&s).unwrap();
            assert_eq!(back.p, policy);
        }
    }

    #[test]
    fn default_serializes_and_round_trips() {
        let cfg = UpdaterConfig::default();
        let toml = toml::to_string_pretty(&cfg).expect("serialize");
        let back: UpdaterConfig = toml::from_str(&toml).expect("reparse");
        assert_eq!(back.enabled, cfg.enabled);
        assert_eq!(back.methods, cfg.methods);
        assert_eq!(back.native_signature_policy, cfg.native_signature_policy);
        assert_eq!(back.budget_usd_per_run, cfg.budget_usd_per_run);
    }
}
