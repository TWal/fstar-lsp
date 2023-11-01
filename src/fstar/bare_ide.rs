use serde::Serialize;
use tokio::sync::mpsc;
use tokio::process;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use std::ffi::OsStr;
use std::process::Stdio;
use super::*;
use anyhow::Result;

use tracing::{
    trace,
    debug,
    info,
    warn,
    error,
};


#[derive(Serialize, PartialEq, Clone, Debug)]
pub struct FullQuery {
    #[serde(rename = "query-id")]
    pub query_id: String,
    #[serde(flatten)]
    pub query: Query,
}

pub
struct Sender {
    send: mpsc::UnboundedSender<FullQuery>,
}

pub
struct Receiver {
    _child: tokio::process::Child,
    recv: mpsc::UnboundedReceiver<FullResponseOrMessage>,
}

pub fn channel<S, I>(path_to_fstar_exe: S, args: I, cwd: &std::path::Path) -> (Sender, Receiver)
where
    S: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
{
    let mut child = process::Command::new(path_to_fstar_exe)
        .arg("--ide")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to spawn fstar ide");
    let fstar_stdin = child.stdin.take().unwrap();
    let fstar_stdout = child.stdout.take().unwrap();
    let (send_query, recv_query) = mpsc::unbounded_channel();
    let (send_response, recv_response) = mpsc::unbounded_channel();
    tokio::spawn(fstar_stdout_loop(send_response, fstar_stdout));
    tokio::spawn(fstar_stdin_loop(recv_query, fstar_stdin));
    (Sender {
        send: send_query,
    }, Receiver {
        _child: child,
        recv: recv_response,
    })
}

#[tracing::instrument(skip_all)]
async fn fstar_stdin_loop(mut recv_query: mpsc::UnboundedReceiver<FullQuery>, mut fstar_stdin: tokio::process::ChildStdin) -> Result<()> {
    while let Some(query) = recv_query.recv().await {
        let line = serde_json::to_string(&query)?;
        debug!("sending query {}", line);
        fstar_stdin.write_all(line.as_bytes()).await?;
        fstar_stdin.write_all(b"\n").await?;
        fstar_stdin.flush().await?;
    }
    Ok(())
}

#[tracing::instrument(skip_all)]
async fn fstar_stdout_loop(send_response: mpsc::UnboundedSender<FullResponseOrMessage>, fstar_stdout: tokio::process::ChildStdout) -> Result<()> {
    let mut fstar_stdout_lines = tokio::io::BufReader::new(fstar_stdout).lines();
    let _ = fstar_stdout_lines.next_line().await?;
    while let Some(line) = fstar_stdout_lines.next_line().await? {
        debug!("receiving response {}", line);
        match serde_json::from_str(&line) {
            Err(e) => {
                error!("Cannot parse F*'s stdout: {}", e)
            }
            Ok(response) => {
                send_response.send(response)?;
            }
        };
    }
    Ok(())
}

impl Sender {
    pub fn send(&self, value: FullQuery) -> Result<()> {
        Ok(self.send.send(value)?)
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<FullResponseOrMessage> {
        self.recv.recv().await
    }
}
