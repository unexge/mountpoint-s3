use std::backtrace::Backtrace;
use std::fs::{DirBuilder, OpenOptions};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::prelude::OpenOptionsExt;
use std::panic::{self, PanicInfo};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::{self, JoinHandle};

use crate::metrics::metrics_tracing_span_layer;
use anyhow::Context;
use mountpoint_s3_crt::common::rust_log_adapter::{RustLogAdapter, AWSCRT_LOG_TARGET};
use signal_hook::consts::signal::SIGUSR2;
use signal_hook::iterator::{Handle as SignalsHandle, Signals};
use time::format_description::FormatItem;
use time::macros;
use time::OffsetDateTime;
use tracing::{warn, Subscriber};
use tracing_subscriber::filter::{EnvFilter, Filtered, LevelFilter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, Layer, Registry};

mod syslog;
use self::syslog::SyslogLayer;

/// Configuration for Mountpoint logging
#[derive(Debug)]
pub struct LoggingConfig {
    /// A directory to create log files in. If unspecified, logs will be routed to syslog.
    pub log_directory: Option<PathBuf>,
    /// Whether to duplicate logs to stdout in addition to syslog or the log directory.
    pub log_to_stdout: bool,
    /// The default filter directive (in the sense of [tracing_subscriber::filter::EnvFilter]) to
    /// use for logs. Will be overridden by the `MOUNTPOINT_LOG` environment variable if set.
    pub default_filter: String,
}

#[derive(Default)]
pub struct LoggingHandle {
    _reloadable_env_filter_handle: Option<ReloadableEnvFilterHandle>,
}

/// Set up all our logging infrastructure.
///
/// This method:
/// - initializes the `tracing` subscriber for capturing log output
/// - sets up the logging adapters for the CRT and for metrics
/// - installs a panic hook to capture panics and log them with `tracing`
pub fn init_logging(config: LoggingConfig) -> anyhow::Result<LoggingHandle> {
    let handle = init_tracing_subscriber(config)?;
    install_panic_hook();
    Ok(handle)
}

fn tracing_panic_hook(panic_info: &PanicInfo) {
    let location = panic_info
        .location()
        .map(|l| format!("{}", l))
        .unwrap_or_else(|| String::from("<unknown>"));

    let payload = panic_info.payload();
    let payload = if let Some(s) = payload.downcast_ref::<&'static str>() {
        *s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<unknown payload>"
    };

    let thd = thread::current();

    let backtrace = Backtrace::force_capture();

    tracing::error!("panic on {thd:?} at {location}: {payload}");
    tracing::error!("backtrace:\n{backtrace}");
}

fn install_panic_hook() {
    let old_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        tracing_panic_hook(panic_info);
        old_hook(panic_info);
    }))
}

fn init_tracing_subscriber(config: LoggingConfig) -> anyhow::Result<LoggingHandle> {
    /// Create the logging config from the MOUNTPOINT_LOG environment variable or the default config
    /// if that variable is unset. We do this in a function because [EnvFilter] isn't [Clone] and we
    /// need a copy of the filter for each [Layer].
    fn create_env_filter(filter: &str) -> EnvFilter {
        EnvFilter::try_from_env("MOUNTPOINT_LOG").unwrap_or_else(|_| EnvFilter::new(filter))
    }

    let env_filter = create_env_filter(&config.default_filter);
    // Don't create the files or subscribers if we'll never emit any logs
    if env_filter.max_level_hint() == Some(LevelFilter::OFF) {
        return Ok(LoggingHandle::default());
    }

    RustLogAdapter::try_init().context("failed to initialize CRT logger")?;

    let file_layer = if let Some(path) = &config.log_directory {
        const LOG_FILE_NAME_FORMAT: &[FormatItem<'static>] =
            macros::format_description!("mountpoint-s3-[year]-[month]-[day]T[hour]-[minute]-[second]Z.log");
        let filename = OffsetDateTime::now_utc()
            .format(LOG_FILE_NAME_FORMAT)
            .context("couldn't format log file name")?;

        // log directories and files created by Mountpoint should not be accessible by other users
        let mut dir_builder = DirBuilder::new();
        dir_builder.recursive(true).mode(0o750);
        let mut file_options = OpenOptions::new();
        file_options.mode(0o640).append(true).create(true);

        dir_builder.create(path).context("failed to create log folder")?;
        let file = file_options
            .open(path.join(filename))
            .context("failed to create log file")?;

        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(file)
            .with_filter(env_filter);
        Some(file_layer)
    } else {
        None
    };

    let mut reloadable_env_filter_handle = None;

    let syslog_layer: Option<Filtered<_, _, Registry>> = if config.log_directory.is_none() {
        // TODO decide how to configure the filter for syslog
        let env_filter = create_env_filter(&config.default_filter);
        let (env_filter, handle) = setup_reloadable_env_filter(env_filter, config.default_filter.to_string())?;
        reloadable_env_filter_handle = Some(handle);

        // Don't fail if syslog isn't available on the system, since it's a default
        let syslog_layer = SyslogLayer::new().ok();
        syslog_layer.map(|l| l.with_filter(env_filter))
    } else {
        None
    };

    let console_layer = if config.log_to_stdout {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_ansi(supports_color::on(supports_color::Stream::Stdout).is_some())
            .with_filter(create_env_filter(&config.default_filter));
        Some(fmt_layer)
    } else {
        None
    };

    let registry = tracing_subscriber::registry()
        .with(syslog_layer)
        .with(console_layer)
        .with(file_layer)
        .with(metrics_tracing_span_layer());

    registry.init();

    Ok(LoggingHandle {
        _reloadable_env_filter_handle: reloadable_env_filter_handle,
    })
}

struct ReloadableEnvFilterHandle {
    signals_handle: SignalsHandle,
    thread_handle: Option<JoinHandle<()>>,
}

impl Drop for ReloadableEnvFilterHandle {
    fn drop(&mut self) {
        if !self.signals_handle.is_closed() {
            self.signals_handle.close();
        }
        if let Some(handle) = self.thread_handle.take() {
            _ = handle.join();
        }
    }
}

fn setup_reloadable_env_filter<S: Subscriber>(
    inner: EnvFilter,
    default_filter: String,
) -> anyhow::Result<(reload::Layer<EnvFilter, S>, ReloadableEnvFilterHandle)> {
    let (filter, reload_handle) = reload::Layer::new(inner);

    // Log levels.
    const DEFAULT: u8 = 0;
    const DEBUG: u8 = 1;
    const DEBUG_CRT: u8 = 2;
    const TRACE: u8 = 3;
    const TRACE_CRT: u8 = 4;
    let current_level = AtomicU8::new(DEFAULT);

    fn create_filter(level: LevelFilter, crt_level: LevelFilter) -> EnvFilter {
        EnvFilter::new(format!("{level},{AWSCRT_LOG_TARGET}={crt_level}"))
    }

    let mut signals = Signals::new(&[SIGUSR2])?;
    let signals_handle = signals.handle();

    let thread_handle = thread::spawn(move || {
        for signal in &mut signals.forever() {
            match signal {
                SIGUSR2 => {
                    let current = current_level.fetch_add(1, Ordering::SeqCst) + 1;
                    match current % 5 {
                        DEFAULT => {
                            warn!("Changing log verbosity to default level: {}", &default_filter);
                            _ = reload_handle.modify(|layer| *layer = EnvFilter::new(&default_filter));
                        }
                        DEBUG => {
                            warn!("Changing log verbosity to debug level");
                            _ = reload_handle
                                .modify(|layer| *layer = create_filter(LevelFilter::DEBUG, LevelFilter::OFF));
                        }
                        DEBUG_CRT => {
                            warn!("Changing log verbosity to debug level including CRT");
                            _ = reload_handle
                                .modify(|layer| *layer = create_filter(LevelFilter::DEBUG, LevelFilter::DEBUG));
                        }
                        TRACE => {
                            warn!("Changing log verbosity to trace level");
                            _ = reload_handle
                                .modify(|layer| *layer = create_filter(LevelFilter::TRACE, LevelFilter::OFF));
                        }
                        TRACE_CRT => {
                            warn!("Changing log verbosity to trace level including CRT");
                            _ = reload_handle.modify(|layer| {
                                *layer = create_filter(LevelFilter::TRACE, LevelFilter::TRACE);
                            });
                        }
                        level => {
                            warn!("Ignoring incorrect level: {}", level);
                        }
                    };
                }
                signal => warn!("Ignoring unexpected signal: {}", signal),
            }
        }
    });

    Ok((
        filter,
        ReloadableEnvFilterHandle {
            signals_handle,
            thread_handle: Some(thread_handle),
        },
    ))
}
