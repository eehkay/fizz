# Notes for AI assistants

This file is for coding assistants (Claude Code, Codex and the like) asked
to install, set up or change Fizz. People should read `README.md`.

Fizz is a terminal client for iMessage. It talks to a BlueBubbles Server
running on a Mac that the user controls; it has no service of its own.

## Installing it for someone

1. Decide how to install it. To just use Fizz, run the install script,
   which downloads a prebuilt binary to `~/.local/bin` and needs no Rust:

   ```sh
   sh install.sh
   ```

   To build it instead (wanted if they mean to change the code, or if
   there is no download for their platform), check `cargo --version`; if
   Rust is missing, ask before installing it (`mise use -g rust` on
   Omarchy, rustup elsewhere), then:

   ```sh
   cargo install --path .
   ```

   which puts `fizz` in `~/.cargo/bin`.
2. Confirm `fizz --version` runs. If not, the folder it went to is missing
   from `PATH`.
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
   `fizz check` also reports whether this terminal can show pictures.
   Fizz works best in kitty or Ghostty; in other terminals pictures show as
   coloured blocks. Mention that, but do not install a terminal unasked.
5. Optional extras, used only if present: `ffmpeg` (video stills),
   `wl-clipboard` (pasting pictures), `notify-send` from libnotify
   (notifications). Offer them; do not install without asking.
6. Optional launcher entry: copy `contrib/fizz.desktop` to
   `~/.local/share/applications/` and `contrib/fizz.svg` to
   `~/.local/share/icons/hicolor/scalable/apps/`. On Omarchy, a key binding
   goes in `~/.config/hypr/bindings.lua`; the README has the line. Check
   the key is free first and back the file up.

## Updating it for someone

`fizz update` does everything, whichever way Fizz was installed: a script
install downloads the new release and replaces itself; a source build pulls
into its clone, rebuilds and reinstalls (so that clone must stay where it
is). `fizz update --check` only reports.

## Releasing a new version

Bump `version` in `Cargo.toml`, commit, then push a matching tag
(`git tag v0.2.0 && git push origin v0.2.0`). The Release workflow builds
each platform and publishes the downloads; script installs then see the
update banner. Do this only when the owner asks for a release.

## Rules that matter

- **Never send a message to test something.** Sending reaches real people
  from the user's own number. The same goes for starting a conversation or
  sending a file. Only do it when the user asks for that specific send.
- **Messages are private.** To look at the interface, use the demo, which
  has made-up conversations: `fizz --demo` to run it, or
  `fizz --snapshot 120x36` to print one frame as plain text (useful,
  since you cannot drive a full-screen terminal app directly). Do not
  capture or print the user's real conversations.
- **The database is the user's messages.** `~/.local/share/fizz/fizz.db`
  holds the text of their messages once they turn word search on. Do not
  read, print or copy it, and do not open it with `sqlite3` to look around. `fizz index` and `fizz index count <words>`
  report sizes and counts without showing any text.
- **So is the startup snapshot.** `~/.local/state/fizz/snapshot.json`
  holds the chat list and recent conversations from the last run. Treat it
  like the index: do not read, print or copy it.
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
  - `codes.rs`: spotting one-time codes in message text
  - `store.rs`: the SQLite database (tables, search, read markers)
  - `index.rs`: downloading message text into it and keeping it current
  - `storage.rs`: keeping the caches within their limits, and the
    storage report
  - `metrics.rs`: timings and usage for the stats screen
  - `update.rs`: finding a newer release or commit, the banner's source,
    and `fizz update` for both kinds of install
  - `model.rs`: the server's data shapes
  - `demo.rs`: the made-up data
- Startup paints from the snapshot first and refreshes behind it, so
  anything shown at launch may be a run old until the server answers.
- Server behaviours worth knowing before changing how data is fetched:
  the chat list is built from the newest messages (`/message/query`)
  because the server's own "chats by last message" ordering proved wrong;
  that query takes over a second unless it is bounded by date, so the
  first page asks only for the last three months,
  and link-preview data is only returned by `/message/query`.
- Anything new that sends, deletes or edits must be testable in the demo
  without touching a server.
