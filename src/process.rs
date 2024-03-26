use std::{
    future::ready,
    ops::Deref,
    pin::Pin,
    process::ExitStatus,
    task::{ready, Context, Poll},
};

use bytes::Bytes;
use futures::{Future, FutureExt, Stream, TryFutureExt, TryStream, TryStreamExt};
use pin_project::pin_project;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout},
};

#[cfg(debug_assertions)]
macro_rules! trace {
    ($x:expr) => {
        println!("{}", $x)
    };
}

#[cfg(not(debug_assertions))]
macro_rules! trace {
    ($x:expr) => {};
}

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
pub enum ProcessStreamError<E> {
    Stdout(std::io::Error),
    Stderr(std::io::Error),
    Stdin(std::io::Error),
    Exit(std::io::Error),
    Input(E),
}

impl<E: Into<std::io::Error>> From<ProcessStreamError<E>> for std::io::Error {
    fn from(value: ProcessStreamError<E>) -> Self {
        match value {
            ProcessStreamError::Stdout(io) => io,
            ProcessStreamError::Stderr(io) => io,
            ProcessStreamError::Stdin(io) => io,
            ProcessStreamError::Exit(io) => io,
            ProcessStreamError::Input(io) => io.into(),
        }
    }
}

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
        //exit_code: Some(async move { child.wait().await }),
        exit_code: Some(Box::pin(async move { child.wait().await })),
        //exit_code: Some(ExitCode(child)),
    }
}

pub fn new_process_typed<E, I, U>(
    child: Child,
    input: I,
    output_buffer_size: usize,
) -> ProcessStream<I, impl Future<Output = Result<ExitStatus, std::io::Error>>, U>
where
    I: Stream<Item = Result<U, E>>,
{
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
    ) -> Poll<Option<Result<Output, ProcessStreamError<E>>>> {
        let mut buf_vec = vec![0; *self.as_mut().project().output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.as_mut().project().stdout {
            Some(stdout) => match Pin::new(stdout).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        let read_bytes = readbuf.filled().to_vec();
                        trace!(format!(
                            "--> stdout message: {}",
                            String::from_utf8_lossy(&read_bytes)
                        ));
                        Poll::Ready(Some(Ok(Output::Stdout(Bytes::from(read_bytes)))))
                    } else {
                        trace!(format!("--> stdout closing"));
                        *self.as_mut().project().stdout = None;
                        self.next_stderr(cx)
                    }
                }
                Poll::Ready(Err(err)) => {
                    trace!(format!("--> stdout error"));
                    *self.as_mut().project().stdout = None;
                    Poll::Ready(Some(Err(ProcessStreamError::Stdout(err))))
                }
                Poll::Pending => self.next_stderr(cx),
            },
            None => self.next_stderr(cx),
        }
    }

    fn next_stderr(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Output, ProcessStreamError<E>>>> {
        let mut buf_vec = vec![0; *self.as_mut().project().output_buffer_size];
        let mut readbuf = ReadBuf::new(&mut buf_vec);
        match &mut self.as_mut().project().stderr {
            Some(stderr) => match Pin::new(stderr).poll_read(cx, &mut readbuf) {
                Poll::Ready(Ok(())) => {
                    if readbuf.filled().len() != 0 {
                        trace!(format!("--> stderr msg"));
                        Poll::Ready(Some(Ok(Output::Stderr(Bytes::from(
                            readbuf.filled().to_vec(),
                        )))))
                    } else {
                        trace!(format!("--> stderr closing"));
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
                Poll::Ready(Err(err)) => {
                    trace!(format!("--> stderr error"));
                    *self.as_mut().project().stderr = None;
                    Poll::Ready(Some(Err(ProcessStreamError::Stderr(err))))
                }
                Poll::Pending => self.next_stdin(cx),
            },
            None => {
                if self.as_mut().project().stdout.is_none() {
                    trace!(format!("--> stdout and stderr are closed"));
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
    ) -> Poll<Option<Result<Output, ProcessStreamError<E>>>> {
        //let status = self.as_mut().project().status;
        if let Some((v, offset)) = self.as_mut().project().input_buffer.take() {
            trace!(format!("--> Non empty buffer"));
            self.as_mut().push_to_stdin(v, offset, cx);
        }
        let mut input_pending = false;
        while let (true, false, true, Some(input)) = (
            self.as_mut().project().stdin.is_some(),
            input_pending,
            self.as_mut().project().input_buffer.is_none(),
            self.as_mut().project().input.as_pin_mut(),
        ) {
            trace!(format!("--> polling input"));
            match input.poll_next(cx) {
                Poll::Ready(Some(Ok(v))) => {
                    if let Some(err) = self.as_mut().push_to_stdin(v, 0, cx) {
                        return Poll::Ready(Some(Err(err)));
                    }
                }
                Poll::Ready(Some(Err(err))) => {
                    trace!(format!("--> input error"));
                    *self.as_mut().project().stdin = None;
                    self.as_mut().project().input.set(None);
                    return Poll::Ready(Some(Err(ProcessStreamError::Input(err))));
                }
                Poll::Ready(None) => {
                    trace!(format!("--> input ending"));
                    *self.as_mut().project().stdin = None;
                    self.as_mut().project().input.set(None);
                }
                Poll::Pending => {
                    trace!(format!("--> input pending"));
                    input_pending = true;
                }
            }
        }
        if self.as_mut().project().stdin.is_none() {
            trace!(format!("--> stdin is closed after polling input"));
            self.as_mut().project().input.set(None);
            *self.as_mut().project().input_buffer = None;
        } else if self.as_mut().project().input.is_none() {
            trace!(format!("--> input is closed"));
            if let Some((v, offset)) = self.as_mut().project().input_buffer.take() {
                // il faut vider le buffer dans stdin
                if let Some(err) = self.as_mut().push_to_stdin(v, offset, cx) {
                    return Poll::Ready(Some(Err(err)));
                }
                if self.as_mut().project().input_buffer.is_none() {
                    trace!(format!("--> close stdin after emptying buffer"));
                    *self.as_mut().project().stdin = None;
                }
            } else {
                trace!(format!("--> directly close stdin because buffer is empty"));
                *self.as_mut().project().stdin = None;
            }
        }
        Poll::Pending
    }

    fn next_exit_code(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Output, ProcessStreamError<E>>>> {
        if let Some(exit_code) = self.as_mut().project().exit_code.as_pin_mut().take() {
            trace!(format!("--> start waiting for exit code"));
            match exit_code.poll(cx) {
                Poll::Ready(Ok(x)) => {
                    trace!(format!("--> exit code is {}", x));
                    self.as_mut().project().exit_code.set(None);
                    Poll::Ready(Some(Ok(Output::ExitCode(x))))
                }
                Poll::Ready(Err(err)) => {
                    trace!(format!("--> error waiting for exit code: {}", err));
                    self.as_mut().project().exit_code.set(None);
                    Poll::Ready(Some(Err(ProcessStreamError::Exit(err))))
                }
                Poll::Pending => {
                    trace!(format!("--> waiting for exit code"));
                    Poll::Pending
                }
            }
        } else {
            trace!(format!("--> exit code already given, close stream"));
            Poll::Ready(None)
        }
    }

    fn push_to_stdin(
        mut self: Pin<&mut Self>,
        v: U,
        offset: usize,
        cx: &mut Context,
    ) -> Option<ProcessStreamError<E>> {
        let proj = self.as_mut().project();
        let stdin = proj.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_write(cx, &v.as_ref()[offset..]) {
            Poll::Ready(Ok(size)) => {
                if size == 0 {
                    trace!(format!("--> stdin ending"));
                    *proj.stdin = None;
                } else {
                    trace!(format!("--> stdin accepting"));
                    if size + offset < v.as_ref().len() {
                        *proj.input_buffer = Some((v, size + offset));
                    }
                    if let Some(err) = self.flush_stdin(cx) {
                        return Some(err);
                    }
                }
            }
            Poll::Ready(Err(err)) => {
                *proj.stdin = None;
                return Some(ProcessStreamError::Stdin(err));
            }
            Poll::Pending => {
                trace!(format!("--> stdin pending"));
                *proj.input_buffer = Some((v, offset));
            }
        }
        None
    }

    fn flush_stdin(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Option<ProcessStreamError<E>> {
        let proj = self.project();
        let stdin = proj.stdin.as_mut().unwrap();
        match Pin::new(stdin).poll_flush(cx) {
            Poll::Ready(Ok(())) => {
                trace!(format!("--> flush stdin OK"));
            }
            Poll::Ready(Err(err)) => {
                *proj.stdin = None;
                return Some(ProcessStreamError::Stdin(err));
            }
            Poll::Pending => {
                trace!(format!("--> flush stdin pending"));
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
    type Item = Result<Output, ProcessStreamError<E>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        trace!(format!("poll_next"));
        self.next_stdout(cx)
    }
}

impl<T: TryStream<Ok = Output>> ProcStreamExt for T {}

pub trait ProcStreamExt
where
    Self: TryStream<Ok = Output>,
{
    fn stdout(self) -> impl Stream<Item = Result<Bytes, <Self as TryStream>::Error>>
    where
        Self: Sized,
    {
        self.try_filter_map(|o| match o {
            Output::Stdout(b) => ready(Ok(Some(b))),
            Output::Stderr(_) => ready(Ok(None)),
            Output::ExitCode(_) => ready(Ok(None)),
        })
    }

    fn stderr(self) -> impl Stream<Item = Result<Bytes, <Self as TryStream>::Error>>
    where
        Self: Sized,
    {
        self.try_filter_map(|o| match o {
            Output::Stdout(_) => ready(Ok(None)),
            Output::Stderr(b) => ready(Ok(Some(b))),
            Output::ExitCode(_) => ready(Ok(None)),
        })
    }

    fn foreach_err<FN, FUT, E>(self, f: FN) -> ForEachErr<Self, FN, FUT>
    where
        Self: Sized,
        FN: FnMut(Bytes) -> FUT,
        FUT: Future<Output = Result<(), E>>,
    {
        ForEachErr {
            stream: self,
            f,
            fut: None,
        }
    }
}

#[pin_project]
pub struct ForEachErr<S, FN, FUT> {
    #[pin]
    stream: S,
    #[pin]
    fut: Option<FUT>,
    f: FN,
}

impl<S, FN, FUT, E> Stream for ForEachErr<S, FN, FUT>
where
    S: Stream<Item = Result<Output, E>>,
    FN: FnMut(Bytes) -> FUT,
    FUT: Future<Output = Result<(), E>>,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        trace!(format!("ForEachErr --> poll_next"));
        match self.as_mut().project().stream.poll_next(cx) {
            Poll::Ready(None) => {
                trace!(format!("ForEachErr --> Done"));
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(Output::Stdout(b)))) => {
                trace!(format!(
                    "ForEachErr --> stdout : {}",
                    String::from_utf8_lossy(&b)
                ));
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Ok(Output::Stderr(b)))) => {
                if self.as_mut().project().fut.is_none() {
                    let fut = (self.as_mut().project().f)(b);
                    self.as_mut().project().fut.set(Some(fut));
                }

                let fut = self
                    .as_mut()
                    .project()
                    .fut
                    .as_pin_mut()
                    .expect("This future has just been provided");
                let ready_fut = match fut.poll(cx) {
                    Poll::Ready(t) => t,
                    Poll::Pending => {
                        trace!(format!("ForEachErr --> future not ready"));
                        return Poll::Pending;
                    }
                };
                self.as_mut().project().fut.set(None);
                match ready_fut {
                    Ok(()) => {
                        trace!(format!("ForEachErr --> future done"));
                        Poll::Pending
                    }
                    Err(e) => {
                        trace!(format!("ForEachErr --> future error"));
                        Poll::Ready(Some(Err(e)))
                    }
                }
            }
            Poll::Ready(Some(Ok(Output::ExitCode(_)))) => {
                trace!(format!("ForEachErr --> exit code"));
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(err))) => {
                trace!(format!("ForEachErr --> error"));
                Poll::Ready(Some(Err(err)))
            }
            Poll::Pending => {
                trace!(format!("ForEachErr --> Pending"));
                Poll::Pending
            }
        }
    }
}

#[cfg(test)]
mod process_stream_test {
    use crate::process::new_process;
    use crate::process::ProcStreamExt;
    use std::future::ready;
    use std::{cell::Cell, iter, process::Stdio, rc::Rc, time::Duration};

    use bytes::Bytes;
    use futures::FutureExt;
    use futures::TryStreamExt;
    use futures::{
        stream::{self, repeat_with},
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

    #[tokio::test]
    async fn filter_stderr_out_test() {
        let mut n = 0;
        let stream = repeat_with(|| {
            n = n + 1;
            if n % 2 == 0 {
                Ok::<crate::process::Output, String>(crate::process::Output::Stdout(
                    Bytes::from_static("Ok".as_bytes()),
                ))
            } else {
                Ok(crate::process::Output::Stderr(Bytes::from_static(
                    "Err".as_bytes(),
                )))
            }
        })
        .take(10);
        let out_stream = stream.foreach_err(|b| {
            println!("enter foreach function");
            sleep(Duration::from_millis(2000))
                .then(move |()| ready(Ok(println!("STDERR : {}", String::from_utf8_lossy(&b)))))
        });
        let out = out_stream
            .try_fold("".to_string(), |s, b| async move {
                println!("STDOUT: {}", String::from_utf8_lossy(&b));
                Ok(s + &String::from_utf8_lossy(&b))
            })
            .await;
        assert_eq!(out, Ok("".to_string()));
    }
}
