use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use thiserror::Error;

use crate::domain::{Quality, QuotaSnapshotInput};

pub const MAX_PROTOCOL_LINE_BYTES: usize = 256 * 1024;
const MAX_MESSAGES_PER_REQUEST: usize = 64;
const INITIALIZE_ID: u64 = 1;
const RATE_LIMITS_ID: u64 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct CodexObservation {
    pub snapshots: Vec<QuotaSnapshotInput>,
    pub rejected_windows: usize,
}

impl CodexObservation {
    pub fn quality(&self) -> Quality {
        if self.rejected_windows == 0
            && !self.snapshots.is_empty()
            && self
                .snapshots
                .iter()
                .all(|row| row.quality == Quality::Fresh)
        {
            Quality::Fresh
        } else {
            Quality::Partial
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransportError {
    #[error("transport I/O failed")]
    Io,
    #[error("transport closed")]
    Closed,
    #[error("transport timed out")]
    Timeout,
    #[error("protocol line exceeds the size limit")]
    Oversized,
}

pub trait LineTransport {
    fn send_line(&mut self, line: &[u8]) -> Result<(), TransportError>;
    fn receive_line(&mut self, timeout: Duration) -> Result<Vec<u8>, TransportError>;
    fn shutdown(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CodexError {
    #[error("unable to start the Codex app-server")]
    Spawn,
    #[error("Codex {method} request {request_id} failed at the transport boundary")]
    Transport {
        method: &'static str,
        request_id: u64,
    },
    #[error("Codex {method} request {request_id} timed out")]
    Timeout {
        method: &'static str,
        request_id: u64,
    },
    #[error("Codex {method} response {request_id} violated the protocol")]
    Protocol {
        method: &'static str,
        request_id: u64,
    },
    #[error("Codex {method} request {request_id} was rejected")]
    Rpc {
        method: &'static str,
        request_id: u64,
    },
}

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    executable: PathBuf,
    timeout: Duration,
    source_version: Option<String>,
}

impl CodexAdapter {
    pub fn new(executable: impl AsRef<Path>) -> Self {
        Self {
            executable: executable.as_ref().to_path_buf(),
            timeout: Duration::from_secs(10),
            source_version: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_source_version(mut self, version: Option<String>) -> Self {
        self.source_version = version;
        self
    }

    pub fn read_rate_limits(&self, observed_at: i64) -> Result<CodexObservation, CodexError> {
        let mut transport = ChildTransport::spawn(&self.executable)?;
        query_with_transport(
            &mut transport,
            observed_at,
            self.source_version.as_deref(),
            self.timeout,
        )
    }
}

pub fn query_with_transport<T: LineTransport>(
    transport: &mut T,
    observed_at: i64,
    source_version: Option<&str>,
    timeout: Duration,
) -> Result<CodexObservation, CodexError> {
    let result = query_inner(transport, observed_at, source_version, timeout);
    transport.shutdown();
    result
}

fn query_inner<T: LineTransport>(
    transport: &mut T,
    observed_at: i64,
    source_version: Option<&str>,
    timeout: Duration,
) -> Result<CodexObservation, CodexError> {
    send(
        transport,
        "initialize",
        INITIALIZE_ID,
        &json!({"method":"initialize","id":INITIALIZE_ID,"params":{"clientInfo":{
            "name":"agent_usage_dashboard","title":"Agent Usage Dashboard",
            "version":env!("CARGO_PKG_VERSION")
        }}}),
    )?;
    receive_result(transport, "initialize", INITIALIZE_ID, timeout)?;
    send_notification(transport, &json!({"method":"initialized","params":{}}))?;
    send(
        transport,
        "account/rateLimits/read",
        RATE_LIMITS_ID,
        &json!({"method":"account/rateLimits/read","id":RATE_LIMITS_ID}),
    )?;
    let result = receive_result(
        transport,
        "account/rateLimits/read",
        RATE_LIMITS_ID,
        timeout,
    )?;
    Ok(parse_rate_limits_result(
        &result,
        observed_at,
        source_version,
    ))
}

fn send<T: LineTransport>(
    transport: &mut T,
    method: &'static str,
    request_id: u64,
    message: &Value,
) -> Result<(), CodexError> {
    let bytes =
        serde_json::to_vec(message).map_err(|_| CodexError::Protocol { method, request_id })?;
    transport
        .send_line(&bytes)
        .map_err(|_| CodexError::Transport { method, request_id })
}

fn send_notification<T: LineTransport>(
    transport: &mut T,
    message: &Value,
) -> Result<(), CodexError> {
    let bytes = serde_json::to_vec(message).map_err(|_| CodexError::Protocol {
        method: "initialized",
        request_id: INITIALIZE_ID,
    })?;
    transport
        .send_line(&bytes)
        .map_err(|_| CodexError::Transport {
            method: "initialized",
            request_id: INITIALIZE_ID,
        })
}

fn receive_result<T: LineTransport>(
    transport: &mut T,
    method: &'static str,
    request_id: u64,
    timeout: Duration,
) -> Result<Value, CodexError> {
    let deadline = Instant::now() + timeout;
    for _ in 0..MAX_MESSAGES_PER_REQUEST {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CodexError::Timeout { method, request_id });
        }
        let line = transport
            .receive_line(remaining)
            .map_err(|error| match error {
                TransportError::Timeout => CodexError::Timeout { method, request_id },
                _ => CodexError::Transport { method, request_id },
            })?;
        if line.len() > MAX_PROTOCOL_LINE_BYTES {
            return Err(CodexError::Protocol { method, request_id });
        }
        let message: Value = serde_json::from_slice(&line)
            .map_err(|_| CodexError::Protocol { method, request_id })?;
        if !id_matches(message.get("id"), request_id) {
            continue;
        }
        if message.get("error").is_some() {
            return Err(CodexError::Rpc { method, request_id });
        }
        return message
            .get("result")
            .cloned()
            .ok_or(CodexError::Protocol { method, request_id });
    }
    Err(CodexError::Protocol { method, request_id })
}

fn id_matches(value: Option<&Value>, expected: u64) -> bool {
    value.is_some_and(|id| {
        id.as_u64() == Some(expected)
            || id.as_str().and_then(|text| text.parse::<u64>().ok()) == Some(expected)
    })
}

#[path = "codex_parse.rs"]
mod parse;
pub use parse::parse_rate_limits_result;

#[path = "codex_transport.rs"]
mod transport;
use transport::ChildTransport;

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
