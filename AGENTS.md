# Notes for AI assistants

This file is for coding assistants (Claude Code, Codex and the like) asked
to install, set up or change Fizz. People should read `README.md`.

Fizz is a terminal client for iMessage. It talks to a BlueBubbles Server
running on a Mac that the user controls; it has no service of its own.

## Installing it for someone

1. Check for Rust: `cargo --version`. If it is missing, ask before
   installing it. On Omarchy use `mise use -g rust`; elsewhere, rustup.
2. Build and install from this repository:

   ```sh
   cargo install --path .
   ```

   This puts `fizz` in `~/.cargo/bin`. Confirm `fizz --version` runs; if
   not, that folder is missing from `PATH`.
3. Server details. Fizz needs the address of the user's BlueBubbles Server
   and its password. **Ask the user for both. Do not guess, scan the
   network, or search their files for them.**
   - Best: have the user run `fizz setup` themselves. It prompts for both
     values, so the password never passes through you.
   - If they ask you to write the file instead, it is
     `~/.config/fizz/env`, in the format shown in `env.example`, with mode
     `600`. Do not echo the password back or leave it in shell history or
     logs. Never write it anywhere inside this repository.
4. Verify with `fizz check`. Success prints `Messages ok` and `Live ok`.
   "can't reach the server" means the address is wrong or unreachable from
   this machine, not that Fizz is broken.
5. Optional extras, used only if present: `ffmpeg` (video stills),
   `wl-clipboard` (pasting pictures), `notify-send` from libnotify
   (notifications). Offer them; do not install without asking.
6. Optional launcher entry: copy `contrib/fizz.desktop` to
   `~/.local/share/applications/` and `contrib/fizz.svg` to
   `~/.local/share/icons/hicolor/scalable/apps/`. On Omarchy, a key binding
   goes in `~/.config/hypr/bindings.lua`; the README has the line. Check
   the key is free first and back the file up.

## Rules that matter

- **Never send a message to test something.** Sending reaches real people
  from the user's own number. The same goes for starting a conversation or
  sending a file. Only do it when the user asks for that specific send.
- **Messages are private.** To look at the interface, use the demo, which
  has made-up conversations: `fizz --demo` to run it, or
  `fizz --snapshot 120x36` to print one frame as plain text (useful,
  since you cannot drive a full-screen terminal app directly). Do not
  capture or print the user's real conversations.
- **Do not turn off System Integrity Protection** or change BlueBubbles
  Server settings on the Mac unless the user asks; the README explains
  what that costs.
- Preferences are in `~/.config/fizz/config.toml`, created with defaults on
  first run. Unknown keys are rejected, so check names against the
  template in `src/config.rs` before editing it.

## Working on the code

- `cargo test` runs the unit tests; `cargo build` should be warning-free.
- Layout of `src/`:
  - `main.rs`: commands and the event loop
  - `app.rs`: all state and everything that changes it (keys, mouse,
    events from background work)
  - `ui.rs`: drawing only
  - `api.rs`: the BlueBubbles REST calls
  - `live.rs`: live updates (socket.io) plus a catch-up poll
  - `images.rs`, `files.rs`: picture thumbnails and video stills;
    opening, saving, attaching, clipboard, notifications
  - `config.rs`: server details and the settings file
  - `theme.rs`: colours, including following the Omarchy theme
  - `model.rs`: the server's data shapes
  - `demo.rs`: the made-up data
- Server behaviours worth knowing before changing how data is fetched:
  the chat list is built from the newest messages (`/message/query`)
  because the server's own "chats by last message" ordering proved wrong,
  and link-preview data is only returned by `/message/query`.
- Anything new that sends, deletes or edits must be testable in the demo
  without touching a server.
