#[cfg(feature = "actix")]
pub mod actix;

use dashmap::{DashMap, Entry};
#[cfg(unix)]
use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    process::{ExitStatus, Stdio},
    sync::Arc,
};
use tokio::{
    process::{ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Mutex, OwnedMutexGuard},
};
use uuid::Uuid;

#[derive(Default)]
pub struct Kindergarten {
    access: DashMap<Ticket, MaybeChild>,
}

#[derive(Debug, Clone)]
pub enum MaybeChild {
    Kind(Kind),
    Tombstone(ExitStatus),
}

impl MaybeChild {
    fn child(&self) -> Option<&Kind> {
        match self {
            MaybeChild::Kind(kind) => Some(kind),
            MaybeChild::Tombstone(_) => None,
        }
    }

    fn into_child(self) -> Option<Kind> {
        match self {
            MaybeChild::Kind(kind) => Some(kind),
            MaybeChild::Tombstone(_) => None,
        }
    }
}

impl From<Kind> for MaybeChild {
    fn from(value: Kind) -> Self {
        Self::Kind(value)
    }
}

#[derive(Debug, Clone)]
pub struct Kind {
    inner: KindInner,
}

#[derive(Debug, Clone)]
struct KindInner {
    inner: Arc<Mutex<tokio::process::Child>>,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Arc<Mutex<ChildStdout>>,
    stderr: Arc<Mutex<ChildStderr>>,
}

impl Kind {
    fn new(mut cmd: Command) -> std::io::Result<Self> {
        let mut inner = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = inner.stdin.take().unwrap();
        let stdout = inner.stdout.take().unwrap();
        let stderr = inner.stderr.take().unwrap();

        Ok(Self {
            inner: KindInner {
                inner: Arc::new(Mutex::new(inner)),
                stdin: Arc::new(Mutex::new(stdin)),
                stdout: Arc::new(Mutex::new(stdout)),
                stderr: Arc::new(Mutex::new(stderr)),
            },
        })
    }

    pub async fn stdin(&self) -> OwnedMutexGuard<ChildStdin> {
        self.inner.stdin.clone().lock_owned().await
    }

    pub async fn stdout(&self) -> OwnedMutexGuard<ChildStdout> {
        self.inner.stdout.clone().lock_owned().await
    }

    pub async fn stderr(&self) -> OwnedMutexGuard<ChildStderr> {
        self.inner.stderr.clone().lock_owned().await
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Ticket(Uuid);

impl<'de> Deserialize<'de> for Ticket {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Uuid::deserialize(deserializer).map(Ticket)
    }
}

impl Serialize for Ticket {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl Kindergarten {
    pub async fn spawn(&self, cmd: Command) -> Result<Ticket, std::io::Error> {
        let ticket = Ticket(Uuid::new_v4());
        self.access.insert(ticket, Kind::new(cmd)?.into());
        Ok(ticket)
    }

    /// Gets a handle to a child instance, streams must be locked separately to use
    ///
    /// Directly using the getters for streams is slightly more efficient
    pub fn get(&self, t: Ticket) -> Option<Kind> {
        self.access.get(&t).and_then(|k| k.child().cloned())
    }

    /// Removes a child handle from the garden.
    ///
    /// Dropping the returned [`Kind`] closes any still-owned stdio pipe handles.
    pub fn remove(&self, t: Ticket) -> Option<Kind> {
        self.access
            .remove(&t)
            .and_then(|(_, kind)| kind.into_child())
    }

    /// Gets a handle to a child instance, streams must be locked separately to use
    ///
    /// Directly using the getters for streams is slightly more efficient
    pub fn get_or_insert_with(
        &self,
        t: Ticket,
        f: impl Fn() -> Command,
    ) -> std::io::Result<MaybeChild> {
        Ok(match self.access.entry(t) {
            Entry::Occupied(occupied_entry) => occupied_entry.get().clone(),
            Entry::Vacant(vacant_entry) => vacant_entry
                .insert(Kind::new((f)())?.into())
                .value()
                .clone(),
        })
    }

    pub async fn stdin(&self, t: Ticket) -> Option<OwnedMutexGuard<ChildStdin>> {
        match self.access.get(&t) {
            Some(child) => Some(child.child()?.inner.stdin.clone().lock_owned().await),
            None => None,
        }
    }

    pub async fn stdout(&self, t: Ticket) -> Option<OwnedMutexGuard<ChildStdout>> {
        match self.access.get(&t) {
            Some(child) => Some(child.child()?.inner.stdout.clone().lock_owned().await),
            None => None,
        }
    }

    pub async fn stderr(&self, t: Ticket) -> Option<OwnedMutexGuard<ChildStderr>> {
        match self.access.get(&t) {
            Some(child) => Some(child.child()?.inner.stderr.clone().lock_owned().await),
            None => None,
        }
    }

    pub async fn has_terminated(&self, t: Ticket) -> Option<bool> {
        let kind = self.get(t)?;
        let has_terminated = kind
            .inner
            .inner
            .lock()
            .await
            .try_wait()
            .map(|x| x.is_some())
            .unwrap_or_default();

        if has_terminated {
            self.remove(t);
        }

        Some(has_terminated)
    }

    pub async fn success(&self, t: Ticket) -> Option<std::io::Result<bool>> {
        let kind = self.get(t)?;
        let success = kind
            .inner
            .inner
            .lock()
            .await
            .try_wait()
            .map(|x| x.is_some_and(|s| s.success()));

        if success.is_ok() {
            self.remove(t);
        }

        Some(success)
    }

    #[cfg(unix)]
    pub async fn pid(&self, t: Ticket) -> Option<Pid> {
        self.access
            .get(&t)?
            .child()?
            .inner
            .inner
            .lock()
            .await
            .id()
            .and_then(|pid| pid.try_into().ok())
            .map(Pid::from_raw)
    }

    #[cfg(unix)]
    /// Send SIGTERM and wait for the process to terminate
    pub async fn terminate(&self, t: Ticket) -> Option<std::io::Result<ExitStatus>> {
        self.send_signal_and_wait(t, Signal::SIGTERM).await
    }

    #[cfg(unix)]
    /// Send SIGKILL and wait for the process to terminate
    pub async fn kill(&self, t: Ticket) -> Option<std::io::Result<ExitStatus>> {
        self.send_signal_and_wait(t, Signal::SIGKILL).await
    }

    #[cfg(unix)]
    /// Send SIGINT and wait for the process to terminate
    pub async fn interrupt(&self, t: Ticket) -> Option<std::io::Result<ExitStatus>> {
        self.send_signal_and_wait(t, Signal::SIGINT).await
    }

    #[cfg(unix)]
    /// Send a signal and wait for the process to terminate
    pub async fn send_signal_and_wait(
        &self,
        t: Ticket,
        sig: Signal,
    ) -> Option<std::io::Result<ExitStatus>> {
        self.send_signal(t, sig).await;
        self.wait(t).await
    }

    #[cfg(unix)]
    /// Send an arbitrarry unix signal
    pub async fn send_signal(&self, t: Ticket, sig: Signal) -> Option<std::io::Result<()>> {
        let result = signal::kill(self.pid(t).await?, sig)
            .map_err(|erno| std::io::Error::from_raw_os_error(erno as i32));

        match result {
            Err(err) => return Some(Err(err)),
            Ok(_) => Some(Ok(())),
        }
    }

    pub async fn wait(&self, t: Ticket) -> Option<std::io::Result<ExitStatus>> {
        let mut slot = self.access.get_mut(&t)?;
        let status = match &*slot {
            MaybeChild::Kind(kind) => {
                let status = kind.inner.inner.lock().await.wait().await;
                if let Ok(status) = status {
                    *slot = MaybeChild::Tombstone(status);
                }
                status
            }
            MaybeChild::Tombstone(exit_status) => Ok(*exit_status),
        };

        Some(status)
    }
}
