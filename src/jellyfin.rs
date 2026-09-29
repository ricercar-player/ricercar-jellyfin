//! A small client for the Jellyfin REST API: sign-in, item queries, playback
//! reporting. Only documented endpoints of Jellyfin 10.8 and later.

use std::time::Duration;

use serde_json::{Value, json};

/// A signed-in user on one server, as stored in `<data_dir>/auth.json`.
#[derive(Clone, Debug)]
pub struct Session {
    pub server: String,
    pub token: String,
    pub user_id: String,
    pub user_name: String,
    pub server_name: String,
}

impl Session {
    pub fn to_json(&self) -> Value {
        json!({
            "server": self.server, "token": self.token, "user_id": self.user_id,
            "user_name": self.user_name, "server_name": self.server_name,
        })
    }

    pub fn from_json(v: &Value) -> Option<Session> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Session {
            server: s("server")?,
            token: s("token")?,
            user_id: s("user_id")?,
            user_name: s("user_name").unwrap_or_default(),
            server_name: s("server_name").unwrap_or_default(),
        })
    }
}

#[derive(Debug)]
pub enum Error {
    /// 401: the token is gone or was revoked.
    Auth,
    /// 404.
    NotFound,
    /// The server said no for another reason (4xx, 5xx).
    Status(u16, String),
    /// DNS, TCP, TLS, timeouts, unreadable answers.
    Network(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Auth => write!(f, "not signed in"),
            Error::NotFound => write!(f, "not found"),
            Error::Status(code, msg) if msg.is_empty() => write!(f, "server answered {code}"),
            Error::Status(code, msg) => write!(f, "server answered {code}: {msg}"),
            Error::Network(e) => write!(f, "{e}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// `https://host:8096/jellyfin/` → `https://host:8096/jellyfin`; a bare
/// host gets `http://`, Jellyfin's default.
pub fn normalize_server(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() || s.chars().any(char::is_whitespace) {
        return None;
    }
    let (scheme, rest) = match s.split_once("://") {
        Some((sc @ ("http" | "https"), rest)) => (sc, rest),
        Some(_) => return None,
        None => ("http", s),
    };
    let rest = rest.trim_end_matches('/');
    (!rest.is_empty() && !rest.starts_with('/')).then(|| format!("{scheme}://{rest}"))
}

pub struct Client {
    agent: ureq::Agent,
    device: String,
    device_id: String,
}

impl Client {
    pub fn new(device: String, device_id: String) -> Client {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .user_agent(concat!("ricercar-jellyfin/", env!("CARGO_PKG_VERSION")))
            .build();
        Client {
            agent,
            device,
            device_id,
        }
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    fn auth_header(&self, token: Option<&str>) -> String {
        let clean = |s: &str| s.replace(['"', ',', '\n', '\r'], "");
        let mut h = format!(
            "MediaBrowser Client=\"ricercar\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
            clean(&self.device),
            clean(&self.device_id),
            env!("CARGO_PKG_VERSION"),
        );
        if let Some(t) = token {
            h.push_str(&format!(", Token=\"{}\"", clean(t)));
        }
        h
    }

    fn call(
        &self,
        method: &str,
        server: &str,
        path: &str,
        token: Option<&str>,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self
            .agent
            .request(method, &format!("{server}{path}"))
            .set("Authorization", &self.auth_header(token))
            .set("Accept", "application/json");
        for (k, v) in query {
            req = req.query(k, v);
        }
        let resp = match body {
            Some(b) => req.send_json(b),
            None if method == "POST" => req.set("Content-Length", "0").call(),
            None => req.call(),
        };
        match resp {
            Ok(r) => {
                let text = r.into_string().map_err(|e| Error::Network(e.to_string()))?;
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }
                Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
            }
            Err(ureq::Error::Status(401, _)) => Err(Error::Auth),
            Err(ureq::Error::Status(404, _)) => Err(Error::NotFound),
            Err(ureq::Error::Status(code, r)) => {
                let msg: String = r
                    .into_string()
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                Err(Error::Status(code, msg.trim().to_string()))
            }
            Err(e) => Err(Error::Network(e.to_string())),
        }
    }

    // ------------------------------------------------------------ sign-in

    /// `GET /System/Info/Public`: `{ServerName, Version, …}`. Also tells
    /// whether the address points at a Jellyfin server at all.
    pub fn public_info(&self, server: &str) -> Result<Value> {
        let v = self.call("GET", server, "/System/Info/Public", None, &[], None)?;
        if v.get("Version").is_none() {
            return Err(Error::Network("not a Jellyfin server".into()));
        }
        Ok(v)
    }

    pub fn quick_connect_enabled(&self, server: &str) -> bool {
        self.call("GET", server, "/QuickConnect/Enabled", None, &[], None)
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Start a Quick Connect request: `(secret, code)`. The user types the
    /// code in another signed-in Jellyfin app to approve this device.
    pub fn quick_connect_initiate(&self, server: &str) -> Result<(String, String)> {
        // POST since 10.9, GET before.
        let v = match self.call("POST", server, "/QuickConnect/Initiate", None, &[], None) {
            Err(Error::NotFound) | Err(Error::Status(405, _)) => {
                self.call("GET", server, "/QuickConnect/Initiate", None, &[], None)?
            }
            r => r?,
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match (s("Secret"), s("Code")) {
            (Some(secret), Some(code)) => Ok((secret, code)),
            _ => Err(Error::Network("unexpected Quick Connect answer".into())),
        }
    }

    pub fn quick_connect_approved(&self, server: &str, secret: &str) -> Result<bool> {
        let v = self.call(
            "GET",
            server,
            "/QuickConnect/Connect",
            None,
            &[("secret", secret.to_string())],
            None,
        )?;
        Ok(v.get("Authenticated")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub fn sign_in_quick_connect(&self, server: &str, secret: &str) -> Result<Session> {
        let v = self.call(
            "POST",
            server,
            "/Users/AuthenticateWithQuickConnect",
            None,
            &[],
            Some(json!({ "Secret": secret })),
        )?;
        self.session_from(server, &v)
    }

    pub fn sign_in_password(&self, server: &str, user: &str, password: &str) -> Result<Session> {
        let v = self.call(
            "POST",
            server,
            "/Users/AuthenticateByName",
            None,
            &[],
            Some(json!({ "Username": user, "Pw": password })),
        )?;
        self.session_from(server, &v)
    }

    /// A session from an access token the user pasted.
    pub fn sign_in_token(&self, server: &str, token: &str) -> Result<Session> {
        let me = self.call("GET", server, "/Users/Me", Some(token), &[], None)?;
        let v = json!({ "AccessToken": token, "User": me });
        self.session_from(server, &v)
    }

    fn session_from(&self, server: &str, auth: &Value) -> Result<Session> {
        let token = auth["AccessToken"].as_str();
        let user_id = auth["User"]["Id"].as_str();
        let (Some(token), Some(user_id)) = (token, user_id) else {
            return Err(Error::Network("unexpected sign-in answer".into()));
        };
        let server_name = self
            .public_info(server)
            .ok()
            .and_then(|i| i["ServerName"].as_str().map(str::to_string))
            .unwrap_or_default();
        Ok(Session {
            server: server.to_string(),
            token: token.to_string(),
            user_id: user_id.to_string(),
            user_name: auth["User"]["Name"].as_str().unwrap_or("").to_string(),
            server_name,
        })
    }

    pub fn sign_out(&self, s: &Session) {
        let _ = self.call(
            "POST",
            &s.server,
            "/Sessions/Logout",
            Some(&s.token),
            &[],
            None,
        );
    }

    // ------------------------------------------------------------ queries

    pub fn get(&self, s: &Session, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call("GET", &s.server, path, Some(&s.token), query, None)
    }

    pub fn post(&self, s: &Session, path: &str, body: Value) -> Result<Value> {
        self.call("POST", &s.server, path, Some(&s.token), &[], Some(body))
    }

    pub fn post_empty(&self, s: &Session, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call("POST", &s.server, path, Some(&s.token), query, None)
    }

    pub fn delete(&self, s: &Session, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call("DELETE", &s.server, path, Some(&s.token), query, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_addresses() {
        assert_eq!(
            normalize_server("jf.lan:8096").as_deref(),
            Some("http://jf.lan:8096")
        );
        assert_eq!(
            normalize_server(" https://x.org/jellyfin/ ").as_deref(),
            Some("https://x.org/jellyfin")
        );
        assert_eq!(normalize_server("ftp://x"), None);
        assert_eq!(normalize_server("http://"), None);
        assert_eq!(normalize_server("a b"), None);
    }
}
