# Network projects over a local RedWeb relay

Keep a `.fcproj` and its source media on one computer and edit from FilmCraft on another Windows,
macOS or Linux computer. No matching drive letters or mounted network shares are needed. The
editor decodes and renders locally; a separate RedWeb service on the storage computer supplies
file metadata and byte ranges, accepts uploaded media and saves project changes.

This uses the [RedWeb framework](https://redweb.magnisolution.com/), pinned to 0.16.5. That site is
documentation, not a hosted relay: start the relay below on the computer holding the files.

## Fork releases and updates

Get the desktop app and relay bundle from [lakam99/filmcraft releases](https://github.com/lakam99/filmcraft/releases).
The first release contains a Windows x64 portable app and a relay source ZIP for any host with
Node.js 22 or newer. Extract the desktop ZIP and run `filmcraft.exe`; the included `portable.txt`
keeps preferences and recovery data in its adjacent `data` folder. Extract the relay ZIP on the
storage computer and run `npm ci` in its `tools/network-relay` folder before following the host setup below.

Startup and Help → Check for Updates query this fork's GitHub releases. Update downloads must
belong to `lakam99/filmcraft`; an upstream release cannot replace a fork build. The initial
Windows binaries are unsigned, so updates must be downloaded and installed manually. Automatic
Windows replacement requires a trusted code-signing configuration. Preserve the portable
`data` folder when replacing the application. Each release provides `SHA256SUMS.txt` for its assets.

## Start the storage computer

Install Node.js 22 or newer. In this checkout:

```sh
cd tools/network-relay
npm ci
```

Set `FILMCRAFT_RELAY_TOKEN` to a random access token of at least 32 printable, non-space ASCII
characters. Keep it private and share it only with the editing computer. Do not put it in a
project, command-line argument, committed file or URL. For example, enter it without echoing:

PowerShell:

```powershell
$env:FILMCRAFT_RELAY_TOKEN = [System.Net.NetworkCredential]::new('', (Read-Host 'Access token' -AsSecureString)).Password
npm start -- "D:\Films\Short\cut.fcproj" --root "D:\Films\Short"
```

Bash:

```bash
read -rs -p 'Access token: ' FILMCRAFT_RELAY_TOKEN
export FILMCRAFT_RELAY_TOKEN
npm start -- /srv/films/short/cut.fcproj --root /srv/films/short
```

Choose a shared root containing the project, originals and proxies. A project in a subdirectory
can reference media elsewhere inside that root. Referenced files outside it are refused: choose
a suitable common parent or use FilmCraft's Project Manager to collect the project first.
The root is an explicit sharing boundary; authenticated clients can browse and read its files.

The default listener is `0.0.0.0:9877`. `PORT` and `BIND` override it. Allow that port on the host's
local-network firewall if necessary. The service serves one selected project per process.

## Connect the editing computer

1. Run a desktop FilmCraft build containing this change. Node.js is needed only on the host.
2. Save or close any open, unsaved project.
3. Choose **File → Network Project…**.
4. Enter `http://<storage-computer-LAN-address>:9877`, enter the access token, and click **Connect**.
5. Edit normally. **Save** / **Save to Host** saves to the storage computer. The dialog reports
   success only after the host acknowledges the write.
6. Use **Media Browser → Network → Network Project** to import more files already on the host.
   Ordinary local media imports are uploaded when the project is saved.

HTTP is intended for an isolated, trusted LAN. The native client also supports HTTPS through a
TLS reverse proxy in front of the relay; it does not disable certificate verification or follow
redirects carrying the token. Do not expose an unencrypted relay to an untrusted network.

## Files, saves and interruptions

- Media uses portable paths relative to the project when saved on the host. Private client
  mount paths and access tokens are never stored in the host project.
- The relay preserves numeric JSON literals exactly, including large integer timeline ticks.
- Large streamable media is read in 1 MiB chunks, with at most eight cached chunks per reader.
  Requests pin a media version, so changing the source during editing produces an error rather
  than mixing byte ranges from two versions. Network throughput and local codec support still
  determine playback performance.
- The host serializes saves and checks the exact prior project version. A stale editor receives
  a conflict; its edits remain open and unsaved. There is no automatic merge or simultaneous
  collaborative editing. Avoid editing the same project directly on the storage machine while
  remote editing; that application's writes do not participate in the relay's save queue.
- Host saves use a temporary sibling file and rename. Media uploads finish before the project
  references them. Failed uploads/saves do not clear the editor's dirty flag. A completed upload
  can remain unused if a later save conflicts; it is not deleted automatically.
- New local files are uploaded to **FilmCraft Incoming**. Existing originals remain on the host.
  New local image sequences must be copied to the host as a group and imported through its
  Network Project browser; saving only the first frame is refused.
- Auto-save and crash recovery stay on the editing computer. Even when “Auto Save also saves the
  current project” is enabled, a local recovery write cannot mark a remote project saved. Use
  explicit Save to send changes to the host. Scratch-disk paths remain specific to each machine.
- On connection loss, uncached reads and host saves report errors. Keep the session open and
  retry after connectivity returns. A relay restart with an unchanged project can be retried with
  the same token. If the project changed, preserve a local Save a Copy before reopening and
  reconcile the edits; this feature does not replay conflicting changes automatically.
- Tokens are held in memory only. After restarting FilmCraft, reconnect to the host before using
  a local recovery copy's remote media references. Local copies contain edit decisions, not a
  complete offline copy of the original media. Reopening a recovery copy does not itself reconnect.
- Exports run on the editing computer and write to the selected local destination. Remote whole-file
  reads and individual range requests have a 512 MiB limit; use proxies or streamable media for
  larger sources. Project documents are limited to 32 MiB and individual uploads to 32 GiB.
- File-manager actions on virtual remote paths, Project Manager consolidation and generating
  proxies directly from virtual paths are not relay-aware. Use the host's ordinary FilmCraft
  session for those operations. The browser version does not connect to these native relays.

## Protocol and validation

Authenticated endpoints are `GET /v1/project`, `PUT /v1/project` (with `If-Match`),
`HEAD/GET /v1/file?path=...` (GET requires a bounded Range), `GET /v1/list?path=...` and
`PUT /v1/upload?name=...`. All file keys are slash-separated and relative to the shared root.
Traversal, foreign roots, URLs, alternate streams and symlink escapes are refused. Credentials
are supplied in the Authorization header. No browser CORS grant is provided.

Desktop automation can open the dialog with `network.project`, fill `network.address` and
`network.token` through widget input, then dispatch `network.connect`. `network.status`,
`network.save` and `network.disconnect` provide the remaining actions without exposing the token
in serialized UI state.

Run the relay tests and native integration tests as described in
[crates/network/README.md](../crates/network/README.md). Test results must distinguish loopback
integration on one OS from actual two-computer Windows/macOS/Linux acceptance testing.
