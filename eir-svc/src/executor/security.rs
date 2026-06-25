//! Security self-healing actions: re-enable the Windows Firewall and refresh /
//! turn on Windows Defender. All three are safe and reversible — they restore a
//! protective default, never weaken it. Each pairs with a signal in
//! `SystemState.security` so a disabled firewall / stale definitions / disabled
//! real-time protection becomes a detectable, fixable fault.

use anyhow::{bail, Result};

/// Map a profile name from the AI to the `netsh advfirewall` argument.
/// Accepts domain | private | public | all (case-insensitive).
fn firewall_profile_arg(profile: &str) -> Result<&'static str> {
    match profile.trim().to_lowercase().as_str() {
        "domain" => Ok("domainprofile"),
        "private" => Ok("privateprofile"),
        "public" => Ok("publicprofile"),
        "all" | "allprofiles" | "" => Ok("allprofiles"),
        other => bail!("Unknown firewall profile '{other}' (use domain|private|public|all)"),
    }
}

/// Turn the Windows Firewall on for the named profile (or all profiles).
pub async fn firewall_enable(profile: &str) -> Result<String> {
    let arg = firewall_profile_arg(profile)?;
    // netsh is the most reliable way to set firewall state from a service.
    let script = format!(
        "netsh advfirewall set {arg} state on; \
         if ($LASTEXITCODE -ne 0) {{ throw 'netsh advfirewall failed' }}; \
         Write-Output 'Firewall enabled for {arg}'"
    );
    super::powershell::run_diagnostic(&script).await
}

/// Refresh Windows Defender's signature definitions. Only ever pulls newer
/// definitions, so it is always safe to run.
pub async fn defender_signature_update() -> Result<String> {
    // Bounded longer than the default — a definition download can take a minute.
    let script = "Update-MpSignature -ErrorAction Stop; \
                  Write-Output 'Defender signatures updated'";
    super::powershell::run_diagnostic_with_timeout(script, std::time::Duration::from_secs(300))
        .await
}

/// Re-enable Windows Defender real-time (on-access) protection.
pub async fn defender_realtime_enable() -> Result<String> {
    let script = "Set-MpPreference -DisableRealtimeMonitoring $false -ErrorAction Stop; \
                  Write-Output 'Defender real-time protection enabled'";
    super::powershell::run_diagnostic(script).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_profiles_map_to_netsh_args() {
        assert_eq!(firewall_profile_arg("domain").unwrap(), "domainprofile");
        assert_eq!(firewall_profile_arg("Private").unwrap(), "privateprofile");
        assert_eq!(firewall_profile_arg(" public ").unwrap(), "publicprofile");
        assert_eq!(firewall_profile_arg("all").unwrap(), "allprofiles");
        // Blank defaults to all profiles rather than erroring.
        assert_eq!(firewall_profile_arg("").unwrap(), "allprofiles");
    }

    #[test]
    fn unknown_firewall_profile_is_rejected() {
        assert!(firewall_profile_arg("dmz").is_err());
    }
}
