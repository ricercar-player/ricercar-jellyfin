//! Jellyfin source plugin for ricercar (plugin protocol 1).
//!
//! Speaks JSON-RPC over stdin/stdout with the player, and the Jellyfin REST
//! API with the user's server. Tracks play from the original file (bit for
//! bit) unless the DAC cannot take its rate or depth, in which case Jellyfin
//! transcodes to FLAC at a rate it does take.
//!
//! Options:
//!   --server URL   prefill the server address on the sign-in page

mod items;
mod jellyfin;
mod login;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use items::{Output, Plan};
use jellyfin::{Client, Error, Session};

const PROTOCOL: u64 = 1;
const PAGE: u64 = 200;
/// Fields asked for with every item list.
const FIELDS: &str = "MediaStreams,Genres,ChildCount,ProductionYear";

/// `<n>` random bytes from the kernel, as hex.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

struct RpcError {
    code: i64,
    message: String,
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
    }
}

type Reply = Result<Value, RpcError>;

struct Out(Mutex<std::io::Stdout>);

impl Out {
    fn send(&self, v: Value) {
        let mut out = self.0.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }
}

struct Plugin {
    out: Arc<Out>,
    server_hint: String,
    data_dir: Mutex<PathBuf>,
    french: Mutex<bool>,
    output: Mutex<Output>,
    client: Mutex<Option<Arc<Client>>>,
    session: Mutex<Option<Session>>,
    /// The token was refused: signed in, but it needs renewing.
    expired: Mutex<bool>,
    login: Mutex<Option<login::Login>>,
}

impl Plugin {
    fn client(&self) -> Result<Arc<Client>, RpcError> {
        self.client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| rpc_err(-32600, "not initialized"))
    }

    fn session(&self) -> Result<(Arc<Client>, Session), RpcError> {
        let client = self.client()?;
        let s = self.session.lock().unwrap().clone();
        match s {
            Some(s) if !*self.expired.lock().unwrap() => Ok((client, s)),
            _ => Err(rpc_err(-32001, "sign in to Jellyfin first")),
        }
    }

    fn auth_path(&self) -> PathBuf {
        self.data_dir.lock().unwrap().join("auth.json")
    }

    fn auth_status(&self) -> Value {
        match &*self.session.lock().unwrap() {
            None => json!({"state": "signed_out"}),
            Some(s) => {
                let state = if *self.expired.lock().unwrap() {
                    "expired"
                } else {
                    "signed_in"
                };
                let detail = if s.server_name.is_empty() {
                    s.server.clone()
                } else {
                    format!("{} · {}", s.server_name, s.server)
                };
                json!({"state": state, "account": {"display_name": s.user_name, "detail": detail}})
            }
        }
    }

    fn store(&self, s: Session) {
        let path = self.auth_path();
        let tmp = path.with_extension("tmp");
        let written = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(s.to_json().to_string().as_bytes())?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = written {
            eprintln!("cannot save the session in {}: {e}", path.display());
        }
        eprintln!("signed in as {} on {}", s.user_name, s.server);
        *self.session.lock().unwrap() = Some(s);
        *self.expired.lock().unwrap() = false;
        self.out.notify("auth.changed", self.auth_status());
    }

    /// Map a Jellyfin failure; a refused token marks the session expired.
    fn fail(&self, e: Error) -> RpcError {
        match e {
            Error::Auth => {
                let was = std::mem::replace(&mut *self.expired.lock().unwrap(), true);
                if !was {
                    eprintln!("the server refused the access token");
                    self.out.notify("auth.changed", self.auth_status());
                }
                rpc_err(-32001, "the Jellyfin session has expired")
            }
            Error::NotFound => rpc_err(-32002, "not found on the server"),
            Error::Status(503, m) | Error::Status(429, m) => RpcError {
                code: -32004,
                message: m,
            },
            Error::Status(code, m) if code >= 500 => {
                rpc_err(-32005, format!("server error {code} {m}"))
            }
            Error::Status(code, m) => rpc_err(-32603, format!("server answered {code} {m}")),
            Error::Network(m) => rpc_err(-32005, m),
        }
    }

    // ---------------------------------------------------------------- setup

    fn initialize(&self, p: &Value) -> Reply {
        let data_dir = p["data_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&data_dir);
        *self.french.lock().unwrap() = p["locale"].as_str().is_some_and(|l| l.starts_with("fr"));
        *self.output.lock().unwrap() = Output::from_json(&p["output"]);

        let id_path = data_dir.join("device_id");
        let device_id = std::fs::read_to_string(&id_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() == 32)
            .unwrap_or_else(|| {
                let id = random_hex(16);
                let _ = std::fs::write(&id_path, &id);
                id
            });
        let device = std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "ricercar".into());
        *self.client.lock().unwrap() = Some(Arc::new(Client::new(device, device_id)));
        *self.data_dir.lock().unwrap() = data_dir;
        *self.session.lock().unwrap() = std::fs::read_to_string(self.auth_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .and_then(|v| Session::from_json(&v));

        let proto = p["protocol"].as_u64().unwrap_or(0);
        if proto != PROTOCOL {
            eprintln!("host speaks protocol {proto}, this plugin {PROTOCOL}");
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "jellyfin", "name": "Jellyfin", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                "favorites": true, "reporting": true, "remote_control": false
            }
        }))
    }

    // ----------------------------------------------------------------- auth

    fn auth_begin(self: &Arc<Self>) -> Reply {
        let client = self.client()?;
        let french = *self.french.lock().unwrap();
        let mut login = self.login.lock().unwrap();
        if login.is_none() {
            let me = self.clone();
            let hint = self
                .session
                .lock()
                .unwrap()
                .as_ref()
                .map(|s| s.server.clone())
                .unwrap_or_else(|| self.server_hint.clone());
            *login = Some(
                login::Login::start(client, Arc::new(move |s| me.store(s)), hint, french)
                    .map_err(|e| rpc_err(-32603, format!("cannot open the sign-in page: {e}")))?,
            );
        }
        let instructions = if french {
            "Saisissez l'adresse de votre serveur Jellyfin dans la page qui s'ouvre, puis validez le code Quick Connect ou utilisez votre mot de passe. Depuis un autre appareil, collez ici « adresse jeton » (un jeton d'accès Jellyfin)."
        } else {
            "Enter your Jellyfin server address on the page that opens, then approve the Quick Connect code or use your password. From another device, paste “address token” here (a Jellyfin access token)."
        };
        Ok(json!({
            "url": login.as_ref().unwrap().url,
            "instructions": instructions,
            "expects_input": false
        }))
    }

    /// The paste field: `<server address> <access token>`, or nothing (the
    /// page may already have signed the user in).
    fn auth_complete(&self, p: &Value) -> Reply {
        let input = p["input"].as_str().unwrap_or("").trim();
        let mut words = input.split_whitespace();
        if let (Some(addr), Some(token), None) = (words.next(), words.next(), words.next()) {
            let client = self.client()?;
            let server = jellyfin::normalize_server(addr)
                .ok_or_else(|| rpc_err(-32602, "expected “server-address access-token”"))?;
            match client.sign_in_token(&server, token) {
                Ok(s) => self.store(s),
                Err(Error::Auth) => {}
                Err(e) => return Err(self.fail(e)),
            }
        }
        Ok(self.auth_status())
    }

    fn sign_out(&self) -> Reply {
        let s = self.session.lock().unwrap().take();
        if let (Some(s), Ok(client)) = (s, self.client()) {
            client.sign_out(&s);
        }
        *self.expired.lock().unwrap() = false;
        let _ = std::fs::remove_file(self.auth_path());
        Ok(Value::Null)
    }

    // --------------------------------------------------------------- browse

    fn root(&self) -> Reply {
        self.session()?;
        let fr = *self.french.lock().unwrap();
        let t = |en: &'static str, f: &'static str| if fr { f } else { en };
        let sections = [
            ("recent", t("Recently added", "Ajouts récents")),
            ("albums", t("Albums", "Albums")),
            ("artists", t("Artists", "Artistes")),
            ("playlists", t("Playlists", "Listes de lecture")),
            ("favorites", t("Favourites", "Favoris")),
            ("libraries", t("Folders", "Dossiers")),
        ];
        let sections: Vec<Value> = sections
            .iter()
            .map(
                |(r, title)| json!({"ref": r, "kind": "folder", "title": title, "browsable": true}),
            )
            .collect();
        Ok(json!({ "sections": sections }))
    }

    fn list(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let r = p["ref"].as_str().unwrap_or("");
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(PAGE).clamp(1, PAGE);
        let uid = s.user_id.clone();
        let mut q: Vec<(&str, String)> = vec![
            ("userId", uid.clone()),
            ("StartIndex", offset.to_string()),
            ("Limit", limit.to_string()),
            ("Fields", FIELDS.into()),
            ("EnableTotalRecordCount", "true".into()),
        ];
        let mut path = "/Items".to_string();
        let music = "MusicAlbum,Audio";
        match r {
            "recent" => q.extend([
                ("IncludeItemTypes", "MusicAlbum".into()),
                ("Recursive", "true".into()),
                ("SortBy", "DateCreated,SortName".into()),
                ("SortOrder", "Descending".into()),
            ]),
            "albums" => q.extend([
                ("IncludeItemTypes", "MusicAlbum".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "artists" => {
                path = "/Artists/AlbumArtists".into();
                q.push(("SortBy", "SortName".into()));
            }
            "playlists" => q.extend([
                ("IncludeItemTypes", "Playlist".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "favorites" => q.extend([
                ("IncludeItemTypes", music.into()),
                ("Filters", "IsFavorite".into()),
                ("Recursive", "true".into()),
                ("SortBy", "SortName".into()),
            ]),
            "libraries" => {
                // The user's music libraries, whole: no paging on the server.
                let views = client
                    .get(&s, &format!("/Users/{uid}/Views"), &[])
                    .map_err(|e| self.fail(e))?;
                let all: Vec<Value> = views["Items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|v| v["CollectionType"] == "music")
                    .filter_map(|v| items::item(&s.server, v))
                    .collect();
                return Ok(page(all, offset, limit));
            }
            _ => match items::split_ref(r) {
                Some(("a", id)) => q.extend([
                    ("ParentId", id.to_string()),
                    ("IncludeItemTypes", "Audio".into()),
                    ("Recursive", "true".into()),
                    ("SortBy", "ParentIndexNumber,IndexNumber,SortName".into()),
                ]),
                Some(("r", id)) => q.extend([
                    ("AlbumArtistIds", id.to_string()),
                    ("IncludeItemTypes", "MusicAlbum".into()),
                    ("Recursive", "true".into()),
                    ("SortBy", "ProductionYear,SortName".into()),
                    ("SortOrder", "Descending,Ascending".into()),
                ]),
                Some(("p", id)) => path = format!("/Playlists/{id}/Items"),
                Some(("f", id)) => q.extend([
                    ("ParentId", id.to_string()),
                    ("SortBy", "IsFolder,SortName".into()),
                ]),
                _ => return Err(rpc_err(-32002, "no such list")),
            },
        }
        let v = client.get(&s, &path, &q).map_err(|e| self.fail(e))?;
        let list = items::items(&s.server, &v);
        let total = v["TotalRecordCount"].as_u64();
        let seen = v["Items"].as_array().map_or(0, Vec::len) as u64;
        let has_more = match total {
            Some(t) => offset + seen < t,
            None => seen == limit,
        };
        Ok(json!({"items": list, "total": total, "has_more": has_more}))
    }

    fn search(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let query = p["query"].as_str().unwrap_or("").trim().to_string();
        let offset = p["offset"].as_u64().unwrap_or(0);
        let limit = p["limit"].as_u64().unwrap_or(50).clamp(1, PAGE);
        let wanted: Vec<String> = p["kinds"]
            .as_array()
            .map(|k| {
                k.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| {
                ["artist", "album", "track", "playlist"]
                    .map(String::from)
                    .to_vec()
            });
        if query.is_empty() {
            return Ok(json!({ "groups": [] }));
        }
        let mut groups = Vec::new();
        for kind in wanted {
            let (path, jf_type) = match kind.as_str() {
                "artist" => ("/Artists/AlbumArtists", None),
                "album" => ("/Items", Some("MusicAlbum")),
                "track" => ("/Items", Some("Audio")),
                "playlist" => ("/Items", Some("Playlist")),
                _ => continue,
            };
            let mut q: Vec<(&str, String)> = vec![
                ("userId", s.user_id.clone()),
                ("searchTerm", query.clone()),
                ("StartIndex", offset.to_string()),
                ("Limit", limit.to_string()),
                ("Fields", FIELDS.into()),
                ("Recursive", "true".into()),
                ("EnableTotalRecordCount", "true".into()),
            ];
            if let Some(t) = jf_type {
                q.push(("IncludeItemTypes", t.into()));
            }
            let v = client.get(&s, path, &q).map_err(|e| self.fail(e))?;
            let found = items::items(&s.server, &v);
            let total = v["TotalRecordCount"].as_u64();
            let seen = v["Items"].as_array().map_or(0, Vec::len) as u64;
            groups.push(json!({
                "kind": kind,
                "items": found,
                "total": total,
                "has_more": total.is_some_and(|t| offset + seen < t),
            }));
        }
        Ok(json!({ "groups": groups }))
    }

    /// One Jellyfin item by id, with `fields`.
    fn fetch(
        &self,
        client: &Client,
        s: &Session,
        id: &str,
        fields: &str,
    ) -> Result<Value, RpcError> {
        let v = client
            .get(
                s,
                "/Items",
                &[
                    ("userId", s.user_id.clone()),
                    ("Ids", id.to_string()),
                    ("Fields", fields.to_string()),
                ],
            )
            .map_err(|e| self.fail(e))?;
        v["Items"]
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .ok_or_else(|| rpc_err(-32002, "not found on the server"))
    }

    fn item_get(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let (_, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        let v = self.fetch(&client, &s, id, FIELDS)?;
        items::item(&s.server, &v).ok_or_else(|| rpc_err(-32002, "not a music item"))
    }

    fn favorite(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let (_, id) = items::split_ref(p["ref"].as_str().unwrap_or(""))
            .ok_or_else(|| rpc_err(-32002, "no such item"))?;
        let on = p["on"].as_bool().unwrap_or(false);
        let q = [("userId", s.user_id.clone())];
        let call = |path: &str| {
            if on {
                client.post_empty(&s, path, &q)
            } else {
                client.delete(&s, path, &q)
            }
        };
        // 10.9 and later, then the older route.
        let r = match call(&format!("/UserFavoriteItems/{id}")) {
            Err(Error::NotFound) => call(&format!("/Users/{}/FavoriteItems/{id}", s.user_id)),
            r => r,
        };
        r.map(|_| Value::Null).map_err(|e| self.fail(e))
    }

    // -------------------------------------------------------------- resolve

    fn resolve(&self, p: &Value) -> Reply {
        let (client, s) = self.session()?;
        let r = p["ref"].as_str().unwrap_or("");
        let Some(("t", id)) = items::split_ref(r) else {
            return Err(rpc_err(-32002, "not a track"));
        };
        let v = self.fetch(&client, &s, id, "MediaSources,MediaStreams")?;
        if v["Type"] != "Audio" {
            return Err(rpc_err(-32002, "not a track"));
        }
        let stored = items::format(&v).unwrap_or(json!({}));
        let rate = stored["sample_rate"].as_u64().map(|r| r as u32);
        let bits = stored["bits"].as_u64().map(|b| b as u8);
        let plan = items::plan(&self.output.lock().unwrap(), rate, bits);
        let source = v["MediaSources"][0]["Id"].as_str().unwrap_or(id);
        let mut url = format!(
            "{}/Audio/{id}/stream?static=true&mediaSourceId={source}&deviceId={}&ApiKey={}",
            s.server,
            client.device_id(),
            s.token
        );
        let mut format = stored.clone();
        if let Plan::Flac { rate, bits } = plan {
            url = format!(
                "{}/Audio/{id}/stream.flac?container=flac&audioCodec=flac&audioSampleRate={rate}&maxAudioBitDepth={bits}\
                 &mediaSourceId={source}&deviceId={}&playSessionId={}&ApiKey={}",
                s.server,
                client.device_id(),
                random_hex(8),
                s.token
            );
            format["sample_rate"] = rate.into();
            format["bits"] = bits.into();
            format["codec"] = "flac".into();
            eprintln!("{r}: {rate} Hz / {bits} bits transcode for this output");
        }
        let mut res = json!({
            "url": url,
            "duration_ms": v["RunTimeTicks"].as_i64().map(|t| t / 10_000),
            "format": format,
            "live": false,
        });
        // Jellyfin 10.9+ measures loudness (EBU R128) and stores a gain.
        if let Some(g) = v["NormalizationGain"].as_f64() {
            res["replaygain"] = json!({ "track_gain": g });
        }
        if let Some(o) = res.as_object_mut() {
            o.retain(|_, v| !v.is_null());
        }
        Ok(res)
    }

    // ------------------------------------------------------------ reporting

    fn report(&self, method: &str, p: &Value) {
        let Ok((client, s)) = self.session() else {
            return;
        };
        let Some(("t", id)) = items::split_ref(p["ref"].as_str().unwrap_or("")) else {
            return;
        };
        let ticks = |ms: &Value| ms.as_i64().unwrap_or(0) * 10_000;
        let (path, body) = match method {
            "playback.started" => (
                "/Sessions/Playing",
                json!({"ItemId": id, "PositionTicks": 0, "CanSeek": true, "PlayMethod": "DirectPlay"}),
            ),
            "playback.progress" => (
                "/Sessions/Playing/Progress",
                json!({"ItemId": id, "PositionTicks": ticks(&p["pos_ms"]), "CanSeek": true,
                       "PlayMethod": "DirectPlay", "EventName": "TimeUpdate"}),
            ),
            "playback.ended" => {
                // A track played to its end counts as played in Jellyfin.
                let pos = if p["reason"] == "ended" {
                    self.fetch(&client, &s, id, "")
                        .ok()
                        .and_then(|v| v["RunTimeTicks"].as_i64())
                        .unwrap_or_else(|| ticks(&p["listened_ms"]))
                } else {
                    ticks(&p["listened_ms"])
                };
                (
                    "/Sessions/Playing/Stopped",
                    json!({"ItemId": id, "PositionTicks": pos}),
                )
            }
            _ => return,
        };
        if let Err(e) = client.post(&s, path, body) {
            eprintln!("{method}: {e}");
        }
    }

    // ------------------------------------------------------------- dispatch

    fn handle(self: &Arc<Self>, method: &str, p: &Value) -> Reply {
        match method {
            "initialize" => self.initialize(p),
            "auth.status" => Ok(self.auth_status()),
            "auth.begin" => self.auth_begin(),
            "auth.complete" => self.auth_complete(p),
            "auth.sign_out" => self.sign_out(),
            "browse.root" => self.root(),
            "browse.list" => self.list(p),
            "search" => self.search(p),
            "item.get" => self.item_get(p),
            "favorites.set" => self.favorite(p),
            "track.resolve" => self.resolve(p),
            _ => Err(rpc_err(-32601, format!("method not found: {method}"))),
        }
    }
}

fn page(all: Vec<Value>, offset: u64, limit: u64) -> Value {
    let total = all.len() as u64;
    let items: Vec<Value> = all
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    json!({"items": items, "total": total, "has_more": offset + limit < total})
}

fn main() {
    let mut server_hint = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server_hint = args.next().unwrap_or_default(),
            "--version" => {
                println!("ricercar-jellyfin {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => eprintln!("unknown option {a}"),
        }
    }
    let out = Arc::new(Out(Mutex::new(std::io::stdout())));
    let plugin = Arc::new(Plugin {
        out: out.clone(),
        server_hint,
        data_dir: Mutex::new(std::env::temp_dir()),
        french: Mutex::new(false),
        output: Mutex::new(Output::default()),
        client: Mutex::new(None),
        session: Mutex::new(None),
        expired: Mutex::new(false),
        login: Mutex::new(None),
    });

    for line in BufReader::new(std::io::stdin()).lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = msg["method"].as_str().map(str::to_string) else {
            continue; // an answer; this plugin sends no requests
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = msg.get("id").cloned() else {
            // Notifications.
            match method.as_str() {
                "output.changed" => {
                    *plugin.output.lock().unwrap() = Output::from_json(&params["output"]);
                }
                m if m.starts_with("playback.") => {
                    let plugin = plugin.clone();
                    std::thread::spawn(move || plugin.report(&method, &params));
                }
                _ => {}
            }
            continue;
        };
        if method == "shutdown" {
            out.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            return;
        }
        let first = method == "initialize";
        let run = {
            let plugin = plugin.clone();
            let out = out.clone();
            move || {
                let reply = match plugin.handle(&method, &params) {
                    Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id,
                                     "error": {"code": e.code, "message": e.message}}),
                };
                out.send(reply);
            }
        };
        // The handshake first, in order; everything else may overlap.
        if first {
            run();
        } else {
            std::thread::spawn(run);
        }
    }
}
