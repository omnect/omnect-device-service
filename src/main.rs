pub mod bootloader_env;
pub mod reboot_reason;
pub mod systemd;
pub mod twin;
pub mod web_service;

use env_logger::{Builder, Env, Target};
use log::{error, info};
use std::{io::Write, process};
use twin::Twin;

#[tokio::main]
async fn main() {
    log_panics::init();

    let mut builder = if cfg!(debug_assertions) {
        Builder::from_env(Env::default().default_filter_or(
            "warn, \
            azure_iot_sdk=debug, \
            eis_utils=debug, \
            omnect_device_service=debug",
        ))
    } else {
        Builder::from_env(Env::default().default_filter_or(
            "warn, \
            azure_iot_sdk=info, \
            eis_utils=info, \
            omnect_device_service=info",
        ))
    };

    builder.format(|buf, record| match record.level() {
        log::Level::Info => writeln!(buf, "<6>{}: {}", record.target(), record.args()),
        log::Level::Warn => writeln!(buf, "<4>{}: {}", record.target(), record.args()),
        log::Level::Error => {
            eprintln!("<3>{}: {}", record.target(), record.args());
            Ok(())
        }
        _ => writeln!(buf, "<7>{}: {}", record.target(), record.args()),
    });

    builder.target(Target::Stdout).init();

    if let Err(e) = Twin::run().await {
        error!("application error: {e:#}");

        process::exit(1);
    }

    info!("application shutdown");

    // FIXME:
    //   under some circumstances the app can hang at termination what can be
    //   worked aroung by explicitly issuing exit here.
    //   to ease further examination of that problem allow setting an
    //   environment variable to skip the workaround.
    if let Err(dont_explicitly_exit) = env::var("DONT_EXPLICITLY_EXIT_ON_TERMINATION") {
        // ensure that we terminate here right now
        process::exit(0);
    }
}
