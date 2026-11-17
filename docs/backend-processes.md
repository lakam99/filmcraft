# Native background rendering

The desktop window presents cached frames and keeps input, navigation and editing available
while rendering runs. It retains the editable project, undo history and recovery state. Render
workers receive immutable snapshots; they cannot replace the window's current project or discard
edits made after a job starts.

Two persistent preview processes serve monitor frames and thumbnails. A separate pool admits up
to two long jobs: AAF/OMF export, media export and video/audio preview generation. A long video
mixdown therefore cannot occupy the monitor workers. The native host uses its own executable for
both worker roles, so a separately installed CLI is not required.

This boundary covers decoding, compositing, mixing and encoding for those render paths. Lightweight
editing commands, project/view state, undo and recovery coordination remain in the frontend.
Realtime audio retains its existing dedicated audio threads. Other document and analysis commands
keep their existing execution paths; this is not an arbitrary remote-command executor.

## Using long jobs

The desktop commands `file.exportAaf`, `file.exportOmf`, `file.exportMedia`, `export.quick` and
sequence preview-render commands return a job acknowledgement immediately. In the desktop app,
`wait: true` does not block the render loop. A typical acknowledgement is:

```json
{"job":7,"path":"Example.aaf","poll":"jobs.list","cancel":"jobs.cancel"}
```

Follow it with `engine.execute {"command":"jobs.list","params":{}}` and select the matching ID.
`finished: true` and a successful `result` mean the output is complete. An acknowledgement alone
does not mean the export succeeded. AAF/OMF reports and generated media paths are available in
`backend.output`; `backend.workerPid`, `documentEpoch` and `revision` identify its origin.
Use `jobs.cancel {"job":7}` to stop the job. The status bar and Progress panel expose the same jobs.
`export.queue.list` describes queued exports, not every direct export.

The built-in assistant waits for these jobs locally inside the original tool call. It sends the
model the completed result and AAF/OMF warnings, rather than spending repeated model tool calls
polling a long render. Stopping its pending tool cancels that job. The editor remains interactive
while the assistant waits.

Headless sessions that do not opt into the native backend retain their existing blocking/wait
behavior. MCP clients connected to the desktop must follow the desktop acknowledgement contract.

## Failure and output ownership

Serialization, file access, pipe reads/writes, process startup and process waits run away from the
render loop. Anonymous inherited pipes carry bounded messages and source-byte ranges. The host
serves reads through its existing services, including LAN-project services; relay credentials and
AI preferences are not sent to workers. Each worker owns its decoders and preserves the project's
stored-path provenance. Proxy preferences and explicit source invalidations cross the boundary.

Exports render to owned staging paths. AAF/OMF sidecars use a fresh media directory, preserving
files referenced by an earlier export. The final document is replaced atomically after rendering
finishes. Cancellation or a worker failure removes only the new staging files. Multi-file media
publication keeps backups and rolls back a failed replacement; if rollback itself fails, its error
identifies the retained backup directory.

A supervisor cancels cooperatively, then terminates and reaps a stalled worker. A disconnected or
failed worker becomes a job error; the frontend project remains open and another job can be started.
Preview workers restart after errors and retry without GPU compositing after a renderer failure.
The window keeps the last cached frame while a replacement is unavailable. Preview worker PIDs
are reported in frame performance diagnostics.

The private worker arguments (`--backend-worker`, `--frame-worker`) are host entry points, not a
network service or a general plugin protocol. Web and embedded hosts retain their existing execution
model.

## Regression coverage

Native integration tests launch the actual app executable without opening a window. They exercise
blocked virtual-source reads while frontend frames continue, cancellation and retry, preservation
of an existing AAF and unsaved edits, video snapshot consistency, media/audio-preview jobs, missing
executables, and persistent preview processes receiving updated snapshots. Protocol tests reject
truncated packets and hostile lengths before allocating their payloads.
