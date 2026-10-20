//! The identity rule for `seat send`.
//!
//! hapi sessions are bound to seats by `<root>/hapi-sessions.json`
//! (written by `seat-role`, not by this crate):
//! `{"schema_version": 1, "seats": {"<seat>": {"url":
//! "https://hapi.bll.dev/sessions/<uuid>", "moved_at": "..."}}}`.
//! The session id is the URL's last path segment. A missing,
//! unreadable, or schema-mismatching file (including a
//! `schema_version` other than 1) means no bindings.
//!
//! The rule, given the caller's `HAPI_SESSION_ID` (read once at the
//! CLI edge and passed in — this module never touches the process
//! environment):
//!
//! - A session id bound to seat X may send only as X: an omitted
//!   `--from` means X, and any other `--from` is refused with one line
//!   naming both seats.
//! - Anyone else (no session id, or an id bound to no seat: services,
//!   bridges, an ad-hoc terminal) sends with an explicit `--from`
//!   exactly as before. On `send`, an omitted `--from` means Diego:
//!   the message goes out as `inbox` and may go only to a factory seat
//!   (a seat present in hapi-sessions.json), because the Grok bots
//!   trust `from=inbox` as verified hq mail.
//! - Only an unbound sender that has a session id writes the optional
//!   `session` header field (covered by the HMAC; a bound session is
//!   identified by its seat and writes none).
//!
//! Threat model: this prevents mistakes on one Unix user, not a
//! deliberate forgery. Anyone who can run `seat` as this user can read
//! the seat tokens and forge any sender regardless of this rule.
use crate::error::{Result, SeatError};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The bindings file name at the board root.
pub const BINDINGS_FILE: &str = "hapi-sessions.json";

/// The seat the omitted `--from` means on send: hq's Inbox bridge.
pub const INBOX: &str = "inbox";

#[derive(Debug, Deserialize)]
struct BindingsFile {
    schema_version: u64,
    seats: BTreeMap<String, BindingEntry>,
}

#[derive(Debug, Deserialize)]
struct BindingEntry {
    url: String,
    // The schema also carries `moved_at`; it is not read here (unknown
    // fields are ignored by the parse, so files with it still load).
}

/// seat name → session id (the URL's last path segment), parsed from
/// hapi-sessions.json. Missing/unreadable/mismatched file → empty.
#[derive(Debug, Default, Clone)]
pub struct Bindings {
    seats: BTreeMap<String, String>,
}

/// The session id of a session URL: its last non-empty path segment.
pub fn session_id_from_url(url: &str) -> Option<String> {
    url.rsplit('/').find(|s| !s.is_empty()).map(str::to_string)
}

impl Bindings {
    /// Read `<root>/hapi-sessions.json`. Any read or parse failure —
    /// including a schema that does not match — means no bindings,
    /// never an error.
    pub fn load(root: &Path) -> Bindings {
        let raw = match std::fs::read_to_string(root.join(BINDINGS_FILE)) {
            Ok(r) => r,
            Err(_) => return Bindings::default(),
        };
        let parsed: BindingsFile = match serde_json::from_str(&raw) {
            Ok(f) => f,
            Err(_) => return Bindings::default(),
        };
        if parsed.schema_version != 1 {
            return Bindings::default();
        }
        Bindings {
            seats: parsed
                .seats
                .into_iter()
                .filter_map(|(seat, entry)| session_id_from_url(&entry.url).map(|id| (seat, id)))
                .collect(),
        }
    }

    /// Is this seat present in the bindings file (a factory seat)?
    pub fn contains(&self, seat: &str) -> bool {
        self.seats.contains_key(seat)
    }

    /// The hapi session id a seat is bound to, if any (the adapter's
    /// `GET/POST /api/sessions/<id>` target).
    pub fn session_id(&self, seat: &str) -> Option<&str> {
        self.seats.get(seat).map(|s| s.as_str())
    }

    /// The seat a session id is bound to, if any.
    pub fn bound_seat(&self, session_id: &str) -> Option<&str> {
        self.seats
            .iter()
            .find(|(_, id)| id.as_str() == session_id)
            .map(|(seat, _)| seat.as_str())
    }
}

/// The resolved sender for one outgoing message.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The seat the message is sent as.
    pub from: String,
    /// The optional `session` header value to write (an unbound
    /// sender's session id; `None` for bound senders and callers
    /// without one).
    pub session: Option<String>,
}

/// Resolve the sender for `seat send`.
pub fn resolve_for_send(
    root: &Path,
    explicit_from: Option<&str>,
    env_session: Option<&str>,
    to: &str,
) -> Result<Identity> {
    let bindings = Bindings::load(root);
    if let Some(bound) = env_session.and_then(|id| bindings.bound_seat(id)) {
        bound_branch(bound, explicit_from)
    } else {
        match explicit_from {
            Some(from) => Ok(unbound(from, env_session)),
            None => {
                if bindings.contains(to) {
                    Ok(unbound(INBOX, env_session))
                } else {
                    Err(SeatError::Identity(format!(
                        "no --from given: sending as {INBOX} is limited to factory seats (those in {BINDINGS_FILE}); '{to}' is not one"
                    )))
                }
            }
        }
    }
}

/// A bound session: `--from` must be its own seat (omitted means it).
fn bound_branch(bound: &str, explicit_from: Option<&str>) -> Result<Identity> {
    match explicit_from {
        None => Ok(Identity {
            from: bound.to_string(),
            session: None,
        }),
        Some(f) if f == bound => Ok(Identity {
            from: bound.to_string(),
            session: None,
        }),
        Some(other) => Err(SeatError::Identity(format!(
            "this session is bound to {bound}; it cannot send as {other}"
        ))),
    }
}

/// An unbound caller: the explicit seat as given, plus the session
/// header when the caller has a session id.
fn unbound(from: &str, env_session: Option<&str>) -> Identity {
    Identity {
        from: from.to_string(),
        session: env_session.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{temp_root, write_bindings};

    fn write_bindings_file(root: &Path, text: &str) {
        std::fs::write(root.join(BINDINGS_FILE), text).unwrap();
    }

    const OPUS: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const FACTORY: &str = "11111111-2222-3333-4444-555555555555";

    fn sample(root: &Path) {
        write_bindings(root, &[("opus-main", OPUS), ("bll-factory", FACTORY)]);
    }

    #[test]
    fn session_id_is_the_last_url_segment() {
        assert_eq!(
            session_id_from_url("https://hapi.bll.dev/sessions/abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            session_id_from_url("https://hapi.bll.dev/sessions/").as_deref(),
            Some("sessions")
        );
        assert_eq!(session_id_from_url(""), None);
    }

    #[test]
    fn missing_unreadable_or_mismatched_file_means_no_bindings() {
        let root = temp_root("identity-none");
        assert_eq!(Bindings::load(&root).bound_seat("x"), None);
        // Unparseable JSON.
        write_bindings_file(&root, "{ nope");
        assert!(Bindings::load(&root).bound_seat(OPUS).is_none());
        // Wrong schema version.
        write_bindings_file(
            &root,
            r#"{"schema_version": 2, "seats": {"opus-main": {"url": "https://hapi.bll.dev/sessions/x"}}}"#,
        );
        assert!(Bindings::load(&root).bound_seat("x").is_none());
        // Not the binding shape at all.
        write_bindings_file(&root, r#"{"schema_version": 1, "seats": []}"#);
        assert!(Bindings::load(&root).bound_seat("x").is_none());
        // An entry without a url invalidates the file (no bindings).
        write_bindings_file(
            &root,
            r#"{"schema_version": 1, "seats": {"opus-main": {"moved_at": "x"}}}"#,
        );
        assert!(Bindings::load(&root).bound_seat("x").is_none());
    }

    #[test]
    fn bindings_map_session_ids_to_seats() {
        let root = temp_root("identity-map");
        sample(&root);
        let b = Bindings::load(&root);
        assert_eq!(b.bound_seat(OPUS), Some("opus-main"));
        assert_eq!(b.bound_seat(FACTORY), Some("bll-factory"));
        assert_eq!(b.bound_seat("no-such-session"), None);
        assert!(b.contains("opus-main"));
        assert!(!b.contains("alpha"));
        assert_eq!(b.session_id("opus-main"), Some(OPUS));
        assert_eq!(b.session_id("alpha"), None);
    }

    // The identity-rule cases themselves (bound as self, bound as
    // another seat refused, unbound explicit, unbound omitted → inbox)
    // are covered end to end by the `cmd_send` tests in verbs.rs.
}
