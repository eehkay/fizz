# Fizz

iMessage in your terminal, through a [BlueBubbles](https://bluebubbles.app) server.

- Conversations, contact names, tapbacks, group chats, replies, link titles
- Photos shown inline as real pixels, and a still frame for videos
- Start new conversations, send text, files and pictures, insert emoji
- Open or save any attachment, copy message text, open links
- New messages arrive live, with desktop notifications
- Follows the current [Omarchy](https://omarchy.org) theme and re-colours
  itself when you switch; uses your terminal's colours everywhere else

## What you need

- A Mac running **BlueBubbles Server**, reachable from the machine you run
  fizz on (same network, Tailscale, or the server's proxy URL), and its
  server password.
- A terminal. Pictures need one of: kitty or Ghostty (best), foot or WezTerm
  (sixel), iTerm2. Anything else falls back to coloured blocks.
- Rust, to build it: <https://rustup.rs>
- Optional: `ffmpeg` (video stills), `wl-clipboard` (pasting pictures),
  `notify-send` (notifications).

## Install

```sh
git clone https://github.com/eehkay/fizz.git
cd fizz
cargo install --path .
```

The repository is private, so you need to have been invited to it and be
signed in to GitHub on that machine (`gh auth login` is the easy way).
`cargo install` puts `fizz` in `~/.cargo/bin`; make sure that is on your
`PATH`.

## Use

```sh
fizz           # first run asks for the server address and password
fizz check     # test the connection without opening the app
fizz setup     # change the server details
fizz --demo    # look around with made-up conversations, no server needed
```

Settings are stored in `~/.config/fizz/env`:

```sh
BB_URL=http://my-mac:1234
BB_PASSWORD=...
```

## Keys

**Press `?` in the app for the full list** (`F1` while typing a message).
The essentials:

| Where        | Key                         | Does                         |
| ------------ | --------------------------- | ---------------------------- |
| Chat list    | `↑` `↓` / `j` `k`           | Move                         |
|              | `Enter`                     | Open and start typing        |
|              | `n`                         | New conversation             |
|              | `/`                         | Search loaded conversations  |
|              | `q`                         | Quit                         |
| Writing      | `Enter`                     | Send                         |
|              | `Shift+Enter` or `Ctrl+J`   | New line                     |
|              | `:name`                     | Emoji: `:thu`, then `↑` `↓` and `Tab`; or finish it as `:fire:` |
|              | drop a file on the window   | Attach it                    |
|              | `Ctrl+V`                    | Attach a picture from the clipboard |
|              | `Tab`                       | Select messages, pictures and files |
|              | `Esc`                       | Back to the chat list        |
| Selecting    | `↑` `↓`                     | Move between messages, pictures, files |
|              | `Enter`                     | Open the picture, video, file, or the link in a message |
|              | `s`                         | Save to `~/Downloads`        |
|              | `y`                         | Copy the message's text      |
| Anywhere     | `?` / `F1`                  | Help                         |
|              | `Ctrl+C`                    | Quit                         |

The mouse works too: click a conversation to open it, wheel to scroll, click
a picture or file to open it, click a message to select it.

## New conversations

Press `n` in the chat list and type a name, a phone number or an email
address. Contacts come from the Mac's address book. If you already have a
conversation with that person it opens; otherwise a new one is created when
you send its first message. Ten-digit numbers are taken as US/Canada (`+1`);
type a leading `+` for anywhere else. Only one-to-one conversations can be
started.

## Pictures, video and files

Pictures show inline. Videos up to 40 MB show a still frame (needs
`ffmpeg`); larger ones, voice messages and other files show as a named row.
Opening one hands it to whatever app your desktop uses for that type (via
`xdg-open`, or `open` on macOS). HEIC pictures arrive already converted to
JPEG, so any viewer can show them.

## Sending files

Drag files from your file manager onto the terminal window while a
conversation is open. They appear above the message box; Enter sends them,
followed by whatever you typed. This works in any terminal that pastes the
path of a dropped file, which most do. `Ctrl+V` attaches a picture from the
clipboard (Wayland, needs `wl-clipboard`).

## Notifications

A desktop notification appears for each new message, unless the window is
focused and you are looking at that conversation. Set `FIZZ_NOTIFY=0` to
turn them off.

## Colours

On Omarchy, fizz reads `~/.local/state/omarchy/current/theme/colors.toml`
and notices theme changes within a second. Elsewhere it uses the terminal's
own palette.

- `FIZZ_THEME=ansi` forces terminal colours.
- `FIZZ_THEME=/path/to/colors.toml` uses a specific Omarchy-style palette
  (`background`, `foreground` and `accent` are enough).

## App launcher entry

`contrib/` has a desktop entry and an icon:

```sh
cp contrib/fizz.desktop ~/.local/share/applications/
cp contrib/fizz.svg ~/.local/share/icons/hicolor/scalable/apps/
```

On Omarchy, bind a key to open it (or focus it if it is already open) in
`~/.config/hypr/bindings.lua`:

```lua
o.bind("SUPER + SHIFT + U", "Messages (Fizz)", { tui = "fizz", focus = true })
```

## Good to know

- Sending uses the server's AppleScript method, so it works without the
  BlueBubbles Private API. Sending tapbacks, edits and typing indicators,
  and deleting chats, need that API and are not implemented.
- Opening a conversation does not mark it read on your other devices.
- Shrunken copies of pictures are cached in `~/.cache/fizz/thumbs`; delete
  the folder any time. Failures are noted in `~/.cache/fizz/fizz.log`.
- Opened files (and videos fetched for their still) are kept in
  `~/.cache/fizz/files` for two weeks, then removed.
- Inside tmux, pictures show as coloured blocks rather than real pixels.
- A file the Mac itself never downloaded from iCloud can't be fetched;
  fizz says so when you try.
