# Linked EffectCraft compositions

FilmCraft can use a saved EffectCraft composition directly as a source clip. Keep the effects,
layers, animation and source footage editable in EffectCraft; no intermediate video export is needed.
Install both updated native fork apps through the Craft Fork Launcher.

1. Save your composition in EffectCraft. Optionally choose **File > Copy FilmCraft Composition Link**.
2. In FilmCraft choose **File > Link EffectCraft Composition…**. Browse for the `.ecproj` file, or
   paste the copied link into **Source project**, then choose **Find compositions**.
3. Choose a composition and **Link composition**. Drag the new linked source from the Project
   panel onto your timeline. It supports ordinary trims, FilmCraft effects, audio and transparency.
4. Right-click the linked Project item and choose **Open in EffectCraft**. Make changes and save.
   FilmCraft checks for saved project changes every second, even with both windows visible.

**Clip > Linked Composition > Linked Compositions…** shows the source paths, status and repair
controls. **Refresh** also rereads source footage that was overwritten without saving the project.
**Relink…** repairs a moved project or selects a different composition without changing the
FilmCraft item's identity. Existing timeline positions and trims are preserved when source duration,
size or frame rate changes; review those edits if you shorten a composition.

The native EffectCraft application is detected beside FilmCraft in the fork launcher's installation.
For separate portable installs, choose it in the link window's **EffectCraft application** section
or **Settings > Media > EffectCraft application**. This trusted local preference is saved between
sessions; a project file cannot choose an executable. Both the `.ecproj` and its footage must be
accessible to EffectCraft on this machine, including through a mounted network share.

## AI and automation

Commands use the same engine in the editor, control channel, CLI and MCP:

| Command | Parameters | Result |
| --- | --- | --- |
| `composition.list` | `{path}` | Background job; completed result lists composition IDs, names, duration (ticks), rate, size, audio and missing footage |
| `file.linkComposition` | `{path, composition}` | Job; completed `result.items` contains the imported source item ID |
| `composition.inspect` | `{item?}` | Links, source paths, stable composition IDs and unavailable reasons |
| `composition.open` | `{item?}` | Open the saved source and selected composition in EffectCraft |
| `composition.refresh` | `{item?}` | Job; one link or all links; reopens project and footage |
| `composition.relink` | `{item, path, composition?}` | Job; preserves the previous composition ID when omitted |
| `composition.job` | `{job}` | `running`, `done` with `result`, or `error` with an explanation |

List/link/refresh/relink return promptly with `{job, status:"running"}`. Check `composition.job`
after yielding; do not busy-poll or repeatedly discover the command registry. Headless callers
can supply `wait:true` instead. Atomic command batches require `wait:true`. Place the imported
source through the ordinary timeline/source commands; ask the assistant to inspect live IDs first.
EffectCraft's `composition.linkInfo {comp?}` returns a saved project's path and stable composition
ID. The assistants in both apps include this workflow in their built-in guidance.

## Rendering and safety

An isolated, read-only EffectCraft process serves on-demand frames and stereo composition audio
over private pipes, with no open network port. It uses EffectCraft's CPU composition renderer,
including built-in effects, expressions and nested compositions. Frames are straight-alpha sRGB
RGBA8; this first bridge is SDR, not a scene-linear/HDR interchange. Compositions support up to
16 megapixels, 1–240 fps, and stereo audio at 8–192 kHz. Frame caches and process counts are bounded.
Native rendering still takes time for expensive effects; preview at reduced resolution as needed.

Each renderer retains its loaded project structure. Saving creates a new preview renderer; an
export owns an independent renderer so a preview refresh cannot change its composition mid-job.
Source footage remains external: avoid changing those files during an export. A failed private
renderer stops linked rendering rather than silently substituting an older preview. Missing
projects, deleted compositions and missing required footage report actionable errors. Unrelated
missing footage in another composition does not prevent linking a self-contained composition.
The browser edition cannot start local renderers and reports linked sources as unavailable.
