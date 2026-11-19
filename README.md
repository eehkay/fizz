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

## Settings

```sh
fizz config     # opens the settings file in your $EDITOR
```

`~/.config/fizz/config.toml` is created on first run with every option and
its default written out, each with a comment. It covers:

- **Notifications:** on or off, and whether they show the message text
- **Appearance:** theme (`auto`, `terminal`, or a `colors.toml` path), 12 or
  24 hour clock, chat list width, conversation width, mouse on or off
- **Media:** inline pictures on or off, how large they are drawn, video
  stills and their size limit, where files are saved
- **Messages:** default country code for new chats, send method, the emoji
  popup, how often to poll

Restart Fizz after changing it. A typo does not stop Fizz starting: it says
so in the status line (and in `fizz check`) and uses the defaults.

The server address and password are kept apart from those, in
`~/.config/fizz/env`, so the settings file is safe to share:

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
you send its first message. On some macOS versions the server can only
create a chat with the Private API helper connected (see below). Numbers typed without a country code get the default one from the settings
file (`+1` unless changed); a leading `+` is always respected. Only one-to-one conversations can be
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
focused and you are looking at that conversation. Turn them off, or hide
the message text, in the settings file.

## Colours

On Omarchy, fizz reads `~/.local/state/omarchy/current/theme/colors.toml`
and notices theme changes within a second. Elsewhere it uses the terminal's
own palette.

Set `theme` in the settings file to `"terminal"` to always use terminal
colours, or to the path of an Omarchy-style `colors.toml` (`background`,
`foreground` and `accent` are enough). The `FIZZ_THEME` environment variable
does the same for a single run.

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

## Private API helper (optional)

BlueBubbles Server can load a helper into the Mac's Messages app, which it
calls the Private API. Fizz works without it. With it, the server can do
things plain AppleScript cannot: tapbacks, replies, edits and unsends,
typing indicators, marking chats read, and creating and deleting chats.

What Fizz uses it for today:

- `fizz check` reports whether the helper is connected.
- Setting `send_method = "private-api"` in the settings file sends through
  it.
- Starting a new conversation may need it, depending on the macOS version,
  since creating chats is one of the things the helper is for. If a new
  conversation fails to start, this is the likely reason.

Sending tapbacks, edits, typing indicators and deleting chats from Fizz are
not built yet; they need the helper first.

### Setting it up

This is done on the Mac that runs BlueBubbles Server, and it means turning
off System Integrity Protection (SIP), one of macOS's security layers. The
BlueBubbles documentation compares this to jailbreaking a phone and says to
do it at your own risk. On Apple Silicon Macs it also stops iPhone and iPad
apps from running on that Mac. It suits a Mac that mostly sits there being
a message server; think twice on one you use day to day.

The steps below are a summary of the
[official guide](https://docs.bluebubbles.app/private-api/installation) for
macOS 11 and newer. Follow that page for older macOS, virtual machines and
patched (OpenCore) Macs, which differ.

1. In Terminal on the Mac, allow the helper to be loaded:

   ```sh
   sudo defaults write /Library/Preferences/com.apple.security.libraryvalidation.plist DisableLibraryValidation -bool true
   ```

2. Start the Mac in Recovery mode.

   - **Apple Silicon:** shut down, then hold the power button until
     "Loading startup options" appears. Choose Options, then Continue. On
     recent macOS, hold Left Shift while clicking Continue and keep holding
     it while you enter your password; otherwise SIP may not turn off.
   - **Intel:** save your work, then run these (the Mac restarts at once):

     ```sh
     sudo nvram internet-recovery-mode=RecoveryModeDisk
     sudo reboot recovery
     ```

3. In Recovery, open Utilities > Terminal and run:

   ```sh
   csrutil disable
   ```

   Then choose Restart from the Apple menu.

4. In BlueBubbles Server's settings, turn on **Private API**, and press the
   refresh button in the **Private API Status** box to confirm it loaded.
   If it does not, quit and reopen the server with the switch on.

5. From the machine running Fizz, confirm:

   ```sh
   fizz check
   ```

   It should print `Private API on`. "Enabled, helper not connected" means
   the switch is on but the helper did not load, which usually means SIP
   is still on (`csrutil status` on the Mac tells you).

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
