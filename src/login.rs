//! The sign-in page, served on 127.0.0.1 while the user signs in.
//!
//! ricercar opens it in the browser (`auth.begin`). The user enters the
//! server address, then either approves a Quick Connect code from another
//! Jellyfin app or types a user name and password. The password goes from
//! the browser to this process to the Jellyfin server only; ricercar never
//! sees it, and it is not stored (the access token is).
//!
//! Every path starts with a random secret, so other web pages open in the
//! same browser cannot drive the page, and the `Host` header must be the
//! loopback address (no DNS rebinding).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::jellyfin::{Client, Error, Session, normalize_server};

pub type OnSignedIn = Arc<dyn Fn(Session) + Send + Sync>;

pub struct Login {
    pub url: String,
}

struct Page {
    client: Arc<Client>,
    on_signed_in: OnSignedIn,
    secret: String,
    host: String,
    server_hint: String,
    french: bool,
    /// Pending Quick Connect: `(server, secret)`.
    quick: Mutex<Option<(String, String)>>,
}

const MAX_BODY: usize = 16 * 1024;

impl Login {
    pub fn start(
        client: Arc<Client>,
        on_signed_in: OnSignedIn,
        server_hint: String,
        french: bool,
    ) -> std::io::Result<Login> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let secret = crate::random_hex(16);
        let host = format!("127.0.0.1:{port}");
        let url = format!("http://{host}/{secret}/");
        let page = Arc::new(Page {
            client,
            on_signed_in,
            secret,
            host,
            server_hint,
            french,
            quick: Mutex::new(None),
        });
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let page = page.clone();
                std::thread::spawn(move || page.handle(stream));
            }
        });
        Ok(Login { url })
    }
}

impl Page {
    fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let Ok(clone) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(clone);
        let mut first = String::new();
        if reader.read_line(&mut first).is_err() {
            return;
        }
        let mut host = String::new();
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "host" => host = v.trim().to_string(),
                    "content-length" => len = v.trim().parse().unwrap_or(usize::MAX),
                    _ => {}
                }
            }
        }
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        let reply = if host != self.host {
            (403, "text/plain", "forbidden".to_string())
        } else if len > MAX_BODY {
            (413, "text/plain", "too large".to_string())
        } else {
            let mut body = vec![0; len];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            match target.strip_prefix(&format!("/{}/", self.secret)) {
                Some(route) => self.route(method, route, &body),
                None => (404, "text/plain", "not found".to_string()),
            }
        };
        let (code, ctype, body) = reply;
        let status = match code {
            200 => "OK",
            403 => "Forbidden",
            413 => "Payload Too Large",
            _ => "Not Found",
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {code} {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nX-Frame-Options: DENY\r\n\
             Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
    }

    fn route(&self, method: &str, route: &str, body: &Value) -> (u16, &'static str, String) {
        let json = |v: Value| (200, "application/json", v.to_string());
        let arg = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("");
        match (method, route) {
            ("GET", "") => (200, "text/html; charset=utf-8", self.html()),
            ("POST", "check") => json(self.check(arg("server"))),
            ("POST", "password") => {
                json(self.password(arg("server"), arg("user"), arg("password")))
            }
            ("POST", "quick") => json(self.quick_start(arg("server"))),
            ("POST", "quick/poll") => json(self.quick_poll()),
            _ => (404, "text/plain", "not found".to_string()),
        }
    }

    fn fail(&self, e: &Error) -> Value {
        let msg = match (e, self.french) {
            (Error::Auth, true) => "Nom d'utilisateur ou mot de passe incorrect.".to_string(),
            (Error::Auth, false) => "Wrong user name or password.".to_string(),
            (Error::Network(m), true) => format!("Serveur injoignable : {m}"),
            (Error::Network(m), false) => format!("Cannot reach the server: {m}"),
            (e, _) => e.to_string(),
        };
        json!({ "ok": false, "error": msg })
    }

    fn server(&self, input: &str) -> Result<String, Value> {
        normalize_server(input).ok_or_else(|| {
            let msg = if self.french {
                "Adresse invalide, par exemple http://192.168.1.10:8096"
            } else {
                "Invalid address, for example http://192.168.1.10:8096"
            };
            json!({ "ok": false, "error": msg })
        })
    }

    fn check(&self, input: &str) -> Value {
        let server = match self.server(input) {
            Ok(s) => s,
            Err(e) => return e,
        };
        match self.client.public_info(&server) {
            Ok(info) => json!({
                "ok": true,
                "server": server,
                "name": info["ServerName"],
                "version": info["Version"],
                "quick_connect": self.client.quick_connect_enabled(&server),
            }),
            Err(e) => self.fail(&e),
        }
    }

    fn done(&self, r: Result<Session, Error>) -> Value {
        match r {
            Ok(s) => {
                let who = json!({ "user": s.user_name, "server": s.server_name });
                (self.on_signed_in)(s);
                json!({ "ok": true, "done": true, "who": who })
            }
            Err(e) => self.fail(&e),
        }
    }

    fn password(&self, input: &str, user: &str, password: &str) -> Value {
        match self.server(input) {
            Ok(server) => self.done(self.client.sign_in_password(&server, user.trim(), password)),
            Err(e) => e,
        }
    }

    fn quick_start(&self, input: &str) -> Value {
        let server = match self.server(input) {
            Ok(s) => s,
            Err(e) => return e,
        };
        match self.client.quick_connect_initiate(&server) {
            Ok((secret, code)) => {
                *self.quick.lock().unwrap() = Some((server, secret));
                json!({ "ok": true, "code": code })
            }
            Err(e) => self.fail(&e),
        }
    }

    fn quick_poll(&self) -> Value {
        let Some((server, secret)) = self.quick.lock().unwrap().clone() else {
            return json!({ "ok": false, "error": "no request" });
        };
        match self.client.quick_connect_approved(&server, &secret) {
            Ok(false) => json!({ "ok": true, "done": false }),
            Ok(true) => {
                *self.quick.lock().unwrap() = None;
                self.done(self.client.sign_in_quick_connect(&server, &secret))
            }
            Err(e) => self.fail(&e),
        }
    }

    fn html(&self) -> String {
        let t = if self.french { FR } else { EN };
        let mut page = PAGE.to_string();
        for (k, v) in t {
            page = page.replace(&format!("{{{{{k}}}}}"), &escape(v));
        }
        page.replace("{{lang}}", if self.french { "fr" } else { "en" })
            .replace("{{server_hint}}", &escape(&self.server_hint))
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const EN: &[(&str, &str)] = &[
    ("title", "Sign in to Jellyfin"),
    (
        "lead",
        "ricercar will play music from your Jellyfin server.",
    ),
    ("server", "Server address"),
    ("next", "Continue"),
    ("quick", "Quick Connect"),
    (
        "quick_help",
        "In another Jellyfin app where you are signed in, open your profile, then Quick Connect, and enter this code:",
    ),
    ("waiting", "Waiting for approval…"),
    ("or", "or sign in with a password"),
    ("user", "User name"),
    ("password", "Password"),
    ("sign_in", "Sign in"),
    (
        "done",
        "Signed in. You can close this page and go back to ricercar.",
    ),
    ("no_qc", "Quick Connect is off on this server."),
];

const FR: &[(&str, &str)] = &[
    ("title", "Connexion à Jellyfin"),
    (
        "lead",
        "ricercar lira la musique de votre serveur Jellyfin.",
    ),
    ("server", "Adresse du serveur"),
    ("next", "Continuer"),
    ("quick", "Quick Connect"),
    (
        "quick_help",
        "Dans une autre application Jellyfin où vous êtes connecté, ouvrez votre profil, puis Quick Connect, et saisissez ce code :",
    ),
    ("waiting", "En attente de validation…"),
    ("or", "ou connectez-vous avec un mot de passe"),
    ("user", "Nom d'utilisateur"),
    ("password", "Mot de passe"),
    ("sign_in", "Se connecter"),
    (
        "done",
        "Connecté. Vous pouvez fermer cette page et revenir à ricercar.",
    ),
    ("no_qc", "Quick Connect est désactivé sur ce serveur."),
];

const PAGE: &str = r#"<!doctype html>
<html lang="{{lang}}"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{title}}</title>
<style>
:root{color-scheme:light dark;--bg:#f6f5f2;--card:#fff;--fg:#1d1c1a;--mute:#6b6862;--line:#dedbd4;--accent:#7a4ce0;--bad:#b3261e}
@media (prefers-color-scheme:dark){:root{--bg:#141413;--card:#1e1e1c;--fg:#eceae4;--mute:#a19d95;--line:#34332f;--accent:#aa8cf5;--bad:#f2b8b5}}
*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif;padding:16px}
main{width:100%;max-width:400px;background:var(--card);border:1px solid var(--line);border-radius:14px;padding:28px}
h1{font-size:20px;margin:0 0 4px}p{margin:0 0 18px;color:var(--mute)}
label{display:block;font-size:13px;color:var(--mute);margin:12px 0 4px}
input{width:100%;padding:10px 12px;border:1px solid var(--line);border-radius:8px;background:transparent;color:inherit;font:inherit}
button{margin-top:16px;width:100%;padding:10px;border:0;border-radius:8px;background:var(--accent);color:#fff;font:inherit;font-weight:600;cursor:pointer}
button:disabled{opacity:.5;cursor:default}
.code{font:600 32px/1.2 ui-monospace,monospace;letter-spacing:.2em;text-align:center;margin:12px 0}
.sep{text-align:center;color:var(--mute);font-size:13px;margin:22px 0 0}
.err{color:var(--bad);margin:12px 0 0;min-height:1em}.hidden{display:none}.srv{font-size:13px;color:var(--mute)}
</style></head><body><main>
<h1>{{title}}</h1><p>{{lead}}</p>
<form id="s1"><label for="server">{{server}}</label>
<input id="server" name="server" value="{{server_hint}}" placeholder="http://192.168.1.10:8096" autocomplete="url" required autofocus>
<button>{{next}}</button></form>
<section id="s2" class="hidden"><div class="srv" id="srv"></div>
<div id="qc"><label>{{quick}}</label><div>{{quick_help}}</div><div class="code" id="code">······</div><div class="srv" id="qcstate">{{waiting}}</div></div>
<div id="noqc" class="srv hidden">{{no_qc}}</div>
<div class="sep">{{or}}</div>
<form id="pw"><label for="user">{{user}}</label><input id="user" autocomplete="username" required>
<label for="password">{{password}}</label><input id="password" type="password" autocomplete="current-password">
<button>{{sign_in}}</button></form></section>
<section id="s3" class="hidden"><p id="who"></p><p>{{done}}</p></section>
<div class="err" id="err" role="alert"></div>
</main><script>
const $=id=>document.getElementById(id);let server="",timer=null;
async function call(path,body){try{const r=await fetch(path,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(body||{})});return await r.json()}catch(e){return{ok:false,error:String(e)}}}
function show(r){$("err").textContent=r.ok?"":(r.error||"");return r.ok}
function done(r){clearInterval(timer);$("s2").classList.add("hidden");$("s3").classList.remove("hidden");$("who").textContent=[r.who.user,r.who.server].filter(Boolean).join(" · ")}
$("s1").onsubmit=async e=>{e.preventDefault();const b=e.submitter;b.disabled=true;const r=await call("check",{server:$("server").value});b.disabled=false;if(!show(r))return;
server=r.server;$("s1").classList.add("hidden");$("s2").classList.remove("hidden");$("srv").textContent=[r.name,r.server,r.version].filter(Boolean).join(" · ");
if(!r.quick_connect){$("qc").classList.add("hidden");$("noqc").classList.remove("hidden");$("user").focus();return}
const q=await call("quick",{server});if(!q.ok){$("qc").classList.add("hidden");$("noqc").classList.remove("hidden");return}
$("code").textContent=q.code;timer=setInterval(async()=>{const p=await call("quick/poll");if(p.ok&&p.done)done(p);else if(!p.ok)$("qcstate").textContent=p.error},2000)};
$("pw").onsubmit=async e=>{e.preventDefault();const b=e.submitter;b.disabled=true;const r=await call("password",{server,user:$("user").value,password:$("password").value});b.disabled=false;if(show(r))done(r)};
</script></body></html>
"#;
