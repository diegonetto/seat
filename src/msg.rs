use crate::error::{Result, SeatError};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub type HmacSha256 = Hmac<Sha256>;

/// Token files hold 16 random bytes as 32 hex chars (mode 0600, O_EXCL).
pub const TOKEN_HEX_LEN: usize = 32;
/// MAC is the **full** SHA-256 digest as 64 hex chars. Never truncate.
#[allow(dead_code)] // spec constant (FR-002); asserted in the tests below
pub const MAC_HEX_LEN: usize = 64;
/// Message id: 4 random bytes as 8 hex chars.
pub const ID_HEX_LEN: usize = 8;

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub dest: String,
    pub from: String,
    pub id: String,
    pub ts: String,
    pub hmac: String,
    /// Optional sender session id (the unbound sender's
    /// `HAPI_SESSION_ID`). Absent on every message written before the
    /// identity rule and on every bound sender's message; covered by
    /// the HMAC like the other header fields when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// Strip a leading UTF-8 BOM from a body before MACing (write and verify
/// both strip, so a BOM in a body never breaks verification).
pub fn strip_bom(bytes: &[u8]) -> &[u8] {
    match bytes.strip_prefix(BOM) {
        Some(rest) => strip_bom(rest),
        None => bytes,
    }
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// MAC input is `dest|from|id|ts|body` with the body BOM-stripped.
/// When the sender wrote a `session` header, its value is fed to the
/// MAC after the body through the same `|` framing — so the input is
/// exactly `dest|from|id|ts|body|session` when present, and stays
/// byte-identical to the old format when absent (every existing
/// message still verifies).
fn mac_update(
    mac: &mut HmacSha256,
    dest: &str,
    from: &str,
    id: &str,
    ts: &str,
    body: &[u8],
    session: Option<&str>,
) {
    mac.update(dest.as_bytes());
    mac.update(b"|");
    mac.update(from.as_bytes());
    mac.update(b"|");
    mac.update(id.as_bytes());
    mac.update(b"|");
    mac.update(ts.as_bytes());
    mac.update(b"|");
    mac.update(strip_bom(body));
    if let Some(s) = session {
        mac.update(b"|");
        mac.update(s.as_bytes());
    }
}

fn token_key(token_hex: &str) -> Result<Vec<u8>> {
    hex::decode(token_hex.trim()).map_err(|_| SeatError::BadToken("sender".to_string()))
}

/// HMAC-SHA256 **full** hex digest (64 chars), keyed by the sender token.
pub fn compute_hmac(
    token_hex: &str,
    dest: &str,
    from: &str,
    id: &str,
    ts: &str,
    body: &[u8],
    session: Option<&str>,
) -> Result<String> {
    let key = token_key(token_hex)?;
    let mut mac =
        HmacSha256::new_from_slice(&key).map_err(|_| SeatError::BadToken("sender".to_string()))?;
    mac_update(&mut mac, dest, from, id, ts, body, session);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Constant-time verification via `hmac::Mac::verify_slice`. A truncated
/// or malformed MAC claim simply does not verify.
pub fn verify_hmac(
    token_hex: &str,
    dest: &str,
    from: &str,
    id: &str,
    ts: &str,
    body: &[u8],
    session: Option<&str>,
    claimed_hex: &str,
) -> Result<bool> {
    let Ok(tag) = hex::decode(claimed_hex.trim()) else {
        return Ok(false);
    };
    let key = token_key(token_hex)?;
    let mut mac =
        HmacSha256::new_from_slice(&key).map_err(|_| SeatError::BadToken("sender".to_string()))?;
    mac_update(&mut mac, dest, from, id, ts, body, session);
    Ok(mac.verify_slice(&tag).is_ok())
}

/// Mint a seat token: 16 random bytes, hex-encoded (32 hex chars),
/// written mode 0600 with O_CREAT|O_EXCL. Existing token files are an
/// error — tokens are minted exactly once per seat.
pub fn mint_token(path: &Path) -> Result<String> {
    let hex_str = hex::encode(random_bytes(TOKEN_HEX_LEN / 2)?);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(hex_str.as_bytes())?;
    Ok(hex_str)
}

pub fn read_token(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw.trim().to_string())
}

/// Board timestamp used in both filename and header:
/// `YYYYMMDDTHHMMSS.ffffff` (lexicographically sortable).
pub fn now_ts() -> String {
    Utc::now().format("%Y%m%dT%H%M%S%.6f").to_string()
}

pub fn new_id() -> Result<String> {
    Ok(hex::encode(random_bytes(ID_HEX_LEN / 2)?))
}

pub fn filename_for(ts: &str, id: &str) -> String {
    format!("{ts}-{id}.msg")
}

/// Write a `.msg` file: line 1 JSON header, blank line, raw body.
/// `session` is the optional sender-session header field (fed to the
/// MAC after the body); `None` writes none, exactly the old format.
/// Returns the path and the header that was written.
pub fn write_msg(
    dir: &Path,
    dest: &str,
    from: &str,
    token_hex: &str,
    body: &str,
    session: Option<&str>,
) -> Result<(PathBuf, Header)> {
    let ts = now_ts();
    let id = new_id()?;
    let mac = compute_hmac(token_hex, dest, from, &id, &ts, body.as_bytes(), session)?;
    let mut header = Header {
        dest: dest.to_string(),
        from: from.to_string(),
        id: id.clone(),
        ts: ts.clone(),
        hmac: mac,
        session: session.map(str::to_string),
    };
    let mut path = dir.join(filename_for(&ts, &id));
    // Same-microsecond + same random id is vanishingly rare, but never
    // clobber: regenerate until the name is free.
    for _ in 0..8 {
        if !path.exists() {
            break;
        }
        let id2 = new_id()?;
        let ts2 = now_ts();
        let mac2 = compute_hmac(token_hex, dest, from, &id2, &ts2, body.as_bytes(), session)?;
        header = Header {
            hmac: mac2,
            id: id2.clone(),
            ts: ts2.clone(),
            ..header.clone()
        };
        path = dir.join(filename_for(&ts2, &id2));
    }
    let content = format!("{}\n\n{}", serde_json::to_string(&header)?, body);
    // Atomic publish: write a dot-prefixed `.tmp` scratch name in the
    // SAME directory (not ending in `.msg`, so a concurrent drain —
    // which only lists `*.msg` — never sees a half-written message),
    // fsync, then rename into place.
    let tmp = dir.join(format!(".{}.tmp", filename_for(&header.ts, &header.id)));
    let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok((path, header))
}

/// Read a `.msg` file into (header, body). Corrupt headers are a named
/// error, not a panic.
pub fn read_msg(path: &Path) -> Result<(Header, String)> {
    let raw = std::fs::read(path)?;
    let split = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .ok_or_else(|| SeatError::BadBody {
            path: path.to_path_buf(),
        })?;
    let (head, body) = (&raw[..split], &raw[split + 2..]);
    let header: Header = serde_json::from_slice(head).map_err(|source| SeatError::CorruptMsg {
        path: path.to_path_buf(),
        source,
    })?;
    let body = String::from_utf8(body.to_vec()).map_err(|_| SeatError::BadBody {
        path: path.to_path_buf(),
    })?;
    Ok((header, body))
}

/// Read a message and verify its MAC with the sender's token. The
/// header's optional `session` field (absent on old messages) is fed
/// to the MAC when present.
pub fn verify_msg(path: &Path, sender_token: &str) -> Result<bool> {
    let (header, body) = read_msg(path)?;
    verify_hmac(
        sender_token,
        &header.dest,
        &header.from,
        &header.id,
        &header.ts,
        body.as_bytes(),
        header.session.as_deref(),
        &header.hmac,
    )
}

/// List `*.msg` files in a directory, oldest first (filenames are
/// timestamp-prefixed and sortable). A missing directory is an empty
/// inbox, not an error.
pub fn list_msg_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut names: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".msg"))
            })
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SeatError::Io(e)),
    };
    names.sort();
    Ok(names)
}

/// The sortable timestamp prefix of a `.msg` filename
/// (`20260923T071412.123456-ab12cd34.msg` → `20260923T071412.123456`).
pub fn ts_prefix(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.split_once('-').map(|(ts, _)| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_root;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn good_mac_verifies() {
        let root = temp_root("msg-good");
        let token = mint_token(&root.join("token")).unwrap();
        let (path, header) =
            write_msg(&root, "beta", "alpha", &token, "hello board", None).unwrap();

        // Layout: JSON header line, blank line, raw body.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut parts = raw.splitn(3, '\n');
        let head_line = parts.next().unwrap();
        assert!(serde_json::from_str::<Header>(head_line).is_ok());
        assert_eq!(parts.next().unwrap(), "");
        assert_eq!(parts.next().unwrap(), "hello board");

        // Filename: <YYYYMMDDTHHMMSS.ffffff>-<8-hex-id>.msg
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(check_filename(name));

        // Full 64-hex MAC, not truncated.
        assert_eq!(header.hmac.len(), MAC_HEX_LEN);
        assert!(header.hmac.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(verify_msg(&path, &token).unwrap());
    }

    #[test]
    fn write_msg_publishes_atomically_no_tmp_left() {
        let root = temp_root("msg-atomic");
        let token = mint_token(&root.join("token")).unwrap();
        let (path, _) = write_msg(&root, "beta", "alpha", &token, "atomic body", None).unwrap();
        assert!(verify_msg(&path, &token).unwrap());
        // The scratch name is gone: only the final .msg (plus the token)
        // remains, and no non-.msg name lingers for a drain to trip on.
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.ends_with(".msg") && n != "token")
            .collect();
        assert!(
            leftovers.is_empty(),
            "leftover scratch files: {leftovers:?}"
        );
    }

    fn check_filename(name: &str) -> bool {
        // 8 date + 'T' + 6 time + '.' + 6 micros + '-' + 8 hex + ".msg"
        let bytes = name.as_bytes();
        if bytes.len() != 8 + 1 + 6 + 1 + 6 + 1 + 8 + 4 {
            return false;
        }
        if &name[8..9] != "T"
            || &name[15..16] != "."
            || &name[22..23] != "-"
            || &name[31..] != ".msg"
        {
            return false;
        }
        name[..8].bytes().all(|b| b.is_ascii_digit())
            && name[9..15].bytes().all(|b| b.is_ascii_digit())
            && name[16..22].bytes().all(|b| b.is_ascii_digit())
            && name[23..31].bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[test]
    fn dest_mismatch_does_not_verify() {
        let token = hex::encode([7u8; 16]);
        let mac = compute_hmac(&token, "beta", "alpha", "id1", "ts1", b"hi", None).unwrap();
        assert!(verify_hmac(&token, "beta", "alpha", "id1", "ts1", b"hi", None, &mac).unwrap());
        assert!(!verify_hmac(&token, "gamma", "alpha", "id1", "ts1", b"hi", None, &mac).unwrap());
        // Wrong key also fails.
        let other = hex::encode([8u8; 16]);
        assert!(!verify_hmac(&other, "beta", "alpha", "id1", "ts1", b"hi", None, &mac).unwrap());
    }

    #[test]
    fn bom_in_body_does_not_break_verify() {
        let root = temp_root("msg-bom");
        let token = mint_token(&root.join("token")).unwrap();
        let body = "\u{feff}hello".to_string();
        let (path, _) = write_msg(&root, "beta", "alpha", &token, &body, None).unwrap();
        assert!(verify_msg(&path, &token).unwrap());
        // Direct: MAC of stripped body verifies against BOM'd body and vice versa.
        let mac = compute_hmac(&token, "b", "a", "i", "t", "\u{feff}x".as_bytes(), None).unwrap();
        assert!(verify_hmac(&token, "b", "a", "i", "t", "x".as_bytes(), None, &mac).unwrap());
        assert!(strip_bom(BOM) == b"".as_slice());
    }

    #[test]
    fn truncated_16_hex_mac_is_rejected() {
        let token = hex::encode([1u8; 16]);
        let mac = compute_hmac(&token, "beta", "alpha", "id9", "ts9", b"body", None).unwrap();
        assert_eq!(mac.len(), 64);
        let truncated = &mac[..16];
        assert!(
            !verify_hmac(&token, "beta", "alpha", "id9", "ts9", b"body", None, truncated).unwrap()
        );
        // Any 16-hex claim is too short to verify.
        assert!(!verify_hmac(
            &token,
            "beta",
            "alpha",
            "id9",
            "ts9",
            b"body",
            None,
            &"a".repeat(16)
        )
        .unwrap());
    }

    #[test]
    fn tampered_body_does_not_verify() {
        let root = temp_root("msg-tamper");
        let token = mint_token(&root.join("token")).unwrap();
        let (path, _) = write_msg(&root, "beta", "alpha", &token, "original", None).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("original", "tampered")).unwrap();
        assert!(!verify_msg(&path, &token).unwrap());
    }

    #[test]
    fn token_is_32_hex_mode_0600_and_exclusive() {
        let root = temp_root("msg-token");
        let tpath = root.join("token");
        let token = mint_token(&tpath).unwrap();
        assert_eq!(token.len(), TOKEN_HEX_LEN);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        let mode = std::fs::metadata(&tpath).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // O_EXCL: a second mint must fail, and must not clobber.
        assert!(mint_token(&tpath).is_err());
        assert_eq!(read_token(&tpath).unwrap(), token);
        // Bad token hex is a named error, not a panic.
        assert!(matches!(
            compute_hmac("nothex", "b", "a", "i", "t", b"x", None),
            Err(SeatError::BadToken(_))
        ));
    }

    #[test]
    fn corrupt_header_is_named_error() {
        let root = temp_root("msg-corrupt");
        let p = root.join("x.msg");
        std::fs::write(&p, "not json\n\nbody").unwrap();
        assert!(matches!(read_msg(&p), Err(SeatError::CorruptMsg { .. })));
        std::fs::write(&p, "{\"dest\":\"a\"}").unwrap();
        assert!(matches!(read_msg(&p), Err(SeatError::BadBody { .. })));
    }

    #[test]
    fn session_header_is_written_covered_by_mac_and_readable() {
        let root = temp_root("msg-session");
        let token = mint_token(&root.join("token")).unwrap();
        let (path, header) = write_msg(
            &root,
            "beta",
            "alpha",
            &token,
            "with session",
            Some("sess-42"),
        )
        .unwrap();
        assert_eq!(header.session.as_deref(), Some("sess-42"));
        // read_msg exposes it.
        let (back, body) = read_msg(&path).unwrap();
        assert_eq!(back.session.as_deref(), Some("sess-42"));
        assert_eq!(body, "with session");
        // The raw header line carries the field.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"session\":\"sess-42\""), "{raw}");
        // It verifies — the MAC covered the session value.
        assert!(verify_msg(&path, &token).unwrap());

        // Tampering the session value breaks the MAC.
        std::fs::write(&path, raw.replace("sess-42", "sess-99")).unwrap();
        assert!(!verify_msg(&path, &token).unwrap());
    }

    #[test]
    fn absent_session_omits_the_field_and_matches_the_old_mac_input() {
        let root = temp_root("msg-no-session");
        let token = mint_token(&root.join("token")).unwrap();
        let (path, header) = write_msg(&root, "beta", "alpha", &token, "plain body", None).unwrap();
        assert_eq!(header.session, None);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("\"session\""), "{raw}");
        assert!(verify_msg(&path, &token).unwrap());
    }

    /// A message written before the identity rule — no `session` key in
    /// the header, MAC over exactly `dest|from|id|ts|body` — still
    /// parses and verifies.
    #[test]
    fn legacy_message_without_session_still_verifies() {
        let root = temp_root("msg-legacy");
        let token = hex::encode([9u8; 16]);
        let dest = "beta";
        let from = "alpha";
        let id = "deadbeef";
        let ts = "20260101T000000.000000";
        let body = "written before sessions existed";
        let mac = compute_hmac(&token, dest, from, id, ts, body.as_bytes(), None).unwrap();
        // Hand-write the old wire format: exactly the five old fields.
        let legacy = format!(
            "{{\"dest\":\"{dest}\",\"from\":\"{from}\",\"id\":\"{id}\",\"ts\":\"{ts}\",\"hmac\":\"{mac}\"}}\n\n{body}"
        );
        let p = root.join("20260101T000000.000000-deadbeef.msg");
        std::fs::write(&p, legacy).unwrap();
        let (header, body_back) = read_msg(&p).unwrap();
        assert_eq!(header.session, None, "old header parses with no session");
        assert_eq!(body_back, body);
        assert!(verify_msg(&p, &token).unwrap());
    }
}
