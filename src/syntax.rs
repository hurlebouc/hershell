use bytes::Bytes;
use futures::TryStream;
use std::{ffi::OsStr, future::Future, path::Path, process::ExitStatus};
use tokio::process::Command;

use crate::process::{new_process, ProcessStream};
use tokio::io::AsyncWriteExt;

pub struct Cmd(Command);

impl Cmd {
    pub fn with_envs<K, V, E>(mut self, vars: E) -> Self
    where
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.0.envs(vars);
        self
    }

    pub fn with_cwd<P>(mut self, dir: P) -> Self
    where
        P: AsRef<Path>,
    {
        self.0.current_dir(dir);
        self
    }

    pub async fn run(self) -> Result<(), ProcessRunError> {
        run(self).await
    }

    pub async fn apply<U: AsRef<[u8]> + std::marker::Send + 'static>(
        self,
        input: U,
    ) -> Result<std::process::Output, ProcessApplyError> {
        apply(self, input).await
    }

    pub fn stream<I, U, C>(
        self,
        stdin: I,
    ) -> std::io::Result<
        ProcessStream<I, impl Future<Output = Result<ExitStatus, std::io::Error>>, U>,
    >
    where
        U: AsRef<[u8]>,
        I: TryStream<Ok = Bytes>,
        C: Into<Cmd>,
    {
        stream(self, stdin)
    }
}

/// Ce trait est utilisé pour ne pas dupliquer l'implémentation `impl<T: AsRef<OsStr>> From<T> for Cmd`, tout en limitant
/// la porté de cette implémentation aux types spécifiquement marqués par `CmdDesc`. Ce contournent permet de définir d'autre implémentations
/// qui ne sont pas couvertes par `AsRef`
trait CmdDesc {}
impl CmdDesc for String {}
impl CmdDesc for &str {}
impl CmdDesc for &OsStr {}

impl<T: AsRef<OsStr> + CmdDesc> From<T> for Cmd {
    fn from(prog: T) -> Self {
        let command = Command::new(prog);
        Cmd(command)
    }
}

impl From<Command> for Cmd {
    fn from(value: Command) -> Self {
        Cmd(value)
    }
}

impl<A, AS, T> From<(T, AS)> for Cmd
where
    A: AsRef<OsStr>,
    AS: IntoIterator<Item = A>,
    T: AsRef<OsStr>,
{
    fn from((prog, args): (T, AS)) -> Self {
        let mut command = Command::new(prog);
        command.args(args);
        Cmd(command)
    }
}

impl<A, AS, T, K, E, V, P> From<(E, P, T, AS)> for Cmd
where
    A: AsRef<OsStr>,
    AS: IntoIterator<Item = A>,
    T: AsRef<OsStr>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
    P: AsRef<Path>,
    E: IntoIterator<Item = (K, V)>,
{
    fn from((env, cwd, prog, args): (E, P, T, AS)) -> Self {
        let mut command = Command::new(prog);
        command.args(args).current_dir(cwd).envs(env);
        Cmd(command)
    }
}

#[derive(Debug)]
pub enum ProcessRunError {
    IoError(std::io::Error),
    BadExitStatus(ExitStatus),
}

impl From<std::io::Error> for ProcessRunError {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value)
    }
}

impl From<ProcessApplyError> for ProcessRunError {
    fn from(value: ProcessApplyError) -> Self {
        match value {
            ProcessApplyError::IoError(io) => ProcessRunError::IoError(io),
            ProcessApplyError::BadExitStatus(output) => {
                ProcessRunError::BadExitStatus(output.status)
            }
        }
    }
}

pub async fn run<C: Into<Cmd>>(cmd: C) -> Result<(), ProcessRunError> {
    let mut command: Command = cmd.into().0;
    let exit_status = command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .await?;
    if !exit_status.success() {
        return Err(ProcessRunError::BadExitStatus(exit_status));
    }
    return Ok(());
}

#[derive(Debug)]
pub enum ProcessApplyError {
    IoError(std::io::Error),
    BadExitStatus(std::process::Output),
}

impl From<std::io::Error> for ProcessApplyError {
    fn from(value: std::io::Error) -> Self {
        Self::IoError(value)
    }
}

impl From<ProcessRunError> for ProcessApplyError {
    fn from(value: ProcessRunError) -> Self {
        match value {
            ProcessRunError::IoError(io) => ProcessApplyError::IoError(io),
            ProcessRunError::BadExitStatus(code) => {
                let output = std::process::Output {
                    status: code,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                };
                ProcessApplyError::BadExitStatus(output)
            }
        }
    }
}

pub async fn apply<C: Into<Cmd>, U: AsRef<[u8]> + std::marker::Send + 'static>(
    cmd: C,
    input: U,
) -> Result<std::process::Output, ProcessApplyError> {
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

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        return Err(ProcessApplyError::BadExitStatus(output));
    }

    return Ok(output);
}

pub fn stream<I, U, C>(
    cmd: C,
    stdin: I,
) -> std::io::Result<ProcessStream<I, impl Future<Output = Result<ExitStatus, std::io::Error>>, U>>
where
    U: AsRef<[u8]>,
    I: TryStream<Ok = Bytes>,
    C: Into<Cmd>,
{
    let mut command = cmd.into().0;
    let child = command
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    Ok(new_process(child, stdin, 1024))
}
