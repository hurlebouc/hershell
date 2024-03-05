use bytes::Bytes;
use futures::TryStream;
use std::{ffi::OsStr, future::Future, process::ExitStatus};
use tokio::process::Command;

use crate::process::{new_process, ProcessStream};
use tokio::io::AsyncWriteExt;

pub struct Cmd(Command);

impl<T: AsRef<OsStr>> From<T> for Cmd {
    fn from(value: T) -> Self {
        let command = Command::new(value);
        Cmd(command)
    }
}

pub async fn run<C: Into<Cmd>>(cmd: C) -> std::io::Result<ExitStatus> {
    let mut command: Command = cmd.into().0;
    command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await
}

pub async fn apply<C: Into<Cmd>, U: AsRef<[u8]> + std::marker::Send + 'static>(
    cmd: C,
    input: U,
) -> std::io::Result<std::process::Output> {
    let mut command: Command = cmd.into().0;
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        match stdin.write_all(input.as_ref()).await {
            Ok(()) => {}
            Err(_) => {} // cas où le process n'a pas lu toute son entrée
        }
    });
    child.wait_with_output().await
}

pub fn stream<I: TryStream<Ok = Bytes>, C: Into<Cmd>>(
    cmd: C,
    stdin: I,
) -> std::io::Result<ProcessStream<I, impl Future<Output = Result<ExitStatus, std::io::Error>>>> {
    let mut command = cmd.into().0;
    let child = command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    Ok(new_process(child, stdin, 1024))
}
