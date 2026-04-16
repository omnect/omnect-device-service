use log::{error, info};
use omnect_device_service::{logging, twin::Twin};
use std::process;

#[tokio::main]
async fn main() -> process::ExitCode {
    log_panics::init();
    if let Err(e) = logging::init() {
        // The logger is not installed; use stderr directly with a syslog
        // priority prefix so journald still classifies it correctly.
        eprintln!("<3>failed to initialize logger: {e:#}");
        process::exit(1)
    }

    if let Err(e) = Twin::run().await {
        error!("application error: {e:#}");

        process::exit(1)
    }

    info!("application shutdown");

    process::exit(0)
}
