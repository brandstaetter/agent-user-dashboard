use super::*;

pub(super) struct ChildTransport {
    child: Child,
    stdin: Option<ChildStdin>,
    receiver: Receiver<Result<Vec<u8>, TransportError>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stopped: bool,
}

impl ChildTransport {
    pub(super) fn spawn(executable: &Path) -> Result<Self, CodexError> {
        let mut child = Command::new(executable)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| CodexError::Spawn)?;
        let stdin = child.stdin.take().ok_or(CodexError::Spawn)?;
        let stdout = child.stdout.take().ok_or(CodexError::Spawn)?;
        let stderr = child.stderr.take().ok_or(CodexError::Spawn)?;
        let (sender, receiver) = mpsc::sync_channel(MAX_MESSAGES_PER_REQUEST);
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let read = reader
                    .by_ref()
                    .take((MAX_PROTOCOL_LINE_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line);
                match read {
                    Ok(0) => {
                        try_enqueue(&sender, Err(TransportError::Closed));
                        break;
                    }
                    Ok(_) if line.len() > MAX_PROTOCOL_LINE_BYTES => {
                        try_enqueue(&sender, Err(TransportError::Oversized));
                        break;
                    }
                    Ok(_) => {
                        if line.last() == Some(&b'\n') {
                            line.pop();
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                        }
                        if !try_enqueue(&sender, Ok(line)) {
                            break;
                        }
                    }
                    Err(_) => {
                        try_enqueue(&sender, Err(TransportError::Io));
                        break;
                    }
                }
            }
        });
        let stderr_thread = thread::spawn(move || {
            let mut stderr = stderr;
            let mut buffer = [0_u8; 4096];
            while let Ok(count) = stderr.read(&mut buffer) {
                if count == 0 {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            receiver,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stopped: false,
        })
    }
}

/// A full queue means the peer is producing faster than the bounded protocol
/// consumer can validate messages. Stop reading instead of ever blocking the
/// producer; queued messages drain and then the receiver reports closure.
fn try_enqueue(
    sender: &mpsc::SyncSender<Result<Vec<u8>, TransportError>>,
    message: Result<Vec<u8>, TransportError>,
) -> bool {
    sender.try_send(message).is_ok()
}

impl LineTransport for ChildTransport {
    fn send_line(&mut self, line: &[u8]) -> Result<(), TransportError> {
        if line.len() > MAX_PROTOCOL_LINE_BYTES || line.contains(&b'\n') {
            return Err(TransportError::Oversized);
        }
        let stdin = self.stdin.as_mut().ok_or(TransportError::Closed)?;
        stdin.write_all(line).map_err(|_| TransportError::Io)?;
        stdin.write_all(b"\n").map_err(|_| TransportError::Io)?;
        stdin.flush().map_err(|_| TransportError::Io)
    }

    fn receive_line(&mut self, timeout: Duration) -> Result<Vec<u8>, TransportError> {
        self.receiver
            .recv_timeout(timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => TransportError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => TransportError::Closed,
            })?
    }

    fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ChildTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_protocol_queue_never_blocks_the_producer() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender.try_send(Ok(vec![b'1'])).unwrap();
        let (done_sender, done_receiver) = mpsc::channel();
        let producer = thread::spawn(move || {
            let accepted = try_enqueue(&sender, Ok(vec![b'2']));
            done_sender.send(accepted).unwrap();
        });

        assert_eq!(
            done_receiver.recv_timeout(Duration::from_millis(100)),
            Ok(false)
        );
        drop(receiver);
        producer.join().unwrap();
    }
}
