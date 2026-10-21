//! The Unix socket: one reader thread per connection, feeding the main loop.
//!
//! The scheduler is single-threaded and owns all state; connections only
//! turn bytes into events. Writes happen on the main loop, with a timeout,
//! so a client that stops reading can't stall the queue for everyone.

use std::collections::HashMap;
use std::io::{self, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use headroom_core::protocol::{ClientMsg, DaemonMsg, read_msg, write_msg};
use headroom_core::scheduler::ConnId;

pub enum Event {
    /// Boxed: an enqueue carries a whole job spec.
    Msg(ConnId, Box<ClientMsg>),
    Closed(ConnId),
}

pub type Conns = Arc<Mutex<HashMap<ConnId, UnixStream>>>;

/// Owns the listener and its readers. Dropping it wakes accept immediately,
/// closes the clients, and joins all server threads before returning.
pub struct Server {
    wake: UnixStream,
    thread: Option<thread::JoinHandle<Vec<thread::JoinHandle<()>>>>,
    conns: Conns,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        let readers = self
            .thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_default();
        for (_, stream) in self.conns.lock().unwrap_or_else(|p| p.into_inner()).drain() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        for reader in readers {
            let _ = reader.join();
        }
    }
}

pub fn spawn(listener: UnixListener, tx: Sender<Event>, conns: Conns) -> io::Result<Server> {
    let (wake, shutdown) = UnixStream::pair()?;
    let writers = conns.clone();
    let thread = thread::spawn(move || {
        let mut readers: Vec<thread::JoinHandle<()>> = vec![];
        let mut id = 0;
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: shutdown.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
            if rc < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if fds[1].revents != 0 {
                break;
            }
            let Ok((stream, _)) = listener.accept() else {
                continue;
            };
            id += 1;
            let Ok(writer) = stream.try_clone() else {
                continue;
            };
            let _ = writer.set_write_timeout(Some(Duration::from_secs(1)));
            conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, writer);
            let tx = tx.clone();
            readers.retain(|reader| !reader.is_finished());
            readers.push(thread::spawn(move || {
                let mut r = BufReader::new(stream);
                // Anything but a well-formed message ends the connection:
                // a confused client is treated as a gone one.
                while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut r) {
                    if tx.send(Event::Msg(id, Box::new(msg))).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Event::Closed(id));
            }));
        }
        readers
    });
    Ok(Server {
        wake,
        thread: Some(thread),
        conns: writers,
    })
}

/// Send to one connection, dropping it if it can't take the message.
pub fn send(conns: &Conns, id: ConnId, msg: &DaemonMsg) {
    let mut map = conns.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = map.get_mut(&id)
        && write_msg(s, msg).is_err()
    {
        let _ = s.shutdown(std::net::Shutdown::Both);
        map.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::mpsc;

    #[test]
    fn stopping_the_server_closes_listener_and_blocked_readers() {
        let dir = std::env::temp_dir().join(format!("hr-server-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("server.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, rx) = mpsc::channel();
        let conns = Arc::new(Mutex::new(HashMap::new()));
        let server = spawn(listener, tx, conns.clone()).unwrap();
        let mut client = UnixStream::connect(&socket).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        // After the first event, the reader blocks on an unfinished message.
        client.write_all(b"{\"t\":\"list\"}\n{").unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Msg(_, _)
        ));
        drop(server);
        let mut bytes = vec![];
        client.read_to_end(&mut bytes).unwrap();
        assert!(conns.lock().unwrap().is_empty());
        assert!(UnixStream::connect(&socket).is_err(), "listener was closed");
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(3)).unwrap(),
            Event::Closed(_)
        ));
        std::fs::remove_file(socket).unwrap();
    }
}
