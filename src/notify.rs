//! Local-only notification adapters. Alert bodies are never persisted here.

use std::io::Write;

use thiserror::Error;

use crate::alerts::AlertEvent;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("notification delivery is unavailable")]
pub struct NotifyError;

pub trait Notifier {
    fn notify(&mut self, event: &AlertEvent) -> Result<(), NotifyError>;
}

pub struct TerminalNotifier<W> {
    writer: W,
    bell: bool,
}

impl<W: Write> TerminalNotifier<W> {
    pub fn new(writer: W, bell: bool) -> Self {
        Self { writer, bell }
    }

    pub fn capability_message(&mut self) -> Result<(), NotifyError> {
        writeln!(
            self.writer,
            "Native notification unavailable; the terminal alert was retained."
        )
        .map_err(|_| NotifyError)
    }
}

impl<W: Write> Notifier for TerminalNotifier<W> {
    fn notify(&mut self, event: &AlertEvent) -> Result<(), NotifyError> {
        if self.bell {
            write!(self.writer, "\u{7}").map_err(|_| NotifyError)?;
        }
        writeln!(self.writer, "ALERT: {}", event.message()).map_err(|_| NotifyError)
    }
}

#[derive(Debug, Default)]
pub struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    fn notify(&mut self, event: &AlertEvent) -> Result<(), NotifyError> {
        notify_rust::Notification::new()
            .summary("Agent Usage Dashboard")
            .body(&event.message())
            .show()
            .map(|_| ())
            .map_err(|_| NotifyError)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReport {
    pub terminal_failures: usize,
    pub native_failures: usize,
}

pub fn deliver(
    events: &[AlertEvent],
    terminal: &mut impl Notifier,
    mut native: Option<&mut dyn Notifier>,
) -> DeliveryReport {
    let mut report = DeliveryReport::default();
    for event in events {
        if terminal.notify(event).is_err() {
            report.terminal_failures += 1;
        }
        if let Some(notifier) = native.as_deref_mut()
            && notifier.notify(event).is_err()
        {
            report.native_failures += 1;
        }
    }
    report
}
