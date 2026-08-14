use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};

pub struct PtySession {
    pub writer: Box<dyn Write + Send>,
    pub reader: Box<dyn Read + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

pub fn spawn(cmd: &str, args: &[String]) -> anyhow::Result<PtySession> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 40,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut command = CommandBuilder::new(cmd);
    for arg in args {
        command.arg(arg);
    }

    let child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;

    Ok(PtySession {
        writer,
        reader,
        child,
    })
}

/// Spawns a `PtySession` for a given command. `launch_pm`/`launch_subagent`
/// depend on this trait instead of calling `spawn` directly, so their
/// orchestration logic (build prompt, write it, wait for exit, log) can be
/// tested against `test_support::FakeSpawner` — no real subprocess or PTY —
/// instead of only being exercised by hand against a real agent CLI.
pub trait Spawner {
    fn spawn(&self, cmd: &str, args: &[String]) -> anyhow::Result<PtySession>;
}

pub struct RealSpawner;

impl Spawner for RealSpawner {
    fn spawn(&self, cmd: &str, args: &[String]) -> anyhow::Result<PtySession> {
        spawn(cmd, args)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{PtySession, Spawner};
    use portable_pty::{Child, ChildKiller, ExitStatus};
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    /// A `Child` that reports "already exited successfully" the moment it's
    /// asked, so `watch_and_log` never blocks in a test.
    #[derive(Debug)]
    pub(crate) struct FakeChild;

    impl ChildKiller for FakeChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeChild)
        }
    }

    impl Child for FakeChild {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            Ok(Some(ExitStatus::with_exit_code(0)))
        }

        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    /// A `Write` sink that stays readable after being handed off — the real
    /// `PtySession::writer` is consumed by production code with no way to
    /// inspect what was written, so tests need their own shared buffer.
    #[derive(Clone, Default)]
    pub(crate) struct SharedBuf(pub(crate) Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A `Spawner` that never touches a real process or PTY. `written`
    /// captures everything the caller writes to the session's stdin
    /// (inspect it after the call via `written.lock().unwrap()`);
    /// `reader_bytes` is served once as the session's "PTY output" and then
    /// the reader reports EOF; the child reports "already exited";
    /// `calls` records every `(cmd, args)` pair `spawn` was invoked with.
    pub(crate) struct FakeSpawner {
        pub(crate) written: Arc<Mutex<Vec<u8>>>,
        pub(crate) reader_bytes: Vec<u8>,
        pub(crate) calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeSpawner {
        pub(crate) fn new(reader_bytes: impl Into<Vec<u8>>) -> Self {
            FakeSpawner {
                written: Arc::new(Mutex::new(Vec::new())),
                reader_bytes: reader_bytes.into(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Spawner for FakeSpawner {
        fn spawn(&self, cmd: &str, args: &[String]) -> anyhow::Result<PtySession> {
            self.calls
                .lock()
                .unwrap()
                .push((cmd.to_string(), args.to_vec()));
            Ok(PtySession {
                writer: Box::new(SharedBuf(self.written.clone())),
                reader: Box::new(Cursor::new(self.reader_bytes.clone())),
                child: Box::new(FakeChild),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn spawn_echo_and_read_output() {
        let mut session =
            spawn("sh", &["-c".to_string(), "echo hello-from-pty".to_string()]).unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match session.reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
            if buf
                .windows(b"hello-from-pty".len())
                .any(|w| w == b"hello-from-pty")
            {
                break;
            }
        }
        let _ = session.child.wait();
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("hello-from-pty"), "got: {text}");
    }
}
