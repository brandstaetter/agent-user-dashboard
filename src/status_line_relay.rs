use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("wrapped status-line command could not be started")]
    Spawn,
    #[error("wrapped status-line command I/O failed")]
    ChildIo,
    #[error("status-line relay I/O failed")]
    RelayIo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayOutcome {
    pub exit_code: i32,
}

/// The bounded copy of the status-line bytes presented to a best-effort
/// consumer after the wrapped process has completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapturedInput<'a> {
    Complete(&'a [u8]),
    Overflow,
}

enum CaptureBuffer {
    Complete(Vec<u8>),
    Overflow,
}

struct ChildOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: i32,
}

/// Runs a formatter without a shell and relays its authoritative result.
///
/// The original input is always drained and forwarded in full unless relay or
/// child I/O itself fails. Only the bounded capture is discarded on overflow.
/// The callback is deliberately best-effort: its declared failure cannot
/// replace the wrapped command's stdout, stderr, or exit code.
pub fn relay<R, O, E, F>(
    mut reader: R,
    mut stdout: O,
    mut stderr: E,
    program: &OsStr,
    args: &[OsString],
    capture_limit: usize,
    on_capture: F,
) -> Result<RelayOutcome, RelayError>
where
    R: Read,
    O: Write,
    E: Write,
    F: FnOnce(CapturedInput<'_>) -> Result<(), ()>,
{
    let (child, captured) = run_process(&mut reader, program, args, capture_limit)?;
    complete_relay(child, captured, &mut stdout, &mut stderr, on_capture)
}

fn complete_relay(
    child: ChildOutput,
    captured: CaptureBuffer,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    on_capture: impl FnOnce(CapturedInput<'_>) -> Result<(), ()>,
) -> Result<RelayOutcome, RelayError> {
    stdout
        .write_all(&child.stdout)
        .map_err(|_| RelayError::RelayIo)?;
    stderr
        .write_all(&child.stderr)
        .map_err(|_| RelayError::RelayIo)?;

    let callback_result = match &captured {
        CaptureBuffer::Complete(input) => on_capture(CapturedInput::Complete(input)),
        CaptureBuffer::Overflow => on_capture(CapturedInput::Overflow),
    };
    let _ = callback_result;

    Ok(RelayOutcome {
        exit_code: child.exit_code,
    })
}

fn run_process(
    reader: &mut impl Read,
    program: &OsStr,
    args: &[OsString],
    capture_limit: usize,
) -> Result<(ChildOutput, CaptureBuffer), RelayError> {
    let mut child = Command::new(resolve_program(program))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| RelayError::Spawn)?;
    let mut child_stdin = child.stdin.take().ok_or(RelayError::ChildIo)?;
    let mut child_stdout = child.stdout.take().ok_or(RelayError::ChildIo)?;
    let mut child_stderr = child.stderr.take().ok_or(RelayError::ChildIo)?;

    // Drain both child output pipes concurrently so either stream can exceed
    // the platform pipe buffer without deadlocking the wrapped command.
    let stdout_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_thread = thread::spawn(move || {
        let mut bytes = Vec::new();
        child_stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let captured = pipe_input(reader, &mut child_stdin, capture_limit)?;
    drop(child_stdin);
    let status = child.wait().map_err(|_| RelayError::ChildIo)?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| RelayError::ChildIo)?
        .map_err(|_| RelayError::ChildIo)?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| RelayError::ChildIo)?
        .map_err(|_| RelayError::ChildIo)?;

    Ok((
        ChildOutput {
            stdout,
            stderr,
            exit_code: status.code().unwrap_or(1),
        },
        captured,
    ))
}

fn pipe_input(
    reader: &mut impl Read,
    writer: &mut impl Write,
    capture_limit: usize,
) -> Result<CaptureBuffer, RelayError> {
    let mut captured = CaptureBuffer::Complete(Vec::new());
    let mut child_stdin_open = true;
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let count = reader.read(&mut buffer).map_err(|_| RelayError::RelayIo)?;
        if count == 0 {
            break;
        }

        if child_stdin_open && let Err(error) = writer.write_all(&buffer[..count]) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                // The formatter may exit without consuming stdin. Keep
                // draining the original input so the formatter's output and
                // status remain authoritative.
                child_stdin_open = false;
            } else {
                return Err(RelayError::ChildIo);
            }
        }

        if let CaptureBuffer::Complete(input) = &mut captured {
            if count <= capture_limit.saturating_sub(input.len()) {
                input.extend_from_slice(&buffer[..count]);
            } else {
                captured = CaptureBuffer::Overflow;
            }
        }
    }

    Ok(captured)
}

fn resolve_program(program: &OsStr) -> OsString {
    #[cfg(windows)]
    {
        let path = env::var_os("PATH").unwrap_or_default();
        let path_ext = env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        resolve_windows_program(program, &path, &path_ext).unwrap_or_else(|| program.to_owned())
    }
    #[cfg(not(windows))]
    {
        program.to_owned()
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn resolve_windows_program(program: &OsStr, path: &OsStr, path_ext: &OsStr) -> Option<OsString> {
    let requested = Path::new(program);
    if requested.components().count() > 1 || requested.is_absolute() {
        return requested.is_file().then(|| program.to_owned());
    }

    let extensions = path_ext.to_string_lossy();
    for directory in env::split_paths(path) {
        let base = directory.join(requested);
        if requested.extension().is_none() {
            for extension in extensions.split(';').filter(|value| !value.is_empty()) {
                let mut candidate = base.as_os_str().to_owned();
                candidate.push(extension);
                let candidate = PathBuf::from(candidate);
                if candidate.is_file() {
                    return Some(candidate.into_os_string());
                }
            }
        }
        if base.is_file() {
            return Some(base.into_os_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command, sync::OnceLock};

    use super::*;
    use tempfile::tempdir;

    const HELPER_SOURCE: &str = r#"
use std::{env, io::{Read, Write}, process};

fn main() {
    match env::args().nth(1).as_deref() {
        Some("echo") => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            std::io::stdout().write_all(&input).unwrap();
            std::io::stderr().write_all(&[0, b'e', b'r', b'r', 255]).unwrap();
            process::exit(23);
        }
        Some("large") => {
            std::io::stdout().write_all(&vec![b'o'; 256 * 1024]).unwrap();
            std::io::stderr().write_all(&vec![b'e'; 256 * 1024]).unwrap();
        }
        Some("close") => {
            std::io::stdout().write_all(b"closed").unwrap();
            std::io::stderr().write_all(b"early").unwrap();
            process::exit(37);
        }
        _ => process::exit(64),
    }
}
"#;

    fn relay_helper() -> &'static PathBuf {
        static HELPER: OnceLock<PathBuf> = OnceLock::new();
        HELPER.get_or_init(|| {
            let directory = tempdir().unwrap().keep();
            let source = directory.join("relay_helper.rs");
            let executable = directory.join(format!("relay-helper{}", env::consts::EXE_SUFFIX));
            fs::write(&source, HELPER_SOURCE).unwrap();
            let compiler = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
            let output = Command::new(compiler)
                .arg("--edition=2024")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "relay helper compilation failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            executable
        })
    }

    #[test]
    fn preserves_exact_streams_exit_and_complete_capture_despite_callback_failure() {
        let input = [0, b'{', b'}', b'\r', b'\n', 255];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut callback_input = None;

        let outcome = relay(
            input.as_slice(),
            &mut stdout,
            &mut stderr,
            relay_helper().as_os_str(),
            &[OsString::from("echo")],
            input.len(),
            |captured| {
                if let CapturedInput::Complete(bytes) = captured {
                    callback_input = Some(bytes.to_vec());
                }
                Err(())
            },
        )
        .unwrap();

        assert_eq!(outcome.exit_code, 23);
        assert_eq!(stdout, input);
        assert_eq!(stderr, [0, b'e', b'r', b'r', 255]);
        assert_eq!(callback_input.as_deref(), Some(input.as_slice()));
    }

    #[test]
    fn overflow_is_reported_without_truncating_forwarded_input() {
        let input = b"one byte beyond";
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut overflow = false;

        let outcome = relay(
            input.as_slice(),
            &mut stdout,
            &mut stderr,
            relay_helper().as_os_str(),
            &[OsString::from("echo")],
            input.len() - 1,
            |captured| {
                overflow = captured == CapturedInput::Overflow;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(outcome.exit_code, 23);
        assert_eq!(stdout, input);
        assert_eq!(stderr, [0, b'e', b'r', b'r', 255]);
        assert!(overflow);
    }

    #[test]
    fn capture_limit_boundary_is_inclusive() {
        let input = b"12345";
        let mut forwarded = Vec::new();
        let captured = pipe_input(&mut input.as_slice(), &mut forwarded, input.len()).unwrap();
        assert!(matches!(
            captured,
            CaptureBuffer::Complete(bytes) if bytes == input
        ));
        assert_eq!(forwarded, input);

        let mut forwarded = Vec::new();
        let captured = pipe_input(&mut input.as_slice(), &mut forwarded, input.len() - 1).unwrap();
        assert!(matches!(captured, CaptureBuffer::Overflow));
        assert_eq!(forwarded, input);
    }

    #[test]
    fn drains_large_stdout_and_stderr_without_deadlock() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let outcome = relay(
            io::empty(),
            &mut stdout,
            &mut stderr,
            relay_helper().as_os_str(),
            &[OsString::from("large")],
            0,
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(stdout, vec![b'o'; 256 * 1024]);
        assert_eq!(stderr, vec![b'e'; 256 * 1024]);
    }

    #[test]
    fn broken_child_stdin_drains_input_and_preserves_child_result() {
        struct CountingReader {
            remaining: usize,
        }

        impl Read for CountingReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.remaining.min(buffer.len());
                buffer[..count].fill(b'x');
                self.remaining -= count;
                Ok(count)
            }
        }

        let mut reader = CountingReader {
            remaining: 8 * 1024 * 1024,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut overflow = false;
        let outcome = relay(
            &mut reader,
            &mut stdout,
            &mut stderr,
            relay_helper().as_os_str(),
            &[OsString::from("close")],
            0,
            |captured| {
                overflow = captured == CapturedInput::Overflow;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(reader.remaining, 0);
        assert_eq!(outcome.exit_code, 37);
        assert_eq!(stdout, b"closed");
        assert_eq!(stderr, b"early");
        assert!(overflow);
    }

    #[test]
    fn explicit_broken_pipe_keeps_draining_and_capturing() {
        struct ClosedStdin;

        impl Write for ClosedStdin {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let input = b"complete original input";
        let captured = pipe_input(&mut input.as_slice(), &mut ClosedStdin, input.len()).unwrap();
        assert!(matches!(
            captured,
            CaptureBuffer::Complete(bytes) if bytes == input
        ));
    }

    #[test]
    fn spawn_failure_is_sanitized() {
        let error = relay(
            b"{}".as_slice(),
            Vec::new(),
            Vec::new(),
            OsStr::new("agent-usage-dashboard-command-that-does-not-exist"),
            &[],
            2,
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, RelayError::Spawn));
        assert!(!error.to_string().contains("agent-usage-dashboard-command"));
        assert!(!error.to_string().contains("{}"));
    }

    #[test]
    fn windows_resolver_finds_cmd_from_bare_name_by_pathext_order() {
        let temp = tempdir().unwrap();
        let command = temp.path().join("npx.cmd");
        fs::write(&command, b"fake").unwrap();
        let found = resolve_windows_program(
            OsStr::new("npx"),
            temp.path().as_os_str(),
            OsStr::new(".EXE;.cmd"),
        );
        assert_eq!(found.as_deref(), Some(command.as_os_str()));
    }

    #[test]
    fn windows_resolver_prefers_pathext_command_over_posix_shim() {
        let temp = tempdir().unwrap();
        let posix_shim = temp.path().join("npx");
        let windows_command = temp.path().join("npx.cmd");
        fs::write(&posix_shim, b"#!/bin/sh\n").unwrap();
        fs::write(&windows_command, b"@echo off\r\n").unwrap();

        let found = resolve_windows_program(
            OsStr::new("npx"),
            temp.path().as_os_str(),
            OsStr::new(".COM;.EXE;.BAT;.cmd"),
        );

        assert_eq!(
            fs::canonicalize(found.unwrap()).unwrap(),
            fs::canonicalize(windows_command).unwrap()
        );
    }
}
