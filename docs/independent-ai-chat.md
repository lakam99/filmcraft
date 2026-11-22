# Independently updated AI Chat

FilmCraft, EffectCraft, SoundCraft and PhotoCraft use the shared native
[Craft AI Chat component](https://github.com/lakam99/craft-ai-chat/blob/v0.6.2/docs/runtime.md).
This editor release installs the bridge. Later chat updates can change the
panel layout, AI behavior, history handling and context compression without
reinstalling the editor.

Use **Update chat component** in the chat panel or ArkCraft Launcher Settings.
The component is shared across these editors; each editor retains its own
preferences, saved key and per-document conversation. Restart an editor when
convenient to activate an installed update. Updates never replace an active chat.

The panel stays docked and native. The editor still validates every tool call,
checks the current document and enforces its automation permissions. Highlights
and walkthroughs use current editor widgets. Adding editor capabilities or new
bridge primitives still requires an editor update.

The component communicates through private process pipes, with no network
listener. GitHub downloads are size-bounded, checksum-verified and checked for
bridge compatibility before activation. Previous and bundled versions remain
available for recovery. An offline installation without the component retains
the source-integrated chat. The OpenAI API itself still needs connectivity.
