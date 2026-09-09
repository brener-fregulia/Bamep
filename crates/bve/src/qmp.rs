//! The minimal QMP (QEMU Machine Protocol) client the BVE lifecycle needs.
//!
//! QMP is QEMU's line-delimited JSON control protocol over a socket. This
//! module implements only the subset Issue #67 requires — the capabilities
//! handshake, `query-status`, `system_reset`, `quit` — and no more. It is not
//! a general QMP library.
//!
//! Split for deterministic testing: command generation and response
//! classification are pure functions; [`QmpConnection`] adds the blocking
//! Unix-socket I/O and the real handshake around them.
//!
//! Handshake (respected, not bypassed): QEMU sends a `{"QMP": {...}}`
//! greeting; the client must send `{"execute": "qmp_capabilities"}` and
//! receive its `{"return": {}}` before any other command is accepted.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A classified QMP response line (asynchronous `event` lines excluded — see
/// [`classify_response`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QmpResponse {
    /// A successful command result (`{"return": ...}`).
    Return(Value),
    /// A command error (`{"error": {"class": ..., "desc": ...}}`).
    Error {
        /// QMP error class, e.g. `GenericError`.
        class: String,
        /// Human-readable description.
        desc: String,
    },
    /// An asynchronous event (`{"event": "NAME", ...}`); carries the name.
    Event(String),
}

/// The coarse run state Issue #67 distinguishes. The BVE lifecycle maps this
/// onto `Stopped` / `Running`; it deliberately does not model
/// paused/prelaunch/migrating as distinct lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// The guest CPUs are executing (`query-status` -> `running`).
    Running,
    /// The VM exists but its CPUs are not executing (paused, prelaunch, ...).
    NotRunning,
}

/// Why a QMP interaction failed.
#[derive(Debug, thiserror::Error)]
pub enum QmpError {
    /// Socket I/O error.
    #[error("QMP socket I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The stream closed while a specific message was expected.
    #[error("QMP stream closed while awaiting {expected}")]
    UnexpectedEof {
        /// What was being awaited.
        expected: &'static str,
    },

    /// A line was not valid JSON.
    #[error("malformed QMP JSON line: {line:?}")]
    MalformedJson {
        /// The offending raw line.
        line: String,
    },

    /// The first line was not a `{"QMP": ...}` greeting.
    #[error("expected a QMP greeting, got: {line:?}")]
    NotAGreeting {
        /// The offending raw line.
        line: String,
    },

    /// A command returned a QMP `error` object.
    #[error("QMP command {command:?} failed: {class}: {desc}")]
    CommandFailed {
        /// The `execute` name that failed.
        command: String,
        /// QMP error class.
        class: String,
        /// QMP error description.
        desc: String,
    },

    /// The capabilities handshake did not complete.
    #[error("QMP capabilities handshake did not complete: {0}")]
    Handshake(String),

    /// The QMP socket did not appear within the allotted time.
    #[error("QMP socket {path} did not become connectable within {millis} ms")]
    SocketTimeout {
        /// The socket path awaited.
        path: PathBuf,
        /// The timeout that elapsed.
        millis: u128,
    },
}

/// `{"execute": "qmp_capabilities"}` — the handshake command.
pub fn capabilities_command() -> String {
    r#"{"execute":"qmp_capabilities"}"#.to_string()
}

/// `{"execute": "query-status"}`.
pub fn query_status_command() -> String {
    r#"{"execute":"query-status"}"#.to_string()
}

/// `{"execute": "system_reset"}` — reboots the same VM in place.
pub fn system_reset_command() -> String {
    r#"{"execute":"system_reset"}"#.to_string()
}

/// `{"execute": "quit"}` — asks QEMU to exit.
pub fn quit_command() -> String {
    r#"{"execute":"quit"}"#.to_string()
}

/// Validates that `line` is a QMP greeting (`{"QMP": {...}}`).
pub fn parse_greeting(line: &str) -> Result<(), QmpError> {
    let value: Value = serde_json::from_str(line).map_err(|_| QmpError::MalformedJson {
        line: line.to_string(),
    })?;
    if value.get("QMP").is_some() {
        Ok(())
    } else {
        Err(QmpError::NotAGreeting {
            line: line.to_string(),
        })
    }
}

/// Classifies one response line as a `return`, an `error`, or an asynchronous
/// `event`. Unknown-shaped objects are reported as [`QmpError::MalformedJson`].
pub fn classify_response(line: &str) -> Result<QmpResponse, QmpError> {
    let value: Value = serde_json::from_str(line).map_err(|_| QmpError::MalformedJson {
        line: line.to_string(),
    })?;

    if let Some(ret) = value.get("return") {
        return Ok(QmpResponse::Return(ret.clone()));
    }
    if let Some(err) = value.get("error") {
        let class = err
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or("UnknownError")
            .to_string();
        let desc = err
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok(QmpResponse::Error { class, desc });
    }
    if let Some(event) = value.get("event").and_then(Value::as_str) {
        return Ok(QmpResponse::Event(event.to_string()));
    }
    Err(QmpError::MalformedJson {
        line: line.to_string(),
    })
}

/// Interprets a `query-status` `return` object as a [`RunState`].
///
/// The object is `{"status": "<name>", "running": <bool>, ...}`; anything
/// other than an explicit `running: true` with `status == "running"` is
/// treated as [`RunState::NotRunning`].
pub fn run_state_from_status_return(value: &Value) -> RunState {
    let running = value
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let status_running = value.get("status").and_then(Value::as_str) == Some("running");
    if running && status_running {
        RunState::Running
    } else {
        RunState::NotRunning
    }
}

/// A blocking QMP client bound to one BVE's control socket, past the
/// capabilities handshake and ready for commands.
pub struct QmpConnection {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl QmpConnection {
    /// Connects to `socket` and completes the capabilities handshake.
    pub fn connect(socket: &Path) -> Result<Self, QmpError> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut conn = Self {
            writer: stream,
            reader,
        };

        let greeting = conn.read_line("QMP greeting")?;
        parse_greeting(&greeting)?;

        match conn.execute("qmp_capabilities", &capabilities_command())? {
            Value::Object(_) | Value::Null => Ok(conn),
            other => Err(QmpError::Handshake(format!(
                "unexpected qmp_capabilities return: {other}"
            ))),
        }
    }

    /// Retries [`QmpConnection::connect`] until it succeeds or `timeout`
    /// elapses. Used at `start` to confirm the control boundary came up.
    pub fn connect_with_retry(socket: &Path, timeout: Duration) -> Result<Self, QmpError> {
        let deadline = Instant::now() + timeout;
        let mut last_err: Option<QmpError> = None;
        while Instant::now() < deadline {
            match Self::connect(socket) {
                Ok(conn) => return Ok(conn),
                Err(err) => {
                    last_err = Some(err);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        Err(last_err.unwrap_or(QmpError::SocketTimeout {
            path: socket.to_path_buf(),
            millis: timeout.as_millis(),
        }))
    }

    /// Runs `query-status` and returns the coarse [`RunState`].
    pub fn query_run_state(&mut self) -> Result<RunState, QmpError> {
        let value = self.execute("query-status", &query_status_command())?;
        Ok(run_state_from_status_return(&value))
    }

    /// Issues `system_reset` against this VM.
    pub fn system_reset(&mut self) -> Result<(), QmpError> {
        self.execute("system_reset", &system_reset_command())?;
        Ok(())
    }

    /// Issues `quit`. QEMU may close the socket before or after replying;
    /// end-of-stream here is treated as success.
    pub fn quit(&mut self) -> Result<(), QmpError> {
        self.write_command(&quit_command())?;
        loop {
            let line = match self.read_line("quit acknowledgement") {
                Ok(line) => line,
                Err(QmpError::UnexpectedEof { .. }) => return Ok(()),
                Err(other) => return Err(other),
            };
            match classify_response(&line)? {
                QmpResponse::Return(_) => return Ok(()),
                QmpResponse::Error { class, desc } => {
                    return Err(QmpError::CommandFailed {
                        command: "quit".to_string(),
                        class,
                        desc,
                    })
                }
                QmpResponse::Event(_) => continue,
            }
        }
    }

    /// Writes one command line and reads until a non-event response, mapping
    /// a QMP `error` to [`QmpError::CommandFailed`].
    fn execute(&mut self, name: &str, line: &str) -> Result<Value, QmpError> {
        self.write_command(line)?;
        loop {
            let response = self.read_line("command response")?;
            match classify_response(&response)? {
                QmpResponse::Return(value) => return Ok(value),
                QmpResponse::Error { class, desc } => {
                    return Err(QmpError::CommandFailed {
                        command: name.to_string(),
                        class,
                        desc,
                    })
                }
                QmpResponse::Event(_) => continue,
            }
        }
    }

    fn write_command(&mut self, line: &str) -> Result<(), QmpError> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_line(&mut self, expected: &'static str) -> Result<String, QmpError> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line)?;
        if read == 0 {
            return Err(QmpError::UnexpectedEof { expected });
        }
        Ok(line.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execute_name(command: &str) -> String {
        let value: Value = serde_json::from_str(command).unwrap();
        value["execute"].as_str().unwrap().to_string()
    }

    #[test]
    fn commands_carry_the_expected_execute_names() {
        assert_eq!(execute_name(&capabilities_command()), "qmp_capabilities");
        assert_eq!(execute_name(&query_status_command()), "query-status");
        assert_eq!(execute_name(&system_reset_command()), "system_reset");
        assert_eq!(execute_name(&quit_command()), "quit");
    }

    #[test]
    fn greeting_parsing_accepts_only_a_qmp_greeting() {
        assert!(parse_greeting(r#"{"QMP":{"version":{"qemu":{}},"capabilities":[]}}"#).is_ok());
        assert!(matches!(
            parse_greeting(r#"{"return":{}}"#),
            Err(QmpError::NotAGreeting { .. })
        ));
        assert!(matches!(
            parse_greeting("not json"),
            Err(QmpError::MalformedJson { .. })
        ));
    }

    #[test]
    fn responses_are_classified() {
        assert_eq!(
            classify_response(r#"{"return":{"status":"running","running":true}}"#).unwrap(),
            QmpResponse::Return(serde_json::json!({"status":"running","running":true}))
        );
        assert_eq!(
            classify_response(r#"{"error":{"class":"GenericError","desc":"nope"}}"#).unwrap(),
            QmpResponse::Error {
                class: "GenericError".to_string(),
                desc: "nope".to_string(),
            }
        );
        assert_eq!(
            classify_response(r#"{"event":"RESET","timestamp":{"seconds":1,"microseconds":2}}"#)
                .unwrap(),
            QmpResponse::Event("RESET".to_string())
        );
        assert!(matches!(
            classify_response(r#"{"unknown":1}"#),
            Err(QmpError::MalformedJson { .. })
        ));
    }

    #[test]
    fn run_state_requires_explicit_running_true() {
        assert_eq!(
            run_state_from_status_return(&serde_json::json!({"status":"running","running":true})),
            RunState::Running
        );
        assert_eq!(
            run_state_from_status_return(
                &serde_json::json!({"status":"prelaunch","running":false})
            ),
            RunState::NotRunning
        );
        assert_eq!(
            run_state_from_status_return(&serde_json::json!({})),
            RunState::NotRunning
        );
    }
}
