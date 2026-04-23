//! Bundled skill files, embedded at compile time.
//!
//! Skill content ships inside the `b3` binary via include_bytes! so every
//! `b3 update` atomically refreshes both the MCP runtime and the skills that
//! depend on it — impossible to have skills on v0.3.X with binary on v0.3.Y.

use std::path::Path;

/// (relative_path_under_skills_root, file_bytes)
///
/// Sorted. New skill files require a new line here — no build.rs auto-walker
/// because it would silently ship anything dropped into skills/. Explicit is safer.
pub const SKILL_FILES: &[(&str, &[u8])] = &[
    ("b3-codebase/SKILL.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/SKILL.md")),
    ("b3-codebase/examples/voice-round-trip.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/examples/voice-round-trip.md")),
    ("b3-codebase/references/app-proxy.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/app-proxy.md")),
    ("b3-codebase/references/child-agents.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/child-agents.md")),
    ("b3-codebase/references/daemon-architecture.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/daemon-architecture.md")),
    ("b3-codebase/references/gpu-worker.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/gpu-worker.md")),
    ("b3-codebase/references/mcp-tools.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/mcp-tools.md")),
    ("b3-codebase/references/reliability-layer.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/reliability-layer.md")),
    ("b3-codebase/references/webrtc-transport.md",
     include_bytes!("../../../plugins/b3/skills/b3-codebase/references/webrtc-transport.md")),
    ("incident-investigation/SKILL.md",
     include_bytes!("../../../plugins/b3/skills/incident-investigation/SKILL.md")),
    ("incident-investigation/examples/pusher-buffer-investigation.md",
     include_bytes!("../../../plugins/b3/skills/incident-investigation/examples/pusher-buffer-investigation.md")),
    ("incident-investigation/references/anti-pattern-catalog.md",
     include_bytes!("../../../plugins/b3/skills/incident-investigation/references/anti-pattern-catalog.md")),
    ("incident-investigation/references/assumption-lock-in.md",
     include_bytes!("../../../plugins/b3/skills/incident-investigation/references/assumption-lock-in.md")),
    ("incident-investigation/references/audit-report-template.md",
     include_bytes!("../../../plugins/b3/skills/incident-investigation/references/audit-report-template.md")),
    ("incident-investigation/references/investigation-template.md",
     include_bytes!("../../../plugins/b3/skills/incident-investigation/references/investigation-template.md")),
    ("session-recall/SKILL.md",
     include_bytes!("../../../plugins/b3/skills/session-recall/SKILL.md")),
    ("session-recall/references/configuration.md",
     include_bytes!("../../../plugins/b3/skills/session-recall/references/configuration.md")),
    ("session-recall/references/index-cache.md",
     include_bytes!("../../../plugins/b3/skills/session-recall/references/index-cache.md")),
    ("session-recall/references/resurface-api.md",
     include_bytes!("../../../plugins/b3/skills/session-recall/references/resurface-api.md")),
];

/// Write all embedded skills into the target directory (e.g. ~/.claude/skills/).
///
/// Overwrites b3-owned files unconditionally. Files not in SKILL_FILES are left
/// untouched — user additions inside the same directory are preserved.
pub fn install_to(target_dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::with_capacity(SKILL_FILES.len());
    for (rel, bytes) in SKILL_FILES {
        let dst = target_dir.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dst, bytes)?;
        written.push(dst);
    }
    Ok(written)
}
