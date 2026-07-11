use log::{LevelFilter, Metadata, Record};

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
use alloc::string::String;
#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
use core::fmt::Write as _;
#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
use core::sync::atomic::{AtomicU8, Ordering};

#[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
use crate::store::store_path;
#[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
use anyhow::Context;
#[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{Mutex, OnceLock},
};

pub fn init(level: LevelFilter) -> anyhow::Result<()> {
    #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
    {
        init_trueos(level)?;
    }

    #[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
    {
        init_native(level)?;
    }

    Ok(())
}

pub fn new_game_probe(args: core::fmt::Arguments<'_>) {
    #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
    {
        let mut line = String::from("[rebels-new-game-probe:INFO] ");
        let _ = line.write_fmt(args);
        line.push('\n');
        trueos::logl::log(trueos::logl::level::INFO, line.as_str());
    }

    #[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
    log::info!("[rebels-new-game-probe:INFO] {args}");
}

pub fn multi_rt_probe(args: core::fmt::Arguments<'_>) {
    #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
    {
        let mut line = String::from("[rebels-multi-rt-probe:INFO] ");
        let _ = line.write_fmt(args);
        line.push('\n');
        trueos::logl::log(trueos::logl::level::INFO, line.as_str());
    }

    #[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
    log::info!("[rebels-multi-rt-probe:INFO] {args}");
}

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
fn init_trueos(level: LevelFilter) -> Result<(), log::SetLoggerError> {
    static LOGGER: TrueosLogger = TrueosLogger;
    TRUEOS_BASE_LEVEL.store(level_filter_rank(level), Ordering::Relaxed);
    log::set_logger(&LOGGER)?;
    log::set_max_level(LevelFilter::Trace);
    Ok(())
}

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
struct TrueosLogger;

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
static TRUEOS_BASE_LEVEL: AtomicU8 = AtomicU8::new(0);

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
const fn level_filter_rank(level: LevelFilter) -> u8 {
    match level {
        LevelFilter::Off => 0,
        LevelFilter::Error => 1,
        LevelFilter::Warn => 2,
        LevelFilter::Info => 3,
        LevelFilter::Debug => 4,
        LevelFilter::Trace => 5,
    }
}

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
const fn level_rank(level: log::Level) -> u8 {
    match level {
        log::Level::Error => 1,
        log::Level::Warn => 2,
        log::Level::Info => 3,
        log::Level::Debug => 4,
        log::Level::Trace => 5,
    }
}

#[cfg(any(target_os = "trueos", target_os = "zkvm"))]
impl log::Log for TrueosLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        level_rank(metadata.level()) <= TRUEOS_BASE_LEVEL.load(Ordering::Relaxed)
            || (metadata.target().starts_with("rebels::network")
                && matches!(metadata.level(), log::Level::Debug | log::Level::Trace))
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            let level = match record.level() {
                log::Level::Error => trueos::logl::level::ERROR,
                log::Level::Warn => trueos::logl::level::WARN,
                log::Level::Info => trueos::logl::level::INFO,
                log::Level::Debug => trueos::logl::level::DEBUG,
                log::Level::Trace => trueos::logl::level::TRACE,
            };
            let _ = trueos::logl::log_record(level, record.target(), *record.args());
        }
    }

    fn flush(&self) {}
}

#[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
fn init_native(level: LevelFilter) -> anyhow::Result<()> {
    static LOGGER: OnceLock<NativeLogger> = OnceLock::new();

    let path = store_path("rebels.log")?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;

    let logger = LOGGER.get_or_init(|| NativeLogger {
        file: Mutex::new(file),
    });
    log::set_logger(logger)?;
    log::set_max_level(level);
    Ok(())
}

#[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
struct NativeLogger {
    file: Mutex<File>,
}

#[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
impl log::Log for NativeLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        if let Ok(mut file) = self.file.lock() {
            let _ = writeln!(file, "{} - {}", record.level(), record.args());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}
