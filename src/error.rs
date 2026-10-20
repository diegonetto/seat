use std::path::PathBuf;
use thiserror::Error;

/// Named errors for the seat board. Nothing in this crate may panic on a
/// bad board: every corrupt or missing artifact surfaces as one of these.
#[derive(Debug, Error)]
pub enum SeatError {
    #[error("no seat board at {0} (run `seat init` or set SEAT_ROOT)")]
    MissingBoard(PathBuf),

    #[error(
        "unrecognized root {0}: marker is missing or not 'seat' (only `seat reset` may touch it)"
    )]
    ForeignRoot(PathBuf),

    #[error("corrupt seat meta at {path}")]
    CorruptMeta {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("corrupt message at {path}")]
    CorruptMsg {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("message body at {path} is not valid UTF-8")]
    BadBody { path: PathBuf },

    #[error("drain: message from '{0}' failed mark verification; not delivered")]
    BadMark(String),

    #[error("drain: unreadable message was not delivered")]
    Unreadable,

    #[error("seat {0} has no token on this board")]
    GhostSeat(String),

    #[error("seat {0} is not registered on this board")]
    UnknownSeat(String),

    #[error("bad name {0:?}: letters, digits, '-', '_', '.' only; no leading dot; max 64 chars")]
    BadName(String),

    #[error("token for seat {0} is not valid hex")]
    BadToken(String),

    #[error("seat '{0}' is busy: another drain holds its lock (gave up after 30s)")]
    SeatBusy(String),

    #[error("another dispatcher holds the lock {0} (one dispatcher per board)")]
    DispatchBusy(PathBuf),

    #[error("{0}")]
    Identity(String),

    #[error("bad message header: {0}")]
    BadHeader(String),

    #[error("tmux: {0}")]
    Tmux(String),

    #[error("register: --cwd must be an absolute path, got {0:?}")]
    BadCwd(String),

    #[error("up: seat {0} has no command stored")]
    NoCmd(String),

    #[error("up: seat {0} has no working directory stored")]
    NoCwd(String),

    #[error("up: seat {seat}: working directory {path} is not a directory")]
    NotADirectory { seat: String, path: PathBuf },

    #[error("up: seat {0} did not start: {1}")]
    StartFailed(String, String),

    #[error("hub: {0}")]
    Hub(String),

    #[error("webhook: {0}")]
    Webhook(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, SeatError>;
