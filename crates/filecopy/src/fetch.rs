//! What a native reader asks the core for, and what the core tells it back.
//!
//! An offer publishes names and sizes only; the bytes of a file are fetched
//! when something reads it, by a [`Fetch`] the core turns into a protocol
//! transfer. The reader then waits, and how long it waits is the question
//! this module answers: not a fixed time, because a large file on a slow link
//! takes as long as it takes, but for as long as bytes keep arriving.

use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::time::Duration;

/// A request for the contents of one offered file.
pub struct Fetch {
    /// The path as the peer offered it.
    pub remote: String,
    /// Where the core is to put the contents.
    pub destination: PathBuf,
    /// Where the core reports on the transfer: progress while it runs, then
    /// how it ended. The reader drops its end when it stops waiting, which
    /// is how the core learns to cancel a transfer nobody will read.
    pub result: Sender<FetchReply>,
}

/// What comes back on [`Fetch::result`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchReply {
    /// Bytes received so far. The core sends one on every housekeeping tick
    /// whether or not the count moved: the reader tells a stalled transfer
    /// from a silent core by whether the number grows, and the core tells a
    /// reader that gave up from one still waiting by whether the send lands.
    Progress(u64),
    /// The whole file is at this path.
    Done(PathBuf),
    /// The transfer failed or was cancelled.
    Failed,
}

/// How long a reader waits with no new bytes before giving up.
///
/// Inactivity rather than duration, because a transfer runs at whatever rate
/// the tunnel allows and any fixed cap is a size above which pasting simply
/// fails on a slow enough link. A minute is longer than every keepalive
/// between the two ends -- ssh gives up on a dead link in 45 s, the protocol
/// pings sooner -- so a link that is merely slow shows a byte in that time
/// and one that is dead has been noticed and torn down, which fails the fetch
/// through its channel rather than this clock.
pub const FETCH_IDLE: Duration = Duration::from_secs(60);

/// Why a fetch did not produce a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchError {
    /// The core reported failure, or went away.
    Failed,
    /// Nothing arrived for [`FETCH_IDLE`].
    Stalled,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Failed => "File could not be transferred",
            Self::Stalled => "Clipboard transfer stalled",
        })
    }
}

impl std::error::Error for FetchError {}

/// Wait for a fetch to finish, giving up once [`FETCH_IDLE`] passes without
/// progress.
///
/// Returning the error *is* how the reader gives up: the receiver goes with
/// the caller's frame, and the core cancels the transfer when its next report
/// finds nobody listening.
///
/// The FUSE backend waits on every fetch at once in a select loop of its
/// own, so this has no caller on Linux outside the tests.
#[cfg(any(not(target_os = "linux"), test))]
pub fn wait(reply: &crossbeam_channel::Receiver<FetchReply>) -> Result<PathBuf, FetchError> {
    use crossbeam_channel::RecvTimeoutError;
    use std::time::Instant;

    let mut received = 0;
    let mut deadline = Instant::now() + FETCH_IDLE;
    loop {
        match reply.recv_deadline(deadline) {
            Ok(FetchReply::Progress(n)) => {
                if n > received {
                    received = n;
                    deadline = Instant::now() + FETCH_IDLE;
                }
            }
            Ok(FetchReply::Done(path)) => return Ok(path),
            Ok(FetchReply::Failed) | Err(RecvTimeoutError::Disconnected) => {
                return Err(FetchError::Failed)
            }
            Err(RecvTimeoutError::Timeout) => return Err(FetchError::Stalled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Progress that does not grow is a heartbeat, not activity: it keeps
    /// the core's abandonment probe working without keeping a stalled
    /// transfer alive forever.
    #[test]
    fn a_wait_ends_on_the_outcome_and_not_on_unchanged_progress() {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(FetchReply::Progress(0)).unwrap();
        tx.send(FetchReply::Progress(5)).unwrap();
        tx.send(FetchReply::Progress(5)).unwrap();
        tx.send(FetchReply::Done(PathBuf::from("/x"))).unwrap();
        assert_eq!(wait(&rx), Ok(PathBuf::from("/x")));
        tx.send(FetchReply::Failed).unwrap();
        assert_eq!(wait(&rx), Err(FetchError::Failed));
        drop(tx);
        assert_eq!(wait(&rx), Err(FetchError::Failed));
    }
}
