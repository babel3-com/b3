//! b3 version — Print version info.

pub async fn run() -> anyhow::Result<()> {
    println!("b3 {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
