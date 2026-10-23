//! A shell on this computer, in a pseudo-terminal. It speaks the same
//! [`Event`]s and input as an SSH session, so a pane shows either.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;

use crate::forward::ForwardState;
use crate::ssh::{ConnectError, Event, Input, TermSize};

/// The shell a local terminal runs: PowerShell 7 when it's installed, else
/// Windows PowerShell; elsewhere `$SHELL`, else `/bin/sh`.
pub fn default_shell() -> String {
    if cfg!(windows) {
        let on_path = |program: &str| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
            })
        };
        if on_path("pwsh.exe") {
            "pwsh.exe".into()
        } else {
            "powershell.exe".into()
        }
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

/// What to call a local shell, given the title it set: a Windows console
/// starts out titled with its program's full path, which reads better as
/// the program's name. Other titles stay as they are.
pub fn title(title: &str) -> String {
    let name = title.rsplit(['/', '\\']).next().unwrap_or(title);
    let program = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"));
    match program {
        Some(program) if !program.is_empty() && name.len() < title.len() => program.to_string(),
        _ => title.to_string(),
    }
}

fn pty_size(size: TermSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Runs the shell until it exits or its input closes, reporting to
/// `events`. Blocks; run it on its own thread.
pub(crate) fn run(
    size: TermSize,
    mut input: mpsc::UnboundedReceiver<Input>,
    events: mpsc::UnboundedSender<Event>,
) {
    let failed = |message: String| {
        let _ = events.send(Event::Failed(ConnectError::Other(message)));
    };
    let pair = match native_pty_system().openpty(pty_size(size)) {
        Ok(pair) => pair,
        Err(error) => return failed(format!("can't open a terminal: {error}")),
    };
    let shell = default_shell();
    let mut command = CommandBuilder::new(&shell);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    if let Some(home) = std::env::home_dir() {
        command.cwd(home);
    }
    let mut child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(error) => return failed(format!("can't start {shell}: {error}")),
    };
    drop(pair.slave);
    let (mut reader, mut writer) = match (pair.master.try_clone_reader(), pair.master.take_writer())
    {
        (Ok(reader), Ok(writer)) => (reader, writer),
        (Err(error), _) | (_, Err(error)) => {
            let _ = child.kill();
            return failed(format!("can't open a terminal: {error}"));
        }
    };
    let master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>> =
        Arc::new(Mutex::new(Some(pair.master)));
    let _ = events.send(Event::Connected);

    let output = events.clone();
    let pump = std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || output.send(Event::Output(buffer[..read].to_vec())).is_err() {
                break;
            }
        }
    });

    let mut killer = child.clone_killer();
    let resizer = master.clone();
    let replies = events.clone();
    std::thread::spawn(move || {
        while let Some(command) = input.blocking_recv() {
            match command {
                Input::Data(bytes) => {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                Input::Resize(size) => {
                    if let Some(master) = resizer.lock().unwrap().as_ref() {
                        let _ = master.resize(pty_size(size));
                    }
                }
                Input::Forward(forward, _) => {
                    let _ = replies.send(Event::Forwarding {
                        forward,
                        state: ForwardState::Failed("not on a local terminal".into()),
                    });
                }
                Input::OpenSftp(reply) => {
                    let _ = reply.send(Err("not on a local terminal".into()));
                }
                Input::Answers(_) => {}
                Input::Close => break,
            }
        }
        // The pane closed or asked to: end the shell.
        let _ = killer.kill();
    });

    let exit_status = child.wait().ok().map(|status| status.exit_code());
    // Windows' ConPTY keeps the output open until the terminal itself is
    // closed, so the pump would wait forever after the shell exits.
    master.lock().unwrap().take();
    let _ = pump.join();
    let _ = events.send(Event::Closed { exit_status });
}

#[cfg(test)]
mod tests {
    use super::title;

    #[test]
    fn names_a_console_by_its_program() {
        assert_eq!(
            title(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe"),
            "powershell"
        );
        assert_eq!(title(r"Administrator: C:\Windows\system32\cmd.exe"), "cmd");
        assert_eq!(title("~/src: vim"), "~/src: vim");
        assert_eq!(title("pwsh.exe"), "pwsh.exe");
    }
}
