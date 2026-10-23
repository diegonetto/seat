//! The hapi hub client: address and token from `~/.hapi/settings.json`
//! (`cliApiToken`; base URL from `listenHost`/`listenPort`; env
//! `HAPI_API_URL`/`CLI_API_TOKEN` override), `POST /api/auth` for a
//! 4 h JWT re-exchanged near expiry or on any 401, every request
//! bounded at 10 s (ureq 3; no async runtime, no handwritten HTTP).
use crate::error::{Result, SeatError};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const JWT_LIFETIME: Duration = Duration::from_secs(4 * 60 * 60);
const REAUTH_MARGIN: Duration = Duration::from_secs(20 * 60);

/// Where the hub lives and how to authenticate.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub base: String,
    pub token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsFile {
    cli_api_token: Option<String>,
    listen_host: Option<String>,
    listen_port: Option<u16>,
}

/// The base URL: a missing or wildcard listenHost means localhost.
pub fn base_url(host: &str, port: u16) -> String {
    let host = match host {
        "" | "*" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}")
}

impl HubConfig {
    fn settings(path: &Path) -> Option<SettingsFile> {
        Some(serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?)
    }

    /// Each value comes from its own override or its own field,
    /// independently: a settings file holding only `cliApiToken`
    /// never discards the address, and one holding only the port
    /// never discards the token.
    pub fn discover_with(
        home: &Path,
        env_base: Option<&str>,
        env_token: Option<&str>,
    ) -> Result<HubConfig> {
        let file = Self::settings(&home.join(".hapi").join("settings.json"));
        let pick = |env: Option<&str>, from_file: Option<String>| {
            env.filter(|s| !s.is_empty())
                .map(str::to_string)
                .or(from_file)
        };
        let file_base = file.as_ref().and_then(|s| {
            s.listen_port
                .map(|p| base_url(s.listen_host.as_deref().unwrap_or(""), p))
        });
        let base = pick(env_base, file_base).ok_or_else(|| {
            SeatError::Hub(
                "no hub address: set HAPI_API_URL or write listenPort in ~/.hapi/settings.json"
                    .into(),
            )
        })?;
        let token = pick(env_token, file.and_then(|s| s.cli_api_token)).ok_or_else(|| {
            SeatError::Hub(
                "no hub token: set CLI_API_TOKEN or write cliApiToken in ~/.hapi/settings.json"
                    .into(),
            )
        })?;
        Ok(HubConfig { base, token })
    }

    /// Env overrides first, then `~/.hapi/settings.json` (each value
    /// independently — see `discover_with`).
    pub fn discover() -> Result<HubConfig> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::discover_with(
            &home,
            std::env::var("HAPI_API_URL").ok().as_deref(),
            std::env::var("CLI_API_TOKEN").ok().as_deref(),
        )
    }
}

/// The queued-state answer for one batch of localIds.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct QueuedState {
    pub queued: Vec<String>,
    pub indeterminate: Vec<String>,
    pub invoked: Vec<String>,
}

/// The session flags `GET /api/sessions/<id>` reports (`session`
/// nested, top level still accepted): `thinking` and `thinkingAt`
/// are `None` when the key is missing or not the right type, so a
/// missing fact can never read as a quiet session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionState {
    pub active: bool,
    pub thinking: Option<bool>,
    pub thinking_at: Option<u64>,
}

type Response = ureq::http::Response<ureq::Body>;

fn hub_err(e: ureq::Error) -> SeatError {
    SeatError::Hub(e.to_string())
}

/// One authenticated hub connection. A worker owns its Hub (never
/// shared), so one seat's slow request never holds another seat's
/// turn.
pub struct Hub {
    agent: ureq::Agent,
    cfg: HubConfig,
    jwt: Option<(String, Instant)>,
}

impl Hub {
    pub fn new(cfg: HubConfig) -> Hub {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .http_status_as_error(false)
                .timeout_global(Some(REQUEST_TIMEOUT))
                .build(),
        );
        Hub {
            agent,
            cfg,
            jwt: None,
        }
    }

    fn auth(&mut self) -> Result<String> {
        let url = format!("{}/api/auth", self.cfg.base);
        let mut resp = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(
                serde_json::json!({ "accessToken": self.cfg.token })
                    .to_string()
                    .as_str(),
            )
            .map_err(hub_err)?;
        if !resp.status().is_success() {
            return Err(SeatError::Hub(format!("auth failed: {}", resp.status())));
        }
        let token = Self::json_body(&mut resp)?
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| SeatError::Hub("auth answer has no token".into()))?
            .to_string();
        self.jwt = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// A usable JWT, re-exchanged near expiry (it lasts 4 h).
    fn jwt(&mut self) -> Result<String> {
        if let Some((t, at)) = &self.jwt {
            if at.elapsed() < JWT_LIFETIME - REAUTH_MARGIN {
                return Ok(t.clone());
            }
        }
        self.auth()
    }

    /// One authorized request; a 401 re-authenticates once and
    /// retries.
    fn call(&mut self, method: &str, url: &str, body: &str) -> Result<Response> {
        for _ in 0..2 {
            let auth = format!("Bearer {}", self.jwt()?);
            let resp = if method == "GET" {
                self.agent
                    .get(url)
                    .header("Authorization", &auth)
                    .call()
                    .map_err(hub_err)?
            } else {
                self.agent
                    .post(url)
                    .header("Authorization", &auth)
                    .header("Content-Type", "application/json")
                    .send(body)
                    .map_err(hub_err)?
            };
            if resp.status().as_u16() == 401 {
                self.jwt = None;
                continue;
            }
            return Ok(resp);
        }
        Err(SeatError::Hub("auth refused twice".into()))
    }

    fn json_body(resp: &mut Response) -> Result<serde_json::Value> {
        let raw = resp.body_mut().read_to_string().map_err(hub_err)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// One list from the queued-state answer: an id is a string or an
    /// object's `localId` (`invokedLocalMessages` may carry objects;
    /// dropping them would make every item look absent and re-POST
    /// forever).
    fn list(body: &serde_json::Value, name: &str) -> Vec<String> {
        body.get(name)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| {
                        v.as_str().map(str::to_string).or_else(|| {
                            v.get("localId")
                                .and_then(|l| l.as_str())
                                .map(str::to_string)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `GET /api/sessions/<id>` as the sleep rule needs it: the
    /// session's flags from the body already fetched (`session`
    /// nested, top level still accepted). `thinking` and
    /// `thinkingAt` are `None` when the key is missing or not the
    /// right type — never a silent `false` — and a 404 (no session)
    /// leaves them `None` while `active` reads false, so the sleep
    /// rule can never feed a missing session's answer in as
    /// permission to sleep.
    pub fn session_state(&mut self, session_id: &str) -> Result<SessionState> {
        let url = format!("{}/api/sessions/{session_id}", self.cfg.base);
        let mut resp = self.call("GET", &url, "")?;
        match resp.status().as_u16() {
            200 => {
                let body = Self::json_body(&mut resp)?;
                let flag = |name: &str| {
                    body.get("session")
                        .and_then(|s| s.get(name))
                        .or_else(|| body.get(name))
                };
                Ok(SessionState {
                    active: flag("active").and_then(|a| a.as_bool()).unwrap_or(false),
                    thinking: flag("thinking").and_then(|t| t.as_bool()),
                    thinking_at: flag("thinkingAt").and_then(|t| t.as_u64()),
                })
            }
            404 => Ok(SessionState {
                active: false,
                thinking: None,
                thinking_at: None,
            }),
            c => Err(SeatError::Hub(format!(
                "GET session {session_id}: status {c}"
            ))),
        }
    }

    /// `GET /api/sessions/<id>`: false on 404 or `active` not true.
    /// Kept on top of `session_state`, with its old meaning, for the
    /// delivery gate.
    pub fn session_active(&mut self, session_id: &str) -> Result<bool> {
        Ok(self.session_state(session_id)?.active)
    }

    /// One message per POST, the delivery key as `localId`; the hub
    /// dedupes on `(session, localId)` forever, so a retry with the
    /// same key is harmless.
    pub fn post_message(&mut self, session_id: &str, key: &str, text: &str) -> Result<()> {
        let url = format!("{}/api/sessions/{session_id}/messages", self.cfg.base);
        let resp = self.call(
            "POST",
            &url,
            &serde_json::json!({ "text": text, "localId": key }).to_string(),
        )?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(SeatError::Hub(format!(
                "POST message {key}: status {}",
                resp.status()
            )))
        }
    }

    /// `POST .../messages/queued-state` (the caller batches to at
    /// most 1,000 ids).
    pub fn queued_state(&mut self, session_id: &str, keys: &[String]) -> Result<QueuedState> {
        let url = format!(
            "{}/api/sessions/{session_id}/messages/queued-state",
            self.cfg.base
        );
        let mut resp = self.call(
            "POST",
            &url,
            &serde_json::json!({ "localIds": keys }).to_string(),
        )?;
        if !resp.status().is_success() {
            return Err(SeatError::Hub(format!(
                "queued-state: status {}",
                resp.status()
            )));
        }
        let raw = Self::json_body(&mut resp)?;
        Ok(QueuedState {
            queued: Self::list(&raw, "queuedLocalIds"),
            indeterminate: Self::list(&raw, "indeterminateLocalIds"),
            invoked: Self::list(&raw, "invokedLocalMessages"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_with_settings(json: &str) -> PathBuf {
        let home = crate::testutil::temp_root("hub-settings");
        std::fs::create_dir_all(home.join(".hapi")).unwrap();
        std::fs::write(home.join(".hapi").join("settings.json"), json).unwrap();
        home
    }

    /// Finding 3c: address and token each come from their own override
    /// or field, independently — a file holding only one half never
    /// discards the other.
    #[test]
    fn each_half_comes_from_its_own_source() {
        // File address + CLI_API_TOKEN: the file has no token to give.
        let home = home_with_settings(r#"{"listenPort":4242,"cliApiToken":"file-token"}"#);
        let cfg = HubConfig::discover_with(&home, None, Some("env-token")).unwrap();
        assert_eq!(
            (cfg.base.as_str(), cfg.token.as_str()),
            ("http://127.0.0.1:4242", "env-token")
        );
        // HAPI_API_URL + file token: the file has no port to give.
        let home = home_with_settings(r#"{"cliApiToken":"file-token"}"#);
        let cfg = HubConfig::discover_with(&home, Some("http://127.0.0.1:9999"), None).unwrap();
        assert_eq!(
            (cfg.base.as_str(), cfg.token.as_str()),
            ("http://127.0.0.1:9999", "file-token")
        );
    }

    /// The live hub nests the flag under `session`; `session_active`
    /// reads it there (a top-level `active` still accepted), and a 404
    /// reads false.
    #[test]
    fn session_active_reads_the_flag_where_the_hub_puts_it() {
        let hub = crate::testutil::fakehub::FakeHub::start();
        {
            let mut ctl = hub.ctl.lock().unwrap();
            ctl.sessions.insert("nested-true".into(), true);
            ctl.sessions.insert("nested-false".into(), false);
            ctl.get_bodies
                .insert("flat-true".into(), "{\"active\":true}".into());
        }
        let mut client = Hub::new(hub.config());
        assert!(client.session_active("nested-true").unwrap());
        assert!(!client.session_active("nested-false").unwrap());
        assert!(client.session_active("flat-true").unwrap());
        assert!(!client.session_active("absent").unwrap());
    }

    /// `session_state` reads `thinking`/`thinkingAt` where the hub
    /// puts them, and nothing unreadable ever reads as `false`: a
    /// missing key, a non-boolean, a 404, and a GET error all leave
    /// `thinking` unset (the sleep rule fails closed on them), while
    /// `session_active` keeps its old meaning on top.
    #[test]
    fn session_state_never_turns_a_missing_fact_into_false() {
        let hub = crate::testutil::fakehub::FakeHub::start();
        {
            let mut ctl = hub.ctl.lock().unwrap();
            ctl.get_bodies.insert(
                "thinking".into(),
                "{\"session\":{\"active\":true,\"thinking\":true,\"thinkingAt\":123}}".into(),
            );
            ctl.get_bodies.insert(
                "quiet".into(),
                "{\"session\":{\"active\":true,\"thinking\":false,\"thinkingAt\":0}}".into(),
            );
            ctl.get_bodies.insert(
                "not-bool".into(),
                "{\"session\":{\"active\":true,\"thinking\":\"yes\",\"thinkingAt\":0}}".into(),
            );
            ctl.get_bodies
                .insert("no-key".into(), "{\"session\":{\"active\":true}}".into());
            ctl.fail_get.insert("boom".into());
        }
        let mut client = Hub::new(hub.config());
        assert_eq!(
            client.session_state("thinking").unwrap(),
            SessionState {
                active: true,
                thinking: Some(true),
                thinking_at: Some(123)
            }
        );
        assert_eq!(
            client.session_state("quiet").unwrap(),
            SessionState {
                active: true,
                thinking: Some(false),
                thinking_at: Some(0)
            }
        );
        for session in ["not-bool", "no-key"] {
            let s = client.session_state(session).unwrap();
            assert_eq!(s.active, true);
            assert_eq!(s.thinking, None, "{session}: not idle either way");
        }
        assert_eq!(
            client.session_state("absent").unwrap(),
            SessionState {
                active: false,
                thinking: None,
                thinking_at: None
            },
            "a 404 is no session, never a quiet one"
        );
        assert!(client.session_state("boom").is_err(), "a GET error is an error");
        assert!(client.session_active("thinking").unwrap());
        assert!(!client.session_active("absent").unwrap());
    }
}
