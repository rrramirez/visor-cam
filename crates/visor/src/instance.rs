//! One Visor at a time. The first instance holds a lock file and listens on a
//! loopback port; launching Visor again hands its request (show the recorder
//! panel, open a recording) to that instance and exits. Agents control the
//! running Visor through the same port (see [`control`]).
//!
//! The port and a random token live in a file in the user's settings folder,
//! so only the same user's processes can make requests. If anything here
//! fails, Visor just starts normally.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

/// What a launch asks the running Visor to do.
#[derive(Debug, Clone)]
pub enum Request {
    ShowPanel,
    Edit(PathBuf),
    /// An agent command (see `app::agent`), answered through the sender.
    Control(Value, crossbeam_channel::Sender<Value>),
}

pub enum Instance {
    /// This is the only Visor running. Keep it alive and [`serve`](Primary::serve).
    Primary(Primary),
    /// The running Visor took the request; this process should exit.
    HandedOver,
    /// Couldn't tell (no settings folder, locking failed, the other
    /// instance didn't answer); run normally.
    Unchecked,
}

pub struct Primary {
    _lock: File,
    listener: TcpListener,
    token: String,
}

const TIMEOUT: Duration = Duration::from_secs(2);
/// How long an agent command may take: starting waits for the camera and
/// the countdown, stopping for the files to be finalized.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);

/// Becomes the running Visor, or hands `request` to the one already running.
pub fn claim(request: &Request) -> Instance {
    let Some(dir) = settings_dir() else {
        return Instance::Unchecked;
    };
    match try_claim(&dir, request) {
        Ok(instance) => instance,
        Err(e) => {
            tracing::warn!("single-instance check failed: {e}");
            Instance::Unchecked
        }
    }
}

fn settings_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Visor"))
}

fn open_lock(dir: &Path) -> std::io::Result<File> {
    fs::create_dir_all(dir)?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("instance.lock"))
}

fn try_claim(dir: &Path, request: &Request) -> std::io::Result<Instance> {
    let lock = open_lock(dir)?;
    match lock.try_lock() {
        Ok(()) => {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let token = random_token();
            let port = listener.local_addr()?.port();
            write_private(&dir.join("instance"), &format!("{port} {token}\n"))?;
            Ok(Instance::Primary(Primary {
                _lock: lock,
                listener,
                token,
            }))
        }
        Err(TryLockError::WouldBlock) => match hand_over(dir, request) {
            Ok(()) => Ok(Instance::HandedOver),
            Err(e) => {
                tracing::warn!("another Visor is running but didn't answer: {e}");
                Ok(Instance::Unchecked)
            }
        },
        Err(TryLockError::Error(e)) => Err(e),
    }
}

impl Primary {
    /// Answers later launches and agents on background threads, calling
    /// `handle` for each request.
    pub fn serve(self, handle: impl Fn(Request) + Send + Sync + 'static) {
        let Self {
            _lock,
            listener,
            token,
        } = self;
        let spawned = std::thread::Builder::new()
            .name("visor-instance".into())
            .spawn(move || {
                // The lock is held for as long as this process runs.
                let _lock = _lock;
                let handle = std::sync::Arc::new(handle);
                for stream in listener.incoming().flatten() {
                    // Agent commands can take a while; answer each
                    // connection on its own thread.
                    let (handle, token) = (handle.clone(), token.clone());
                    let spawned = std::thread::Builder::new()
                        .name("visor-request".into())
                        .spawn(move || {
                            if let Err(e) = answer(stream, &token, &*handle) {
                                tracing::warn!("ignored a request: {e}");
                            }
                        });
                    if let Err(e) = spawned {
                        tracing::warn!("could not answer a request: {e}");
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("could not listen for later launches: {e}");
        }
    }
}

/// Reads one request, hands it to `handle` and writes the reply: `ok` for
/// launches, a line of JSON for agent commands.
fn answer(mut stream: TcpStream, token: &str, handle: &dyn Fn(Request)) -> std::io::Result<()> {
    match read_request(&stream, token)? {
        Parsed::Launch(request) => {
            stream.write_all(b"ok\n")?;
            handle(request);
        }
        Parsed::Control(command) => {
            let (tx, rx) = crossbeam_channel::bounded(1);
            handle(Request::Control(command, tx));
            let reply = rx.recv_timeout(CONTROL_TIMEOUT).unwrap_or_else(
                |_| serde_json::json!({"ok": false, "error": "Visor didn't answer in time."}),
            );
            stream.write_all(format!("{reply}\n").as_bytes())?;
        }
    }
    Ok(())
}

enum Parsed {
    Launch(Request),
    Control(Value),
}

/// Request format: the token, then `show`, `edit` and a path, or `control`
/// and a JSON command, one per line.
fn read_request(stream: &TcpStream, token: &str) -> std::io::Result<Parsed> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    let mut text = String::new();
    stream.take(64 * 1024).read_to_string(&mut text)?;
    let mut lines = text.lines();
    if lines.next() != Some(token) {
        return Err(std::io::Error::other("wrong token"));
    }
    Ok(match (lines.next(), lines.next()) {
        (Some("show"), _) => Parsed::Launch(Request::ShowPanel),
        (Some("edit"), Some(path)) if !path.is_empty() => {
            Parsed::Launch(Request::Edit(PathBuf::from(path)))
        }
        (Some("control"), Some(json)) => {
            Parsed::Control(serde_json::from_str(json).map_err(std::io::Error::other)?)
        }
        _ => return Err(std::io::Error::other("unknown request")),
    })
}

/// Why an agent command didn't reach Visor.
#[derive(Debug)]
pub enum ControlError {
    NotRunning,
    Io(std::io::Error),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => write!(f, "Visor isn't running"),
            Self::Io(e) => write!(f, "couldn't reach Visor: {e}"),
        }
    }
}

/// Whether a Visor is running for the user whose settings are in `dir`.
fn running_in(dir: &Path) -> bool {
    // Taking the lock (and letting go of it at once) means nobody holds it.
    matches!(
        open_lock(dir).map(|lock| lock.try_lock()),
        Ok(Err(TryLockError::WouldBlock))
    )
}

/// Sends an agent command to the running Visor and returns its reply.
pub fn control(command: &Value) -> Result<Value, ControlError> {
    control_in(&settings_dir().ok_or(ControlError::NotRunning)?, command)
}

fn control_in(dir: &Path, command: &Value) -> Result<Value, ControlError> {
    if !running_in(dir) {
        return Err(ControlError::NotRunning);
    }
    let message = format!("control\n{command}\n");
    let reply = send(dir, &message, CONTROL_TIMEOUT + TIMEOUT).map_err(ControlError::Io)?;
    serde_json::from_str(&reply).map_err(|e| ControlError::Io(std::io::Error::other(e)))
}

fn hand_over(dir: &Path, request: &Request) -> std::io::Result<()> {
    let message = match request {
        Request::ShowPanel => "show\n".to_string(),
        Request::Edit(path) => {
            // The running Visor has its own working directory.
            let path = std::path::absolute(path)?;
            format!("edit\n{}\n", path.display())
        }
        Request::Control(..) => return Err(std::io::Error::other("not a launch request")),
    };
    if send(dir, &message, TIMEOUT)?.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other("no acknowledgement"))
    }
}

/// Sends `message` after the token and returns the first line of the reply.
fn send(dir: &Path, message: &str, timeout: Duration) -> std::io::Result<String> {
    let info = fs::read_to_string(dir.join("instance"))?;
    let (port, token) = info
        .trim()
        .split_once(' ')
        .ok_or_else(|| std::io::Error::other("malformed instance file"))?;
    let port: u16 = port.parse().map_err(std::io::Error::other)?;
    let mut stream = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), TIMEOUT)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(format!("{token}\n{message}").as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    Ok(reply)
}

/// 128 bits from the standard library's randomly seeded hasher keys.
fn random_token() -> String {
    (0..2)
        .map(|i| {
            let mut h = RandomState::new().build_hasher();
            h.write_u32(i);
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        })
        .collect()
}

/// Writes a file only the current user can read (on Unix; on Windows the
/// settings folder already is per user).
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_hands_its_request_to_the_first() {
        let dir = std::env::temp_dir().join(format!("visor-instance-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let Instance::Primary(primary) = try_claim(&dir, &Request::ShowPanel).unwrap() else {
            panic!("first claim should win");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        primary.serve(move |r| tx.send(r).unwrap());

        let edit = Request::Edit(dir.join("Some Recording"));
        assert!(matches!(
            try_claim(&dir, &edit).unwrap(),
            Instance::HandedOver
        ));
        match rx.recv_timeout(TIMEOUT).unwrap() {
            Request::Edit(path) => assert_eq!(path, dir.join("Some Recording")),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            try_claim(&dir, &Request::ShowPanel).unwrap(),
            Instance::HandedOver
        ));
        assert!(matches!(
            rx.recv_timeout(TIMEOUT).unwrap(),
            Request::ShowPanel
        ));
    }

    #[test]
    fn agent_commands_get_replies() {
        let dir = std::env::temp_dir().join(format!("visor-control-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(matches!(
            control_in(&dir, &serde_json::json!({})),
            Err(ControlError::NotRunning)
        ));
        let Instance::Primary(primary) = try_claim(&dir, &Request::ShowPanel).unwrap() else {
            panic!("first claim should win");
        };
        primary.serve(|request| {
            if let Request::Control(command, reply) = request {
                let _ = reply.send(serde_json::json!({"ok": true, "echo": command["command"]}));
            }
        });
        let reply = control_in(&dir, &serde_json::json!({"command": "status"})).unwrap();
        assert_eq!(reply, serde_json::json!({"ok": true, "echo": "status"}));
    }

    #[test]
    fn requests_need_the_token() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.write_all(b"guess\nshow\n").unwrap();
            s.shutdown(Shutdown::Write).unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(read_request(&stream, "secret").is_err());
        client.join().unwrap();
    }
}
