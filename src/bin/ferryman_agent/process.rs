//! One owner for an Orinfer process and all of its descendant workers.
use rustix::process::{kill_process_group, Pid, Signal};
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

pub(super) struct ModelProcess {
    child: Child,
    pid: u32,
    group: Option<Pid>,
}

impl ModelProcess {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("model process has no PID"))?;
        let group =
            Pid::from_raw(pid as i32).ok_or_else(|| io::Error::other("invalid model PID"))?;
        Ok(Self {
            child,
            pid,
            group: Some(group),
        })
    }

    pub fn id(&self) -> u32 {
        self.pid
    }
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub fn stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reap_group();
        }
        Ok(status)
    }

    pub async fn stop(mut self, force: bool) -> io::Result<()> {
        self.signal(if force { Signal::KILL } else { Signal::TERM });
        let status = match tokio::time::timeout(Duration::from_secs(20), self.child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                self.signal(Signal::KILL);
                self.child.wait().await
            }
        };
        self.reap_group();
        status.map(|_| ())
    }

    fn signal(&self, signal: Signal) {
        if let Some(group) = self.group {
            let _ = kill_process_group(group, signal);
        }
    }

    fn reap_group(&mut self) {
        if let Some(group) = self.group.take() {
            let _ = kill_process_group(group, Signal::KILL);
        }
    }
}

impl Drop for ModelProcess {
    fn drop(&mut self) {
        self.reap_group();
    }
}
