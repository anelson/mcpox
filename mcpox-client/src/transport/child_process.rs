use std::{
    io::{self, IoSlice},
    pin::Pin,
    process::Command,
    task::{Context, Poll},
};

use futures::{FutureExt, SinkExt, StreamExt};
use pin_project::pin_project;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::Instrument;

use crate::{McpClientError, Result};

/// Max length of a JSON message allowed on this transport
///
/// TODO: Make this configurable.
const MAX_LINE_LENGTH: usize = 8 * 1024 * 1024;

/// Transport implementation for local MCP servers that are launched as child processes of the
/// client.
pub struct ChildProcess {
    _child: tokio::process::Child,
    io: Framed<ChildDuplex, LinesCodec>,
    span: tracing::Span,
}

impl ChildProcess {
    /// Launch the MCP server as a child process described by the given [`Command`].
    ///
    /// This will launch the child process and then immediately return.  When this struct is
    /// dropped, the child process will be terminated.
    pub async fn run(command: Command) -> Result<Self> {
        // Internally we use tokio to manage the process
        let mut command: tokio::process::Command = command.into();

        // Configure the command to run as a child process with stdout and stdin piped
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());

        let program = command.as_std().get_program().to_string_lossy().to_string();
        let args = command
            .as_std()
            .get_args()
            .collect::<Vec<_>>()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();

        tracing::debug!(%program,
            args = %args.join(" "),
            "Spawning MCP server as child process"
        );

        let mut child = command.spawn().map_err(|e| McpClientError::SpawnServer {
            source: e,
            program,
            args,
        })?;

        let pid = child.id();
        tracing::debug!(pid, "Child process spawned");

        let stdin = child.stdin.take().expect("BUG: stdin is hard-coded as piped");
        let stdout = child.stdout.take().expect("BUG: stdout is hard-coded as piped");
        let stderr = child.stderr.take().expect("BUG: stderr is hard-coded as piped");
        let span = tracing::info_span!("child_process", pid);

        // Monitor stderr output for the child process and log anything that we get there.
        // This can be useful for diagnostic purposes of the server is failing cryptically when
        // performing certain operations
        tokio::spawn(
            async move {
                let stderr = BufReader::new(stderr);
                let mut lines = stderr.lines();

                while let Some(line) = lines.next_line().await.unwrap() {
                    tracing::error!("{}", line);
                }
            }
            .instrument(tracing::info_span!("child_process_stderr", pid)),
        );

        Ok(Self {
            _child: child,
            io: Framed::new(
                ChildDuplex { stdin, stdout },
                LinesCodec::new_with_max_length(MAX_LINE_LENGTH),
            ),
            span,
        })
    }
}

/// A pair of stdin and stdout streams for a child process that together works as a duplex channel
/// implementing [`AsyncRead`] and [`AsyncWrite`] both.
#[derive(Debug)]
#[pin_project]
pub struct ChildDuplex {
    #[pin]
    stdin: tokio::process::ChildStdin,
    #[pin]
    stdout: tokio::process::ChildStdout,
}

impl AsyncRead for ChildDuplex {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(self.project().stdout, cx, buf)
    }
}

impl AsyncWrite for ChildDuplex {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(self.project().stdin, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(self.project().stdin, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_shutdown(self.project().stdin, cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        AsyncWrite::poll_write_vectored(self.project().stdin, cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        AsyncWrite::is_write_vectored(&self.stdin)
    }
}

impl mcpox_jsonrpc::Transport for ChildProcess {
    type Error = tokio_util::codec::LinesCodecError;

    fn span(&self) -> tracing::Span {
        self.span.clone()
    }

    fn populate_metadata(&self, _metadata: &mut mcpox_jsonrpc::TypeMap) {
        // there's no transport metadata for this transport
    }

    fn send_message(
        &mut self,
        message: String,
    ) -> impl Future<Output = mcpox_jsonrpc::Result<(), Self::Error>> + Send + '_ {
        self.io.send(message)
    }

    fn receive_message(
        &mut self,
    ) -> impl Future<Output = mcpox_jsonrpc::Result<Option<String>, Self::Error>> + Send + '_ {
        self.io.next().map(
            |opt_result: Option<Result<_, tokio_util::codec::LinesCodecError>>| {
                // Convert this from Option<Result<T>> to Result<Option<T>>
                opt_result.transpose()
            },
        )
    }
}

/// Tests for this rely on UNIX-specific commands, so on Windows we'll just have to assume this
/// works for now.
#[cfg(all(unix, test))]
mod tests {
    use std::time::Duration;
    use tokio::time::timeout;

    use super::*;
    use crate::testing;
    use mcpox_jsonrpc::Transport;

    /// Creates a simple echo command that reads from stdin and writes to stdout
    fn create_echo_command() -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("while IFS= read -r line; do echo \"$line\"; echo \"DEBUG: Received message\" >&2; done");
        cmd
    }

    /// Creates a command that exits after N messages
    fn create_limited_echo_command(count: usize) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!(
                "i=0; while IFS= read -r line && [ $i -lt {} ]; do echo \"$line\"; echo \"DEBUG: Received message $i\" >&2; i=$((i+1)); done",
                count
            ));
        cmd
    }

    /// Creates a command that emits specific stderr messages
    fn create_stderr_log_command() -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo \"Starting process\" >&2; while IFS= read -r line; do echo \"$line\"; echo \"STDERR: $line\" >&2; done");
        cmd
    }

    #[tokio::test]
    async fn test_basic_echo() {
        testing::init_test_logging();

        let command = create_echo_command();
        let mut process = ChildProcess::run(command).await.unwrap();

        // Test single message
        let test_message = "Hello, world!";
        process.send_message(test_message.to_string()).await.unwrap();

        let response = process
            .receive_message()
            .await
            .unwrap()
            .expect("Should receive a response");

        assert_eq!(response, test_message);
    }

    #[tokio::test]
    async fn test_multiple_messages() {
        testing::init_test_logging();

        let command = create_echo_command();
        let mut process = ChildProcess::run(command).await.unwrap();

        for i in 0..5 {
            let message = format!("Test message {}", i);
            process.send_message(message.clone()).await.unwrap();

            let response = process
                .receive_message()
                .await
                .unwrap()
                .expect("Should receive a response");

            assert_eq!(response, message);
        }
    }

    #[tokio::test]
    async fn test_process_exit() {
        testing::init_test_logging();

        let message_count = 3;
        let command = create_limited_echo_command(message_count);
        let mut process = ChildProcess::run(command).await.unwrap();

        // Send and receive the exact number of messages the process will handle
        for i in 0..message_count {
            let message = format!("Test message {}", i);
            process.send_message(message.clone()).await.unwrap();

            let response = process
                .receive_message()
                .await
                .unwrap()
                .expect("Should receive a response");

            assert_eq!(response, message);
        }

        // Send one more message - the process should have exited so we shouldn't get a response
        let final_message = "Final message";
        process.send_message(final_message.to_string()).await.unwrap();

        // The process should have exited, so receive_message should return None
        let result = timeout(Duration::from_millis(500), process.receive_message()).await;

        match result {
            Ok(Ok(None)) => {
                // Expected result: process exited cleanly
            }
            Ok(Ok(Some(_))) => {
                panic!("Expected process to exit, but it's still responding");
            }
            Ok(Err(e)) => {
                // Also acceptable: we might get an error when the process terminates
                println!("Process terminated with error: {:?}", e);
            }
            Err(_) => {
                panic!("receive_message timed out, process might be hanging");
            }
        }
    }

    #[tokio::test]
    async fn test_stderr_logging() {
        testing::init_test_logging();

        let command = create_stderr_log_command();
        let mut process = ChildProcess::run(command).await.unwrap();

        // Send a message and give time for stderr processing
        let test_message = "This should be logged to stderr";
        process.send_message(test_message.to_string()).await.unwrap();

        // Verify the response comes back correctly
        let response = process
            .receive_message()
            .await
            .unwrap()
            .expect("Should receive a response");

        assert_eq!(response, test_message);

        // Note: We can't directly verify the stderr output is logged since
        // it goes through tracing, but this at least exercises the code path
    }

    #[tokio::test]
    async fn test_large_message() {
        testing::init_test_logging();

        let command = create_echo_command();
        let mut process = ChildProcess::run(command).await.unwrap();

        // Create a message that's large but under the limit
        let large_message = "x".repeat(MAX_LINE_LENGTH - 1);

        // Send and receive the large message
        process.send_message(large_message.clone()).await.unwrap();

        let response = process
            .receive_message()
            .await
            .unwrap()
            .expect("Should receive a response");

        assert_eq!(response, large_message);
    }

    #[tokio::test]
    async fn test_too_large_message() {
        testing::init_test_logging();

        let command = create_echo_command();
        let mut process = ChildProcess::run(command).await.unwrap();

        // Create a message larger than MAX_LINE_LENGTH
        let too_large_message = "x".repeat(MAX_LINE_LENGTH + 1);

        // The send might succeed, but reading the response should fail
        process.send_message(too_large_message).await.unwrap();

        // The receive should fail since the message is too large
        let receive_result = process.receive_message().await;
        assert!(receive_result.is_err(), "Receiving oversized message should fail");
    }

    #[tokio::test]
    async fn test_process_termination_on_drop() {
        use nix::sys::signal;
        use nix::unistd::Pid;

        testing::init_test_logging();

        // Create the process
        let command = create_echo_command();
        let process = ChildProcess::run(command).await.unwrap();

        // Extract the PID before dropping
        let pid = process._child.id().expect("Child should have a valid PID");

        // Define is_process_running inline
        let is_process_running = |pid: u32| -> bool {
            // Send None as the signal to check if process exists without actually sending a signal
            signal::kill(Pid::from_raw(pid as i32), None).is_ok()
        };

        // Make sure the process is running
        assert!(is_process_running(pid), "Process should be running");

        // Drop the ChildProcess, which should terminate the child
        drop(process);

        // Give some time for process termination
        for _ in 0..1000 {
            if !is_process_running(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Check that the process is no longer running
        assert!(
            !is_process_running(pid),
            "Process should be terminated after drop"
        );
    }
}
