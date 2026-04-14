pub mod tunnel;
pub mod config;

pub use config::WgConfig;
pub use tunnel::start_tunnel;
#[allow(unused_imports)]
pub use tunnel::TunnelHandle;
