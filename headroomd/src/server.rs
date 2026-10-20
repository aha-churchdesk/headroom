//! The Unix socket: one reader thread per connection, feeding the main loop.
//!
//! The scheduler is single-threaded and owns all state; connections only
//! turn bytes into events. Writes happen on the main loop, with a timeout,
//! so a client that stops reading can't stall the queue for everyone.

use std::collections::HashMap;
use std::io::BufReader;
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

pub fn spawn(listener: UnixListener, tx: Sender<Event>, conns: Conns) {
    thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };
            let id = n as ConnId + 1;
            let Ok(writer) = stream.try_clone() else {
                continue;
            };
            let _ = writer.set_write_timeout(Some(Duration::from_secs(1)));
            conns
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, writer);
            let tx = tx.clone();
            thread::spawn(move || {
                let mut r = BufReader::new(stream);
                // Anything but a well-formed message ends the connection:
                // a confused client is treated as a gone one.
                while let Ok(Some(msg)) = read_msg::<_, ClientMsg>(&mut r) {
                    if tx.send(Event::Msg(id, Box::new(msg))).is_err() {
                        return;
                    }
                }
                let _ = tx.send(Event::Closed(id));
            });
        }
    });
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
