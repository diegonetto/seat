// Test-only helpers. Tests never touch the operator's live board: every
// helper returns a throwaway directory under the system temp dir and
// refuses to hand out the documented default root.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

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
