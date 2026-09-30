#!/usr/bin/env python3
"""End-to-end test of ricercar-jellyfin against a live Jellyfin (see
jellyfin.sh): JSON-RPC over stdio, the sign-in page over HTTP, the server's
state through its REST API with the admin token, the streams with ffprobe.
Never plays anything: streams are downloaded to files."""
import json, subprocess, sys, threading, queue, urllib.request, urllib.error, http.client, time, os, stat
S = sys.argv[1]
BIN = os.environ["BIN"]
JF = os.environ["JF"]
ADMIN = os.environ["ADMIN"]
HOSTPORT = JF.split("://")[1]
USER, PASSWORD = "melomane", "sesame-42-pw"
DATA = S + "/pdata"; os.makedirs(DATA, exist_ok=True)
LOG = S + "/plugin.log"
OUT = {"device": "hw:9,0", "bit_perfect": True, "max_rate": 96000, "max_bits": 24, "rates": [44100, 48000, 88200, 96000]}
CD = {"device": "hw:9,0", "bit_perfect": True, "max_rate": 44100, "max_bits": 16, "rates": [44100]}
SECRETS = {PASSWORD, ADMIN}
MISSING = "0123456789abcdef0123456789abcdef"  # not Guid.Empty, which Jellyfin treats specially

class P:
    def __init__(s, *args):
        s.p = subprocess.Popen([BIN, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=open(LOG, "a"), text=True)
        s.q = {}; s.notes = queue.Queue(); s.n = 0; s.lock = threading.Lock()
        threading.Thread(target=s.read, daemon=True).start()
    def read(s):
        for l in s.p.stdout:
            m = json.loads(l)
            if "id" in m: s.q[m["id"]].put(m)
            else: s.notes.put(m)
    def call(s, method, params=None, timeout=30):
        with s.lock:
            s.n += 1; i = s.n; s.q[i] = queue.Queue()
            s.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params or {}}) + "\n"); s.p.stdin.flush()
        m = s.q[i].get(timeout=timeout); return m.get("result", m.get("error"))
    def notify(s, method, params):
        s.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}) + "\n"); s.p.stdin.flush()
    def note(s, timeout=5):
        try: return s.notes.get(timeout=timeout)
        except queue.Empty: return {}

AH = 'MediaBrowser Client="e2e", Device="e2e", DeviceId="e2e-setup", Version="1"'
def jf(path, method="GET", body=None, token=ADMIN):
    """The server's own view, as the admin (or another token)."""
    data = json.dumps(body).encode() if body is not None else (b"" if method == "POST" else None)
    r = urllib.request.Request(JF + path, data=data, method=method,
                               headers={"Authorization": f'{AH}, Token="{token}"', "Content-Type": "application/json",
                                        "Accept": "application/json"})
    t = urllib.request.urlopen(r, timeout=30).read()
    return json.loads(t) if t.strip() else None

def status_of(path, token):
    try: jf(path, token=token); return 200
    except urllib.error.HTTPError as e: return e.code

def page(url, route, body=None, host=None, method="POST"):
    """A request to the plugin's sign-in page: (status, body)."""
    u = urllib.parse.urlsplit(url)
    c = http.client.HTTPConnection(u.hostname, u.port, timeout=30)
    data = json.dumps(body).encode() if body is not None else None
    c.request(method, u.path + route, body=data,
              headers={"Host": host or u.netloc, **({"Content-Type": "application/json"} if data else {})})
    r = c.getresponse(); t = r.read().decode(); c.close()
    try: return r.status, json.loads(t)
    except ValueError: return r.status, t

def probe(url, path):
    """codec,rate,bits of a stream (FLAC gives its depth as raw bits, WAV as bits per sample)."""
    open(path, "wb").write(urllib.request.urlopen(url, timeout=120).read())
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_name,sample_rate,bits_per_raw_sample,bits_per_sample",
                          "-of", "json", path], capture_output=True, text=True).stdout
    st = (json.loads(out or "{}").get("streams") or [{}])[0]
    bits = next((str(st[k]) for k in ("bits_per_raw_sample", "bits_per_sample") if str(st.get(k, "0")) not in ("0", "N/A")), "?")
    return "%s,%s,%s" % (st.get("codec_name"), st.get("sample_rate"), bits)

def code(r):
    return r.get("code") if isinstance(r, dict) else None

def userdata(uid, item_id):
    return jf(f"/Items?userId={uid}&Ids={item_id}&EnableUserData=true")["Items"][0]["UserData"]

bad = 0; passed = 0
def check(c, msg):
    global bad, passed
    print(("PASS " if c else "FAIL ") + msg, flush=True)
    if c: passed += 1
    else: bad += 1

import urllib.parse
for f in ("auth.json", "device_id"):
    try: os.remove(DATA + "/" + f)
    except FileNotFoundError: pass
open(LOG, "w").close()
uid = [u["Id"] for u in jf("/Users") if u["Name"] == USER][0]

# ------------------------------------------------------------ handshake
p = P()
init = p.call("initialize", {"protocol": 1, "host": {"name": "ricercar", "version": "0.5.0"}, "data_dir": DATA,
                             "cache_dir": S + "/pcache", "locale": "en-GB", "output": OUT})
caps = init.get("capabilities", {})
check(init.get("protocol") == 1 and init["plugin"]["id"] == "jellyfin" and all(caps.get(k) for k in
      ("auth", "browse", "search", "resolve", "favorites", "reporting", "library")) and caps.get("remote_control") is False,
      "initialize: protocol 1, capabilities " + json.dumps(caps))
check(all(caps.get(k) is True for k in ("lyrics", "playlist_edit", "details", "radio")), "initialize: new capabilities")
decl = {s["key"]: s for s in init.get("settings", [])}
check(list(decl) == ["report_playback", "transcode"] and decl["report_playback"]["default"] is True
      and decl["transcode"]["default"] == "auto" and [o["value"] for o in decl["transcode"]["options"]] == ["auto", "never"]
      and decl["report_playback"]["label"] == "Report what I play", "initialize: settings declared " + json.dumps(list(decl)))
dev = open(DATA + "/device_id").read()
check(len(dev) == 32, "device id created: " + dev)
check(p.call("auth.status") == {"state": "signed_out"}, "auth.status signed_out at first")
for m, prm in (("browse.root", {}), ("browse.list", {"ref": "albums", "offset": 0, "limit": 5}),
               ("search", {"query": "x", "offset": 0, "limit": 5}), ("library.albums", {"offset": 0, "limit": 5}),
               ("track.resolve", {"ref": "t/abc", "purpose": "play"})):
    check((p.call(m, prm) or {}).get("code") == -32001, m + " before sign-in -> auth_required")

# paste field
check(p.call("auth.complete", {"input": "ftp://x sometoken"}).get("code") == -32602, "paste: bad address -> invalid params")
check(p.call("auth.complete", {"input": "127.0.0.1:9 sometoken"}).get("code") == -32005, "paste: unreachable server -> network")
r = p.call("auth.complete", {"input": JF + " 0000badtoken0000"})
check(r == {"state": "signed_out"}, "paste: refused token -> still signed out " + json.dumps(r))
check(p.call("auth.complete", {"input": ""}) == {"state": "signed_out"}, "empty paste -> status")

# ------------------------------------------------------ the sign-in page
b = p.call("auth.begin")
url = b.get("url", "")
check(url.startswith("http://127.0.0.1:") and len(url.rstrip("/").rsplit("/", 1)[1]) == 32 and b.get("expects_input") is False
      and "Quick Connect" in b.get("instructions", ""), "auth.begin: loopback page " + url)
check(p.call("auth.begin").get("url") == url, "auth.begin again: same page")
st, html = page(url, "", method="GET")
check(st == 200 and "<title>Sign in to Jellyfin</title>" in html and 'lang="en"' in html, "sign-in page served (%s)" % st)
st, _ = page(url, "", method="GET", host="localhost:" + url.split(":")[2].split("/")[0])
check(st == 403, "foreign Host header -> 403 (%s)" % st)
st, _ = page(url.rsplit("/", 2)[0] + "/" + MISSING + "/", "", method="GET")
check(st == 404, "wrong secret -> 404 (%s)" % st)
st, r = page(url, "check", {"server": "ftp://nowhere"})
check(st == 200 and r.get("ok") is False and "Invalid address" in r.get("error", ""), "check: invalid address")
st, r = page(url, "check", {"server": "127.0.0.1:9"})
check(r.get("ok") is False and "Cannot reach" in r.get("error", ""), "check: unreachable")
st, r = page(url, "check", {"server": HOSTPORT + "/"})
info = jf("/System/Info/Public", token="")
check(r.get("ok") and r.get("server") == JF and r.get("version") == info["Version"] and r.get("quick_connect") is True,
      "check: server found " + json.dumps(r))
st, r = page(url, "password", {"server": JF, "user": USER, "password": "wrong-one"})
check(r.get("ok") is False and "Wrong user name or password" in r.get("error", ""), "password: wrong password refused")
check(p.call("auth.status")["state"] == "signed_out", "still signed out after a wrong password")
st, r = page(url, "quick/poll", {})
check(r.get("ok") is False, "quick/poll without a request -> error")
st, r = page(url, "password", {"server": JF, "user": " " + USER + " ", "password": PASSWORD})
check(r.get("ok") and r.get("done") and r.get("who", {}).get("user") == USER, "password sign-in via the page " + json.dumps(r))
n = p.note()
check(n.get("method") == "auth.changed" and n["params"]["state"] == "signed_in" and n["params"]["account"]["display_name"] == USER
      and JF in n["params"]["account"].get("detail", ""), "auth.changed signed_in " + json.dumps(n.get("params")))
st = p.call("auth.status")
check(st["state"] == "signed_in" and st["account"]["display_name"] == USER, "auth.status signed_in")
a = json.load(open(DATA + "/auth.json")); mode = stat.S_IMODE(os.stat(DATA + "/auth.json").st_mode)
raw = open(DATA + "/auth.json").read()
TOKEN = a.get("token", ""); SECRETS.add(TOKEN)
check(mode == 0o600 and a.get("server") == JF and a.get("user_id") == uid and len(TOKEN) >= 32
      and PASSWORD not in raw and "Pw" not in raw and "password" not in raw.lower(), "auth.json mode %o, token, no password" % mode)
check(not os.path.exists(DATA + "/auth.tmp"), "no temp file left")
devs = jf("/Devices")["Items"]
mine = [d for d in devs if d.get("Id") == dev]
check(len(mine) == 1 and mine[0].get("AppName") == "ricercar" and mine[0].get("LastUserName") == USER,
      "shows in Jellyfin's Devices as ricercar " + json.dumps([(d.get("AppName"), d.get("Name")) for d in mine]))

# --------------------------------------------------------------- browse
root = p.call("browse.root")
check([x["ref"] for x in root["sections"]] == ["recent", "albums", "artists", "playlists", "favorites", "libraries"]
      and all(x["browsable"] and x["kind"] == "folder" for x in root["sections"]), "root sections " + str([x["title"] for x in root["sections"]]))
check([x["ref"] for x in root["home"]] == ["recent", "random"] and all(x["browsable"] for x in root["home"]), "home shelves")
for shelf in ("recent", "random"):
    sh = p.call("browse.list", {"ref": shelf, "offset": 0, "limit": 10})
    check(sorted(x["title"] for x in sh["items"]) == ["HiRes", "Sessions"] and all(x["kind"] == "album" for x in sh["items"]),
          "home shelf %s: %s" % (shelf, [x["title"] for x in sh["items"]]))
al = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 50})
check([x["title"] for x in al["items"]] == ["HiRes", "Sessions"] and al["total"] == 2 and not al["has_more"], "albums " + str([x["title"] for x in al["items"]]))
pg = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 1})
pg2 = p.call("browse.list", {"ref": "albums", "offset": 1, "limit": 1})
check(len(pg["items"]) == 1 and pg["has_more"] and pg["total"] == 2 and [x["title"] for x in pg2["items"]] == ["Sessions"]
      and not pg2["has_more"], "albums paging")
check(len(p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 5000})["items"]) == 2, "limit clamped, still fine")
sess = [x for x in al["items"] if x["title"] == "Sessions"][0]; hires = [x for x in al["items"] if x["title"] == "HiRes"][0]
check(sess["ref"].startswith("a/") and sess["kind"] == "album" and sess["artist"] == "Ensemble" and sess["year"] == 2021
      and sess.get("subtitle") == "Ensemble · 2021" and sess.get("genre") == "Jazz" and sess.get("art", "").startswith(JF + "/Items/"),
      "album fields " + json.dumps(sess, ensure_ascii=False))
h = urllib.request.urlopen(sess["art"], timeout=30)
check(h.headers.get("Content-Type", "").startswith("image/"), "cover url serves an image (%s)" % h.headers.get("Content-Type"))
check("art" not in hires, "album without a cover: no art")
tr = p.call("browse.list", {"ref": sess["ref"], "offset": 0, "limit": 200})
check([t["track_no"] for t in tr["items"]] == [1, 2, 3] and [t["title"] for t in tr["items"]] == ["Track 1", "Track 2", "Track 3"]
      and tr["total"] == 3, "album tracks")
t1, t2, t3 = tr["items"]
check(t1["ref"].startswith("t/") and t1["kind"] == "track" and t1["playable"] and t1["artist"] == "Ensemble" and t1["album"] == "Sessions"
      and t1["album_artist"] == "Ensemble" and t1["year"] == 2021 and t1["duration_ms"] == 20000
      and t1["format"] == {"sample_rate": 44100, "bits": 16, "channels": 1, "codec": "flac"} and t1.get("art") == sess["art"]
      or print("   ", json.dumps(t1, ensure_ascii=False)), "track fields")
check(t1.get("album_ref") == sess["ref"] and t1.get("favorite") is False and "entry_id" not in t1
      and t1.get("actions") == [{"id": "instant_mix", "label": "Instant Mix", "ref": "m/" + t1["ref"][2:], "kind": "play"}],
      "track links, favourite, actions " + json.dumps({k: t1.get(k) for k in ("album_ref", "artist_ref", "favorite", "actions")}))
check([a["id"] for a in sess.get("actions", [])] == ["instant_mix", "similar"] and sess["actions"][1]["kind"] == "browse"
      and sess.get("favorite") is False, "album actions: instant mix, similar albums")
pg = p.call("browse.list", {"ref": sess["ref"], "offset": 1, "limit": 1})
check([x["title"] for x in pg["items"]] == ["Track 2"] and pg["has_more"] and pg["total"] == 3, "album tracks paging")
ar = p.call("browse.list", {"ref": "artists", "offset": 0, "limit": 50})
check([x["title"] for x in ar["items"]] == ["Ensemble", "Trio"] and all(x["kind"] == "artist" and x["ref"].startswith("r/") for x in ar["items"]),
      "artists " + str([x["title"] for x in ar["items"]]))
ens = ar["items"][0]; trio = ar["items"][1]
check(ens.get("art", "").startswith(JF + "/Items/") and "art" not in trio, "artist without picture gets an album cover (Trio has none)")
pg = p.call("browse.list", {"ref": "artists", "offset": 1, "limit": 1})
check([x["title"] for x in pg["items"]] == ["Trio"] and pg["total"] == 2 and not pg["has_more"], "artists paging")
check([x["title"] for x in p.call("browse.list", {"ref": trio["ref"], "offset": 0, "limit": 10})["items"]] == ["HiRes"], "artist -> its albums")
check([x["title"] for x in p.call("browse.list", {"ref": ens["ref"], "offset": 0, "limit": 10})["items"]] == ["Sessions"], "artist Ensemble -> Sessions")
check(t1.get("artist_ref") == ens["ref"] and sess.get("artist_ref") == ens["ref"], "track and album artist_ref -> the artist")
check([a["ref"] for a in ens.get("actions", [])] == ["m/" + ens["ref"][2:], "x/" + ens["ref"][2:]]
      and ens["actions"][1]["label"] == "Similar artists", "artist actions")
# The refs of the actions list their items.
im = p.call("browse.list", {"ref": sess["actions"][0]["ref"], "offset": 0, "limit": 200})
check(sorted(x["title"] for x in im["items"]) == ["Track 1", "Track 2", "Track 3"] and all(x["kind"] == "track" for x in im["items"])
      and im["total"] == 3 and not im["has_more"], "instant mix of an album " + str([x["title"] for x in im["items"]]))
im = p.call("browse.list", {"ref": "m/" + t1["ref"][2:], "offset": 1, "limit": 1})
check(len(im["items"]) == 1 and im["has_more"], "instant mix of a track, paged")
sim = p.call("browse.list", {"ref": ens["actions"][1]["ref"], "offset": 0, "limit": 50})
check(isinstance(sim.get("items"), list) and all(x["kind"] == "artist" for x in sim["items"]),
      "similar artists " + str([x["title"] for x in sim.get("items", [])]))
it = p.call("item.get", {"ref": sess["actions"][0]["ref"]})
check(it.get("kind") == "folder" and it.get("title") == "Instant Mix" and it.get("subtitle") == "Sessions" and it.get("browsable"),
      "item.get of an instant mix ref " + json.dumps(it, ensure_ascii=False))
it = p.call("item.get", {"ref": sess["actions"][1]["ref"]})
check(it.get("title") == "Similar albums" and it.get("subtitle") == "Sessions", "item.get of a similar ref")
check(code(p.call("browse.list", {"ref": "m/" + MISSING, "offset": 0, "limit": 5})) == -32002, "mix of a missing item -> not_found")
libs = p.call("browse.list", {"ref": "libraries", "offset": 0, "limit": 10})
check([(x["title"], x["kind"]) for x in libs["items"]] == [("Music", "folder")] and libs["total"] == 1 and not libs["has_more"]
      and libs["items"][0]["ref"].startswith("f/"), "libraries: " + json.dumps(libs, ensure_ascii=False))
check(p.call("browse.list", {"ref": "libraries", "offset": 1, "limit": 10})["items"] == [], "libraries paging past the end")
fold = p.call("browse.list", {"ref": libs["items"][0]["ref"], "offset": 0, "limit": 10})
check(sorted(x["title"] for x in fold["items"]) == ["Ensemble", "Trio"] and all(x["browsable"] for x in fold["items"]), "library folder content "
      + str([(x["title"], x["kind"]) for x in fold["items"]]))
pg = p.call("browse.list", {"ref": libs["items"][0]["ref"], "offset": 0, "limit": 1})
check(len(pg["items"]) == 1 and pg["has_more"] and pg["total"] == 2, "library folder paging")
check(p.call("browse.list", {"ref": "nope", "offset": 0, "limit": 5}).get("code") == -32002, "unknown list -> not_found")
r = p.call("browse.list", {"ref": "a/" + MISSING, "offset": 0, "limit": 5})
check(code(r) == -32002, "missing album -> not_found " + json.dumps(r))
r = p.call("browse.list", {"ref": "f/" + MISSING, "offset": 0, "limit": 5})
check(code(r) == -32002, "missing folder -> not_found " + json.dumps(r))
r = p.call("browse.list", {"ref": "p/" + MISSING, "offset": 0, "limit": 5})
check(code(r) == -32002, "missing playlist -> not_found " + json.dumps(r))

# playlist, made on the server as a Jellyfin app would, by the user
pl = jf("/Playlists", "POST", {"Name": "Road Mix", "Ids": [t1["ref"][2:], t3["ref"][2:]], "UserId": uid, "MediaType": "Audio"}, token=TOKEN)
pls = p.call("browse.list", {"ref": "playlists", "offset": 0, "limit": 50})
mix = [x for x in pls["items"] if x["title"] == "Road Mix"]
check(len(mix) == 1 and mix[0]["ref"] == "p/" + pl["Id"] and mix[0]["kind"] == "playlist" and mix[0].get("subtitle") == "2 ♪",
      "playlists " + json.dumps(pls["items"], ensure_ascii=False))
pt = p.call("browse.list", {"ref": mix[0]["ref"], "offset": 0, "limit": 50})
check([x["title"] for x in pt["items"]] == ["Track 1", "Track 3"], "playlist tracks")
check(all(x.get("entry_id") for x in pt["items"]), "playlist tracks carry entry ids " + str([x.get("entry_id") for x in pt["items"]]))
check(mix[0].get("editable") is True and p.call("item.get", {"ref": mix[0]["ref"]}).get("editable") is True,
      "the user's own playlist is editable")
pg = p.call("browse.list", {"ref": mix[0]["ref"], "offset": 1, "limit": 1})
check([x["title"] for x in pg["items"]] == ["Track 3"] and not pg["has_more"], "playlist paging " + json.dumps({k: pg.get(k) for k in ("total", "has_more")}))

# --------------------------------------------------------------- search
def groups(q, **kw):
    r = p.call("search", {"query": q, "offset": 0, "limit": 10, **kw})
    return {g["kind"]: [i["title"] for i in g["items"]] for g in r["groups"]}, r
g, r = groups("hi")
check(g == {"artist": [], "album": ["HiRes"], "playlist": [], "track": ["Hi 1", "Hi 2"]}, "search hi " + str(g))
check([x["kind"] for x in r["groups"]] == ["artist", "album", "playlist", "track"], "search group order")
g, _ = groups("tri")
check(g["artist"] == ["Trio"], "search artist " + str(g))
g, _ = groups("road")
check(g["playlist"] == ["Road Mix"], "search playlist " + str(g))
g, r = groups("TRACK", kinds=["track"])
check(list(g) == ["track"] and len(g["track"]) == 3 and r["groups"][0]["total"] == 3, "search tracks only, any case " + str(g))
r = p.call("search", {"query": "track", "kinds": ["track"], "offset": 0, "limit": 2})
check(len(r["groups"][0]["items"]) == 2 and r["groups"][0]["has_more"], "search paging")
check(p.call("search", {"query": "  ", "offset": 0, "limit": 5}) == {"groups": []}, "empty query -> no groups")

# -------------------------------------------------------------- library
for m, exp in (("library.albums", 2), ("library.artists", 2), ("library.tracks", 5), ("library.playlists", 1)):
    r = p.call(m, {"offset": 0, "limit": 200})
    check(len(r["items"]) == exp and r["total"] == exp and not r["has_more"], "%s: %d" % (m, len(r["items"])))
la = p.call("library.albums", {"offset": 0, "limit": 200})["items"]
check(all(x["kind"] == "album" and x["browsable"] and x.get("artist") and x.get("year") for x in la), "library albums: artist, year")
lt = p.call("library.tracks", {"offset": 0, "limit": 200})["items"]
check(all(x["kind"] == "track" and x.get("format") and x.get("duration_ms") for x in lt), "library tracks: format, duration")
pg = p.call("library.tracks", {"offset": 4, "limit": 2})
check(len(pg["items"]) == 1 and not pg["has_more"] and pg["total"] == 5, "library.tracks paging")
hi1 = [x for x in lt if x["title"] == "Hi 1"][0]
check(hi1["format"] == {"sample_rate": 192000, "bits": 24, "channels": 1, "codec": "flac"}, "hi-res track format " + json.dumps(hi1["format"]))

# ------------------------------------------------------------- item.get
it = p.call("item.get", {"ref": t1["ref"]})
check(it.get("title") == "Track 1" and it.get("format") == t1["format"] and it.get("ref") == t1["ref"], "item.get track")
check(p.call("item.get", {"ref": sess["ref"]}).get("kind") == "album", "item.get album")
check(p.call("item.get", {"ref": ens["ref"]}).get("kind") == "artist", "item.get artist")
check(p.call("item.get", {"ref": mix[0]["ref"]}).get("kind") == "playlist", "item.get playlist")
check(p.call("item.get", {"ref": "t/" + MISSING}).get("code") == -32002, "item.get missing -> not_found")
check(p.call("item.get", {"ref": "t/../x"}).get("code") == -32002, "item.get bad ref -> not_found")
check(p.call("item.get", {"ref": "albums"}).get("code") == -32002, "item.get section ref -> not_found")

# ----------------------------------------------------------- favourites
check(p.call("favorites.set", {"ref": t2["ref"], "on": True}) is None, "favorite a track")
check(userdata(uid, t2["ref"][2:])["IsFavorite"] is True, "  ... on the server")
check(p.call("favorites.set", {"ref": hires["ref"], "on": True}) is None, "favorite an album")
check(userdata(uid, hires["ref"][2:])["IsFavorite"] is True, "  ... on the server")
fav = p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50})
check(sorted((x["kind"], x["title"]) for x in fav["items"]) == [("album", "HiRes"), ("track", "Track 2")], "favorites list")
check(p.call("favorites.set", {"ref": hires["ref"], "on": False}) is None and userdata(uid, hires["ref"][2:])["IsFavorite"] is False,
      "unfavorite album, on the server")
check([x["title"] for x in p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50})["items"]] == ["Track 2"], "favorites list after")
jf(f"/UserFavoriteItems/{t3['ref'][2:]}?userId={uid}", "POST", token=TOKEN)
check(any(x["ref"] == t3["ref"] for x in p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50})["items"]),
      "favourite set in another app shows up")
check(code(p.call("favorites.set", {"ref": "t/" + MISSING, "on": True})) == -32002, "favorite missing item -> not_found")
check(userdata(root_uid := [u["Id"] for u in jf("/Users") if u["Name"] == "root"][0], t2["ref"][2:])["IsFavorite"] is False,
      "favourite is the signed-in user's, not the admin's")

check(p.call("item.get", {"ref": t2["ref"]}).get("favorite") is True and all(x.get("favorite") is True for x in fav["items"]),
      "favourite state on items")

# --------------------------------------------------------------- lyrics
ly = p.call("lyrics.get", {"ref": t2["ref"]})
check(ly == {"synced": [{"time_ms": 1500, "text": "First line"}, {"time_ms": 4000, "text": "Second line"},
                        {"time_ms": 62250, "text": "Third line"}]}, "synced lyrics (.lrc) " + json.dumps(ly))
ly = p.call("lyrics.get", {"ref": t3["ref"]})
check(ly == {"plain": "Plain one\nPlain two"}, "plain lyrics (.txt) " + json.dumps(ly))
check(code(p.call("lyrics.get", {"ref": t1["ref"]})) == -32002, "no lyrics -> not_found")
check(code(p.call("lyrics.get", {"ref": sess["ref"]})) == -32002, "lyrics of an album -> not_found")

# ------------------------------------------------- label, details, radio
# Set by the admin as Jellyfin's metadata editor would: a label, an overview
# in HTML, a rating.
hid = hires["ref"][2:]
dto = jf(f"/Items/{hid}?userId={jf('/Users/Me')['Id']}")
dto.update({"Studios": [{"Name": "North Label"}], "CommunityRating": 8.5,
            "Overview": "<p>Recorded <b>live</b> in one take.</p><p>Second &amp; last.</p>"})
jf(f"/Items/{hid}", "POST", dto)
# The label's own item appears with the next library scan; reading it by
# name creates it at once.
jf("/Studios/" + urllib.parse.quote("North Label"))
time.sleep(1)
h2 = p.call("item.get", {"ref": hires["ref"]})
check(h2.get("label_ref", "").startswith("s/"), "album label_ref " + json.dumps(h2.get("label_ref")))
lab = {}
for _ in range(10):  # the label's index follows the update shortly
    lab = p.call("browse.list", {"ref": h2.get("label_ref", "s/x"), "offset": 0, "limit": 10})
    if lab.get("items"): break
    time.sleep(1)
check([x["title"] for x in lab.get("items", [])] == ["HiRes"], "label ref lists its albums " + json.dumps(lab)[:200])
li = p.call("item.get", {"ref": h2.get("label_ref", "s/x")})
check(li.get("title") == "North Label" and li.get("kind") == "folder" and li.get("browsable"), "item.get of a label " + json.dumps(li))
d = p.call("item.details", {"ref": hires["ref"]})
facts = {f["label"]: f["value"] for f in d.get("facts", [])}
check(d.get("biography", {}).get("text") == "Recorded live in one take.\n\nSecond & last.", "album biography, HTML stripped " + json.dumps(d.get("biography")))
check(facts.get("Label") == "North Label" and facts.get("Year") == "2024" and facts.get("Rating") == "8.5/10", "album facts " + json.dumps(facts))
d = p.call("item.details", {"ref": sess["ref"]})
check("biography" not in d and {f["label"]: f["value"] for f in d.get("facts", [])}.get("Genre") == "Jazz"
      and all(s["items"] for s in d.get("related", [])), "details without overview " + json.dumps(d)[:300])
check(code(p.call("item.details", {"ref": "a/" + MISSING})) == -32002, "details of a missing item -> not_found")
check(code(p.call("item.details", {"ref": "albums"})) == -32002, "details of a section -> not_found")
r = p.call("radio.next", {"seed": t1["ref"], "exclude": [t2["ref"]], "limit": 10})
refs = [x["ref"] for x in r.get("items", [])]
check(refs and t1["ref"] not in refs and t2["ref"] not in refs and all(x["kind"] == "track" and x["playable"] for x in r["items"]),
      "radio.next from a track, seed and exclude left out " + str([x["title"] for x in r.get("items", [])]))
r = p.call("radio.next", {"seed": sess["ref"], "exclude": [], "limit": 1})
check(len(r.get("items", [])) == 1, "radio.next from an album, limit 1")
check(code(p.call("radio.next", {"seed": "albums", "exclude": [], "limit": 5})) == -32002, "radio.next from a section -> not_found")

# ------------------------------------------------------ playlist editing
check(code(p.call("playlists.create", {"name": "  "})) == -32602, "create without a name -> bad params")
np = p.call("playlists.create", {"name": "Late Set", "description": "ignored", "public": False})
nid = np.get("ref", "p/")[2:]
check(np.get("kind") == "playlist" and np.get("title") == "Late Set" and np.get("editable") is True
      and jf(f"/Items?userId={uid}&Ids={nid}")["Items"][0]["Name"] == "Late Set", "playlists.create " + json.dumps(np, ensure_ascii=False))
def titles(ref):
    return [x["title"] for x in p.call("browse.list", {"ref": ref, "offset": 0, "limit": 50})["items"]]
def entries(ref):
    return p.call("browse.list", {"ref": ref, "offset": 0, "limit": 50})["items"]
check(p.call("playlists.add", {"ref": np["ref"], "items": [t1["ref"], t2["ref"], t3["ref"]]}) is None
      and titles(np["ref"]) == ["Track 1", "Track 2", "Track 3"], "playlists.add")
check(code(p.call("playlists.add", {"ref": np["ref"], "items": [sess["ref"]]})) == -32602, "add an album ref -> bad params")
e = entries(np["ref"])
check(p.call("playlists.move", {"ref": np["ref"], "entry": e[2]["entry_id"], "to": 0}) is None
      and titles(np["ref"]) == ["Track 3", "Track 1", "Track 2"], "playlists.move to the top")
e = entries(np["ref"])
check(p.call("playlists.move", {"ref": np["ref"], "entry": e[0]["entry_id"], "to": 1}) is None
      and titles(np["ref"]) == ["Track 1", "Track 3", "Track 2"], "playlists.move down: out, then in at 1")
e = entries(np["ref"])
check(p.call("playlists.move", {"ref": np["ref"], "entry": e[0]["entry_id"], "to": 99}) is None
      and titles(np["ref"]) == ["Track 3", "Track 2", "Track 1"], "playlists.move past the end -> last")
e = entries(np["ref"])
check(p.call("playlists.move", {"ref": np["ref"], "entry": e[2]["entry_id"], "to": 1}) is None
      and titles(np["ref"]) == ["Track 3", "Track 1", "Track 2"], "playlists.move up: out, then in at 1")
e = entries(np["ref"])
check(p.call("playlists.remove", {"ref": np["ref"], "entries": [e[1]["entry_id"]]}) is None
      and titles(np["ref"]) == ["Track 3", "Track 2"], "playlists.remove")
check(code(p.call("playlists.remove", {"ref": np["ref"], "entries": ["../x"]})) == -32602, "remove a bad entry -> bad params")
check(p.call("playlists.rename", {"ref": np["ref"], "name": "Early Set"}) is None
      and jf(f"/Items?userId={uid}&Ids={nid}")["Items"][0]["Name"] == "Early Set", "playlists.rename")
# Someone else's playlist, public: listed, not editable, edits refused.
shared = jf("/Playlists", "POST", {"Name": "Shared", "Ids": [t1["ref"][2:]], "UserId": root_uid, "MediaType": "Audio", "IsPublic": True})
sh = [x for x in p.call("browse.list", {"ref": "playlists", "offset": 0, "limit": 50})["items"] if x["title"] == "Shared"]
check(len(sh) == 1 and sh[0].get("editable") is False, "someone else's public playlist: not editable " + json.dumps(sh))
sref = "p/" + shared["Id"]
check(all(code(p.call(m, prm)) == -32602 for m, prm in (
    ("playlists.add", {"ref": sref, "items": [t2["ref"]]}), ("playlists.rename", {"ref": sref, "name": "Mine"}),
    ("playlists.delete", {"ref": sref}), ("playlists.move", {"ref": sref, "entry": t1["ref"][2:], "to": 0}),
    ("playlists.remove", {"ref": sref, "entries": [t1["ref"][2:]]}))), "edits of someone else's playlist -> bad params")
check(jf(f"/Items?userId={root_uid}&Ids={shared['Id']}")["Items"][0]["Name"] == "Shared" and titles(sref) == ["Track 1"],
      "  ... and nothing changed on the server")
check(p.call("auth.status")["state"] == "signed_in", "refused edits leave the session alone")
check(p.call("playlists.delete", {"ref": np["ref"]}) is None and jf(f"/Items?userId={uid}&Ids={nid}")["Items"] == [], "playlists.delete")
jf(f"/Items/{shared['Id']}", "DELETE")

# -------------------------------------------------------------- resolve
r = p.call("track.resolve", {"ref": t1["ref"], "purpose": "play"})
check(f"/Audio/{t1['ref'][2:]}/stream?static=true" in r.get("url", "") and r.get("duration_ms") == 20000 and r.get("live") is False
      and r.get("format") == t1["format"], "resolve direct " + json.dumps({k: v for k, v in r.items() if k != "url"}))
h = urllib.request.urlopen(r["url"], timeout=60); body = h.read()
orig = open(S + "/music/Ensemble/Sessions/01.flac", "rb").read()
check(h.headers.get("Content-Length") == str(len(body)) and body == orig, "direct stream: the original file, byte for byte, with Content-Length")
open(S + "/direct.flac", "wb").write(body)
got = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_name,sample_rate,bits_per_raw_sample", "-of", "csv=p=0",
                      S + "/direct.flac"], capture_output=True, text=True).stdout.strip()
check(got == "flac,44100,16", "direct file ffprobe: " + got)
h = urllib.request.urlopen(urllib.request.Request(r["url"], headers={"Range": "bytes=100-199"}), timeout=30)
check(h.status == 206 and h.read() == orig[100:200], "direct stream: ranges (seekable)")
r = p.call("track.resolve", {"ref": hi1["ref"], "purpose": "preload"})
check("/stream.flac?" in r.get("url", "") and r["format"]["sample_rate"] == 96000 and r["format"]["bits"] == 24 and r["format"]["codec"] == "flac"
      and r.get("duration_ms") == 15000, "192k/24 on a 96k DAC -> transcode " + json.dumps(r.get("format", r)))
got = probe(r["url"], S + "/hi96.flac")
check(got == "flac,96000,24", "transcoded stream ffprobe (96k DAC): " + got)
p.notify("output.changed", {"output": CD}); time.sleep(0.3)
r = p.call("track.resolve", {"ref": hi1["ref"], "purpose": "play"})
check("/stream.wav?" in r.get("url", "") and r.get("format", {}).get("sample_rate") == 44100 and r["format"].get("bits") == 16
      and r["format"].get("codec") == "wav", "after output.changed to a 44.1/16 DAC -> " + json.dumps(r.get("format", r)))
got = probe(r["url"], S + "/hi44.wav")
check(got == "pcm_s16le,44100,16", "depth-reducing transcode ffprobe (CD DAC): " + got)
r = p.call("track.resolve", {"ref": t1["ref"], "purpose": "play"})
check("static=true" in r.get("url", ""), "CD DAC, CD file -> direct")
p.notify("output.changed", {"output": {"device": "default", "bit_perfect": False}}); time.sleep(0.3)
r = p.call("track.resolve", {"ref": hi1["ref"], "purpose": "play"})
check("static=true" in r.get("url", "") and r["format"]["sample_rate"] == 192000, "not bit-perfect output -> original")
p.notify("output.changed", {"output": OUT}); time.sleep(0.3)
r = p.call("track.resolve", {"ref": "t/" + MISSING, "purpose": "play"})
check(r.get("code") == -32002, "resolve missing track -> not_found")
check(p.call("track.resolve", {"ref": sess["ref"], "purpose": "play"}).get("code") == -32002, "resolve an album -> not_found")

# ------------------------------------------------------------ reporting
before = userdata(uid, t3["ref"][2:])
p.notify("playback.started", {"ref": t3["ref"]}); time.sleep(1.5)
now = [s for s in jf("/Sessions") if s.get("DeviceId") == dev]
check(any(s.get("NowPlayingItem", {}).get("Id") == t3["ref"][2:] for s in now), "playback.started: now playing on the server")
p.notify("playback.progress", {"ref": t3["ref"], "pos_ms": 10000}); time.sleep(1)
p.notify("playback.ended", {"ref": t3["ref"], "listened_ms": 19500, "reason": "ended"}); time.sleep(2)
after = userdata(uid, t3["ref"][2:])
check(after.get("PlayCount", 0) == before.get("PlayCount", 0) + 1 and after.get("Played") is True,
      "played to the end: play count %s -> %s, played %s" % (before.get("PlayCount"), after.get("PlayCount"), after.get("Played")))
now = [s for s in jf("/Sessions") if s.get("DeviceId") == dev]
check(not any(s.get("NowPlayingItem") for s in now), "stopped: no longer playing on the server")
# Settings, changed without a restart: nothing reported, originals only.
p.notify("settings.changed", {"settings": {"report_playback": False, "transcode": "never"}}); time.sleep(0.3)
b1 = userdata(uid, t1["ref"][2:])
p.notify("playback.started", {"ref": t1["ref"]}); time.sleep(1.5)
now = [s for s in jf("/Sessions") if s.get("DeviceId") == dev]
check(not any(s.get("NowPlayingItem") for s in now) and userdata(uid, t1["ref"][2:]).get("PlayCount") == b1.get("PlayCount"),
      "report_playback off: nothing sent")
r = p.call("track.resolve", {"ref": hi1["ref"], "purpose": "play"})
check("static=true" in r.get("url", "") and r["format"]["sample_rate"] == 192000, "transcode never: the original on a 96k DAC")
p.notify("settings.changed", {"settings": {"report_playback": True, "transcode": "auto"}}); time.sleep(0.3)
r = p.call("track.resolve", {"ref": hi1["ref"], "purpose": "play"})
check("/stream.flac?" in r.get("url", ""), "transcode auto again")
b2 = userdata(uid, t2["ref"][2:])
p.notify("playback.started", {"ref": t2["ref"]}); time.sleep(1)
# Jellyfin (12.1) counts an audio play when playback *starts* (SessionManager.
# OnPlaybackStart: PlayCount++, Played for items without resume), with or
# without this plugin; so a skip counts once too, and the stop must not add a
# second play.
p.notify("playback.ended", {"ref": t2["ref"], "listened_ms": 500, "reason": "skipped"}); time.sleep(2)
a2 = userdata(uid, t2["ref"][2:])
check(a2.get("PlayCount", 0) == b2.get("PlayCount", 0) + 1, "skip: counted once, at start, by Jellyfin (%s -> %s)" % (b2.get("PlayCount"), a2.get("PlayCount")))
p.call("shutdown")
p.p.wait(timeout=10)

# -------------------------------------------------------------- restart
p = P()
init = p.call("initialize", {"protocol": 1, "data_dir": DATA, "locale": "fr-FR", "output": OUT, "settings": {"transcode": "never", "gone": 1}})
check(init["settings"][0]["label"] == "Signaler les écoutes", "settings labels in French")
while not p.notes.empty(): p.notes.get()
p.notify("locale.changed", {"locale": "en-GB"})
n = p.note()
check(n.get("method") == "settings.declared" and n["params"]["settings"][0]["label"] == "Report what I play",
      "locale.changed: settings declared again in English")
p.notify("locale.changed", {"locale": "en-US"})
check(p.note(timeout=1) == {}, "same language again: nothing sent")
p.notify("locale.changed", {"locale": "fr-BE"})
n = p.note()
check(n.get("method") == "settings.declared" and n["params"]["settings"][0]["label"] == "Signaler les écoutes",
      "locale.changed back to French")
r = p.call("track.resolve", {"ref": hi1["ref"], "purpose": "play"})
check("static=true" in r.get("url", ""), "settings from initialize: transcode never")
check([a["label"] for a in p.call("item.get", {"ref": sess["ref"]}).get("actions", [])] == ["Mix instantané", "Albums similaires"],
      "action labels in French")
st = p.call("auth.status")
check(st["state"] == "signed_in" and st["account"]["display_name"] == USER and open(DATA + "/device_id").read() == dev,
      "session and device id restored after restart")
check(len(p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 5}).get("items", [])) == 2, "browse works after restart")
check(p.call("browse.root")["sections"][1]["title"] == "Albums" and p.call("browse.root")["sections"][2]["title"] == "Artistes", "french titles")

# token revoked on the server (Devices -> delete) -> expired
jf("/Devices?id=" + dev, "DELETE")
check(status_of("/Users/Me", TOKEN) == 401, "token revoked on the server")
while not p.notes.empty(): p.notes.get()
r = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 5})
check(r.get("code") == -32001, "revoked token -> auth_required " + json.dumps(r))
n = p.note()
check(n.get("method") == "auth.changed" and n["params"]["state"] == "expired", "auth.changed expired " + json.dumps(n))
check(p.call("auth.status")["state"] == "expired", "auth.status expired")
check(p.call("item.get", {"ref": t1["ref"]}).get("code") == -32001 and p.notes.empty(), "still auth_required, no second auth.changed")
check(p.call("auth.sign_out") is None and not os.path.exists(DATA + "/auth.json") and p.call("auth.status") == {"state": "signed_out"},
      "auth.sign_out")

# ------------------------------------------------------- Quick Connect
b = p.call("auth.begin"); url = b["url"]
st, html = page(url, "", method="GET")
check('lang="fr"' in html and "Connexion à Jellyfin" in html, "sign-in page in French")
st, r = page(url, "quick", {"server": JF})
code = r.get("code", "")
check(r.get("ok") and len(code) >= 6, "quick connect: code " + code)
st, r = page(url, "quick/poll", {})
check(r == {"ok": True, "done": False}, "quick connect: waiting " + json.dumps(r))
jf(f"/QuickConnect/Authorize?code={code}&userId={uid}", "POST")
r = {}
for _ in range(10):
    st, r = page(url, "quick/poll", {})
    if r.get("done"): break
    time.sleep(1)
check(r.get("ok") and r.get("done") and r["who"]["user"] == USER, "quick connect: approved -> signed in " + json.dumps(r))
n = p.note()
check(n.get("method") == "auth.changed" and n["params"]["state"] == "signed_in", "auth.changed after Quick Connect")
TOKEN2 = json.load(open(DATA + "/auth.json"))["token"]; SECRETS.add(TOKEN2)
check(TOKEN2 != TOKEN and len(p.call("library.tracks", {"offset": 0, "limit": 10}).get("items", [])) == 5, "Quick Connect session works")

# paste field: server + token
p.call("auth.sign_out")
check(status_of("/Users/Me", TOKEN2) == 401, "sign out revokes the token on the server")
tok3 = jf("/Users/AuthenticateByName", "POST", {"Username": USER, "Pw": PASSWORD}, token="")["AccessToken"]; SECRETS.add(tok3)
st = p.call("auth.complete", {"input": f"{JF}/ {tok3}"})
check(st.get("state") == "signed_in" and st["account"]["display_name"] == USER, "paste: address + token -> signed in")
n = p.note(); check(n.get("method") == "auth.changed", "auth.changed after paste")
check(json.load(open(DATA + "/auth.json"))["server"] == JF, "pasted address normalised")
check(p.call("nope").get("code") == -32601, "unknown method -> -32601")
check(p.call("auth.sign_out") is None and p.call("auth.status") == {"state": "signed_out"}, "final sign out")
p.call("shutdown"); p.p.wait(timeout=10)

log = open(LOG).read()
leaks = [w for w in (USER, PASSWORD, *SECRETS) if w and w in log]
check(not leaks, "plugin log has no user name, password or token (%d lines)%s" % (log.count("\n"), " LEAKS: " + str(leaks) if leaks else ""))
print("TOTAL: %d passed, %d failed" % (passed, bad)); sys.exit(1 if bad else 0)
