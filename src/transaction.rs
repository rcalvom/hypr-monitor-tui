use crate::{
    backend,
    model::{Layout, same_state, validate},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub before: Layout,
    pub draft: Layout,
}

pub struct Transaction {
    pub child: Child,
    input: ChildStdin,
    pub messages: Receiver<String>,
}

impl Transaction {
    pub fn start(before: Layout, draft: Layout) -> Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--guard")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The guard must not receive the terminal's Ctrl-C/SIGHUP with the TUI.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let mut input = child.stdin.take().unwrap();
        let (sender, messages) = mpsc::channel();
        let output = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        writeln!(
            input,
            "{}",
            serde_json::to_string(&Request { before, draft })?
        )?;
        Ok(Self {
            child,
            input,
            messages,
        })
    }
    pub fn send(&mut self, command: &str) -> Result<()> {
        writeln!(self.input, "{command}")?;
        self.input.flush()?;
        Ok(())
    }
}

trait Backend {
    fn query(&mut self) -> Result<Layout>;
    fn apply(&mut self, draft: &[crate::model::Monitor]) -> Result<()>;
    fn restore(&mut self, before: &[crate::model::Monitor]) -> Result<()>;
}
struct Real;
impl Backend for Real {
    fn query(&mut self) -> Result<Layout> {
        backend::query()
    }
    fn apply(&mut self, d: &[crate::model::Monitor]) -> Result<()> {
        backend::apply(d)
    }
    fn restore(&mut self, b: &[crate::model::Monitor]) -> Result<()> {
        backend::restore(b)
    }
}

fn run<B: Backend>(
    backend: &mut B,
    request: Request,
    commands: Receiver<String>,
    timeout: Duration,
    mut report: impl FnMut(&str),
) -> Result<()> {
    validate(&request.draft)?;
    ensure!(
        same_state(&request.before, &backend.query()?),
        "Layout changed externally; refresh before applying"
    );
    let result = (|| -> Result<()> {
        backend.apply(&request.draft)?;
        report("READY");
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match commands.recv_timeout(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            ) {
                Ok(s) if s == "CONFIRM" => {
                    ensure!(
                        same_state(&request.draft, &backend.query()?),
                        "State changed before confirmation"
                    );
                    report("CONFIRMED");
                    return Ok(());
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    ensure!(
                        same_state(&request.draft, &backend.query()?),
                        "Output disconnected or layout changed during preview"
                    );
                }
            }
        }
        backend.restore(&request.before)?;
        report("REVERTED");
        Ok(())
    })();
    if let Err(error) = result {
        match backend.restore(&request.before) {
            Ok(()) => anyhow::bail!("{error}; previous layout restored"),
            Err(recovery) => anyhow::bail!("{error}; recovery: {recovery}"),
        }
    }
    Ok(())
}

pub fn guard() -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is required for protected apply locking")?;
    let directory = std::path::Path::new(&runtime).join("hypr-monitor-tui");
    if !directory.exists() {
        fs::DirBuilder::new().create(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "Unsafe runtime directory"
    );
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("apply.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another apply transaction is active"
    );
    backend::check_version()?;
    let mut reader = BufReader::new(std::io::stdin());
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let request: Request = serde_json::from_str(&first)?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    run(&mut Real, request, rx, Duration::from_secs(15), |message| {
        let _ = writeln!(std::io::stdout(), "{message}");
        let _ = std::io::stdout().flush();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        current: Layout,
        restored: bool,
        fail: bool,
    }
    impl Backend for Fake {
        fn query(&mut self) -> Result<Layout> {
            Ok(self.current.clone())
        }
        fn apply(&mut self, d: &[crate::model::Monitor]) -> Result<()> {
            self.current = d.to_vec();
            ensure!(!self.fail, "Partial failure");
            Ok(())
        }
        fn restore(&mut self, b: &[crate::model::Monitor]) -> Result<()> {
            self.current = b.to_vec();
            self.restored = true;
            Ok(())
        }
    }
    fn setup() -> (Fake, Request) {
        let before=backend::parse(r#"[{"name":"A","width":1920,"height":1080,"refreshRate":60,"availableModes":["1920x1080@60Hz"]}]"#).unwrap();
        let mut draft = before.clone();
        draft[0].x = 100;
        (
            Fake {
                current: before.clone(),
                restored: false,
                fail: false,
            },
            Request { before, draft },
        )
    }
    #[test]
    fn timeout_restores() {
        let (mut b, r) = setup();
        let (_tx, rx) = mpsc::channel();
        run(&mut b, r, rx, Duration::ZERO, |_| {}).unwrap();
        assert!(b.restored);
    }
    #[test]
    fn parent_death_restores() {
        let (mut b, r) = setup();
        let (tx, rx) = mpsc::channel();
        drop(tx);
        run(&mut b, r, rx, Duration::from_secs(15), |_| {}).unwrap();
        assert!(b.restored);
    }
    #[test]
    fn partial_apply_restores() {
        let (mut b, r) = setup();
        b.fail = true;
        let (_tx, rx) = mpsc::channel();
        assert!(run(&mut b, r, rx, Duration::ZERO, |_| {}).is_err());
        assert!(b.restored);
    }
    #[test]
    fn confirm_keeps_layout() {
        let (mut b, r) = setup();
        let expected = r.draft.clone();
        let (tx, rx) = mpsc::channel();
        tx.send("CONFIRM".into()).unwrap();
        run(&mut b, r, rx, Duration::from_secs(1), |_| {}).unwrap();
        assert!(!b.restored);
        assert!(same_state(&b.current, &expected));
    }
    #[test]
    fn stale_state_does_not_apply() {
        let (mut b, r) = setup();
        b.current[0].x = 500;
        let (_tx, rx) = mpsc::channel();
        assert!(run(&mut b, r, rx, Duration::ZERO, |_| {}).is_err());
        assert_eq!(b.current[0].x, 500);
        assert!(!b.restored);
    }
}
