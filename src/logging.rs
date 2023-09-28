#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum LogLevel {
    Log = 0,
    Info = 1,
    Warning = 2,
    Error = 3,
}

static LOG_LEVEL: std::sync::OnceLock<LogLevel> = std::sync::OnceLock::new();

pub fn set_log_level(level: LogLevel) {
    LOG_LEVEL.set(level).unwrap();
}

pub fn log_with_level(s: &str, level: LogLevel) -> () {
    let level_str = match level {
        LogLevel::Log => "log",
        LogLevel::Info => "info",
        LogLevel::Warning => "warning",
        LogLevel::Error => "error",
    };

    if LOG_LEVEL.get().unwrap_or(&LogLevel::Log) <= &level {
        eprintln!("[{}] {}", level_str, s)
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        $crate::logging::log_with_level(format!($($arg)*).as_str(), $crate::logging::LogLevel::Log)
    }};
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        $crate::logging::log_with_level(format!($($arg)*).as_str(), $crate::logging::LogLevel::Info)
    }};
}

#[macro_export]
macro_rules! warning {
    ($($arg:tt)*) => {{
        $crate::logging::log_with_level(format!($($arg)*).as_str(), $crate::logging::LogLevel::Warning)
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        $crate::logging::log_with_level(format!($($arg)*).as_str(), $crate::logging::LogLevel::Error)
    }};
}
