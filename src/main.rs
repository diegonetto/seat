// Verb dispatch (FR-002). The keep-list is the whole surface; anything
// else exits 2 and is told the list.
mod board;
mod drain;
mod error;
mod msg;
mod room;
mod seat;
mod serve;
mod session;
#[cfg(test)]
mod testutil;
mod tui;
mod verbs;

use clap::{Parser, Subcommand};
use seat::Lifecycle;
use std::path::Path;

/// The whole verb surface (FR-002).
pub const KEEP_LIST: &str = "init register send arm drain room up down gallery status reset";
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
    /// Send a message from one seat to another
    Send {
        #[arg(long)]
        from: String,
        /// Destination seat
        to: String,
        body: String,
    },
    /// Print a seat's unread inbox and followed-room posts
    Drain {
        #[arg(long)]
        seat: String,
    },
    /// The only waiter: emits a wake notice when mail arrives. Never
    /// drains. exit-wake wakes once and exits 0; poller stays up until
    /// SIGTERM. Needs no tmux pane.
    Arm {
        #[arg(long)]
        seat: String,
        /// Ephemeral lifecycle override (exit-wake | poller); meta.lifecycle stays the authority
        #[arg(long)]
        lifecycle: Option<Lifecycle>,
        /// Run CMD on each wake in addition to the notice
        #[arg(long)]
        exec: Option<String>,
        /// Replace a live waiter for this seat (SIGTERM + wait)
        #[arg(long)]
        takeover: bool,
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
    /// Print the running seats, then attach to the swarm session
    Gallery,
    /// One row per registered seat: running | stopped-with-unread |
    /// needs-operator
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
    /// exit-wake | poller — the durable authority for arm
    #[arg(long)]
    lifecycle: Option<Lifecycle>,
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
    /// Post a message into a room
    Post {
        room: String,
        body: String,
        #[arg(long)]
        from: String,
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

fn dispatch(root: &Path, cmd: Command) -> error::Result<()> {
    match cmd {
        Command::Init => verbs::cmd_init(root),
        Command::Register(args) => verbs::cmd_register(
            root,
            &args.seat,
            &args.harness,
            args.model,
            args.lifecycle,
            args.cwd,
            args.cmd,
        ),
        Command::Send { from, to, body } => verbs::cmd_send(root, &from, &to, &body),
        Command::Drain { seat } => {
            let mut out = std::io::stdout().lock();
            verbs::cmd_drain(root, &seat, &mut out)
        }
        Command::Room { action } => dispatch_room(root, action),
        Command::Up { names } => session::cmd_up(root, &names),
        Command::Down { names } => session::cmd_down(root, &names),
        Command::Gallery => session::cmd_gallery(root),
        Command::Status => session::cmd_status(root),
        Command::Reset => verbs::cmd_reset(root),
        Command::Arm {
            seat,
            lifecycle,
            exec,
            takeover,
        } => {
            let mut out = std::io::stdout().lock();
            serve::cmd_arm(root, &seat, lifecycle, exec.as_deref(), takeover, &mut out)
        }
        // Unknown verbs are refused in main() before dispatch.
        Command::Unknown(_) => unreachable!("unknown verbs refuse before dispatch"),
    }
}

fn dispatch_room(root: &Path, action: RoomAction) -> error::Result<()> {
    match action {
        RoomAction::Create { room } => room::cmd_create(root, &room),
        RoomAction::Post { room, body, from } => room::cmd_post(root, &room, &from, &body),
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
        // Bare seat on a tty is the interactive roster (FR-005); it
        // only reads. tui::launch decides and fails with one clean
        // line, never a panic.
        None => match tui::launch(&root) {
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
        Some(cmd) => match dispatch(&root, cmd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("seat: {e}");
                1
            }
        },
    };
    std::process::exit(code);
}
