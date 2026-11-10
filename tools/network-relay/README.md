# FilmCraft RedWeb LAN relay

Run this standalone service on the computer storing the FilmCraft project and media. The editing
computer needs only FilmCraft with network-project support. RedWeb 0.16.5 (ISC) owns the service's
HTTP listener and lifecycle; the native editor stays Rust.

Requires Node.js 22 or newer:

```sh
npm ci
```

Set `FILMCRAFT_RELAY_TOKEN` to a private, random token of at least 32 printable, non-space ASCII
characters. Do not pass the token on the command line, put it in a URL or commit it to a file.
Then run:

```sh
npm start -- /path/to/cut.fcproj --root /path/to/shared-project-folder
```

On Windows, quote the project and folder paths. `--root` must contain the project and all media
you want to share. Without it, the project's parent directory is shared. The default listener is
`0.0.0.0:9877`; set `PORT` or `BIND` to override it.

On the editing computer, choose **File → Network Project…** and enter
`http://<storage-computer-LAN-address>:9877` and the token. Use HTTP on a trusted isolated LAN,
or put a TLS reverse proxy in front of the relay and connect with HTTPS.

All authenticated clients can browse/read the selected root and save the selected project.
The host rejects escaping paths, stale saves and incomplete uploads. It does not merge concurrent
edits. Keep the host process running while editing. Stop it normally with Ctrl+C.

Run `npm test` for the real HTTP integration tests. See the full
[network-project guide](../../docs/network-projects.md) for setup on PowerShell/Bash, recovery,
format limits and supported workflows.
