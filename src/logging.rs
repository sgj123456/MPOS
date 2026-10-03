//! Kernel logging: `tracing` events, formatted onto the serial console.
//!
//! The kernel's own voice goes out COM1, which is `ttyS0`. `println!` is the
//! raw primitive underneath - a line of bytes with no level, target or fields -
//! and this is the layer on top: `tracing` decides what is worth saying and
//! this subscriber writes it, so a boot can be turned up or down (`loglevel=`,
//! `quiet`, `debug`, `trace` on the command line) without editing the calls
//! that produce the output.
//!
//! # Why the subscriber is ours
//!
//! `tracing-subscriber` needs `std` and, for its formatting layers, an
//! executor. The kernel has neither. What a kernel needs from a subscriber is
//! one thing - turn an event into a line - and the event machinery itself is
//! `no_std` with `default-features = false`, so the line-shaped half lives
//! here.
//!
//! # What may be logged
//!
//! Nothing in the panic path, and nothing from an interrupt handler that is
//! itself printing: a log line takes the same console lock as `println!`, and
//! a handler that tried to log while the console was held would be waiting on
//! the code it interrupted. The serial receiver's drain is the one such place,
//! and it is why it does not log.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tracing::Level;
use tracing::field::{Field, Visit};
#[cfg(target_os = "none")]
use tracing::{Event, Metadata};

/// A rank that compares like verbosity: bigger is more verbose. `Level` itself
/// is a struct of static names and cannot be matched as a constant pattern, so
/// the ordering is spelled out once, here.
pub fn rank(level: &Level) -> u8 {
    if *level == Level::ERROR {
        1
    } else if *level == Level::WARN {
        2
    } else if *level == Level::INFO {
        3
    } else if *level == Level::DEBUG {
        4
    } else {
        5
    }
}

/// `loglevel=<n|name>`, or the bare `quiet`/`debug`/`trace` words, as the
/// kernel command line spells them. Everything else is `info`, which is the
/// level that leaves a normal boot quiet enough to read.
pub fn level_from_cmdline(cmdline: &str) -> u8 {
    for word in cmdline.split_whitespace() {
        match word {
            "quiet" => return 2,
            "debug" => return 4,
            "trace" => return 5,
            _ => {}
        }
        if let Some(level) = word.strip_prefix("loglevel=") {
            return match level {
                "0" | "1" | "2" | "4" | "warn" => 2,
                "3" | "info" => 3,
                "7" | "debug" => 4,
                "5" | "6" | "8" | "trace" => 5,
                _ => 3,
            };
        }
    }
    3
}

/// Collects an event's fields, keeping the prose (`message`) apart from the
/// structured ones so the line can read `msg key=value key=value`.
#[derive(Default)]
pub struct FieldCollector {
    pub message: String,
    pub rest: Vec<(String, String)>,
}

impl FieldCollector {
    fn store(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = value;
        } else {
            self.rest.push((String::from(name), value));
        }
    }
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let rendered = alloc::format!("{value:?}");
        self.store(field.name(), rendered);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.store(field.name(), String::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.store(field.name(), alloc::format!("{value}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.store(field.name(), alloc::format!("{value}"));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.store(field.name(), alloc::format!("{value}"));
    }
}
/// The most verbose level that reaches the console.
#[cfg(target_os = "none")]
static MAX_LEVEL: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(3);

/// Install the console subscriber and set the level the command line asked
/// for. Called once, from the boot task, as soon as `/proc/cmdline` exists.
#[cfg(target_os = "none")]
pub fn init() {
    use core::sync::atomic::Ordering;

    MAX_LEVEL.store(
        level_from_cmdline(crate::boot::cmdline()),
        Ordering::Relaxed,
    );
    // A second call would be a programming error rather than a runtime
    // condition, and no path does it: the global default is set once, before
    // anything else can log.
    let _ = tracing::subscriber::set_global_default(ConsoleSubscriber);
}

#[cfg(target_os = "none")]
struct ConsoleSubscriber;

#[cfg(target_os = "none")]
impl tracing::subscriber::Subscriber for ConsoleSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        use core::sync::atomic::Ordering;
        rank(metadata.level()) <= MAX_LEVEL.load(Ordering::Relaxed)
    }

    // The kernel logs events, not spans: a span would have to outlive the boot
    // phase that opened it, and nothing here is instrumented with one yet.
    // These answer the cheapest thing that is still true.
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = FieldCollector::default();
        event.record(&mut fields);
        let line = render(event.metadata(), &fields);
        crate::console::write_bytes(line.as_bytes());
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

/// One line, in the shape `dmesg` prints: a boot-relative timestamp, the
/// level, the target that spoke, the message, then any structured fields.
#[cfg(target_os = "none")]
fn render(metadata: &Metadata<'_>, fields: &FieldCollector) -> String {
    let ticks = crate::hal::ticks();
    let hz = crate::hal::TICK_HZ as u64;
    let mut line = alloc::format!(
        "[{:>5}.{:06}] {:>5} {}: {}",
        ticks / hz,
        (ticks % hz) * 1_000_000 / hz,
        metadata.level().as_str(),
        metadata.target(),
        fields.message
    );
    for (name, value) in &fields.rest {
        line.push(' ');
        line.push_str(name);
        line.push('=');
        line.push_str(value);
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_compare_by_verbosity() {
        assert!(rank(&Level::ERROR) < rank(&Level::INFO));
        assert!(rank(&Level::INFO) < rank(&Level::TRACE));
    }

    #[test]
    fn the_command_line_turns_logging_up_and_down() {
        assert_eq!(level_from_cmdline("root=/dev/vda2 quiet"), 2);
        assert_eq!(level_from_cmdline("root=/dev/vda2 loglevel=7"), 4);
        assert_eq!(level_from_cmdline("loglevel=trace"), 5);
        assert_eq!(level_from_cmdline("root=/dev/vda2"), 3);
    }

    #[test]
    fn the_message_is_kept_apart_from_the_fields() {
        let mut collector = FieldCollector::default();
        collector.store("message", String::from("mounted"));
        collector.store("path", String::from("/mnt"));
        assert_eq!(collector.message, "mounted");
        assert_eq!(collector.rest.len(), 1);
        assert_eq!(collector.rest[0].0, "path");
        assert_eq!(collector.rest[0].1, "/mnt");
    }
}
