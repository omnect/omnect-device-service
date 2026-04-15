use anyhow::{Context, Result};

const DEFAULT_FILTER_DEBUG: &str = "warn,\
    azure_iot_sdk=debug,\
    eis_utils=debug,\
    omnect_device_service=debug";
const DEFAULT_FILTER_RELEASE: &str = "warn,\
    azure_iot_sdk=info,\
    eis_utils=info,\
    omnect_device_service=info";

fn default_filter() -> &'static str {
    if cfg!(debug_assertions) {
        DEFAULT_FILTER_DEBUG
    } else {
        DEFAULT_FILTER_RELEASE
    }
}

#[cfg(feature = "mock")]
pub fn init() -> Result<()> {
    use env_logger::{Builder, Env, Target};
    use std::io::Write;

    Builder::from_env(Env::default().default_filter_or(default_filter()))
        .format(|buf, record| match record.level() {
            log::Level::Info => writeln!(buf, "<6>{}", record.args()),
            log::Level::Warn => writeln!(buf, "<4>{}", record.args()),
            log::Level::Error => {
                eprintln!("<3>{}", record.args());
                Ok(())
            }
            _ => writeln!(buf, "<7>{}", record.args()),
        })
        .target(Target::Stdout)
        .try_init()
        .context("install env_logger")
}

#[cfg(not(feature = "mock"))]
struct FilteredLog<L: log::Log> {
    filter: env_filter::Filter,
    inner: L,
}

#[cfg(not(feature = "mock"))]
impl<L: log::Log> log::Log for FilteredLog<L> {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        self.filter.enabled(m)
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.filter.matches(record) {
            self.inner.log(record);
        }
    }
    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(not(feature = "mock"))]
pub fn init() -> Result<()> {
    use systemd_journal_logger::JournalLog;

    let filter_spec = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default_filter().to_string());
    let filter = env_filter::Builder::new().parse(&filter_spec).build();
    let max_level = filter.filter();

    let journal = JournalLog::new().context("create JournalLog")?;
    log::set_boxed_logger(Box::new(FilteredLog {
        filter,
        inner: journal,
    }))
    .context("install journal logger")?;
    log::set_max_level(max_level);
    Ok(())
}
