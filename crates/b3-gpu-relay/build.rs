// Read version from the workspace Cargo.toml at compile time.
// This bakes the version into the binary — no env var needed at runtime.
fn main() {
    // Walk up to find the workspace Cargo.toml
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        dir.pop();
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            let content = std::fs::read_to_string(&cargo_toml).unwrap_or_default();
            // Look for: version = "0.1.XXXX" in [workspace.package] section
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("version") && trimmed.contains('"') {
                    if let Some(start) = trimmed.find('"') {
                        if let Some(end) = trimmed[start+1..].find('"') {
                            let version = &trimmed[start+1..start+1+end];
                            println!("cargo:rustc-env=B3_VERSION={version}");
                            return;
                        }
                    }
                }
            }
        }
        if !dir.pop() { break; }
    }
    // Fallback
    println!("cargo:rustc-env=B3_VERSION=unknown");
}
