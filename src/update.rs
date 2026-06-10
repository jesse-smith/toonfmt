//! `toonfmt update` — in-binary self-update for installer-based builds.
//!
//! Uses [`axoupdater`] as a library (not dist's standalone updater binary, which
//! is disabled via `install-updater = false`) so the single-binary ethos holds: no
//! second `toonfmt-update` executable. The updater reads the install receipt the
//! `dist` shell/PowerShell installer wrote, checks GitHub Releases for a newer
//! version, and re-runs the installer in place.
//!
//! On a `cargo install` / source build there is **no receipt**: the command then
//! exits gracefully with guidance rather than erroring or clobbering the binary.

use anyhow::{Result, bail};
use axoupdater::AxoUpdater;

/// Check for updates and self-update `toonfmt` if a newer version is available.
///
/// Synchronous (the `blocking` axoupdater feature), so it is called directly from
/// the dispatch in `main.rs` without an async runtime on the update path.
pub fn run_update() -> Result<()> {
    let mut updater = AxoUpdater::new_for("toonfmt");
    let version: axoupdater::Version = env!("CARGO_PKG_VERSION").parse()?;
    updater.set_current_version(version)?;

    // Try to load the install receipt. If there is no receipt, the binary was
    // installed via `cargo install` or built from source — guide the user to the
    // method they used and return Ok (never error or clobber).
    if let Err(e) = updater.load_receipt() {
        if is_no_receipt(&e) {
            eprintln!("toonfmt was not installed via the shell/PowerShell installer.");
            eprintln!("Self-update is only available for installer-based installations.");
            eprintln!("Please update with: cargo install toonfmt");
            return Ok(());
        }
        // Receipt exists but could not be loaded — treat as a mismatch.
        eprintln!("This copy of toonfmt was not installed by the shell/PowerShell installer.");
        eprintln!("Please update with the method you originally used to install it.");
        return Ok(());
    }

    eprintln!("Checking for updates...");

    match updater.run_sync() {
        Ok(Some(result)) => {
            let old = result
                .old_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            eprintln!("Updated toonfmt: {} => {}", old, result.new_version);
            Ok(())
        }
        Ok(None) => {
            eprintln!(
                "toonfmt v{} is already up to date.",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Err(e) => {
            if is_network_error(&e) {
                bail!("unable to check for updates \u{2014} are you connected to the internet?");
            }
            if is_no_installer_error(&e) {
                bail!("no installer found for your platform in the latest release.");
            }
            bail!("update failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Error classification.
//
// axoupdater surfaces failures as miette diagnostics; it does NOT expose a stable,
// matchable error enum for these cases, so we classify by lowercasing the rendered
// message and substring-matching. This is brittle across axoupdater versions.
//
// ⚠️ RE-VERIFY THESE STRING MATCHES ON ANY axoupdater BUMP. If a version change
// reworded its errors, these predicates can silently misclassify (e.g. a genuine
// network failure would fall through to the generic "update failed" bail). Pinned
// against axoupdater 0.10.0.
// ---------------------------------------------------------------------------

/// Whether the error indicates no install receipt was found.
fn is_no_receipt(e: &axoupdater::AxoupdateError) -> bool {
    is_no_receipt_msg(&e.to_string().to_lowercase())
}

/// Whether the error is a network/reqwest error.
fn is_network_error(e: &axoupdater::AxoupdateError) -> bool {
    is_network_error_msg(&e.to_string().to_lowercase())
}

/// Whether the error indicates no installer asset exists for this platform.
fn is_no_installer_error(e: &axoupdater::AxoupdateError) -> bool {
    is_no_installer_error_msg(&e.to_string().to_lowercase())
}

// String-matching helpers, split out so they're testable without constructing an
// `AxoupdateError` (which has no public constructor for these cases).

fn is_no_receipt_msg(msg: &str) -> bool {
    // axoupdater 0.10.0's `NoReceipt` renders as "Unable to load receipt for app
    // <name>" — note it matches NONE of the 0.9-era qualifiers below, hence the
    // explicit "unable to load" arm (verified empirically against 0.10.0). This is
    // the canary the plan flagged: re-check this set whenever axoupdater is bumped.
    // NOTE: the qualifier is `"no receipt"`, not a bare `"no"` — a bare substring
    // would match incidental "no" inside `not`, `node`, `diagnostic`, `ignore`, …,
    // misrouting a genuine failure into the swallowed no-receipt path. "unable to
    // load" is axoupdater 0.10.0's actual `NoReceipt` wording (pinned by a test).
    msg.contains("receipt")
        && (msg.contains("not found")
            || msg.contains("no receipt")
            || msg.contains("missing")
            || msg.contains("couldn't")
            || msg.contains("unable to load"))
}

fn is_network_error_msg(msg: &str) -> bool {
    msg.contains("reqwest")
        || msg.contains("network")
        || msg.contains("connect")
        || msg.contains("dns")
        || msg.contains("timed out")
}

fn is_no_installer_error_msg(msg: &str) -> bool {
    msg.contains("installer") && (msg.contains("not found") || msg.contains("no "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_receipt_returns_ok() {
        // Pin axoupdater's receipt search to an empty dir so this exercises the
        // genuine no-receipt branch — hermetically and OFFLINE. Without the
        // override it would find any real install receipt on the dev box and make a
        // live GitHub call (a network-dependent test that "passes" for the wrong
        // reason). `AXOUPDATER_CONFIG_PATH` is read by axoupdater's
        // `get_config_paths`. No other test touches this var, so the process-global
        // set is safe here.
        let empty = std::env::temp_dir().join(format!("toonfmt-noreceipt-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        unsafe { std::env::set_var("AXOUPDATER_CONFIG_PATH", &empty) };

        let result = run_update();

        unsafe { std::env::remove_var("AXOUPDATER_CONFIG_PATH") };
        let _ = std::fs::remove_dir_all(&empty);

        assert!(result.is_ok(), "no-receipt case should return Ok, not Err");
    }

    // --- is_no_receipt_msg ---

    #[test]
    fn no_receipt_matches_receipt_not_found() {
        assert!(is_no_receipt_msg("install receipt not found"));
    }

    #[test]
    fn no_receipt_matches_no_receipt() {
        assert!(is_no_receipt_msg("no receipt for this app"));
    }

    #[test]
    fn no_receipt_matches_receipt_missing() {
        assert!(is_no_receipt_msg("receipt file is missing"));
    }

    #[test]
    fn no_receipt_matches_couldnt_receipt() {
        assert!(is_no_receipt_msg("couldn't load receipt"));
    }

    #[test]
    fn no_receipt_matches_unable_to_load() {
        // The verbatim wording axoupdater 0.10.0 emits for a missing receipt
        // (`AxoupdateError::NoReceipt`). Pinned so a future version reword — or an
        // edit to the matcher — fails here loudly instead of silently routing the
        // no-receipt case to the vaguer fallback message. If this breaks on a bump,
        // that IS the "re-verify the string matches" signal.
        assert!(is_no_receipt_msg(
            &"Unable to load receipt for app toonfmt".to_lowercase()
        ));
    }

    #[test]
    fn no_receipt_rejects_unrelated_error() {
        assert!(!is_no_receipt_msg("network timeout"));
    }

    #[test]
    fn no_receipt_rejects_receipt_without_qualifier() {
        // "receipt" alone, without not found / no receipt / missing / couldn't.
        assert!(!is_no_receipt_msg("receipt loaded successfully"));
    }

    #[test]
    fn no_receipt_rejects_incidental_no_substring() {
        // Pins finding from Q3 review: the qualifier must be "no receipt", not a bare
        // "no". A real failure that mentions "receipt" and contains an incidental
        // "no" (here inside "diagnostic"/"cannot") must NOT be swallowed as the
        // no-receipt (→ Ok) path — it has to surface.
        assert!(!is_no_receipt_msg(
            "receipt diagnostic: cannot reach server"
        ));
    }

    // --- is_network_error_msg ---

    #[test]
    fn network_error_matches_reqwest() {
        assert!(is_network_error_msg("reqwest error: connection refused"));
    }

    #[test]
    fn network_error_matches_network() {
        assert!(is_network_error_msg("network is unreachable"));
    }

    #[test]
    fn network_error_matches_connect() {
        assert!(is_network_error_msg("failed to connect to host"));
    }

    #[test]
    fn network_error_matches_dns() {
        assert!(is_network_error_msg("dns resolution failed"));
    }

    #[test]
    fn network_error_matches_timed_out() {
        assert!(is_network_error_msg("request timed out"));
    }

    #[test]
    fn network_error_rejects_auth_error() {
        assert!(!is_network_error_msg("invalid token"));
    }

    // --- is_no_installer_error_msg ---

    #[test]
    fn no_installer_matches_installer_not_found() {
        assert!(is_no_installer_error_msg(
            "installer not found for this platform"
        ));
    }

    #[test]
    fn no_installer_matches_no_installer() {
        assert!(is_no_installer_error_msg("no installer available"));
    }

    #[test]
    fn no_installer_rejects_installer_without_qualifier() {
        assert!(!is_no_installer_error_msg("installer ran successfully"));
    }

    #[test]
    fn no_installer_rejects_unrelated_error() {
        assert!(!is_no_installer_error_msg("network timeout"));
    }
}
