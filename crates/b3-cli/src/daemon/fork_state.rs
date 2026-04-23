/// State set by the parent process before fork() and read by the daemon child.
///
/// fork() inherits the parent's address space (copy-on-write), so OnceLock statics
/// set before fork are visible to the child without any IPC. This replaces the
/// previous env-var-based approach, which leaked B3_CONFIG_DIR / B3_START_CWD /
/// B3_BROWSER_DIR / B3_APP_PORT / B3_APP_PUBLIC into every descendant process.
///
/// B3_SESSION is intentionally NOT here — it's ambient state set for arbitrary
/// child processes (user shells, Claude Code) to detect nesting, not parent↔fork IPC.

use std::sync::OnceLock;

static CONFIG_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();
static START_CWD: OnceLock<std::path::PathBuf> = OnceLock::new();
static BROWSER_DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
static APP_PORT: OnceLock<Option<u16>> = OnceLock::new();
static APP_PUBLIC: OnceLock<bool> = OnceLock::new();

/// Called by pre_fork (and start_in_process on Windows) before the daemon starts.
/// OnceLock::set is idempotent — second call is a no-op.
pub fn init(
    config_dir: std::path::PathBuf,
    start_cwd: std::path::PathBuf,
    browser_dir: Option<std::path::PathBuf>,
    app_port: Option<u16>,
    app_public: bool,
) {
    let _ = CONFIG_DIR.set(config_dir);
    let _ = START_CWD.set(start_cwd);
    let _ = BROWSER_DIR.set(browser_dir);
    let _ = APP_PORT.set(app_port);
    let _ = APP_PUBLIC.set(app_public);
}

pub fn config_dir() -> Option<&'static std::path::PathBuf> {
    CONFIG_DIR.get()
}

pub fn start_cwd() -> Option<&'static std::path::PathBuf> {
    START_CWD.get()
}

pub fn browser_dir() -> Option<&'static std::path::PathBuf> {
    BROWSER_DIR.get().and_then(|opt| opt.as_ref())
}

pub fn app_port() -> Option<u16> {
    APP_PORT.get().copied().flatten()
}

pub fn app_public() -> bool {
    APP_PUBLIC.get().copied().unwrap_or(false)
}
