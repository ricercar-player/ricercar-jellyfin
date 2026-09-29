# ricercar-jellyfin

A [ricercar](https://github.com/ricercar-player/ricercar) source plugin that plays the
music of your own [Jellyfin](https://jellyfin.org) server.

- **Library:** the albums, artists, tracks and playlists of your server
  join ricercar's own Albums, Artists and Tracks pages, its search and its
  Playlists list, next to your local music. The Home page gets
  "Recently added" and "Random albums" shelves.
- **Browse:** recently added, albums, artists, playlists, favourites, and
  your music libraries as folders.
- **Search:** artists, albums, tracks and playlists.
- **Bit-perfect:** tracks play from the original file, byte for byte. Only
  when your DAC cannot take a file's sample rate or bit depth does the
  plugin ask Jellyfin for a FLAC transcode, at the closest rate the DAC
  accepts (same 44.1/48 kHz family, never higher than the original).
- **Favourites** sync both ways, and plays are reported back to Jellyfin
  (play count, "played", the dashboard's "now playing").
- **Loudness:** when Jellyfin has measured a track (10.9 and later, with
  the LUFS scan on), its gain is passed to ricercar as ReplayGain.

The plugin talks to Jellyfin's documented REST API only. It works with
Jellyfin 10.9 and later (tested with 12.1).

## Install

**From ricercar (0.4.0 and later):** open **Plugins** in the sidebar and
install *Jellyfin*.

**By hand:** download `jellyfin-x86_64` or `jellyfin-aarch64` from the
[releases](https://github.com/ricercar-player/ricercar-jellyfin/releases), check it
against its `.sha256` file, make it executable, and declare it in
`~/.config/ricercar/config.toml`:

```toml
[[plugins]]
id = "jellyfin"
command = "/home/you/.local/bin/jellyfin-x86_64"
# args = ["--server", "http://192.168.1.10:8096"]   # prefills the sign-in page
```

**From source:**

```sh
cargo build --release
# target/release/ricercar-jellyfin
```

## Sign in

Click **Sign in** next to Jellyfin in ricercar. A page opens in your browser
(served by the plugin on 127.0.0.1): enter the server address, then either

- approve the **Quick Connect** code it shows from another Jellyfin app where
  you are signed in (profile → Quick Connect), or
- type your user name and password.

The password goes from your browser to the plugin to your server; ricercar
never sees it and the plugin does not store it. The plugin keeps the access
token Jellyfin gives it, in
`~/.local/share/ricercar/plugins/jellyfin/auth.json` (mode 600). It shows up
in Jellyfin's **Devices** list as *ricercar* and can be revoked there.

Signing in from another computer than the one running ricercar: paste
`<server address> <access token>` in ricercar's sign-in field, for example
`https://jf.example.org 0123456789abcdef…` (an access token of your user).

## Notes

- Transcoded streams (only when the DAC cannot take the original) have no
  known length, so they cannot be seeked. Originals can.
- The server address is fixed at sign-in. To use another server, sign out
  and in again. To use two servers at once, declare the plugin twice with
  different `id`s.
- Stream URLs carry the access token (Jellyfin's `ApiKey` parameter), as
  every Jellyfin client does. ricercar never stores resolved URLs.
- Use `https://` for a server outside your home network.

## Protocol

Plugin protocol 1, as described in ricercar's
[docs/plugins.md](https://github.com/ricercar-player/ricercar/blob/main/docs/plugins.md).

| Ref | Meaning |
|---|---|
| `recent`, `albums`, `artists`, `playlists`, `favorites`, `libraries` | Top-level sections |
| `random` | Home shelf: albums in random order |
| `tracks` | Every track (behind `library.tracks`) |
| `t/<id>` | Track (Jellyfin `Audio` item) |
| `a/<id>` | Album |
| `r/<id>` | Artist (its albums) |
| `p/<id>` | Playlist |
| `f/<id>` | Library or folder |

`library.albums`, `library.artists`, `library.tracks` and
`library.playlists` list everything the signed-in user can see on the
server, not only favourites. Artists without a picture of their own get the
cover of one of their albums.

Error codes follow the protocol: a refused token marks the session expired
(`auth_required`) and sends `auth.changed`; missing items answer
`not_found`; unreachable servers answer `network`.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The CI builds static binaries (musl) for x86_64 and aarch64 on every tag
`v*` and attaches them, with their SHA-256, to a GitHub release.
`contrib/hub-entry.toml` is the entry for the
[ricercar plugin hub](https://github.com/ricercar-player/ricercar-plugins).

## Licence

MIT. Jellyfin is a trademark of its owners; this plugin is not affiliated
with the Jellyfin project.
