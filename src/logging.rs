#![allow(unused)]
use clap::Args;
use std::{
    backtrace::{Backtrace, BacktraceStatus},
    ffi::OsStr,
    io::IsTerminal,
    panic::PanicHookInfo,
    path::{Path, PathBuf},
};

use lazy_static::lazy_static;
use std::sync::Mutex;
use tracing::info;
use tracing_appender::{
    non_blocking::{NonBlocking, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_log::LogTracer;
use tracing_subscriber::{
    filter::Filtered,
    fmt::{
        self,
        format::{Format, Json, JsonFields},
    },
    layer::SubscriberExt,
    reload, EnvFilter, Layer, Registry,
};

#[derive(Args, Debug, Clone)]
pub struct LoggingArgs {
    #[arg(help = "Logging level for debugging (info/debug)", long = "log-level")]
    pub log_level: Option<String>,

    #[arg(help = "Directory for rotated log files", long = "log-dir")]
    pub log_dir: Option<PathBuf>,

    #[arg(
        help = "Logging level for file debugging (info/debug)",
        long = "log-dir-level"
    )]
    pub log_dir_level: Option<String>,
}

pub fn start(args: &LoggingArgs) -> anyhow::Result<()> {
    init_logging(
        from_str(&args.log_level, EnvFilter::new("info"))?,
        from_str(&args.log_level, EnvFilter::new("info"))?,
    )?;

    if let Some(log_dir) = &args.log_dir {
        update_logging_dir(
            &log_dir,
            from_str(&args.log_dir_level, EnvFilter::new("debug"))?,
        );
    }

    Ok(())
}

pub fn flush_logging() {
    let flush_handle = &mut *FLUSH_HANDLE.lock().unwrap();
    *flush_handle = None;
}

pub fn update_logging_dir(target: &PathBuf, env_filter: EnvFilter) {
    if let Some(reload_state) = &mut *RELOAD_STATE.lock().unwrap() {
        inner_update_logging(target, env_filter, reload_state);
    }
}

pub fn update_logging_dir_filter(env_filter: EnvFilter) {
    info!("setting logging dir filter to {}", env_filter);

    if let Some(reload_state) = &mut *RELOAD_STATE.lock().unwrap() {
        if let Some(target) = &reload_state.0 {
            inner_update_logging(&target.to_owned(), env_filter, reload_state);
        }
    }
}

fn from_str(s: &Option<String>, def: EnvFilter) -> anyhow::Result<EnvFilter> {
    if let Some(l) = s {
        Ok(EnvFilter::new(l.as_str()))
    } else {
        return Ok(def);
    }
}

type OptionalJSONLogger = Filtered<
    Option<fmt::Layer<Registry, JsonFields, Format<Json>, NonBlocking>>,
    EnvFilter,
    Registry,
>;
type FileLoggerHandle = reload::Handle<OptionalJSONLogger, Registry>;

lazy_static! {
    static ref RELOAD_STATE: Mutex<Option<(Option<PathBuf>, FileLoggerHandle)>> = Mutex::new(None);
    static ref FLUSH_HANDLE: Mutex<Option<WorkerGuard>> = Mutex::new(None);
}

fn prog() -> Option<String> {
    std::env::args()
        .next()
        .as_ref()
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(OsStr::to_str)
        .map(String::from)
}

fn inner_update_logging(
    target: &PathBuf,
    env_filter: EnvFilter,
    reload_state: &mut (Option<PathBuf>, FileLoggerHandle),
) {
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY) // rotate daily
        .filename_prefix(prog().unwrap_or("app".to_owned())) // log file names will be prefixed with `myapp.`
        .filename_suffix("log") // log file names will be suffixed with `.log`
        .build(target) // try to build an appender that stores log files in `/var/log`
        .expect("initializing rolling file appender failed");

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let json_layer = fmt::layer()
        .json()
        .with_file(true)
        .with_line_number(true)
        .with_span_list(true)
        .with_level(true)
        .with_target(true)
        .with_writer(non_blocking);

    reload_state.0 = Some(target.clone());
    reload_state
        .1
        .modify(|filter| {
            *filter.filter_mut() = env_filter;
            *filter.inner_mut() = Some(json_layer)
        })
        .expect("Failed to update logging level");

    let flush_handle = &mut *FLUSH_HANDLE.lock().unwrap();
    *flush_handle = Some(guard);
}

struct CustomTimeFormatter;

impl tracing_subscriber::fmt::time::FormatTime for CustomTimeFormatter {
    fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
        let now = chrono::Local::now();
        write!(w, "{}", now.format("[%Y-%m-%d %H:%M:%S%.6f]"))
    }
}

fn init_logging(env_filter: EnvFilter, env_filter_2: EnvFilter) -> anyhow::Result<()> {
    let stdout_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_timer(CustomTimeFormatter) // Use the custom time formatter
        .with_target(false) // Hide the target, which is the module path
        .with_thread_ids(false) // Hide thread ids
        .with_thread_names(false) // Hide thread names
        .with_filter(env_filter);

    let stdout_layer_no_terminal = fmt::layer()
        .with_writer(std::io::stdout)
        .with_timer(CustomTimeFormatter) // Use the custom time formatter
        .with_target(false) // Hide the target, which is the module path
        .with_thread_ids(false) // Hide thread ids
        .with_thread_names(false) // Hide thread names
        .without_time()
        .with_ansi(false)
        .with_filter(env_filter_2);

    let (reloading_layer, reload_handle) =
        tracing_subscriber::reload::Layer::new(None.with_filter(EnvFilter::new("off")));
    {
        let mut handle = RELOAD_STATE.lock().unwrap();
        *handle = Some((None, reload_handle));
    }

    if std::io::stdout().is_terminal() {
        tracing::subscriber::set_global_default(
            Registry::default().with(reloading_layer).with(stdout_layer),
        )
    } else {
        tracing::subscriber::set_global_default(
            Registry::default()
                .with(reloading_layer)
                .with(stdout_layer_no_terminal),
        )
    }
    .expect("Could not set global default subscriber");

    std::panic::set_hook(Box::new(panic_hook));

    LogTracer::init()?;

    Ok(())
}

fn own_panic_hook(panic_info: &PanicHookInfo) {
    let payload = panic_info.payload();

    #[allow(clippy::manual_map)]
    let payload = if let Some(s) = payload.downcast_ref::<&str>() {
        Some(&**s)
    } else if let Some(s) = payload.downcast_ref::<String>() {
        Some(s.as_str())
    } else {
        None
    };

    let location = panic_info.location().map(|l| l.to_string());
    let (backtrace, note) = {
        let backtrace = Backtrace::force_capture();
        let note = (backtrace.status() == BacktraceStatus::Disabled)
            .then_some("run with RUST_BACKTRACE=1 environment variable to display a backtrace");
        (Some(backtrace), note)
    };

    tracing::error!(
        panic.payload = payload,
        panic.location = location,
        panic.backtrace = backtrace.map(tracing::field::display),
        panic.note = note,
        "A panic occurred",
    );
}

fn panic_hook(panic_info: &PanicHookInfo) {
    own_panic_hook(panic_info);
    let flush_handle = &mut *FLUSH_HANDLE.lock().unwrap();
    *flush_handle = None;
}
