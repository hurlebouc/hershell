use std::{
    future::ready,
    pin::Pin,
    process::ExitStatus,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{Future, Stream, TryStream, TryStreamExt};
use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
};

#[pin_project]
pub struct ProcessStream<I, X, U> {
    #[pin]
    input: Option<I>,
    output_buffer_size: usize,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    input_buffer: Option<(U, usize)>,
    //#[pin]
    //exit_code: Box<dyn Future<Output = Result<ExitStatus, std::io::Error>>>,
    #[pin]
    exit_code: Option<X>,
}

#[derive(Debug)]
struct PSStatus {}

#[derive(Debug)]
pub enum Output {
    Stdout(Bytes),
    Stderr(Bytes),
    ExitCode(ExitStatus),
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

    pub fn unwrap_exit_code(self) -> ExitStatus {
        match self {
            Output::Stdout(_) => panic!("Output is on stdout"),
            Output::Stderr(_) => panic!("Output is on stderr"),
            Output::ExitCode(c) => c,
        }
    }
}

#[derive(Debug)]
pub struct ProcessError {}

pub fn new_process<I, U>(
    mut child: Child,
    input: I,
    output_buffer_size: usize,
) -> ProcessStream<I, impl Future<Output = Result<ExitStatus, std::io::Error>>, U> {
    ProcessStream {
        input: Some(input),
        stdin: Some(child.stdin.take().expect("Child stdin must be piped")),
        stdout: Some(child.stdout.take().expect("Child stdout must be piped")),
        stderr: Some(child.stderr.take().expect("Child stderr must be piped")),
        input_buffer: None,
        output_buffer_size,
        //exit_code: Box::new(async move { child.wait().await }),
        exit_code: Some(async move { child.wait().await }),
    }
}

pub fn new_process_typed<E, I: Stream<Item = Result<Bytes, E>>, U>(
    child: Child,
    input: I,
    output_buffer_size: usize,
) -> ProcessStream<I, impl Future<Output = Result<ExitStatus, std::io::Error>>, U> {
    new_process(child, input, output_buffer_size)
}

impl<
        E,
        U: AsRef<[u8]>,
        I: Stream<Item = Result<U, E>>,
        X: Future<Output = Result<ExitStatus, std::io::Error>>,
    > ProcessStream<I, X, U>
{
    fn next_stdout(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; *self.as_mut().project().output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.as_mut().project().stdout {
            Some(stdout) => match Pin::new(stdout).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        println!("--> stdout message");
                        Poll::Ready(Some(Ok(Output::Stdout(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        println!("--> stdout closing");
                        *self.as_mut().project().stdout = None;
                        self.next_stderr(cx)
                    }
                }
                Poll::Ready(Err(todo)) => {
                    println!("--> stdout error");
                    *self.as_mut().project().stdout = None;
                    Poll::Ready(Some(Err(ProcessError {})))
                }
                Poll::Pending => self.next_stderr(cx),
            },
            None => self.next_stderr(cx),
        }
    }

    fn next_stderr(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        let mut buf_vec = vec![0; *self.as_mut().project().output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.as_mut().project().stderr {
            Some(stderr) => match Pin::new(stderr).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        println!("--> stderr msg");
                        Poll::Ready(Some(Ok(Output::Stderr(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        println!("--> stderr closing");
                        *self.as_mut().project().stderr = None;
                        if self.as_mut().project().stdout.is_none() {
                            *self.as_mut().project().stdin = None;
                            self.as_mut().project().input.set(None);
                            self.next_exit_code(cx)
                        } else {
                            self.next_stdin(cx)
                        }
                    }
                }
                Poll::Ready(Err(todo)) => {
                    println!("--> stderr error");
                    *self.as_mut().project().stderr = None;
                    Poll::Ready(Some(Err(ProcessError {})))
                }
                Poll::Pending => self.next_stdin(cx),
            },
            None => {
                if self.as_mut().project().stdout.is_none() {
                    println!("--> stdout and stderr are closed");
                    *self.as_mut().project().stdin = None;
                    self.as_mut().project().input.set(None);
                    self.next_exit_code(cx)
                } else {
                    self.next_stdin(cx)
                }
            }
        }
    }

    fn next_stdin(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        //let status = self.as_mut().project().status;
        if let Some((v, offset)) = self.as_mut().project().input_buffer.take() {
            println!("--> Non empty buffer");
            self.as_mut().push_to_stdin(v, offset, cx);
        }
        let mut input_pending = false;
        while let (true, false, true, Some(input)) = (
            self.as_mut().project().stdin.is_some(),
            input_pending,
            self.as_mut().project().input_buffer.is_none(),
            self.as_mut().project().input.as_pin_mut(),
        ) {
            println!("--> polling input");
            match input.poll_next(cx) {
                Poll::Ready(Some(Ok(v))) => {
                    if let Some(err) = self.as_mut().push_to_stdin(v, 0, cx) {
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                Poll::Ready(Some(Err(todo))) => {
                    println!("--> input error");
                    *self.as_mut().project().stdin = None;
                    self.as_mut().project().input.set(None);
                    return Poll::Ready(Some(Err(ProcessError {})));
                }
                Poll::Ready(None) => {
                    println!("--> input ending");
                    *self.as_mut().project().stdin = None;
                    self.as_mut().project().input.set(None);
                }
                Poll::Pending => {
                    println!("--> input pending");
                    input_pending = true;
                }
            }
        }
        if self.as_mut().project().stdin.is_none() {
            println!("--> stdin is closed after polling input");
            self.as_mut().project().input.set(None);
            *self.as_mut().project().input_buffer = None;
        } else if self.as_mut().project().input.is_none() {
            println!("--> input is closed");
            if let Some((v, offset)) = self.as_mut().project().input_buffer.take() {
                // il faut vider le buffer dans stdin
                if let Some(err) = self.as_mut().push_to_stdin(v, offset, cx) {
                    return Poll::Ready(Some(Err(err)));
                }
                if self.as_mut().project().input_buffer.is_none() {
                    println!("--> close stdin after emptying buffer");
                    *self.as_mut().project().stdin = None;
                }
            } else {
                println!("--> directly close stdin because buffer is empty");
                *self.as_mut().project().stdin = None;
            }
        }
        Poll::Pending
    }

    fn next_exit_code(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Output, ProcessError>>> {
        if let Some(exit_code) = self.as_mut().project().exit_code.as_pin_mut().take() {
            match exit_code.poll(cx) {
                Poll::Ready(Ok(x)) => {
                    self.as_mut().project().exit_code.set(None);
                    Poll::Ready(Some(Ok(Output::ExitCode(x))))
                }
                Poll::Ready(Err(todo)) => {
                    self.as_mut().project().exit_code.set(None);
                    Poll::Ready(Some(Err(ProcessError {})))
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(None)
        }
    }

    fn push_to_stdin(
        mut self: Pin<&mut Self>,
        v: U,
        offset: usize,
        cx: &mut Context,
    ) -> Option<ProcessError> {
        let proj = self.as_mut().project();
        let stdin = proj.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_write(cx, &v.as_ref()[offset..]) {
            Poll::Ready(Ok(size)) => {
                if size == 0 {
                    println!("--> stdin ending");
                    *proj.stdin = None;
                } else {
                    println!("--> stdin accepting");
                    if size + offset < v.as_ref().len() {
                        *proj.input_buffer = Some((v, size + offset));
                    }
                    if let Some(err) = self.flush_stdin(cx) {
                        return Some(err);
                    }
                }
            }
            Poll::Ready(Err(todo)) => {
                *proj.stdin = None;
                return Some(ProcessError {});
            }
            Poll::Pending => {
                println!("--> stdin pending");
                *proj.input_buffer = Some((v, offset));
            }
        }
        None
    }

    fn flush_stdin(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Option<ProcessError> {
        let proj = self.project();
        let stdin = proj.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                println!("--> flush stdin OK");
            }
            Poll::Ready(Err(todo)) => {
                *proj.stdin = None;
                return Some(ProcessError {});
            }
            Poll::Pending => {
                println!("--> flush stdin pending");
            }
        }
        None
    }
}

impl<I, X, E, U> Stream for ProcessStream<I, X, U>
where
    U: AsRef<[u8]>,
    I: Stream<Item = Result<U, E>>,
    X: Future<Output = Result<ExitStatus, std::io::Error>>,
{
    type Item = Result<Output, ProcessError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        println!("poll_next");
        self.next_stdout(cx)
    }
}

impl<T: TryStream<Ok = Output>> ProcStreamExt for T {}

pub trait ProcStreamExt
where
    Self: TryStream<Ok = Output>,
{
    fn stdout(self) -> impl TryStream<Ok = Bytes, Error = <Self as TryStream>::Error>
    where
        Self: Sized,
    {
        self.try_filter_map(|o| match o {
            Output::Stdout(b) => ready(Ok(Some(b))),
            Output::Stderr(_) => ready(Ok(None)),
            Output::ExitCode(_) => ready(Ok(None)),
        })
    }

    fn stderr(self) -> impl TryStream<Ok = Bytes, Error = <Self as TryStream>::Error>
    where
        Self: Sized,
    {
        self.try_filter_map(|o| match o {
            Output::Stdout(_) => ready(Ok(None)),
            Output::Stderr(b) => ready(Ok(Some(b))),
            Output::ExitCode(_) => ready(Ok(None)),
        })
    }
}

#[cfg(test)]
mod process_stream_test {
    use crate::process::new_process;
    use std::{cell::Cell, iter, process::Stdio, rc::Rc, time::Duration};

    use bytes::Bytes;
    use futures::{
        stream::{self},
        StreamExt,
    };
    use tokio::{process::Command, time::sleep};

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
        let process_stream = new_process(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
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
        let process_stream = new_process(child, input, 1);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
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
        let process_stream = new_process(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
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
            Ok::<Bytes, String>(Bytes::from("valuert".as_bytes()))
        });
        let process_stream = new_process(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s, "");
        assert_eq!(rc.get(), 9361);
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
        let process_stream = new_process(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
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
            Ok::<Bytes, String>(Bytes::from("valuert".as_bytes()))
        });
        let process_stream = new_process(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
                    crate::process::Output::ExitCode(_) => None,
                }
            })
            .fold("".to_string(), |s, b| async move {
                println!("RES: {}", String::from_utf8_lossy(&b));
                s + &String::from_utf8_lossy(&b)
            })
            .await;
        assert_eq!(s.len(), 70000);
        let expected: String = iter::repeat("valuert")
            .map(|s| s.chars())
            .flatten()
            .take(70000)
            .collect();
        assert_eq!(s, expected);
        assert_eq!(rc.get(), 19306);
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
        let process_stream = new_process(child, input, 1024);
        let s = process_stream
            .filter_map(|r| async {
                println!("RES: {:?}", r);
                match r.unwrap() {
                    crate::process::Output::Stdout(s) => Some(s),
                    crate::process::Output::Stderr(_) => panic!("error"),
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
