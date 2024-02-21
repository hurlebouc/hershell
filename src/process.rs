use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::Stream;
use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
};

#[pin_project]
pub struct ProcessStream<I> {
    #[pin]
    input: I,
    status: PSStatus,
    output_buffer_size: usize,
    child: Child, // keep reference to child process in order not to drop it before dropping the ProcessStream
}

struct PSStatus {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    input_buffer: Option<Bytes>,
    input_closed: bool,
}

pub enum Output {
    Stdout(Bytes),
    Stderr(Bytes),
}

impl Output {
    pub fn unwrap_out(self) -> Bytes {
        match self {
            Output::Stdout(v) => v,
            Output::Stderr(_) => panic!("Output is err"),
        }
    }

    pub fn unwrap_err(self) -> Bytes {
        match self {
            Output::Stderr(v) => v,
            Output::Stdout(_) => panic!("Output is out"),
        }
    }
}

#[derive(Debug)]
pub struct ProcessError {}

impl<I> ProcessStream<I> {
    /// Creates a new [`ProcessStream<I>`].
    ///
    /// # Panics
    ///
    /// Panics if stdin, stdout or stderr is not piped.
    pub fn new(mut child: Child, input: I, output_buffer_size: usize) -> ProcessStream<I> {
        ProcessStream {
            input,
            status: PSStatus {
                stdin: Some(child.stdin.take().expect("Child stdin must be piped")),
                stdout: Some(child.stdout.take().expect("Child stdout must be piped")),
                stderr: Some(child.stderr.take().expect("Child stderr must be piped")),
                input_buffer: None,
                input_closed: false,
            },
            output_buffer_size,
            child,
        }
    }
}

impl PSStatus {
    fn next_stdout<E>(
        &mut self,
        cx: &mut Context<'_>,
        output_buffer_size: usize,
        poll_input: impl FnOnce(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.stdout {
            Some(stdout) => match Pin::new(stdout).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        Poll::Ready(Some(Ok(Output::Stdout(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        self.stdout = None;
                        self.next_stderr(cx, output_buffer_size, poll_input)
                    }
                }
                Poll::Ready(Err(todo)) => Poll::Ready(Some(Err(ProcessError {}))),
                Poll::Pending => self.next_stderr(cx, output_buffer_size, poll_input),
            },
            None => self.next_stderr(cx, output_buffer_size, poll_input),
        }
    }

    fn next_stderr<E>(
        &mut self,
        cx: &mut Context<'_>,
        output_buffer_size: usize,
        poll_input: impl FnOnce(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.stderr {
            Some(stderr) => match Pin::new(stderr).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        Poll::Ready(Some(Ok(Output::Stderr(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        self.stderr = None;
                        if self.stdout.is_none() {
                            self.stdin = None;
                            Poll::Ready(None)
                        } else {
                            self.next_stdin(cx, poll_input)
                        }
                    }
                }
                Poll::Ready(Err(todo)) => Poll::Ready(Some(Err(ProcessError {}))),
                Poll::Pending => self.next_stdin(cx, poll_input),
            },
            None => {
                if self.stdout.is_none() {
                    self.stdin = None;
                    Poll::Ready(None)
                } else {
                    self.next_stdin(cx, poll_input)
                }
            }
        }
    }

    fn next_stdin<E>(
        &mut self,
        cx: &mut Context<'_>,
        poll_input: impl FnOnce(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        match &mut self.stdin {
            Some(_) => match self.input_buffer.take() {
                Some(v) => self.push_to_stdin(v, cx),
                None => {
                    if !self.input_closed {
                        match poll_input(cx) {
                            Poll::Ready(Some(Ok(v))) => self.push_to_stdin(v, cx),
                            Poll::Ready(Some(Err(todo))) => {
                                self.stdin = None;
                                self.input_closed = true;
                                // todo : logger quelque chose
                                Poll::Pending
                            }
                            Poll::Ready(None) => {
                                self.stdin = None;
                                self.input_closed = true;
                                Poll::Pending
                            }
                            Poll::Pending => Poll::Pending,
                        }
                    } else {
                        Poll::Pending
                    }
                }
            },
            None => {
                self.input_closed = true;
                Poll::Pending
            }
        }
    }

    fn push_to_stdin<O>(
        &mut self,
        mut v: Bytes,
        cx: &mut Context,
    ) -> Poll<Option<Result<O, ProcessError>>> {
        let stdin = self.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_write(cx, &mut v) {
            Poll::Ready(Ok(size)) => {
                if size == 0 {
                    self.stdin = None;
                    Poll::Pending
                } else {
                    if size < v.len() {
                        self.input_buffer = Some(v.slice(size..));
                    }
                    self.flush_stdin(cx)
                }
            }
            Poll::Ready(Err(todo)) => {
                self.stdin = None;
                Poll::Ready(Some(Err(ProcessError {}))) //todo
            }
            Poll::Pending => {
                self.input_buffer = Some(v);
                Poll::Pending
            }
        }
    }

    fn flush_stdin<O>(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<O, ProcessError>>> {
        let stdin = self.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Pending,
            Poll::Ready(Err(todo)) => {
                self.stdin = None;
                Poll::Ready(Some(Err(ProcessError {}))) //todo
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<I, E> Stream for ProcessStream<I>
where
    I: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Output, ProcessError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        println!("poll_next");
        let proj = self.project();
        let status = proj.status;
        status.next_stdout(cx, *proj.output_buffer_size, |cx| proj.input.poll_next(cx))
    }
}

#[cfg(test)]
mod process_stream_test {
    use std::process::Stdio;

    use bytes::Bytes;
    use futures::{
        stream::{self},
        StreamExt,
    };
    use tokio::process::Command;

    use super::ProcessStream;

    #[tokio::test]
    async fn simple_process_test() {
        let child = Command::new("echo")
            .arg("hello")
            .arg("world")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let input = stream::empty::<Result<Bytes, String>>();
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .map(|r| r.unwrap().unwrap_out())
            .fold("".to_string(), |s, b| async move {
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "hello world\n")
    }

    #[tokio::test]
    async fn small_buffer_test() {
        let child = Command::new("echo")
            .arg("hello")
            .arg("world")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let input = stream::empty::<Result<Bytes, String>>();
        let process_stream = ProcessStream::new(child, input, 1);
        let s = process_stream
            .map(|r| r.unwrap().unwrap_out())
            .fold("".to_string(), |s, b| async move {
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "hello world\n")
    }

    //#[tokio::test(flavor = "multi_thread")]
    #[tokio::test]
    async fn read_input_test() {
        println!("read_input_test");
        let child = Command::new("cat")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        //let input = stream::empty::<Result<Bytes, String>>();
        let input = stream::once(async { Ok::<Bytes, String>(Bytes::from("value".as_bytes())) });
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .map(|r| r.unwrap().unwrap_out())
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "value")
    }

    #[tokio::test]
    async fn read_input_slow_test() {
        println!("read_input_test");
        let child = Command::new("cat")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        //let input = stream::empty::<Result<Bytes, String>>();
        let input = stream::once(async { Ok::<Bytes, String>(Bytes::from("value".as_bytes())) });
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .map(|r| r.unwrap().unwrap_out())
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "value")
    }
}
