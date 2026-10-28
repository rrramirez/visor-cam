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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;

/// What a launch asks the running Visor to do.
#[derive(Debug, Clone)]
pub enum Request {
    ShowPanel,
    /// Started in the background for an agent: nothing to show.
    Background,
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
/// Connections at a time whose request hasn't been read yet.
const MAX_UNREAD: usize = 16;
/// How long a launch waits for a Visor that holds the lock to listen.
const STARTUP_GRACE: Duration = Duration::from_secs(3);
/// How long an agent command may take before its reply is given up on.
/// Starting gives up by itself within a minute (see `app::agent`); this
/// only guards against a reply that never comes, so it is generous enough
/// for any stop to finish saving.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(600);

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
        Err(TryLockError::WouldBlock) => {
            // A Visor that has only just started may not be listening yet.
            // Only a request it never took is sent again.
            let deadline = std::time::Instant::now() + STARTUP_GRACE;
            let error = loop {
                match hand_over(dir, request) {
                    Ok(()) | Err(SendError::Failed(_)) => return Ok(Instance::HandedOver),
                    Err(SendError::NotListening(e)) if std::time::Instant::now() > deadline => {
                        break e;
                    }
                    Err(SendError::NotListening(_)) => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            };
            tracing::warn!("another Visor is running but didn't answer: {error}");
            // Started for an agent: the running one will answer it.
            if matches!(request, Request::Background) {
                return Ok(Instance::HandedOver);
            }
            Ok(Instance::Unchecked)
        }
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
                // Connections whose request hasn't been read yet; capped so
                // connections that send nothing can't pile up threads, and
                // each holds its slot for at most TIMEOUT (see read_request).
                let unread = std::sync::Arc::new(AtomicUsize::new(0));
                for stream in listener.incoming().flatten() {
                    if unread.fetch_add(1, Ordering::SeqCst) >= MAX_UNREAD {
                        unread.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                    // Agent commands can take a while; answer each
                    // connection on its own thread.
                    let (handle, token) = (handle.clone(), token.clone());
                    let spawned = std::thread::Builder::new()
                        .name("visor-request".into())
                        .spawn({
                            let unread = unread.clone();
                            move || {
                                let request = read_request(&stream, &token);
                                unread.fetch_sub(1, Ordering::SeqCst);
                                if let Err(e) = request.and_then(|r| answer(stream, r, &*handle)) {
                                    tracing::warn!("ignored a request: {e}");
                                }
                            }
                        });
                    if let Err(e) = spawned {
                        unread.fetch_sub(1, Ordering::SeqCst);
                        tracing::warn!("could not answer a request: {e}");
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("could not listen for later launches: {e}");
        }
    }
}

/// Hands a request to `handle` and writes the reply: `ok` at once, so the
/// sender knows Visor took it, then for agent commands a line of JSON.
fn answer(mut stream: TcpStream, request: Parsed, handle: &dyn Fn(Request)) -> std::io::Result<()> {
    stream.write_all(b"ok\n")?;
    match request {
        Parsed::Launch(request) => handle(request),
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
/// and a JSON command, one per line. The whole request must arrive within
/// [`TIMEOUT`], so a slow sender can't hold a connection slot for long.
fn read_request(mut stream: &TcpStream, token: &str) -> std::io::Result<Parsed> {
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || bytes.len() > 64 * 1024 {
            return Err(std::io::Error::other("request too slow or too long"));
        }
        stream.set_read_timeout(Some(left))?;
        match stream.read(&mut chunk)? {
            0 => break,
            n => bytes.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8(bytes).map_err(std::io::Error::other)?;
    let mut lines = text.lines();
    if lines.next() != Some(token) {
        return Err(std::io::Error::other("wrong token"));
    }
    Ok(match (lines.next(), lines.next()) {
        (Some("show"), _) => Parsed::Launch(Request::ShowPanel),
        (Some("background"), _) => Parsed::Launch(Request::Background),
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

/// Sends an agent command to the running Visor and returns its reply.
pub fn control(command: &Value) -> Result<Value, ControlError> {
    control_in(&settings_dir().ok_or(ControlError::NotRunning)?, command)
}

fn control_in(dir: &Path, command: &Value) -> Result<Value, ControlError> {
    // Whether Visor runs is told by its port answering, not by its lock:
    // taking the lock to check could make a Visor starting at that moment
    // think another one holds it.
    let message = format!("control\n{command}\n");
    let reply = match send(dir, &message, CONTROL_TIMEOUT + TIMEOUT) {
        Ok(reply) => reply,
        Err(SendError::NotListening(_)) => return Err(ControlError::NotRunning),
        Err(SendError::Failed(e)) => return Err(ControlError::Io(e)),
    };
    serde_json::from_str(&reply).map_err(|e| ControlError::Io(std::io::Error::other(e)))
}

fn hand_over(dir: &Path, request: &Request) -> Result<(), SendError> {
    let message = match request {
        Request::ShowPanel => "show\n".to_string(),
        Request::Background => "background\n".to_string(),
        Request::Edit(path) => {
            // The running Visor has its own working directory.
            let path = std::path::absolute(path).map_err(SendError::NotListening)?;
            format!("edit\n{}\n", path.display())
        }
        Request::Control(..) => {
            return Err(SendError::NotListening(std::io::Error::other(
                "not a launch request",
            )));
        }
    };
    send(dir, &message, TIMEOUT).map(drop)
}

/// Why a message to the running Visor failed.
enum SendError {
    /// No Visor took it: there is no instance file, nothing accepted the
    /// connection (refused, or timed out, as on Windows), or what did never
    /// acknowledged it (another program now on Visor's old port).
    NotListening(std::io::Error),
    /// Visor took it, but its reply didn't arrive.
    Failed(std::io::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotListening(e) | Self::Failed(e) => e.fmt(f),
        }
    }
}

/// Sends `message` after the token. Visor acknowledges it at once; returns
/// the reply that follows (empty for launches), waiting up to `timeout`.
fn send(dir: &Path, message: &str, timeout: Duration) -> Result<String, SendError> {
    let mut stream = connect_and_send(dir, message).map_err(SendError::NotListening)?;
    let mut reader = BufReader::new(&mut stream);
    let mut ack = String::new();
    match reader.read_line(&mut ack) {
        Ok(_) if ack.trim() == "ok" => {}
        Ok(_) => {
            return Err(SendError::NotListening(std::io::Error::other(
                "no acknowledgement",
            )));
        }
        Err(e) => return Err(SendError::NotListening(e)),
    }
    // From here Visor has the request: never send it again.
    reader
        .get_ref()
        .set_read_timeout(Some(timeout))
        .map_err(SendError::Failed)?;
    let mut reply = String::new();
    reader.read_line(&mut reply).map_err(SendError::Failed)?;
    Ok(reply)
}

fn connect_and_send(dir: &Path, message: &str) -> std::io::Result<TcpStream> {
    let info = fs::read_to_string(dir.join("instance"))?;
    let (port, token) = info
        .trim()
        .split_once(' ')
        .ok_or_else(|| std::io::Error::other("malformed instance file"))?;
    let port: u16 = port.parse().map_err(std::io::Error::other)?;
    let mut stream = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.write_all(format!("{token}\n{message}").as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    Ok(stream)
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
    fn a_silent_program_on_the_old_port_is_not_visor() {
        let dir = std::env::temp_dir().join(format!("visor-stale-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Accepts connections (the OS does) but never answers.
        let squatter = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = squatter.local_addr().unwrap().port();
        write_private(&dir.join("instance"), &format!("{port} token\n")).unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            control_in(&dir, &serde_json::json!({"command": "status"})),
            Err(ControlError::NotRunning)
        ));
        assert!(started.elapsed() < TIMEOUT * 2);
        drop(squatter);
        fs::remove_dir_all(&dir).unwrap();
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
