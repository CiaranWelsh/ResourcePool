use std::io::Write;
use std::thread;
use log::LevelFilter;

pub fn init_logger(level: LevelFilter) {
    env_logger::builder()
        .format(|buf, record| {
            let thread_name = match thread::current().name() {
                Some(name) => name.to_string(),
                None => "unknown".to_string(),
            };
            writeln!(buf, "[{}:{}] [thread: {}] [{}]: {}",
                     record.file().unwrap_or("unknown"), // File path
                     record.line().unwrap_or(0),          // Line number
                     thread_name,
                     record.level(),                      // Debug level
                     record.args()                        // Message
            )
        })
        .is_test(true)
        .filter_level(level)
        .filter(Some("ureq"), LevelFilter::Off)  // Turn off logging for the `ureq` crate
        .filter(Some("reqwest"), LevelFilter::Off)  // Turn off logging for the `ureq` crate
        .try_init().ok();
}


