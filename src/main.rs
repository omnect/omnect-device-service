use env_logger::{Builder, Env};
use log::{debug, error};
use std::process;

fn main() {
    debug!("module version: {}", env!("CARGO_PKG_VERSION"));
    if cfg!(debug_assertions) {
        Builder::from_env(Env::default().default_filter_or("debug")).init();
    } else {
        Builder::from_env(Env::default().default_filter_or("info")).init();
    }
    if let Err(e) = demo_portal_module::run() {
        error!("Application error: {}", e);

        process::exit(1);
    }
}
