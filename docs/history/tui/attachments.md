# TUI Attachments — files and images in a message

Status: implemented (2026-10-04)

Attach files and images to the **next** message. Images travel to the model as
real multimodal content (`image_url` parts) when the model can take them; text
files are inlined. There is no extra request per image.

## User interface

```
/attach assets/logo.png        → attached: assets/logo.png
/attach notes.md               → attached: notes.md
/detach                        → attachments cleared
```

While anything is attached, the strip above the input shows it:

```
attached ×2  logo.png, notes.md  · /detach
```

The strip is reserved only when something is attached, so the layout of a plain
turn does not move. Attachments are consumed by the next Enter — the following
prompt starts clean.

If an **image** is attached while the configured model cannot take images, the
status line says so right away (`model takes no images — RIJO_VISION=on forces
it`), and the turn names the file in the prompt instead of sending it.

## Pasting from the clipboard

`Ctrl+V` and `/paste` read the **system clipboard**:

```
Ctrl+V   → attached from clipboard: C:\Users\…\rijo-paste-173…-1234.png
/paste   → the same thing, for terminals that never send Ctrl+V
```

A copied **image** becomes an attachment. A copied **image file** — a path, which
is what Explorer's *Copy* or a drag & drop onto the terminal produces — is
attached too, instead of being typed into the prompt. Any other text goes to the
input, or into the palette/picker while one is open — the same routing the
bracketed-paste event uses. An empty clipboard only raises a toast, so a silent
key looks like a deliberate "nothing there" and not like a broken binding.

Why rijo reads the clipboard itself: a terminal can only deliver **text**. A
screenshot on the clipboard has no text form, so the only way in is to ask the OS
for it. Every read spawns a short-lived OS tool — `[Windows.Forms.Clipboard]`
via PowerShell on Windows, `wl-paste`/`xclip` on Linux, `pngpaste` on macOS. That
read runs on a **worker thread** and comes back as an event, so a paste never
freezes the UI (PowerShell costs ~300 ms warm, more under load).

A pasted image is scaled to a long edge of at most **1568 px** and written as PNG
into the temp directory. A 4K screenshot is otherwise ~8 MB, base64 adds a third
on top, and providers reject that. The temp files are **not** cleaned up
automatically.

### Ctrl+V does not work in Windows Terminal — and cannot

Windows Terminal binds `Ctrl+V` to its own `paste` action. With an image on the
clipboard there is no text to paste, so it consumes the keystroke and **nothing
reaches rijo at all**. Upstream, [microsoft/terminal#19517 — "Accept image pastes
from the clipboard for multi-modal LLMs to
consume"](https://github.com/microsoft/terminal/issues/19517) is still **open**
(Backlog, Priority-3, filed 2025-11).

What works there instead:

| Way | How |
|---|---|
| `/paste` | one word, always arrives — the reliable path |
| drag the file onto the terminal | the path lands in the input; type `/attach ` first, or paste a bare image path (it is attached automatically) |
| unbind `paste` from Ctrl+V | in the terminal's `settings.json`, then Ctrl+V reaches rijo. Honest trade-off: it then stops pasting in *every other* app in that terminal |

rijo shows a one-line hint at startup when it detects `WT_SESSION`, phrased as a
requirement ("Ctrl+V needs Windows Terminal's paste binding unbound") rather than
as a claim — the binding may already be unbound. In a plain console (conhost) or
mintty/Git Bash, which pass the key through, Ctrl+V works directly.

## How it works

1. `App.attachments: Vec<String>` holds the pending paths (`tui.rs`).
2. On Enter, the paths are read **in the turn task** (base64 of an image must not
   block the key thread) via `llm::load_attachment`.
3. `Executor::build_user_message` assembles the message:
   - one `{"type":"text"}` part = the prompt plus every inlined text file,
   - one `{"type":"image_url"}` part per image.
4. `ChatMessage.parts` carries those parts. Its hand-written `Serialize` puts
   `content` on the wire **either** as a plain string **or** as the parts array —
   a `#[derive]` cannot switch a field's type at runtime.

The text parts are inlined in the same order the files were attached; images
follow the text part, also in order.

### Wire shapes

| Turn | `content` on the wire |
|---|---|
| no attachments | `"the prompt"` (unchanged from before) |
| text file only | `"the prompt\n\n[attached file: notes.md]\n…"` |
| image, vision model | `[{"type":"text",…},{"type":"image_url",{"url":"data:image/png;base64,…"}}]` |
| image, text-only model | `"the prompt\n\n[attached image not sent: shot.png — …]"` |

A turn without attachments produces a byte-identical body to the text-only path —
the common case is not allowed to change (the prompt cache depends on it).

## Caps and limits

| Thing | Value | Why |
|---|---|---|
| inlined text per file | 20 000 chars (`MAX_ATTACH_CHARS`) | beyond that the file crowds out the task; the cut is marked `(truncated)` in the prompt |
| image size | none locally; base64 ≈ 1.33× the file | providers cap the payload, an oversized image errors at the provider |
| broken file | reported, turn still runs | one unreadable path must not lose the message |

## Model capability

There is no capability endpoint, so `llm::model_supports_vision` is a **heuristic**
over the model name (`vl`, `vision`, `gpt-4o`, `claude`, `gemini`, …). It errs to
"no" for known text-only families (`deepseek-chat`, `deepseek-reasoner`), so an
attached image becomes a clear note instead of a provider-side 400.

`RIJO_VISION=on|off` overrides the guess for a model the list does not know. This
is the honest escape hatch — the heuristic does not claim to be a contract.

## Honest limitations

- **Not persisted on session resume.** The session store keeps `role` and text;
  the image bytes are not written to disk. A resumed session has the text of the
  turn, not the picture.
- **Images are re-sent each turn.** The image stays in `history` (that is what
  makes a follow-up question about the same picture work), and every following
  turn in that session sends it again — that costs tokens.
- **Text goes in whole (capped).** There is no chunking or summarisation of a
  large file; it is cut at the cap and marked.
- **No binary files.** Only images and UTF-8 text: a PDF or archive is read as
  text and will produce garbage — it is not a supported attachment type.
- **One image path only.** `vision_analyze` (the tool) and the attachment path
  share `LlmClient::describe_image` / `llm::image_ref`; the tool still exists for
  asking a *specific* question about an image.

## Tests

- `llm::attach_tests` — path detection, data URL, text cap, capability heuristic,
  plain-string vs parts-array serialization.
- `executor::context_tests` — `build_user_message` for: no attachments, inlined
  text, image + vision model, image + text-only model, several images.
- `tui::display_tests::the_attachment_strip_lists_the_files` — the strip above the
  input.
