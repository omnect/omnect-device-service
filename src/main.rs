use log::{error, info};
use omnect_device_service::{logging, twin::Twin};
use std::process;

#[tokio::main]
async fn main() -> process::ExitCode {
    log_panics::init();
    logging::init();

    if let Err(e) = Twin::run().await {
        error!("application error: {e:#}");

        process::exit(1)
    }

    info!("application shutdown");

    process::exit(0)
}
