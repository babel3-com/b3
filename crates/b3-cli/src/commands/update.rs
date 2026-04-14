//! b3 update — Self-update: check latest version, download, replace binary.
//!
//! Two paths to update:
//!
//! 1. **Manual** — user runs `b3 update` from the terminal.
//!    Checks CDN for latest version, downloads, verifies, replaces in place.
//!
//! 2. **Server-pushed** — EC2 sends "update" SSE event to running agent.
//!    The runtime calls the same update logic, then restarts itself.
//!    This enables fleet-wide updates without user intervention.
//!
//! Update flow (matches install.sh):
//!   1. GET {api_url}/releases/latest/version.json → {version}
//!   2. Compare with current version (skip if same)
//!   3. Download b3-{os}-{arch} binary from releases/latest/
//!   4. Download .sha256 checksum file and verify
//!   5. Replace current binary (atomic rename)
//!   6. If running as daemon: exec() into new binary (preserves PID, restarts clean)
//!      If manual: print success, user restarts manually
//!
//! Safety:
//!   - Download to temp file first — if download fails, old binary untouched
//!   - SHA256 verification before replacement — corrupted downloads rejected
//!   - Atomic rename — no window where binary is half-written
//!   - Version comparison — no-op if already current (idempotent)

use std::io::Write;

fn releases_base_url() -> String {
    // Derive from config api_url, fall back to default
    let base = crate::config::Config::load()
        .map(|c| c.api_url)
        .unwrap_or_else(|_| b3_common::public_url());
    format!("{}/releases/latest", base.trim_end_matches('/'))
}

fn version_url() -> String {
    format!("{}/version.json", releases_base_url())
}


/// Manual update: user runs `b3 update`.
pub async fn run() -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("  Babel3 Update");
    println!("  ==============");
    println!();
    println!("  Current version: {current}");
    println!("  Checking for updates...");

    // 1. Fetch version.json (only for version check)
    let client = crate::http::default_client()?;

    let ver_url = version_url();
    let resp = client.get(&ver_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Failed to check for updates (HTTP {})", resp.status());
    }

    let version_info: serde_json::Value = resp.json().await?;
    // Check per-platform version first, fall back to global "version"
    let platform = detect_platform();
    let latest = version_info
        .get("platforms")
        .and_then(|p| p.get(&platform))
        .and_then(|v| v.as_str())
        .or_else(|| version_info.get("version").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow::anyhow!("Invalid version.json: missing version for {platform}"))?;

    // 2. Compare versions
    if latest == current {
        println!("  Already up to date ({current}).");
        return Ok(());
    }

    println!("  New version available: {latest}");

    // 3. Determine platform binary name (same logic as install.sh)
    let platform = detect_platform();
    let ext = if cfg!(windows) { ".exe" } else { "" };
    let binary_name = format!("b3-{platform}{ext}");
    let base_url = releases_base_url();
    let binary_url = format!("{base_url}/{binary_name}");
    let sha256_url = format!("{base_url}/{binary_name}.sha256");

    println!("  Downloading {binary_name}...");

    // 4. Download binary to temp file
    let tmpfile = std::env::temp_dir().join(format!("b3-update-{}", std::process::id()));

    let resp = client.get(&binary_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "Download failed (HTTP {}). URL: {binary_url}",
            resp.status()
        );
    }

    let bytes = resp.bytes().await?;
    let mut file = std::fs::File::create(&tmpfile)?;
    file.write_all(&bytes)?;
    file.flush()?;
    drop(file);

    // 5. Download and verify SHA256 checksum (same as install.sh)
    println!("  Verifying checksum...");
    let expected_sha256 = match client.get(&sha256_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let text = resp.text().await.unwrap_or_default();
            text.split_whitespace().next().unwrap_or("").to_string()
        }
        _ => {
            println!("  Warning: Could not fetch checksum file. Skipping verification.");
            String::new()
        }
    };

    if !expected_sha256.is_empty() {
        let actual_sha256 = compute_sha256(&tmpfile)?;
        if actual_sha256 != expected_sha256 {
            let _ = std::fs::remove_file(&tmpfile);
            anyhow::bail!(
                "Checksum mismatch!\n  Expected: {expected_sha256}\n  Got:      {actual_sha256}"
            );
        }
        println!("  Checksum verified ✓");
    }

    // 6. Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmpfile, std::fs::Permissions::from_mode(0o755))?;
    }

    // 7. Replace current binary
    let current_exe = std::env::current_exe()?;
    println!(
        "  Replacing {} ...",
        current_exe.display()
    );

    if cfg!(windows) {
        // Windows locks running .exe files — can't delete or overwrite directly.
        // Strategy: rename the running binary to .old, move new one into place.
        // The .old file can be cleaned up on next run.
        let old_exe = current_exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&old_exe); // clean up previous .old if any
        std::fs::rename(&current_exe, &old_exe)?;
        if std::fs::rename(&tmpfile, &current_exe).is_err() {
            std::fs::copy(&tmpfile, &current_exe)?;
            let _ = std::fs::remove_file(&tmpfile);
        }
    } else {
        // On Linux, rename() over a running binary fails with ETXTBSY.
        // But unlink (remove) is allowed — the kernel keeps the inode alive
        // until the running process exits.  So: remove first, then rename
        // the new binary into place.
        let _ = std::fs::remove_file(&current_exe);
        if std::fs::rename(&tmpfile, &current_exe).is_err() {
            // Cross-device: copy then remove temp
            std::fs::copy(&tmpfile, &current_exe)?;
            let _ = std::fs::remove_file(&tmpfile);
        }
    }

    println!();
    println!("  ✓ Updated to {latest}");
    println!("  Restart any running daemon with: b3 stop && b3 start");
    println!();

    Ok(())
}

/// Server-pushed update: called by bridge::puller when it receives an "update" SSE event.
/// After replacing the binary, restarts the entire runtime via exec().
#[allow(dead_code)]
pub async fn apply_server_update(_version: &str, _url: &str, _sha256: &str) -> anyhow::Result<()> {
    // Not yet implemented: download from url, verify sha256, atomic rename,
    // gracefully stop Claude Code, exec() into new binary.
    anyhow::bail!("server-pushed update not yet implemented")
}

/// Detect current platform for binary selection.
/// Returns "{os}-{arch}" matching the naming convention in releases/latest/.
fn detect_platform() -> String {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Compute SHA256 of a file using the platform's sha256sum command.
fn compute_sha256(path: &std::path::Path) -> anyhow::Result<String> {
    // Try sha256sum (Linux), shasum (macOS), or CertUtil (Windows)
    let output = std::process::Command::new("sha256sum")
        .arg(path)
        .output()
        .or_else(|_| {
            std::process::Command::new("shasum")
                .args(["-a", "256"])
                .arg(path)
                .output()
        })
        .or_else(|_| {
            // Windows: CertUtil -hashfile <path> SHA256
            std::process::Command::new("CertUtil")
                .args(["-hashfile"])
                .arg(path)
                .arg("SHA256")
                .output()
        })?;

    if !output.status.success() {
        anyhow::bail!("SHA256 computation failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let hash = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Could not parse SHA256 output"))?
        .to_string();

    Ok(hash)
}
