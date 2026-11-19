mod alarm;
mod backoff;
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
mod seat;
mod session;
mod sleep;
#[cfg(test)]
mod testutil;
mod verbs;
#[cfg(test)]
mod verbs_tests;
mod webhook;
#[cfg(test)]
mod webhook_tests;

use clap::{CommandFactory, Parser, Subcommand};
use std::path::Path;

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
    /// Print a seat's unread inbox. Plain drain prints and archives;
    /// `--hold` prints but leases the mail in `draining/` for 30
    /// minutes instead of archiving, and `--done <ID>...` is
    /// `seat done`: the ids are found board-wide.
    Drain {
        #[arg(long)]
        seat: String,
        /// Print the mail but hold it (leased in `draining/` for 30
        /// minutes) instead of archiving it
        #[arg(long)]
        hold: bool,
        /// `seat done <ID>...`: archive these message ids, found
        /// anywhere on the board (each id 8 lowercase hex characters)
        #[arg(long, value_name = "ID", num_args = 1.., conflicts_with = "hold")]
        done: Vec<String>,
    },
    /// Archive held messages by id, found anywhere on the board: each
    /// id 8 lowercase hex, and each id settles on its own — a refused
    /// id (unread, quarantined, ambiguous, unknown) never blocks the
    /// others, and any refusal exits nonzero
    Done {
        /// Message ids to archive (each 8 lowercase hex characters)
        #[arg(value_name = "ID", num_args = 1.., required = true)]
        ids: Vec<String>,
    },
    /// Deliver every routed seat's mail (hapi session or webhook;
    /// `--once` runs one scan pass; unrouted seats are pull)
    Dispatch {
        /// Run one scan pass and wait for its workers
        #[arg(long)]
        once: bool,
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
    /// One row per registered seat: `<name> pane|asleep|awake|unattended mail=<N>`
    Status,
    /// Remove the board under the root (marker, seats/, leftover rooms/)
    Reset,
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
    /// Idle threshold before the dispatcher sleeps this seat's pane
    /// (e.g. 30m, 2h); omitting means never, and a later register
    /// that omits it keeps the stored value
    #[arg(long, value_name = "DURATION")]
    idle_after: Option<String>,
    /// How long mail may sit owed to this seat before the mail
    /// watchdog reports it (e.g. 30m, 2h); omitting means the
    /// watchdog's default, and a later register that omits it keeps
    /// the stored value
    #[arg(long, value_name = "DURATION")]
    mail_limit: Option<String>,
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
            args.idle_after,
            args.mail_limit,
        ),
        Command::Send { from, to, body } => {
            verbs::cmd_send(root, from.as_deref(), &to, &body, session)
        }
        Command::Drain { seat, hold, done } => {
            if hold {
                let mut out = std::io::stdout().lock();
                drain::hold(root, &seat, &mut out)
            } else if !done.is_empty() {
                // `--done` is `seat done`: the `--seat` is still
                // checked (an unknown or corrupt seat keeps its named
                // error), and its name rides the ack's trace line.
                board::ensure_board(root)?;
                seat::read_meta(&seat::seat_dir(root, &seat))?;
                let mut err = std::io::stderr();
                drain::done(root, Some(&seat), &done, &mut err)
            } else {
                let mut out = std::io::stdout().lock();
                drain::drain(root, &seat, &mut out)
            }
        }
        Command::Done { ids } => {
            let mut err = std::io::stderr();
            drain::done(root, None, &ids, &mut err)
        }
        Command::Dispatch { once } => dispatch::cmd_dispatch(root, once),
        Command::Up { names } => session::cmd_up(root, &names),
        Command::Down { names } => session::cmd_down(root, &names),
        Command::Gallery => session::cmd_gallery(root),
        Command::Status => session::cmd_status(root),
        Command::Reset => verbs::cmd_reset(root),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The drain flags parse exactly as the verb is spelled: `--hold`
    /// alone, `--done` with one or many ids; `--hold` and `--done`
    /// together are refused by the parser, and `--done` with no id is
    /// a parse error, not an empty done. `seat done` parses with one
    /// or many ids, and none is a parse error too.
    #[test]
    fn drain_hold_and_done_flags_parse_as_spelled() {
        let ok = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("seat").chain(args.iter().copied())).is_ok()
        };
        assert!(ok(&["drain", "--seat", "a", "--hold"]));
        assert!(ok(&["drain", "--seat", "a", "--done", "ab12cd34"]));
        assert!(ok(&[
            "drain", "--seat", "a", "--done", "ab12cd34", "ef567890"
        ]));
        assert!(
            !ok(&["drain", "--seat", "a", "--hold", "--done", "ab12cd34"]),
            "--hold and --done together must be refused by the parser"
        );
        assert!(
            !ok(&["drain", "--seat", "a", "--done"]),
            "--done with no id is a parse error"
        );
        assert!(ok(&["done", "ab12cd34"]));
        assert!(ok(&["done", "ab12cd34", "ef567890"]));
        assert!(
            !ok(&["done"]),
            "done with no id is a parse error"
        );
    }

    /// `drain --seat X --done` is `seat done`: the seat is checked
    /// (an unknown seat keeps its named error), then the id is found
    /// board-wide — an ack under the wrong seat archives the right
    /// file.
    #[test]
    fn drain_done_acks_board_wide_after_checking_the_seat() {
        use crate::msg;
        use crate::seat;
        use crate::testutil::temp_root;
        use crate::verbs;
        let root = temp_root("main-drain-done");
        crate::board::init(&root).unwrap();
        verbs::cmd_register(&root, "alpha", "sb", None, None, Vec::new(), None, None).unwrap();
        verbs::cmd_register(&root, "beta", "sb", None, None, Vec::new(), None, None).unwrap();
        verbs::cmd_send(&root, Some("alpha"), "beta", "wrong seat ack", None).unwrap();
        let mut out = Vec::new();
        drain::hold(&root, "beta", &mut out).unwrap();
        let staged = msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("draining")).unwrap();
        assert_eq!(staged.len(), 1);
        let name = staged[0].file_name().unwrap().to_str().unwrap().to_string();
        let id = name
            .strip_suffix(".msg")
            .unwrap()
            .rsplit_once('-')
            .unwrap()
            .1
            .to_string();

        dispatch(
            &root,
            Command::Drain {
                seat: "alpha".into(),
                hold: false,
                done: vec![id],
            },
            None,
        )
        .unwrap();
        assert!(
            msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("draining"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            msg::list_msg_dir(&seat::seat_dir(&root, "beta").join("archive"))
                .unwrap()
                .len(),
            1
        );

        // An unknown --seat keeps its named error.
        let err = dispatch(
            &root,
            Command::Drain {
                seat: "ghost".into(),
                hold: false,
                done: vec!["ab12cd34".into()],
            },
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("ghost"), "{err}");
    }
}
