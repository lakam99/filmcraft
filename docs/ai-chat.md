# AI chat and editor tools

## Command knowledge

The assistant reuses learned command parameters across questions and keeps a bounded
reference through context compression and remembered chat history. Workflow hints
help it choose the right tools; missing related commands can be discovered together
in one call. Current project IDs, selection and command availability are still read
from the editor. Starting a new chat clears its learned reference. This uses the
existing chat settings and requires no MCP configuration.

For scene matching and everyday moods, the assistant uses saved color groups rather than
copying individual grades between clips. Ask it to match selected shots and describe the mood;
it can adjust the whole look later while keeping each member's automatic balance. See
[Color by feeling](color-vibes.md) for the controls, matching limits and command examples.

## Video frames

Automatic Program/Source attachments and the `video_frame` tool share the same capture path.
Captures pin the requested frame without seeking either monitor and remain valid while playback
advances or its resolution changes. A project edit during capture asks for a fresh frame.

AI previews preserve aspect ratio and are capped at 1280 pixels per edge, one million pixels,
and a 4 MiB PNG. Source decoders may return larger frames than requested; capture preparation
resizes those frames to the preview budget. Desktop resizing and encoding run in bounded
background work so preparing attachments does not block the editor. Cached display pixels and
the project's media are unchanged.

## Batches and saved history

Bulk requests use batches of up to 64 discovered editor commands per AI tool call,
with one progress card. Commands execute in order through the normal editor APIs;
a failure stops later commands. Completed edits keep their ordinary Undo entries.
A batch is not an atomic edit or one Undo step. Inspect current state before
continuing unfinished work; completed commands must not be replayed.

Tool-limit and network errors preserve the original request and completed-work
context. Chat history is saved locally per project beside editor preferences;
**Remember chat history locally** controls this in AI preferences. **New chat**
clears the current project's saved conversation. Credentials and attached media
are excluded. History discarded by an older version cannot be recovered.

Older attachments are omitted from later turns. Long model context is compacted
into a continuation summary while the readable transcript stays separate. This
uses the selected model and counts as API usage; a failed summary preserves the
existing context for retry.

Large inspections now provide compact previews instead of repeatedly triggering
context compression. The assistant can retrieve exact fields and pages from a bounded
local observation cache using `read_tool_result`, including several queries at once.
Recent observations survive compaction; changed editor state still requires a fresh
inspection. The cache follows the existing remember-history setting.

Repeated unchanged inspections, discovery calls or identical errors trigger recovery.
If the assistant remains stuck, it explains completed work and the blocker with further
tools disabled. Successful edits are not deduplicated and pending job polling is allowed.
The 32-call safety limit remains, with a final explanation instead of blind continuation.

## Highlighted controls

Ask “Guide me step by step” for one highlighted control at a time. The assistant
waits for your action, checks the observable result, then inspects the updated
editor and highlights the next control. It does not perform the steps for you.
Use **Pause guide** / **Resume guide** to control advancement, or **I've done this**
when automatic detection is unavailable. Confirmation is reported as your
confirmation, not automatic verification. Cancelling a file dialog does not count
as importing a file. **Clear highlights**, **Esc** and **Stop** end the walkthrough.

Ask “How do I…?” to get instructions with numbered outlines around the relevant editor
buttons and panels. The numbers match the guidance shown in the chat. The assistant can
open a relevant panel or menu first; highlighting leaves the actual edit for you to perform.
Outlines follow the controls when the layout changes and disappear when a control is hidden.
Use **Clear highlights** or **Esc** to dismiss the guide. A new question, **Stop**, or opening
another project clears the previous guide.

## Using the chat

Open **Window → AI Chat** to reveal the dedicated dockable panel. It supports tabs, splits, floating
windows and saved workspaces like the other editor panels. Enter an OpenAI API key and describe the
work you want done. Choose a model under **Edit → Preferences → AI Chat** (also available from the
panel’s **Preferences…** button). The default model is `gpt-6.1-sol`; another model must support the
Responses API, image inputs and function calling. API billing and model availability belong to your
OpenAI account.

**Attach current Program and Source frames** sends still images of the videos at their current
playheads with your message. The assistant can request additional frames during a task. These are
rendered video frames, including the Program sequence's effects and composition, without desktop or
chat-window contents. Capturing a frame does not seek or change your selection. Images fit within
1280 × 1280 pixels. A missing source or sequence is reported in the tool log; it does not prevent
text-only chat. This is still-image inspection, not continuous video or audio understanding.

The assistant can discover every engine command and frontend menu command, inspect project and
sequence IDs, edit through the regular command dispatcher, use registered UI controls, change
effects, work with captions and audio settings, and start normal exports. The same command
validation and undo history apply as when you operate the editor. The conversation shows tool calls
and their results. Review edits and use **Edit → Undo** as needed. The assistant has no shell or
arbitrary code execution tool. Its own chat controls and the control channel's forced-quit shortcut
are excluded.

**Stop** prevents further tool dispatch. Already completed edits remain. An in-flight OpenAI request
may take up to 120 seconds to return; the app remains usable. Closing the panel hides it; ongoing
work continues until it finishes or you use **Stop**. **New chat** discards conversation context
after the current task has stopped. **Save key securely** saves the entered key in the OS credential
store. **Load saved key** retries loading it if the store was previously locked. **Forget key**
clears the entered key, its text-field undo history and the saved credential. Errors removing a
credential are reported so you can retry. Changing the model or opening another project starts fresh
context, and a delayed task cannot operate on a replacement project.

The model, frame-sharing option and secure-key-saving switch are stored in the editor’s normal
`preferences.json`, independently of projects. With **Remember the API key in the OS credential
store** enabled (the default), sending also saves your entered key, and the panel loads it on its
next opening after a restart. Each preferences profile has its own credential. Windows uses
Credential Manager, macOS uses Keychain, and Linux uses a Secret Service desktop keyring. An absent
or locked store reports an error; FilmCraft never falls back to a plain-text key file. Disable the
option for a session-only key; a previously saved credential is removed. Credential Save and Forget
actions are immediate, while ordinary Preferences fields use OK / Cancel.

The key never enters preferences JSON, projects, recovery files, logs, or serialized UI state. Chat
history is saved separately in local per-project chat archives when enabled; frame attachments are not persisted. Messages, frames and requested
editor data are transmitted directly to OpenAI over HTTPS; the RedWeb project relay is not involved
in AI requests. Requests use `store: false`; this does not change the account's applicable OpenAI
data-handling policies. Do not send material you do not want processed by OpenAI.

Each turn is limited to 32 model-requested tool calls, 8192 output tokens per response and bounded
request, response and conversation sizes. When a limit or error interrupts a turn, completed edits
remain, conversation context is preserved, and the next message should inspect the current state
before continuing. Executed tools report failures in the conversation. Authentication, quota,
connection and unsupported-model errors are reported without logging server bodies or keys.

Automation uses `ai.chat`, `ai.send`, `ai.stop`, `ai.clear` and `ai.status` through the existing
command dispatcher. The panel is `AiChat` in the docking API. Widgets register `ai.key`,
`ai.settings`, `ai.prompt`, `ai.attach`, `ai.send`, `ai.stop`, `ai.clear`, `ai.saveKey`,
`ai.loadKey` and `ai.forgetKey`. The Preferences page registers `settings.ai.model`,
`settings.ai.attachFrames` and `settings.ai.rememberKey`; its credential controls have the
`settings.ai.` prefix. The API key is entered in the password field, never in command parameters.
`ai.saveKey`, `ai.loadKey` and `ai.forgetKey` manage the entered credential. `ai.status` exposes
progress, credential-operation status and the visible transcript, but never key values.

The integration follows OpenAI's [Responses function-calling
guide](https://developers.openai.com/api/docs/guides/function-calling) and [image-input
guide](https://developers.openai.com/api/docs/guides/images-vision?api-mode=responses). It is
available in native FilmCraft builds; the browser build remains available without this native HTTP
worker.


## Shared Rust plugin

The dock body, Responses client, conversation lifecycle and secure credential store
come from [Craft AI Chat](https://github.com/lakam99/craft-ai-chat), pinned to a
reviewed Git revision in the UI crate. This repository owns only its product
adapter: active-view rendering, command/UI dispatch, preference persistence and
normal dock registration. Existing preference keys and OS credential identities
are preserved.

Other Craft products can integrate the same source plugin through the `Host`
trait, advertising their own tools and document context. See the shared
repository's adapter guide and compiled non-video document example. This requires
rebuilding the host application; it is not a runtime plugin for existing binaries.

## Conversation and Markdown

Use the key-status button at the top of AI Chat to open connection controls.
The composer stays at the bottom: **Enter** sends and **Shift+Enter** adds a line.
During a request, **Stop** replaces Send. **New chat** clears the session.

Messages support Markdown headings, bold and italic text, lists, quotes, links,
fenced code blocks and tables. Use **Copy** on an assistant reply to copy its
original Markdown. Images and HTML are omitted, and local or non-web links are
blocked. Deeply nested Markdown falls back to plain text.

Editor tool calls appear as named action cards with Working, Done, Failed or
Stopped states. Command names come from the editor's command registry. Raw JSON
arguments, payloads and frame encodings stay out of the chat; the model still
receives the full tool results. Errors appear as concise messages on the card.
