// Test-only helpers. Tests never touch the operator's live board: every
// helper returns a throwaway directory under the system temp dir and
// refuses to hand out the documented default root.
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// One process-wide lock for every test that sets process-wide
/// environment (SEAT_TMUX, SEAT_FAKE_STATE, PATH, SEAT_KIT,
/// HAPI_API_URL, CLI_API_TOKEN, SEAT_ROLE_HOME, SEAT_ROOT,
/// HOME): env is shared by all parallel test threads, and the
/// stubs these tests install are visible to every subprocess this
/// process spawns.
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock a process-wide test mutex, treating poison as acquired: the
/// panic that poisoned it is already reported, and a later test that
/// needs the same lock must still run.
pub fn lock_despite_poison(
    lock: &'static std::sync::Mutex<()>,
) -> std::sync::MutexGuard<'static, ()> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// ENV_LOCK taken for the caller's scope (see ENV_LOCK and
/// lock_despite_poison above).
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    lock_despite_poison(&ENV_LOCK)
}

/// The one RAII guard for a test that sets process-wide environment:
/// `lock_env` constructs it holding ENV_LOCK, and drop restores every
/// variable it touched to the value (or absence) saved when the name
/// was first touched — on return and on unwind, before the lock is
/// released.
///
/// Composition: one guard per scope. ENV_LOCK does not reenter, so
/// never construct a second guard (or call `env_lock`) while one is
/// live — a helper that must touch more variables takes `&mut
/// EnvGuard` instead (`with_throwaway_home` composes exactly that
/// way, HOME being one more guarded variable).
pub struct EnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    /// Set one more variable under the held lock. The previous value
    /// (or absence) is saved the first time a name is touched, so a
    /// later reset never loses the original.
    pub fn set(&mut self, name: &'static str, value: OsString) {
        self.save(name);
        std::env::set_var(name, value);
    }

    /// Remove one variable under the held lock, its previous value
    /// (or absence) saved for drop.
    pub fn unset(&mut self, name: &'static str) {
        self.save(name);
        std::env::remove_var(name);
    }

    /// Restore one variable now to the value (or absence) saved when
    /// it was first touched, and forget it: drop will not restore it
    /// again.
    pub fn restore(&mut self, name: &'static str) {
        if let Some(i) = self.saved.iter().position(|(n, _)| *n == name) {
            let (_, value) = self.saved.remove(i);
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }

    fn save(&mut self, name: &'static str) {
        if !self.saved.iter().any(|(n, _)| *n == name) {
            self.saved.push((name, std::env::var_os(name)));
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Runs before the _lock field drops, so the environment is
        // whole again before another test can take ENV_LOCK.
        for (name, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Hold ENV_LOCK for the caller's scope and set each variable,
/// restoring the previous value (or absence) of every one on drop,
/// even on panic. The stubs a test installs are visible to every
/// subprocess this process spawns, so they never run beside a test
/// that reads them.
pub fn lock_env(vars: &[(&'static str, OsString)]) -> EnvGuard {
    let lock = env_lock();
    let mut env = EnvGuard {
        saved: Vec::new(),
        _lock: lock,
    };
    for (name, value) in vars {
        env.set(name, value.clone());
    }
    env
}

pub fn temp_root(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "seat-test-{tag}-{}-{n}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp board root");
    assert_throwaway(&dir);
    dir
}

/// Constitution guard: a test root must never be the default (possibly
/// live) board location.
pub fn assert_throwaway(root: &std::path::Path) {
    let default = crate::board::default_root();
    assert!(
        root != default,
        "test board root {root:?} must not be the default live board {default:?}"
    );
    assert!(
        root.starts_with(std::env::temp_dir()),
        "test board root {root:?} must live under the system temp dir"
    );
}

/// Point `HOME` at a throwaway directory for the duration of `body`,
/// so nothing the test runs can read the operator's
/// `~/.hapi/settings.json` — the frozen alarm contract's requirement:
/// even a path that discovers hub settings mid-test finds the
/// throwaway, never the operator's. HOME is one more variable of the
/// caller's `EnvGuard` (one ENV_LOCK acquisition per scope: a caller
/// with no guard constructs one and passes it in). The helper sets
/// HOME through the guard, `body` may change it again through the
/// `&mut EnvGuard` it receives, and the previous value (or absence)
/// is back when the body returns or unwinds — the helper's own
/// `restore` first, the guard's drop behind it.
pub fn with_throwaway_home<T>(env: &mut EnvGuard, body: impl FnOnce(&mut EnvGuard) -> T) -> T {
    let home = temp_root("home");
    env.set("HOME", home.into_os_string());
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(env)));
    env.restore("HOME");
    match out {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Poll `pred` every 10ms until it holds; `msg` fails the test when
/// 3s pass first (the threads the cases spawn need a moment).
pub fn wait_for(pred: impl Fn() -> bool, msg: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !pred() {
        assert!(std::time::Instant::now() < deadline, "{msg}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The fake multiplexer behind SEAT_TMUX: the committed
/// tests/fake-tmux.sh, whose state file doubles as the record of
/// every call it answered.
pub fn fake_tmux() -> String {
    let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-tmux.sh");
    assert!(Path::new(p).is_file(), "fake-tmux.sh must exist at {p}");
    p.to_string()
}

/// Write a throwaway `<root>/hapi-sessions.json` binding each seat to
/// a session id (the id becomes the url's last path segment), the same
/// shape seat-role writes.
pub fn write_bindings(root: &std::path::Path, seats: &[(&str, &str)]) {
    let mut map = serde_json::Map::new();
    for (seat, id) in seats {
        map.insert(
            seat.to_string(),
            serde_json::json!({
                "url": format!("https://hapi.bll.dev/sessions/{id}"),
                "moved_at": "2026-10-19T00:00:00Z",
            }),
        );
    }
    let doc = serde_json::json!({ "schema_version": 1, "seats": map });
    std::fs::write(root.join(crate::identity::BINDINGS_FILE), doc.to_string())
        .expect("write test bindings");
}

/// A tiny scriptable HTTP hub on 127.0.0.1 (ephemeral port, a thread
/// per connection, a shared control block). Tests point the hub config
/// straight at it; the live hub is never touched.
pub mod fakehub {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

        /// The scripted behavior: sessions (id → active, absent = 404),
        /// exact GET bodies (id → raw JSON, overriding the active flag),
        /// marks (localId → "queued"|"indeterminate"|"invoked"; no entry =
        /// the hub lost it), hung sessions, banned JWTs, sessions to flip
        /// inactive when queued-state is first asked, localIds whose
        /// message POST answers 500, session GETs to answer 500 (the
        /// sleep rule must read that as an error, not a quiet session),
        /// queued-state POSTs to answer 500, each POSTed localId → text.
        #[derive(Default)]
        pub struct HubCtl {
            pub sessions: BTreeMap<String, bool>,
            pub get_bodies: BTreeMap<String, String>,
            pub marks: BTreeMap<String, String>,
            pub hang: BTreeSet<String>,
            pub banned_jwts: BTreeSet<String>,
            pub flip_inactive: BTreeSet<String>,
            pub fail_keys: BTreeSet<String>,
            pub fail_get: BTreeSet<String>,
            pub fail_queued_state: BTreeSet<String>,
            pub texts: BTreeMap<String, String>,
            auths: usize,
            journal: Vec<String>,
        }

    pub struct FakeHub {
        pub ctl: Arc<Mutex<HubCtl>>,
        addr: std::net::SocketAddr,
    }

    impl FakeHub {
        /// Serve on 127.0.0.1:0 until the process exits.
        pub fn start() -> FakeHub {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake hub");
            let addr = listener.local_addr().expect("fake hub addr");
            let ctl = Arc::new(Mutex::new(HubCtl::default()));
            let accept = Arc::clone(&ctl);
            std::thread::spawn(move || {
                for conn in listener.incoming().flatten() {
                    let ctl = Arc::clone(&accept);
                    std::thread::spawn(move || handle(conn, ctl));
                }
            });
            FakeHub { ctl, addr }
        }

        /// The hub config pointing at this fake.
        pub fn config(&self) -> crate::hub::HubConfig {
            crate::hub::HubConfig {
                base: format!("http://{}", self.addr),
                token: "test-token".into(),
            }
        }

        pub fn journal(&self) -> Vec<String> {
            self.ctl.lock().unwrap().journal.clone()
        }

        /// Auth exchanges so far (each mints `jwt-<n>`).
        pub fn auth_count(&self) -> usize {
            self.ctl.lock().unwrap().auths
        }
    }

    fn respond(stream: &mut TcpStream, status: u16, body: &str) {
        let reason = match status {
            200 => "OK",
            401 => "Unauthorized",
            _ => "Not Found",
        };
        let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = stream.flush();
    }

    fn handle(stream: TcpStream, ctl: Arc<Mutex<HubCtl>>) {
        let mut writer = stream.try_clone().expect("clone fake hub stream");
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() {
            return;
        }
        let mut it = request.split_whitespace();
        let method = it.next().unwrap_or_default().to_string();
        let path = it.next().unwrap_or_default().to_string();
        let (mut auth, mut len) = (String::new(), 0usize);
        for line in reader.by_ref().lines() {
            let line = line.unwrap_or_default();
            if line.is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("authorization: bearer ") {
                auth = v.trim().to_string();
            } else if let Some(v) = lower.strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; len];
        if len > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let json: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);

        let mut ctl = ctl.lock().unwrap();
        ctl.journal.push(format!("{method} {path}"));
        if path == "/api/auth" {
            ctl.auths += 1;
            respond(
                &mut writer,
                200,
                &format!("{{\"token\":\"jwt-{}\"}}", ctl.auths),
            );
            return;
        }
        if ctl.banned_jwts.contains(&auth) {
            respond(&mut writer, 401, "{}");
            return;
        }
        let Some(rest) = path.strip_prefix("/api/sessions/") else {
            respond(&mut writer, 404, "{}");
            return;
        };
        let (session, action) = match rest.split_once('/') {
            Some((s, a)) => (s.to_string(), a),
            None => (rest.to_string(), ""),
        };
        if ctl.hang.contains(&session) {
            drop(ctl); // never answer: the client's own timeout ends it
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }
        match (method.as_str(), action) {
            ("GET", "") => {
                if ctl.fail_get.contains(&session) {
                    respond(&mut writer, 500, "{}");
                } else if let Some(raw) = ctl.get_bodies.get(&session) {
                    respond(&mut writer, 200, raw);
                } else {
                    // The live hub's shape — the flag nested under
                    // `session`. A plain top-level `active` here once
                    // hid the parse the dispatcher runs against the
                    // real hub.
                    match ctl.sessions.get(&session) {
                        Some(true) => respond(&mut writer, 200, "{\"session\":{\"active\":true}}"),
                        Some(false) => {
                            respond(&mut writer, 200, "{\"session\":{\"active\":false}}")
                        }
                        None => respond(&mut writer, 404, "{}"),
                    }
                }
            }
            ("POST", "messages") => {
                let key = json["localId"].as_str().unwrap_or_default().to_string();
                ctl.journal.push(format!("POST-MSG {session} {key}"));
                if ctl.fail_keys.contains(&key) {
                    respond(&mut writer, 500, "{}");
                    return;
                }
                ctl.texts
                    .insert(key, json["text"].as_str().unwrap_or_default().to_string());
                respond(&mut writer, 200, "{}");
            }
            ("POST", "messages/queued-state") => {
                if ctl.fail_queued_state.contains(&session) {
                    respond(&mut writer, 500, "{}");
                    return;
                }
                if ctl.flip_inactive.contains(&session) {
                    ctl.sessions.insert(session.clone(), false);
                }
                let asked: Vec<String> = json["localIds"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                let marked = |m: &str| {
                    asked
                        .iter()
                        .filter(|k| ctl.marks.get(*k).map(|s| s == m).unwrap_or(false))
                        .cloned()
                        .collect::<Vec<String>>()
                };
                // `invokedLocalMessages` carries objects — the shape the
                // parser must survive (a plain-string fake would pass
                // while a real hub re-POSTed forever).
                let invoked: Vec<_> = marked("invoked")
                    .into_iter()
                    .map(|k| serde_json::json!({ "localId": k }))
                    .collect();
                respond(
                    &mut writer,
                    200,
                    &serde_json::json!({
                        "queuedLocalIds": marked("queued"),
                        "indeterminateLocalIds": marked("indeterminate"),
                        "invokedLocalMessages": invoked,
                    })
                    .to_string(),
                );
            }
            _ => respond(&mut writer, 404, "{}"),
        }
    }
}

/// A tiny webhook receiver on 127.0.0.1 (ephemeral port, a thread per
/// connection): it records every request's header block and body
/// exactly as sent, and answers with the scripted status (0 = 200).
/// Tests point the webhook config straight at it; the live panel is
/// never touched.
pub mod fakewebhook {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct WebCtl {
        /// Answer every request with this status (0 means 200).
        pub status: u16,
        /// (header block, body) per request, in order.
        pub hits: Vec<(String, String)>,
    }

    pub struct FakeWebhook {
        pub ctl: Arc<Mutex<WebCtl>>,
        addr: std::net::SocketAddr,
    }

    impl FakeWebhook {
        /// Serve on 127.0.0.1:0 until the process exits.
        pub fn start() -> FakeWebhook {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake webhook");
            let addr = listener.local_addr().expect("fake webhook addr");
            let ctl = Arc::new(Mutex::new(WebCtl::default()));
            let accept = Arc::clone(&ctl);
            std::thread::spawn(move || {
                for conn in listener.incoming().flatten() {
                    let ctl = Arc::clone(&accept);
                    std::thread::spawn(move || handle(conn, ctl));
                }
            });
            FakeWebhook { ctl, addr }
        }

        pub fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        pub fn hits(&self) -> Vec<(String, String)> {
            self.ctl.lock().unwrap().hits.clone()
        }
    }

    fn handle(stream: TcpStream, ctl: Arc<Mutex<WebCtl>>) {
        let mut writer = stream.try_clone().expect("clone fake webhook stream");
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        if reader.read_line(&mut request).is_err() {
            return;
        }
        let (mut headers, mut len) = (String::new(), 0usize);
        for line in reader.by_ref().lines() {
            let line = line.unwrap_or_default();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            // Header names normalize to lowercase (senders differ);
            // values stay byte for byte.
            match line.split_once(':') {
                Some((name, value)) => {
                    headers.push_str(&format!("{}:{value}\n", name.to_ascii_lowercase()))
                }
                None => headers.push_str(&line),
            }
        }
        let mut body = vec![0u8; len];
        if len > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let status = {
            let mut c = ctl.lock().unwrap();
            c.hits
                .push((headers, String::from_utf8_lossy(&body).into_owned()));
            if c.status == 0 {
                200
            } else {
                c.status
            }
        };
        let ok = (200..300).contains(&status);
        let _ = write!(
            writer,
            "HTTP/1.1 {status} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            if ok { "OK" } else { "Fail" }
        );
        let _ = writer.flush();
    }
}

/// Shared helpers for the dispatcher tests: a kit per case (board,
/// fake hub, client) and the small readers the cases use.
pub mod dispatchkit {
    use super::fakehub::FakeHub;
    use super::{temp_root, write_bindings};
    use crate::hub::Hub;
    use crate::{board, dstate, msg, seat, verbs};
    use std::path::{Path, PathBuf};

    /// A hapi-bound board, a fake hub with the given active sessions,
    /// and one client — the opening three lines of every case.
    pub fn kit(seats: &[(&str, &str)], active: &[&str]) -> (PathBuf, FakeHub, Hub) {
        let root = board_with(seats);
        let hub = FakeHub::start();
        {
            let mut ctl = hub.ctl.lock().unwrap();
            for s in active {
                ctl.sessions.insert((*s).into(), true);
            }
        }
        let client = Hub::new(hub.config());
        (root, hub, client)
    }

    /// A board whose seats are hapi-bound to the given session ids.
    pub fn board_with(seats: &[(&str, &str)]) -> PathBuf {
        let root = temp_root("dispatch");
        board::init(&root).unwrap();
        for (seat, _) in seats {
            verbs::cmd_register(&root, seat, "sb", None, None, Vec::new(), None).unwrap();
        }
        write_bindings(&root, seats);
        root
    }

    pub fn send(root: &Path, to: &str, body: &str) {
        verbs::cmd_send(root, Some(to), to, body, None).unwrap();
    }

    /// Mark the hub's queued-state answer for one localId.
    pub fn mark(hub: &super::fakehub::FakeHub, key: &str, m: &str) {
        hub.ctl.lock().unwrap().marks.insert(key.into(), m.into());
    }

    /// The delivery key of the seat's single inbox message.
    pub fn only_key(root: &Path, seat: &str) -> String {
        let inbox = seat::seat_dir(root, seat).join("inbox");
        let files = msg::list_msg_dir(&inbox).unwrap();
        assert_eq!(files.len(), 1);
        dstate::direct_key(seat, files[0].file_name().unwrap().to_str().unwrap())
    }

    pub fn log_text(root: &Path) -> String {
        std::fs::read_to_string(root.join("dispatch.log")).unwrap_or_default()
    }

    pub fn n_files(root: &Path, seat: &str, dir: &str) -> usize {
        msg::list_msg_dir(&seat::seat_dir(root, seat).join(dir))
            .unwrap()
            .len()
    }

    pub fn n_posts(journal: &[String]) -> usize {
        journal.iter().filter(|l| l.starts_with("POST-MSG")).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poisoned ENV_LOCK still yields its guard: the panic that
    /// poisoned it is already reported, and every later test that
    /// takes the lock must still run.
    #[test]
    fn env_lock_survives_poison() {
        let _ = std::panic::catch_unwind(|| {
            let _g = env_lock();
            panic!("poison ENV_LOCK for the characterization");
        });
        let _recovered = env_lock();
    }

    /// `unset` removes a set variable and restores it on drop — the
    /// present and the absent pre-value both — and the same holds
    /// when the scope unwinds. The probe names are read by no other
    /// test, so the unguarded setup races no one.
    #[test]
    fn env_guard_unset_removes_and_restores_on_return() {
        std::env::set_var("SEAT_GUARD_TEST_UNSET_PRESENT", "original");
        std::env::remove_var("SEAT_GUARD_TEST_UNSET_ABSENT");
        {
            let mut env = lock_env(&[]);
            env.unset("SEAT_GUARD_TEST_UNSET_PRESENT");
            env.unset("SEAT_GUARD_TEST_UNSET_ABSENT");
            assert_eq!(std::env::var_os("SEAT_GUARD_TEST_UNSET_PRESENT"), None);
            assert_eq!(std::env::var_os("SEAT_GUARD_TEST_UNSET_ABSENT"), None);
        }
        assert_eq!(
            std::env::var_os("SEAT_GUARD_TEST_UNSET_PRESENT"),
            Some(OsString::from("original"))
        );
        assert_eq!(std::env::var_os("SEAT_GUARD_TEST_UNSET_ABSENT"), None);
        std::env::remove_var("SEAT_GUARD_TEST_UNSET_PRESENT");
    }

    /// The unset restoration also holds when the scope unwinds: the
    /// guard's Drop runs during the panic, before any later test
    /// reads the env.
    #[test]
    fn env_guard_unset_restores_on_panic() {
        std::env::set_var("SEAT_GUARD_TEST_UNWIND_PRESENT", "original");
        std::env::remove_var("SEAT_GUARD_TEST_UNWIND_ABSENT");
        let unwound = std::panic::catch_unwind(|| {
            let mut env = lock_env(&[]);
            env.unset("SEAT_GUARD_TEST_UNWIND_PRESENT");
            env.unset("SEAT_GUARD_TEST_UNWIND_ABSENT");
            panic!("unwind under the guard");
        });
        assert!(unwound.is_err(), "the body must have unwound");
        assert_eq!(
            std::env::var_os("SEAT_GUARD_TEST_UNWIND_PRESENT"),
            Some(OsString::from("original"))
        );
        assert_eq!(std::env::var_os("SEAT_GUARD_TEST_UNWIND_ABSENT"), None);
        std::env::remove_var("SEAT_GUARD_TEST_UNWIND_PRESENT");
    }

    /// Resetting a variable through the guard keeps the first saved
    /// value: drop restores what the scope started with, not the
    /// last value the guard itself wrote.
    #[test]
    fn env_guard_reset_keeps_the_first_saved_value() {
        std::env::set_var("SEAT_GUARD_TEST_RESET", "original");
        {
            let mut env = lock_env(&[("SEAT_GUARD_TEST_RESET", OsString::from("first"))]);
            env.set("SEAT_GUARD_TEST_RESET", OsString::from("second"));
            assert_eq!(
                std::env::var_os("SEAT_GUARD_TEST_RESET"),
                Some(OsString::from("second")),
                "the reset is live inside the scope"
            );
        }
        assert_eq!(
            std::env::var_os("SEAT_GUARD_TEST_RESET"),
            Some(OsString::from("original")),
            "drop restores the value saved when the name was first touched"
        );
        std::env::remove_var("SEAT_GUARD_TEST_RESET");
    }

    /// Restoration happens before ENV_LOCK is released: the next lock
    /// holder — here this same thread, re-taking the lock the guard
    /// just dropped — already reads the restored environment.
    #[test]
    fn env_guard_restores_before_releasing_the_lock() {
        std::env::set_var("SEAT_GUARD_TEST_ORDER", "original");
        {
            let _env = lock_env(&[("SEAT_GUARD_TEST_ORDER", OsString::from("stub"))]);
        }
        let _next_holder = env_lock();
        assert_eq!(
            std::env::var_os("SEAT_GUARD_TEST_ORDER"),
            Some(OsString::from("original")),
            "the lock changed hands only after the restoration"
        );
        std::env::remove_var("SEAT_GUARD_TEST_ORDER");
    }

    /// The shared fake multiplexer is exactly the committed script
    /// beside the manifest.
    #[test]
    fn fake_tmux_is_the_committed_fake_script() {
        assert_eq!(
            fake_tmux(),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-tmux.sh")
        );
        assert!(Path::new(&fake_tmux()).is_file());
    }

    /// `with_throwaway_home` composes with the caller's guard — one
    /// ENV_LOCK acquisition for the scope — and HOME is the guard's:
    /// a body that unsets HOME itself still leaves the pre-call value
    /// (or absence) once the body returns.
    #[test]
    fn with_throwaway_home_restores_home_through_the_guard() {
        let mut env = lock_env(&[]);
        let prev = std::env::var_os("HOME");
        with_throwaway_home(&mut env, |env| {
            assert_ne!(std::env::var_os("HOME"), prev, "HOME is the throwaway");
            env.unset("HOME");
            assert_eq!(std::env::var_os("HOME"), None);
        });
        assert_eq!(
            std::env::var_os("HOME"),
            prev,
            "the body's changes do not outlive it"
        );
    }

    /// The same restoration holds when the body unwinds: the helper
    /// restores HOME before the panic leaves it.
    #[test]
    fn with_throwaway_home_restores_home_when_the_body_unwinds() {
        let mut env = lock_env(&[]);
        let prev = std::env::var_os("HOME");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_throwaway_home(&mut env, |_| panic!("unwind inside the throwaway home"))
        }));
        assert!(unwound.is_err(), "the body must have unwound");
        assert_eq!(std::env::var_os("HOME"), prev);
    }

    /// wait_for polls a predicate that starts false, then returns
    /// once it holds.
    #[test]
    fn wait_for_returns_once_the_predicate_holds() {
        let tries = AtomicU32::new(0);
        wait_for(
            || tries.fetch_add(1, Ordering::SeqCst) > 0,
            "the predicate never held",
        );
        assert!(tries.load(Ordering::SeqCst) >= 2);
    }

    /// On timeout, the caller's message is the whole panic payload,
    /// not a substring of it (the failure path every waiter
    /// inherits).
    #[test]
    fn wait_for_fails_with_the_callers_message_on_timeout() {
        let payload = std::panic::catch_unwind(|| {
            wait_for(|| false, "waited 3s and it never came");
        })
        .expect_err("the 3s deadline must fail the waiter");
        let printed = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied());
        assert_eq!(printed, Some("waited 3s and it never came"));
    }
}
