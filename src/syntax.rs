use bytes::Bytes;
use futures::TryStream;
use std::{ffi::OsStr, future::Future, process::ExitStatus};
use tokio::process::Command;

use crate::process::{new_process, ProcessStream};

pub struct Cmd(Command);

impl<T: AsRef<OsStr>> From<T> for Cmd {
    fn from(value: T) -> Self {
        let command = Command::new(value);
        Cmd(command)
    }
}

pub fn run(cmd: Cmd) {}
pub fn apply(cmd: Cmd, stdin: &[u8]) {}
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
