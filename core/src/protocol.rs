//! What `headroom` and `headroomd` say to each other over the Unix socket.
//!
//! One JSON object per line. A `headroom run` holds its connection open for
//! the whole life of its job: the connection *is* the lease, so a wrapper
//! that dies — killed by an agent's timeout, Ctrl-C, a closed terminal —
//! frees its slot without any cleanup protocol.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::admission::Blocked;
use crate::config::Config;
use crate::model::{Bytes, FrozenBy, HistoryEntry, Job, JobId, JobSpec, Millis, Snapshot};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Enqueue(JobSpec),
    Started {
        pid: i32,
        pgid: i32,
    },
    /// Cancel the running lease with the wrapper's INT, TERM or HUP.
    Signal {
        signal: i32,
    },
    /// Terminal job control has stopped or continued the child.
    TerminalStopped {
        stopped: bool,
    },
    /// Report an attempt ending. The daemon replies Ok before any retry grant.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    List,
    Control(Control),
}

/// Changes a user makes from the CLI or the menu bar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Control {
    Hold {
        on: bool,
    },
    Pin {
        id: JobId,
        on: bool,
    },
    Low {
        id: JobId,
        on: bool,
    },
    Top {
        id: JobId,
    },
    Move {
        id: JobId,
        to: usize,
    },
    Cancel {
        id: JobId,
    },
    Kill {
        id: JobId,
    },
    Freeze {
        id: JobId,
    },
    Resume {
        id: JobId,
    },
    ResetProfile {
        key: String,
    },
    /// Forget learned profiles, completed runs and command statistics.
    ClearData,
    /// Replace the settings, as edited in the menu bar.
    Configure {
        config: Box<Config>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum DaemonMsg {
    Queued {
        id: JobId,
        pos: usize,
        of: usize,
        estimate: Bytes,
        samples: u32,
        reason: Option<String>,
    },
    Granted {
        id: JobId,
        retry: u8,
        low: bool,
    },
    Frozen {
        by: FrozenBy,
        why: Option<String>,
    },
    Thawed,
    Killed {
        why: String,
        requeued: bool,
    },
    Cancelled,
    /// The asking process is part of running job `id`: run directly.
    Inside {
        id: JobId,
    },
    Jobs(Listing),
    Ok,
    Error {
        msg: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    /// The daemon's clock, for turning `started_at`/`enqueued_at` into durations.
    pub now: Millis,
    pub snapshot: Snapshot,
    pub held: bool,
    pub running: Vec<Job>,
    pub queued: Vec<Job>,
    pub blocked: Option<Blocked>,
    pub history: Vec<HistoryEntry>,
}

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Read one message. `Ok(None)` is a clean end of stream.
pub fn read_msg<R: BufRead, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// One request, one answer, over a fresh connection to the daemon.
pub fn request(socket: &std::path::Path, msg: &ClientMsg) -> Result<DaemonMsg, String> {
    let mut s = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|_| "headroomd isn't running".to_string())?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    write_msg(&mut s, msg).map_err(|e| e.to_string())?;
    read_msg::<_, DaemonMsg>(&mut io::BufReader::new(s))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "headroomd closed the connection".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn legacy_worker_grants_are_read_but_never_reissued() {
        let grant: DaemonMsg =
            serde_json::from_str(r#"{"t":"granted","id":1,"retry":0,"low":false,"workers":2}"#)
                .unwrap();
        assert_eq!(
            grant,
            DaemonMsg::Granted {
                id: 1,
                retry: 0,
                low: false
            }
        );
        assert!(
            serde_json::to_value(grant)
                .unwrap()
                .get("workers")
                .is_none()
        );
    }

    #[test]
    fn messages_round_trip_one_per_line() {
        let msgs = vec![
            ClientMsg::Enqueue(JobSpec {
                cmd: "pnpm test".into(),
                ..JobSpec::default()
            }),
            ClientMsg::Started { pid: 10, pgid: 10 },
            ClientMsg::Exited {
                code: Some(0),
                signal: None,
            },
            ClientMsg::List,
            ClientMsg::Control(Control::Move { id: 3, to: 1 }),
            ClientMsg::Control(Control::ClearData),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_msg(&mut buf, m).unwrap();
        }
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), msgs.len());
        let mut r = Cursor::new(buf);
        for m in &msgs {
            assert_eq!(read_msg::<_, ClientMsg>(&mut r).unwrap().as_ref(), Some(m));
        }
        assert_eq!(read_msg::<_, ClientMsg>(&mut r).unwrap(), None);
    }

    #[test]
    fn wire_format_is_tagged_and_readable() {
        let mut buf = Vec::new();
        write_msg(
            &mut buf,
            &ClientMsg::Control(Control::Pin { id: 4, on: true }),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"t\":\"control\",\"op\":\"pin\",\"id\":4,\"on\":true}\n"
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let mut r = Cursor::new(b"{not json}\n".to_vec());
        let e = read_msg::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_without_a_daemon_says_so() {
        let e = request(
            std::path::Path::new("/nonexistent/headroomd.sock"),
            &ClientMsg::List,
        )
        .unwrap_err();
        assert_eq!(e, "headroomd isn't running");
    }

    #[test]
    fn unknown_message_type_is_rejected() {
        let mut r = Cursor::new(b"{\"t\":\"self_destruct\"}\n".to_vec());
        assert!(read_msg::<_, ClientMsg>(&mut r).is_err());
    }
}
