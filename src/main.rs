// Verb dispatch (FR-002). The keep-list is the whole surface; anything
// else exits 2 and is told the list.
mod board;
mod dispatch;
#[cfg(test)]
mod dispatch_tests;
mod drain;
mod dstate;
mod error;
mod hub;
mod identity;
mod msg;
mod room;
mod seat;
mod session;
#[cfg(test)]
mod testutil;
mod verbs;

use clap::{CommandFactory, Parser, Subcommand};
use std::path::Path;

/// The whole verb surface (FR-002).
pub const KEEP_LIST: &str = "init register send drain dispatch room up down gallery status reset";
const ROOM_VERBS: &str = "create post read follow list";

#[derive(Parser)]
#[command(
    name = "seat",
    version,
    about = "One operator binary for the seat board"
)]
struct Cli {
    /// Board root (overrides SEAT_ROOT; default ~/.local/share/seat)
    #[arg(long, global = true)]
    root: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Create the board root and write its marker (idempotent)
    Init,
    /// Register a seat and mint its one-time token
    Register(RegisterArgs),
    /// Send a message from one seat to another. `--from` is fixed by
    /// the identity rule when omitted (a bound session sends as its
    /// seat; otherwise the message goes as `inbox` to a factory seat).
    Send {
        #[arg(long)]
        from: Option<String>,
        /// Destination seat
        to: String,
        body: String,
    },
    /// Print a seat's unread inbox and followed-room posts
    Drain {
        #[arg(long)]
        seat: String,
    },
    /// Deliver every hapi-bound seat's mail to its hub session
    /// (`--once` runs one scan pass; unbound seats are pull)
    Dispatch {
        /// Run one scan pass and wait for its workers
        #[arg(long)]
        once: bool,
    },
    /// Room verbs (shared, MAC'd posts)
    Room {
        #[command(subcommand)]
        action: RoomAction,
    },
    /// Bring registered seats into the one swarm session (second call
    /// does not duplicate)
    Up {
        /// Registered seats to bring up (default: every registered seat)
        names: Vec<String>,
    },
    /// Stop seats by killing their panes; prints nothing, drains nothing
    Down {
        /// Registered seats to stop (default: every registered seat)
        names: Vec<String>,
    },
    /// Print the running seats, tile them into one window, then attach
    Gallery,
    /// One row per registered seat: `<name> pane|awake|unattended mail=<N>`
    Status,
    /// Remove the board under the root (marker, seats/, rooms/)
    Reset,
    /// Any verb outside the keep-list lands here.
    #[command(external_subcommand)]
    Unknown(Vec<String>),
}

#[derive(clap::Args)]
struct RegisterArgs {
    #[arg(long)]
    seat: String,
    #[arg(long)]
    harness: String,
    #[arg(long)]
    model: Option<String>,
    /// Absolute working directory the seat's command runs in
    #[arg(long)]
    cwd: Option<String>,
    /// Command words after `--` (may start with `-`); none stores no
    /// command and clears none
    #[arg(last = true)]
    cmd: Vec<String>,
}

#[derive(Subcommand)]
enum RoomAction {
    /// Create a room
    Create { room: String },
    /// Post a message into a room. `--from` may be omitted only by a
    /// session bound to the posting seat (the identity rule).
    Post {
        room: String,
        body: String,
        #[arg(long)]
        from: Option<String>,
    },
    /// Print a room's posts
    Read { room: String },
    /// Follow a room from a seat (cursor on the seat)
    Follow {
        room: String,
        #[arg(long)]
        seat: String,
    },
    /// List rooms
    List,
    /// Anything outside the room verb list.
    #[command(external_subcommand)]
    Unknown(Vec<String>),
}

/// Unknown-verb refusal (exit 2) with the keep-list.
fn refuse(verb: &str) -> ! {
    eprintln!("seat: unknown verb '{verb}'");
    eprintln!("verbs: {KEEP_LIST}");
    std::process::exit(2);
}

fn refuse_room(verb: &str) -> ! {
    eprintln!("seat: unknown room verb '{verb}'");
    eprintln!("room verbs: {ROOM_VERBS}");
    std::process::exit(2);
}

/// The caller's session id, read once at the CLI edge (empty means
/// none) and passed into the verbs — library code never reads the
/// environment for this, so tests never race on it.
fn env_session_id() -> Option<String> {
    std::env::var("HAPI_SESSION_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

fn dispatch(root: &Path, cmd: Command, session: Option<&str>) -> error::Result<()> {
    match cmd {
        Command::Init => verbs::cmd_init(root),
        Command::Register(args) => verbs::cmd_register(
            root,
            &args.seat,
            &args.harness,
            args.model,
            args.cwd,
            args.cmd,
        ),
        Command::Send { from, to, body } => {
            verbs::cmd_send(root, from.as_deref(), &to, &body, session)
        }
        Command::Drain { seat } => {
            let mut out = std::io::stdout().lock();
            verbs::cmd_drain(root, &seat, &mut out)
        }
        Command::Dispatch { once } => dispatch::cmd_dispatch(root, once),
        Command::Room { action } => dispatch_room(root, action, session),
        Command::Up { names } => session::cmd_up(root, &names),
        Command::Down { names } => session::cmd_down(root, &names),
        Command::Gallery => session::cmd_gallery(root),
        Command::Status => session::cmd_status(root),
        Command::Reset => verbs::cmd_reset(root),
        // Unknown verbs are refused in main() before dispatch.
        Command::Unknown(_) => unreachable!("unknown verbs refuse before dispatch"),
    }
}

fn dispatch_room(root: &Path, action: RoomAction, session: Option<&str>) -> error::Result<()> {
    match action {
        RoomAction::Create { room } => room::cmd_create(root, &room),
        RoomAction::Post { room, body, from } => {
            room::cmd_post(root, &room, from.as_deref(), &body, session)
        }
        RoomAction::Read { room } => {
            let mut out = std::io::stdout().lock();
            room::cmd_read(root, &room, &mut out)
        }
        RoomAction::Follow { room, seat } => room::cmd_follow(root, &room, &seat),
        RoomAction::List => {
            let mut out = std::io::stdout().lock();
            room::cmd_list(root, &mut out)
        }
        RoomAction::Unknown(_) => unreachable!("unknown room verbs refuse before dispatch"),
    }
}

fn main() {
    let cli = Cli::parse();
    let root = board::resolve_root(cli.root.as_deref());
    let code = match cli.command {
        None => match Cli::command().print_help() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("seat: {e}");
                1
            }
        },
        Some(Command::Unknown(words)) => {
            refuse(words.first().map(|w| w.as_str()).unwrap_or_default())
        }
        Some(Command::Room {
            action: RoomAction::Unknown(words),
        }) => refuse_room(words.first().map(|w| w.as_str()).unwrap_or_default()),
        Some(cmd) => {
            let session = env_session_id();
            match dispatch(&root, cmd, session.as_deref()) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("seat: {e}");
                    1
                }
            }
        }
    };
    std::process::exit(code);
}
