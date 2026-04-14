//! b3 login — Re-authenticate with a new setup token.
//!
//! For now, this is equivalent to running `b3 setup` again (which
//! will prompt to overwrite existing config). A future version will
//! support key rotation without full re-registration.

use crate::config::Config;

pub async fn run() -> anyhow::Result<()> {
    if !Config::exists() {
        println!("Not set up yet. Run `b3 setup` first.");
        return Ok(());
    }

    println!("To re-authenticate, run `b3 setup` (it will prompt to overwrite).");
    println!("To remove and start fresh, run `b3 uninstall` first.");
    Ok(())
}
