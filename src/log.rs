use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

static LEVEL: OnceLock<Level> = OnceLock::new();

pub fn init() {
    let _ = LEVEL.set(level_from_env());
}

fn current() -> Level {
    *LEVEL.get().unwrap_or(&Level::Info)
}

fn level_from_env() -> Level {
    match std::env::var("RUST_LOG") {
        Ok(value) => {
            let value = value.to_ascii_lowercase();
            if value.contains("trace") || value.contains("debug") {
                Level::Debug
            } else if value.contains("warn") {
                Level::Warn
            } else if value.contains("error") && !value.contains("info") {
                Level::Error
            } else {
                Level::Info
            }
        }
        Err(_) => Level::Info,
    }
}

pub fn log_line(level: Level, kind: &str, args: std::fmt::Arguments<'_>) {
    if current() >= level {
        eprintln!("{kind} {args}");
    }
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => {
        $crate::log::log_line($crate::log::Level::Info, "INFO", format_args!($($t)*))
    };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => {
        $crate::log::log_line($crate::log::Level::Warn, "WARN", format_args!($($t)*))
    };
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => {
        $crate::log::log_line($crate::log::Level::Error, "ERROR", format_args!($($t)*))
    };
}

#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => {
        $crate::log::log_line($crate::log::Level::Debug, "DEBUG", format_args!($($t)*))
    };
}
