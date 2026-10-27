//! One Visor at a time. The first instance holds a lock file and listens on a
//! loopback port; launching Visor again hands its request (show the recorder
//! panel, open a recording) to that instance and exits.
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

/// What a launch asks the running Visor to do.
#[derive(Debug, Clone)]
pub enum Request {
    ShowPanel,
    Edit(PathBuf),
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

/// Becomes the running Visor, or hands `request` to the one already running.
pub fn claim(request: &Request) -> Instance {
    let Some(dir) = dirs::config_dir().map(|d| d.join("Visor")) else {
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

fn try_claim(dir: &Path, request: &Request) -> std::io::Result<Instance> {
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("instance.lock"))?;
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
    /// Answers later launches on a background thread, calling `handle` for
    /// each request.
    pub fn serve(self, handle: impl Fn(Request) + Send + 'static) {
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
                for stream in listener.incoming().flatten() {
                    match read_request(stream, &token) {
                        Ok(request) => handle(request),
                        Err(e) => tracing::warn!("ignored a launch request: {e}"),
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("could not listen for later launches: {e}");
        }
    }
}

/// Request format: the token, then `show` or `edit` and a path, one per line.
fn read_request(mut stream: TcpStream, token: &str) -> std::io::Result<Request> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    let mut text = String::new();
    (&stream).take(64 * 1024).read_to_string(&mut text)?;
    let mut lines = text.lines();
    if lines.next() != Some(token) {
        return Err(std::io::Error::other("wrong token"));
    }
    let request = match (lines.next(), lines.next()) {
        (Some("show"), _) => Request::ShowPanel,
        (Some("edit"), Some(path)) if !path.is_empty() => Request::Edit(PathBuf::from(path)),
        _ => return Err(std::io::Error::other("unknown request")),
    };
    stream.write_all(b"ok\n")?;
    Ok(request)
}

fn hand_over(dir: &Path, request: &Request) -> std::io::Result<()> {
    let info = fs::read_to_string(dir.join("instance"))?;
    let (port, token) = info
        .trim()
        .split_once(' ')
        .ok_or_else(|| std::io::Error::other("malformed instance file"))?;
    let port: u16 = port.parse().map_err(std::io::Error::other)?;
    let mut stream = TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    let message = match request {
        Request::ShowPanel => format!("{token}\nshow\n"),
        Request::Edit(path) => {
            // The running Visor has its own working directory.
            let path = std::path::absolute(path)?;
            format!("{token}\nedit\n{}\n", path.display())
        }
    };
    stream.write_all(message.as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other("no acknowledgement"))
    }
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
    fn requests_need_the_token() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
            s.write_all(b"guess\nshow\n").unwrap();
            s.shutdown(Shutdown::Write).unwrap();
        });
        let (stream, _) = listener.accept().unwrap();
        assert!(read_request(stream, "secret").is_err());
        client.join().unwrap();
    }
}
