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
