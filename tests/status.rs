//! Full CLI characterization on throwaway boards; no live tmux or hub.
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Board(PathBuf);

impl Board {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "seat-status-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_seat"))
            .arg("--root")
            .arg(&self.0)
            .args(args)
            .env(
                "SEAT_TMUX",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-tmux.sh"),
            )
            .env("SEAT_FAKE_STATE", self.0.join("tmux.state"))
            .env_remove("HAPI_SESSION_ID")
            .output()
            .unwrap()
    }

    fn setup(&self, args: &[&str]) {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert_eq!(out.stderr, b"");
    }

    fn assert_status(&self, code: i32, stdout: &str, stderr: &str) {
        let before = contents(&self.0);
        let out = self.run(&["status"]);
        assert_eq!(out.status.code(), Some(code));
        assert_eq!(out.stdout, stdout.as_bytes());
        assert_eq!(out.stderr, stderr.as_bytes());
        assert_eq!(contents(&self.0), before, "status must not alter the board");
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// Include directories and every file's bytes, including any dispatch log.
fn contents(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let key = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(key, None);
                visit(root, &path, out);
            } else {
                out.insert(key, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[test]
fn status_characterizes_presence_and_unchanged_files() {
    let board = Board::new("presence");
    board.setup(&["init"]);
    fs::write(board.0.join("tmux.state"), "").unwrap();
    board.assert_status(0, "", "");

    let now = chrono::Utc::now();
    let delivery = |session: &str, at: String| {
        json!({"last_delivery": {"session": session, "invoked_at": at}})
    };
    let recent = delivery("s1", (now - chrono::TimeDelta::minutes(1)).to_rfc3339());
    let mut asleep = recent.clone();
    asleep["asleep"] = json!({"at": "2026-10-23T00:00:00Z", "reason": "idle 30m"});
    let cases = [
        ("unreadable", None),
        ("unbound", Some(recent.clone())),
        (
            "stale",
            Some(delivery(
                "s1",
                (now - chrono::TimeDelta::minutes(11)).to_rfc3339(),
            )),
        ),
        ("sleeping", Some(asleep.clone())),
        ("paned", Some(asleep)),
        ("moved", Some(delivery("old-session", now.to_rfc3339()))),
        ("missing", None),
        (
            "future",
            Some(delivery(
                "s1",
                (now + chrono::TimeDelta::minutes(11)).to_rfc3339(),
            )),
        ),
        ("empty", Some(json!({}))),
        ("broken", None),
        ("bad-time", Some(delivery("s1", "not a timestamp".into()))),
        ("awake", Some(recent)),
    ];
    let mut bindings = serde_json::Map::new();
    for (name, state) in cases {
        board.setup(&["register", "--seat", name, "--harness", "sb"]);
        if name != "unbound" {
            bindings.insert(
                name.into(),
                json!({"url": "https://example.invalid/sessions/s1"}),
            );
        }
        if let Some(state) = state {
            fs::write(
                board.0.join("seats").join(name).join("dispatch.json"),
                state.to_string(),
            )
            .unwrap();
        }
    }
    fs::write(
        board.0.join("hapi-sessions.json"),
        json!({"schema_version": 1, "seats": bindings}).to_string(),
    )
    .unwrap();
    fs::write(board.0.join("seats/broken/dispatch.json"), "{broken").unwrap();
    fs::create_dir(board.0.join("seats/unreadable/dispatch.json")).unwrap();
    fs::write(
        board.0.join("tmux.state"),
        "P|swarm|paned|%1\nO|%1|@seat|paned\n",
    )
    .unwrap();
    for (name, count) in [("paned", 1), ("sleeping", 1), ("awake", 1), ("missing", 2)] {
        for n in 0..count {
            fs::write(
                board.0.join(format!("seats/{name}/inbox/{n}.msg")),
                "unread mail",
            )
            .unwrap();
        }
    }
    fs::write(board.0.join("seats/empty/inbox/ignored.txt"), "not mail").unwrap();
    let expected = concat!(
        "awake awake mail=1\n",
        "bad-time unattended mail=0\n",
        "broken unattended mail=0\n",
        "empty unattended mail=0\n",
        "future unattended mail=0\n",
        "missing unattended mail=2\n",
        "moved unattended mail=0\n",
        "paned pane mail=1\n",
        "sleeping asleep mail=1\n",
        "stale unattended mail=0\n",
        "unbound unattended mail=0\n",
        "unreadable unattended mail=0\n",
    );
    board.assert_status(0, expected, "");

    // No bindings (missing, malformed or wrong schema) means no awake seat.
    let unbound = expected.replacen("awake awake", "awake unattended", 1);
    fs::remove_file(board.0.join("hapi-sessions.json")).unwrap();
    board.assert_status(0, &unbound, "");
    for raw in ["{broken", "{\"schema_version\":2,\"seats\":{}}"] {
        fs::write(board.0.join("hapi-sessions.json"), raw).unwrap();
        board.assert_status(0, &unbound, "");
    }
}

#[test]
fn status_characterizes_board_errors() {
    let board = Board::new("errors");
    board.assert_status(
        1,
        "",
        &format!(
            "seat: unrecognized root {}: marker is missing or not 'seat' (only `seat reset` may touch it)\n",
            board.0.display()
        ),
    );
    let missing = board.0.join("missing");
    let out = Command::new(env!("CARGO_BIN_EXE_seat"))
        .arg("--root")
        .arg(&missing)
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"");
    assert_eq!(
        out.stderr,
        format!(
            "seat: no seat board at {} (run `seat init` or set SEAT_ROOT)\n",
            missing.display()
        )
        .as_bytes()
    );
    assert!(!missing.exists());
}
