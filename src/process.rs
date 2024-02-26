use std::{
    borrow::Borrow,
    future::IntoFuture,
    ops::Deref,
    pin::{pin, Pin},
    process::ExitStatus,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{executor::block_on, Future, FutureExt, Stream, StreamExt};
use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
};

#[pin_project]
pub struct ProcessStream<I> {
    #[pin]
    input: Option<I>,
    status: PSStatus,
    output_buffer_size: usize,
    //#[pin]
    //exit_code: Box<dyn Future<Output = Result<ExitStatus, std::io::Error>>>,
}

#[derive(Debug)]
struct PSStatus {
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    input_buffer: Option<Bytes>,
    input_closed: bool,
    child: Option<Child>,
}

#[derive(Debug)]
pub enum Output {
    Stdout(Bytes),
    Stderr(Bytes),
    ExitCode(Child),
}

impl Output {
    pub fn unwrap_out(self) -> Bytes {
        match self {
            Output::Stdout(v) => v,
            Output::Stderr(_) => panic!("Output is on stderr"),
            Output::ExitCode(_) => panic!("Output is exit code"),
        }
    }

    pub fn unwrap_err(self) -> Bytes {
        match self {
            Output::Stderr(v) => v,
            Output::Stdout(_) => panic!("Output is on stdout"),
            Output::ExitCode(_) => panic!("Output is exit code"),
        }
    }

    pub fn unwrap_exit_code(self) -> Child {
        match self {
            Output::Stdout(_) => panic!("Output is on stdout"),
            Output::Stderr(_) => panic!("Output is on stderr"),
            Output::ExitCode(c) => c,
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
            input: Some(input),
            status: PSStatus {
                stdin: Some(child.stdin.take().expect("Child stdin must be piped")),
                stdout: Some(child.stdout.take().expect("Child stdout must be piped")),
                stderr: Some(child.stderr.take().expect("Child stderr must be piped")),
                input_buffer: None,
                input_closed: false,
                child: Some(child),
            },
            output_buffer_size,
            //exit_code: Box::new(async move { child.wait().await }),
        }
    }
    
    fn next_stderr<E>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output_buffer_size: usize,
        poll_input: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.as_mut().project().status.stderr {
            Some(stderr) => match Pin::new(stderr).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        println!("--> stderr msg");
                        Poll::Ready(Some(Ok(Output::Stderr(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        println!("--> stderr closing");
                        self.as_mut().project().status.stderr = None;
                        if self.as_mut().project().status.stdout.is_none() {
                            self.as_mut().project().status.stdin = None;
                            self.as_mut().project().status.input_closed = true;
                            if let Some(child) = self.as_mut().project().status.child.take() {
                                Poll::Ready(Some(Ok(Output::ExitCode(child))))
                            } else {
                                Poll::Ready(None)
                            }
                        } else {
                            self.next_stdin(cx, poll_input)
                        }
                    }
                }
                Poll::Ready(Err(todo)) => {
                    println!("--> stderr error");
                    self.as_mut().project().status.stderr = None;
                    Poll::Ready(Some(Err(ProcessError {})))
                }
                Poll::Pending => self.next_stdin(cx, poll_input),
            },
            None => {
                if self.as_mut().project().status.stdout.is_none() {
                    println!("--> stdout and stderr are closed");
                    self.as_mut().project().status.stdin = None;
                    self.as_mut().project().status.input_closed = true;
                    if let Some(child) = self.as_mut().project().status.child.take() {
                        Poll::Ready(Some(Ok(Output::ExitCode(child))))
                    } else {
                        Poll::Ready(None)
                    }
                } else {
                    self.next_stdin(cx, poll_input)
                }
            }
        }
    }

    fn next_stdin<E>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut poll_input: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        //let status = self.as_mut().project().status;
        if let (false, Some(v)) = (
            self.as_mut().project().status.input_closed,
            self.as_mut().project().status.input_buffer.take(),
        ) {
            println!("--> Non empty buffer");
            self.as_mut().push_to_stdin(v, cx);
        }
        let mut input_pending = false;
        while let (true, false, false, true) = (
            self.as_mut().project().status.stdin.is_some(),
            self.as_mut().project().status.input_closed,
            input_pending,
            self.as_mut().project().status.input_buffer.is_none(),
        ) {
            println!("--> polling input");
            match poll_input(cx) {
                Poll::Ready(Some(Ok(v))) => {
                    if let Some(err) = self.as_mut().push_to_stdin(v, cx) {
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                Poll::Ready(Some(Err(todo))) => {
                    println!("--> input error");
                    self.as_mut().project().status.stdin = None;
                    self.as_mut().project().status.input_closed = true;
                    return Poll::Ready(Some(Err(ProcessError {})));
                }
                Poll::Ready(None) => {
                    println!("--> input ending");
                    self.as_mut().project().status.stdin = None;
                    self.as_mut().project().status.input_closed = true;
                }
                Poll::Pending => {
                    println!("--> input pending");
                    input_pending = true;
                }
            }
        }
        if self.as_mut().project().status.stdin.is_none() {
            println!("--> stdin is closed after polling input");
            self.as_mut().project().status.input_closed = true;
            self.as_mut().project().status.input_buffer = None;
        } else if self.as_mut().project().status.input_closed {
            println!("--> input is closed");
            if let Some(v) = self.as_mut().project().status.input_buffer.take() {
                // il faut vider le buffer dans stdin
                if let Some(err) = self.as_mut().push_to_stdin(v, cx) {
                    return Poll::Ready(Some(Err(err)));
                }
                if self.as_mut().project().status.input_buffer.is_none() {
                    println!("--> close stdin after emptying buffer");
                    self.as_mut().project().status.stdin = None;
                }
            } else {
                println!("--> directly close stdin because buffer is empty");
                self.as_mut().project().status.stdin = None;
            }
        }
        Poll::Pending
    }

    fn push_to_stdin(
        mut self: Pin<&mut Self>,
        mut v: Bytes,
        cx: &mut Context,
    ) -> Option<ProcessError> {
        let proj = self.as_mut().project();
        let stdin = proj.status.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_write(cx, &mut v) {
            Poll::Ready(Ok(size)) => {
                if size == 0 {
                    println!("--> stdin ending");
                    proj.status.stdin = None;
                } else {
                    println!("--> stdin accepting");
                    if size < v.len() {
                        proj.status.input_buffer = Some(v.slice(size..));
                    }
                    if let Some(err) = self.flush_stdin(cx) {
                        return Some(err);
                    }
                }
            }
            Poll::Ready(Err(todo)) => {
                proj.status.stdin = None;
                return Some(ProcessError {});
            }
            Poll::Pending => {
                println!("--> stdin pending");
                proj.status.input_buffer = Some(v);
            }
        }
        None
    }

    fn flush_stdin(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Option<ProcessError> {
        let proj = self.project();
        let stdin = proj.status.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                println!("--> flush stdin OK");
            }
            Poll::Ready(Err(todo)) => {
                proj.status.stdin = None;
                return Some(ProcessError {});
            }
            Poll::Pending => {
                println!("--> flush stdin pending");
            }
        }
        None
    }
}

impl PSStatus {
    fn next_stdout<E>(
        &mut self,
        cx: &mut Context<'_>,
        output_buffer_size: usize,
        poll_input: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
        close_input: impl FnOnce(),
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.stdout {
            Some(stdout) => match Pin::new(stdout).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        println!("--> stdout message");
                        Poll::Ready(Some(Ok(Output::Stdout(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        println!("--> stdout closing");
                        self.stdout = None;
                        self.next_stderr(cx, output_buffer_size, poll_input)
                    }
                }
                Poll::Ready(Err(todo)) => {
                    println!("--> stdout error");
                    self.stdout = None;
                    Poll::Ready(Some(Err(ProcessError {})))
                }
                Poll::Pending => self.next_stderr(cx, output_buffer_size, poll_input),
            },
            None => self.next_stderr(cx, output_buffer_size, poll_input),
        }
    }

    fn next_stderr<E>(
        &mut self,
        cx: &mut Context<'_>,
        output_buffer_size: usize,
        poll_input: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.stderr {
            Some(stderr) => match Pin::new(stderr).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        println!("--> stderr msg");
                        Poll::Ready(Some(Ok(Output::Stderr(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        println!("--> stderr closing");
                        self.stderr = None;
                        if self.stdout.is_none() {
                            self.stdin = None;
                            self.input_closed = true;
                            if let Some(child) = self.child.take() {
                                Poll::Ready(Some(Ok(Output::ExitCode(child))))
                            } else {
                                Poll::Ready(None)
                            }
                        } else {
                            self.next_stdin(cx, poll_input)
                        }
                    }
                }
                Poll::Ready(Err(todo)) => {
                    println!("--> stderr error");
                    self.stderr = None;
                    Poll::Ready(Some(Err(ProcessError {})))
                }
                Poll::Pending => self.next_stdin(cx, poll_input),
            },
            None => {
                if self.stdout.is_none() {
                    println!("--> stdout and stderr are closed");
                    self.stdin = None;
                    self.input_closed = true;
                    if let Some(child) = self.child.take() {
                        Poll::Ready(Some(Ok(Output::ExitCode(child))))
                    } else {
                        Poll::Ready(None)
                    }
                } else {
                    self.next_stdin(cx, poll_input)
                }
            }
        }
    }

    fn next_stdin<E>(
        &mut self,
        cx: &mut Context<'_>,
        mut poll_input: impl FnMut(&mut Context<'_>) -> Poll<Option<Result<Bytes, E>>>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        if let (false, Some(v)) = (self.input_closed, self.input_buffer.take()) {
            println!("--> Non empty buffer");
            self.push_to_stdin(v, cx);
        }
        let mut input_pending = false;
        while let (Some(_), false, false, true) = (
            &mut self.stdin,
            self.input_closed,
            input_pending,
            self.input_buffer.is_none(),
        ) {
            println!("--> polling input");
            match poll_input(cx) {
                Poll::Ready(Some(Ok(v))) => {
                    if let Some(err) = self.push_to_stdin(v, cx) {
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                Poll::Ready(Some(Err(todo))) => {
                    println!("--> input error");
                    self.stdin = None;
                    self.input_closed = true;
                    return Poll::Ready(Some(Err(ProcessError {})));
                }
                Poll::Ready(None) => {
                    println!("--> input ending");
                    self.stdin = None;
                    self.input_closed = true;
                }
                Poll::Pending => {
                    println!("--> input pending");
                    input_pending = true;
                }
            }
        }
        if self.stdin.is_none() {
            println!("--> stdin is closed after polling input");
            self.input_closed = true;
            self.input_buffer = None;
        } else if self.input_closed {
            println!("--> input is closed");
            if let Some(v) = self.input_buffer.take() {
                // il faut vider le buffer dans stdin
                if let Some(err) = self.push_to_stdin(v, cx) {
                    return Poll::Ready(Some(Err(err)));
                }
                if self.input_buffer.is_none() {
                    println!("--> close stdin after emptying buffer");
                    self.stdin = None;
                }
            } else {
                println!("--> directly close stdin because buffer is empty");
                self.stdin = None;
            }
        }
        Poll::Pending
    }

    fn push_to_stdin(&mut self, mut v: Bytes, cx: &mut Context) -> Option<ProcessError> {
        let stdin = self.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_write(cx, &mut v) {
            Poll::Ready(Ok(size)) => {
                if size == 0 {
                    println!("--> stdin ending");
                    self.stdin = None;
                } else {
                    println!("--> stdin accepting");
                    if size < v.len() {
                        self.input_buffer = Some(v.slice(size..));
                    }
                    if let Some(err) = self.flush_stdin(cx) {
                        return Some(err);
                    }
                }
            }
            Poll::Ready(Err(todo)) => {
                self.stdin = None;
                return Some(ProcessError {});
            }
            Poll::Pending => {
                println!("--> stdin pending");
                self.input_buffer = Some(v);
            }
        }
        None
    }

    fn flush_stdin(&mut self, cx: &mut Context<'_>) -> Option<ProcessError> {
        let stdin = self.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                println!("--> flush stdin OK");
            }
            Poll::Ready(Err(todo)) => {
                self.stdin = None;
                return Some(ProcessError {});
            }
            Poll::Pending => {
                println!("--> flush stdin pending");
            }
        }
        None
    }
}

impl<I, E> Stream for ProcessStream<I>
where
    I: Stream<Item = Result<Bytes, E>>,
{
    type Item = Result<Output, ProcessError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        println!("poll_next");
        let mut proj = self.project();
        let status = proj.status;
        match proj.input.as_pin_mut() {
            Some(mut input) => status.next_stdout(
                cx,
                *proj.output_buffer_size,
                |cx| input.as_mut().poll_next(cx),
                //|| proj.input.as_mut().set(None),
                || {},
            ),
            None => todo!(),
        }
    }
}

#[cfg(test)]
mod process_stream_test {
    use std::{cell::Cell, process::Stdio, rc::Rc, time::Duration};

    use bytes::Bytes;
    use futures::{
        stream::{self},
        StreamExt,
    };
    use tokio::{process::Command, time::sleep};

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
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
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
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
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
        //let input = stream::once(async { Ok::<Bytes, String>(Bytes::from("value".as_bytes())) });
        let input = stream::repeat_with(|| {
            println!("INPUT: coucou");
            Ok::<Bytes, String>(Bytes::from("value".as_bytes()))
        })
        .take(2);
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "valuevalue")
    }

    #[tokio::test]
    async fn dont_consume_input_test() {
        let rc = Rc::new(Cell::new(0));
        let child = Command::new("sleep")
            .arg("1")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let rc2 = rc.clone();
        let input = stream::repeat_with(|| {
            let v = rc2.get() + 1;
            rc2.set(v);
            println!("INPUT: coucou {}", v);
            Ok::<Bytes, String>(Bytes::from("value".as_bytes()))
        });
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "");
        assert_eq!(rc.get(), 13105);
    }

    #[tokio::test]
    async fn consume_input_slowly_test() {
        let rc = Rc::new(Cell::new(0));
        let child = Command::new("./read_slowly.sh")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let rc2 = rc.clone();
        let input = stream::repeat_with(|| {
            let v = rc2.get() + 1;
            rc2.set(v);
            println!("INPUT: coucou {}", v);
            Ok::<Bytes, String>(Bytes::from("value".as_bytes()))
        });
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "valueval");
        //assert_eq!(rc.get(), 13105);
    }

    #[tokio::test]
    async fn consume_later_test() {
        let rc = Rc::new(Cell::new(0));
        let child = Command::new("./consume_later.sh")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        let rc2 = rc.clone();
        let input = stream::repeat_with(|| {
            let v = rc2.get() + 1;
            rc2.set(v);
            println!("INPUT: coucou {}", v);
            Ok::<Bytes, String>(Bytes::from("value".as_bytes()))
        });
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "");
        assert_eq!(rc.get(), 27028);
    }

    #[tokio::test]
    async fn produce_slowly_test() {
        let child = Command::new("cat")
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn");
        //let input = stream::empty::<Result<Bytes, String>>();
        let input = stream::once(async {
            sleep(Duration::from_millis(1000)).await;
            Ok::<Bytes, String>(Bytes::from("value".as_bytes()))
        })
        .chain(stream::once(async {
            sleep(Duration::from_millis(1000)).await;
            Ok::<Bytes, String>(Bytes::from("value".as_bytes()))
        }));
        let process_stream = ProcessStream::new(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(s) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "valuevalue")
    }
}
