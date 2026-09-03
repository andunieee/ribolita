use crate::external_println;

pub struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn flush(&self) {}

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let level_string = { format!("{:<5}", record.level().to_string()) };
            let target = if !record.target().is_empty() {
                record.target()
            } else {
                record.module_path().unwrap_or_default()
            };
            let message = format!("{} [{}] {}", level_string, target, record.args());

            external_println(message);
        }
    }
}
