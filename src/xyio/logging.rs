//! In the future it should contain optional TRACE level logging.

use std::thread;
use chrono;
use log;
use log::{LogRecord, LogLevel, LogMetadata, SetLoggerError};

struct Logger {
    level: LogLevel,
}

impl Logger {
    fn new(level: LogLevel) -> Logger {
        Logger {
            level: level,
        }
    }
}

fn severity(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "T",
        LogLevel::Debug => "D",
        LogLevel::Info  => "I",
        LogLevel::Warn  => "W",
        LogLevel::Error => "E",
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &LogMetadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &LogRecord) {
        if self.enabled(record.metadata()) {
            let now = chrono::Local::now();

            println!("{}, {} -- {:8.8} : {}",
                severity(record.level()),
                now,
                thread::current().name().unwrap_or("<unnamed>"),
                record.args()
            );
        }
    }
}

pub fn from_usize(v: usize) -> LogLevel {
    match v {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        2 => LogLevel::Info,
        3 => LogLevel::Debug,
        4 | _ => LogLevel::Trace,
    }
}

pub fn init(level: LogLevel) -> Result<(), SetLoggerError> {
    log::set_logger(|max| {
        max.set(level.to_log_level_filter());
        Box::new(Logger::new(level))
    })
}
