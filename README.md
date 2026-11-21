# Fizz

iMessage in your terminal, through a [BlueBubbles](https://bluebubbles.app) server.

**Works best in [kitty](https://sw.kovidgoyal.net/kitty/) or
[Ghostty](https://ghostty.org),** where pictures show inline as real pixels.
It runs in any terminal; see [which ones show pictures](#which-terminals-show-pictures).

![Fizz showing a conversation with an inline photo, a link with its title, and a reply](docs/main.png)

| Search conversations and message text | Every key, on `?` |
| --- | --- |
| ![The search layer listing a matching conversation and messages with the match highlighted](docs/search.png) | ![The help screen listing keys by area](docs/help.png) |

These are the built-in demo (`fizz --demo`), which uses made-up
conversations.

- Conversations, contact names, tapbacks, group chats, replies, link titles
- Photos shown inline as real pixels, GIFs that play, and a still frame
  for videos
- Start new conversations, send text, files and pictures, insert emoji
- Open or save any attachment, copy message text, open links
- Search every conversation and contact by name, and messages by their words
- New messages arrive live, with desktop notifications
- One-time codes are picked out of messages, ready to copy
- Follows the current [Omarchy](https://omarchy.org) theme and re-colours
  itself when you switch; uses your terminal's colours everywhere else

## Getting started

Start to finish, this takes a few minutes.

### 1. Have a BlueBubbles server

Fizz is only the screen; the messages come from a Mac running
[BlueBubbles Server](https://bluebubbles.app), signed in to your
iMessage account. If you already use BlueBubbles on a phone or desktop, you
have this. You need two things from it:

- **Its address,** as reachable from the computer you will run Fizz on. Any
  of these works:
  - a name or IP address on the same network: `http://my-mac:1234` or
    `http://192.168.1.50:1234`
  - its Tailscale name, which also works away from home, written the same
    way: `http://my-mac:1234`
  - the proxy URL shown in the server app: `https://….trycloudflare.com`
    (this one changes whenever the server restarts)
- **Its password,** from BlueBubbles Server > Settings.

### 2. Install Fizz

```sh
curl -fsSL https://raw.githubusercontent.com/eehkay/fizz/main/install.sh | sh
```

That downloads a ready-made `fizz` for your computer, checks it against its
published checksum, and puts it in `~/.local/bin`. It needs no
administrator rights and installs nothing else. There are downloads for
Linux (Intel/AMD and ARM) and for Apple Silicon Macs; the Mac one is built
but has had little testing.

If typing `fizz` afterwards says "command not found", add `~/.local/bin` to
your `PATH`. To read the script before running it, it is
[`install.sh`](install.sh) in this repository.

#### From source

If you would rather build it, or there is no download for your machine, you
need Rust (<https://rustup.rs>, or `mise use -g rust` on Omarchy):

```sh
git clone https://github.com/eehkay/fizz.git
cd fizz
cargo install --path .
```

This puts `fizz` in `~/.cargo/bin`. The first build takes a few minutes.

### 3. Look around without a server (optional)

```sh
fizz --demo
```

Made-up conversations, nothing leaves your machine. Press `?` for the keys
and `q` to quit.

### 4. Connect it to your server

```sh
fizz setup
```

It asks for the address and password from step 1, saves them, and tests
the connection straight away. You are looking for:

```
Messages ok
Contacts 240
Live     ok
```

If it says it can't reach the server, the address is wrong or not reachable
from this computer; run `fizz setup` again. The contacts number is however
many are in the Mac's address book. (To write the file by hand
instead, see [`env.example`](env.example).)

### 5. Open it

```sh
fizz
```

Your conversations load, newest first. `?` shows every key. The essentials:
arrows to move, Enter to open a chat and type, Enter again to send, Esc to
go back, `n` for a new conversation.

Fizz works best in kitty or Ghostty; see
[which terminals show pictures](#which-terminals-show-pictures).

### 6. Make it yours (all optional)

- `fizz config` opens the settings: notifications, colours, clock, picture
  sizes and more. See [Settings](#settings).
- Install `ffmpeg` for video stills, `wl-clipboard` to paste pictures with
  Ctrl+V, and `notify-send` (libnotify) for notifications. Fizz works
  without them and uses each one if it is there.
- Add it to your app launcher and bind a key: see
  [App launcher entry](#app-launcher-entry).
- Tapbacks, edits and the like need the server's Private API helper: see
  [Private API helper](#private-api-helper-optional).

### Or let an AI assistant do it

If you use Claude Code or a similar coding assistant, clone the repository,
open the assistant in that folder and say:

> Install Fizz for me following AGENTS.md, then help me connect it to my
> BlueBubbles server.

[`AGENTS.md`](AGENTS.md) tells it the steps and the ground rules: ask you
for the server details, never send a message as a test, and keep your
conversations private.

## Stats

Press `i` (or `F2` while typing) for a live panel showing:

- **Server:** its address and version, whether the Private API helper is
  connected, the state of the live connection, and the catch-up poll
- **Latency:** how long the last request took, the typical and slowest
  times, and a bar chart of recent requests
- **Fizz:** version, uptime, CPU and memory use, how pictures are being
  drawn
- **Data:** conversations loaded and searchable, contacts, cache size
- **Selected message:** who sent it, exact sent, delivered and read times,
  its attachments and its ID. Select a message with `Tab` first.

## Updating

Fizz checks now and then whether a newer version is out, and shows a banner
across the top of the app when there is one. To install it:

```sh
fizz update
```

Then quit Fizz and open it again. `fizz update --check` only looks.

How it updates depends on how it was installed, and it works that out
itself:

- **Installed with the script:** it downloads the new release for your
  computer, checks it against its checksum, and replaces itself.
- **Built from source:** it pulls the new code into the folder you cloned,
  shows what changed, and rebuilds. Keep that folder where it is.

Nothing about you or your messages is sent: the check only asks GitHub what
the latest version is. To turn checking off, set `check = false` under
`[updates]` in the settings.

## Commands

```sh
fizz           # open the app (asks for server details if there are none)
fizz setup     # enter or change the server address and password
fizz check     # test the connection without opening the app
fizz config    # open the settings file in your $EDITOR
fizz update    # install the newest version (--check only looks)
fizz index     # what the message-text search index holds ("fizz index delete" removes it)
fizz --demo    # made-up conversations, no server needed
fizz --help    # this list, and the environment variables
```

## Settings

```sh
fizz config     # opens the settings file in your $EDITOR
```

`~/.config/fizz/config.toml` is created on first run with every option and
its default written out, each with a comment. It covers:

- **Updates:** whether to check for new versions, and how often
- **Search:** how many years of message text to keep for word search
- **Codes:** whether to pick out one-time codes, and whether to copy them
  automatically
- **Notifications:** on or off, whether they show the message text, and an
  optional sound
- **Appearance:** theme (`auto`, `terminal`, or a `colors.toml` path), 12 or
  24 hour clock, chat list width, conversation width, mouse on or off,
  blue and green bubbles by service
- **Media:** inline pictures on or off, how large they are drawn, playing
  GIFs, video stills and their size limit, where files are saved
- **Messages:** default country code for new chats, send method, the emoji
  popup, how often to poll, whether to mark chats read on the server and
  whether to show others when you are typing

Restart Fizz after changing it. A typo does not stop Fizz starting: it says
so in the status line (and in `fizz check`) and uses the defaults.

The server address and password are kept apart from those, in
`~/.config/fizz/env`, so the settings file is safe to share:

```sh
BB_URL=http://my-mac:1234
BB_PASSWORD=...
```

`fizz setup` writes that file for you. To create it by hand, copy
[`env.example`](env.example) from this repository, which explains both
values and the address forms that work.

## Keys

**Press `?` in the app for the full list** (`F1` while typing a message).
The essentials:

| Where        | Key                         | Does                         |
| ------------ | --------------------------- | ---------------------------- |
| Chat list    | `↑` `↓` / `j` `k`           | Move                         |
|              | `Enter`                     | Open and start typing        |
|              | `n`                         | New conversation             |
|              | `G`                         | New group conversation (capital G) |
|              | `D`                         | Delete the conversation everywhere (capital D; asks first) |
|              | `/`                         | Search conversations, contacts and message text |
|              | `q`                         | Quit                         |
| Writing      | `Enter`                     | Send                         |
|              | `Shift+Enter` or `Ctrl+J`   | New line                     |
|              | `←` `→` `↑` `↓`             | Move through what you are typing (`Ctrl+←` `→` by word); with the box empty, `↑` `↓` scroll the conversation |
|              | `:name`                     | Emoji: `:thu`, then `↑` `↓` and `Tab`; or finish it as `:fire:` |
|              | drop a file on the window   | Attach it                    |
|              | `Ctrl+V`                    | Attach a picture from the clipboard |
|              | `Ctrl+R`                    | Try again with a message that failed to send |
|              | `Tab`                       | Select messages, pictures and files |
|              | `Esc`                       | Back to the chat list        |
| Selecting    | `↑` `↓`                     | Move between messages, pictures, files |
|              | `Enter`                     | Open the picture, video, file, or the link in a message |
|              | `s`                         | Save to `~/Downloads`        |
|              | `c`                         | Copy the message's text (`y` works too) |
|              | `C`                         | Copy just the one-time code in it (capital C) |
|              | `R`                         | Reply to that message (capital R; helper) |
|              | `r`                         | React with a tapback (needs the Private API helper) |
|              | `e`                         | Edit a message you sent (helper; within about 15 minutes) |
|              | `u`                         | Unsend a message you sent (helper; within about 2 minutes) |
| Anywhere     | `?` / `F1`                  | Help                         |
|              | `Ctrl+Y`                    | Copy the latest one-time code that arrived |
|              | `i` / `F2`                  | Stats (see below)            |
|              | `Ctrl+C`                    | Quit                         |

The mouse works too: click a conversation to open it, wheel to scroll, click
a picture or file to open it, click a message to select it.

## Search

Press `/` in the chat list. A search layer opens over the app with one box
and two kinds of result:

- **Conversations:** every conversation on the server, however old, by
  name, group name, member or phone number, plus contacts you have never
  messaged (choosing one starts a new conversation).
- **Messages:** messages containing the words you typed, newest first, with
  the words highlighted. Several words must all appear. Enter opens the
  conversation at that message, with the history around it; `End` returns
  to the latest messages.

Up and Down move, Enter opens, Esc closes. Pressing `/` again brings back
your last search; `Ctrl+U` clears it.

### Searching message text

The server cannot search by words itself, so this part works from a copy of
your message text kept on this computer. It is off until you press `Ctrl+B`
in the search layer.

- **What it stores:** the text of each message, who sent it and when, in
  `~/.local/share/fizz/index`, readable only by you. No pictures or files.
- **How big:** roughly 15 MB for every 100,000 messages.
- **How long:** it downloads in the background, newest first, a few hundred
  messages at a time so the app stays responsive. Half a million messages
  takes around half an hour, and you can search what has arrived so far.
  After that it only adds new messages.
- **Limiting it:** set `years` under `[search]` in the settings to keep only
  that many years.
- **Checking or removing it:** `fizz index` shows what it holds;
  `fizz index delete` removes it and turns it off.

## New conversations

Press `n` in the chat list and type a name, a phone number or an email
address. Contacts come from the Mac's address book. If you already have a
conversation with that person it opens; otherwise a new one is created when
you send its first message. On some macOS versions the server can only
create a chat with the Private API helper connected (see below). Numbers typed without a country code get the default one from the settings
file (`+1` unless changed); a leading `+` is always respected.

For a group, press `G` (capital) instead. Type a name and press Enter to add
that person, and repeat for each one. Once there are two, a "Start the
group" row appears at the top of the list whenever nothing is typed: press
Enter on it, then write the first message, which is what creates the group.
Backspace on an empty line removes the last person added.
Creating a group needs the server's Private API helper.

## Pictures, video and files

Pictures show inline, and GIFs play in place in kitty and Ghostty (the
first frame elsewhere). They play while the Fizz window is focused, up to
six at a time in a conversation; `animate_gifs = false` under `[media]`
shows stills instead. Videos up to 40 MB show a still frame (needs
`ffmpeg`); larger ones, voice messages and other files show as a named row.
Opening one hands it to whatever app your desktop uses for that type (via
`xdg-open`, or `open` on macOS). HEIC pictures arrive already converted to
JPEG, so any viewer can show them.

### Which terminals show pictures

Drawing real pixels is up to the terminal, so this is the one thing that
differs from one to the next. Fizz picks the best method available by
itself; there is nothing to configure.

| Terminal | Pictures |
| --- | --- |
| kitty, Ghostty | Real pictures, best quality. **Recommended.** |
| foot, WezTerm | Real pictures (sixel) |
| iTerm2 | Real pictures |
| Alacritty and most others | Coarse coloured blocks |
| Anything, inside tmux | Coarse coloured blocks |

`fizz check` says which of these you have. Where pictures are blocks, Enter
on one still opens the real thing in your image viewer. Fizz does not
install a terminal for you.

## Sending files

Drag files from your file manager onto the terminal window while a
conversation is open. They appear above the message box; Enter sends them,
followed by whatever you typed. This works in any terminal that pastes the
path of a dropped file, which most do. `Ctrl+V` attaches a picture from the
clipboard (Wayland, needs `wl-clipboard`).

## Blue and green bubbles

Your own messages are coloured by how they were sent, as on a phone: blue
for iMessage, green for SMS and RCS. The colours are your theme's own blue
and green, so they change with it. The message box says which kind the next
message will be ("iMessage to Maya", "Text message (SMS) to Dana").

The server records the service on everything you receive and on some of
what you send; for the rest Fizz goes by the neighbouring messages in the
conversation. Set `service_colors = false` under `[appearance]` to use the
theme's accent colour for everything instead.

## One-time codes

When a message arrives that reads like a verification code ("Your code is
482913"), Fizz picks the code out:

- the desktop notification leads with the code, so you can read it off
  without opening anything
- `Ctrl+Y`, from anywhere in Fizz, copies the most recent one
- in the conversation it is shown beneath the message; select the message
  and press `C` (capital) to copy just the code

To have codes copied to the clipboard the moment they arrive, set
`auto_copy = true` under `[codes]` in the settings. It is off by default
because it replaces whatever you had copied. `detect = false` turns the
whole thing off.

It looks for digit codes (4 to 8 digits, including forms like `123 456` and
`G-123456`) in messages that mention a code, and stays quiet when unsure.

## When a message fails to send

It stays in the conversation marked "Not sent" with the reason. `Ctrl+R`
sends the most recent failure again without disturbing what you are typing.
To pick a particular one, select it with `Tab`: Enter tries again, `x`
discards it.

If the server could not be reached at all, Fizz retries by itself a couple
of times first, since nothing was sent and there is no risk of a duplicate.
For any other failure it leaves the decision to you, because the message
may have gone through before the connection dropped.

## Notifications

A desktop notification appears for each new message, unless the window is
focused and you are looking at that conversation. Turn them off, or hide
the message text, in the settings file.

For a sound as well, set `sound = "pop"` there for a short bubble pop that
Fizz generates itself, or give it the path of a sound file of your own.
`fizz sound` plays whichever is chosen. It is off by default, since many
desktops already chime for notifications.

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
- Opening a chat marks it read on the Mac, which clears it on your other
  devices.
- Tapbacks: select a message with `Tab`, press `r`, and pick one of the
  six (arrows and Enter, or `1` to `6`). Picking the one you already gave
  removes it.
- Edit and unsend: select one of your own messages with `Tab`, then `e` to
  edit its text (Enter saves) or `u` to unsend it (it asks first). Apple
  sets the time limits: about 15 minutes to edit, about 2 to unsend.
- Replies: select a message and press `R`, type, and Enter sends it as an
  inline reply to that message.
- Typing indicators: you see "is typing…" in the conversation and the chat
  list, and the other person sees yours while you write (`send_typing` in
  the settings turns yours off).
- Groups: start a new group conversation with `G`.
- Deleting: `D` in the chat list deletes the highlighted conversation from
  Messages on the Mac, and from your other devices if Messages in iCloud is
  on. It asks first and cannot be undone.
- Starting a new conversation may need it, depending on the macOS version,
  since creating chats is one of the things the helper is for. If a new
  conversation fails to start, this is the likely reason.

Searching message text does not go through the helper: see
[Searching message text](#searching-message-text).

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
  BlueBubbles Private API. Tapbacks, replies, edits, unsending, typing
  indicators, new groups, deleting chats and marking chats read all need
  that API, and work once it is connected.
- Unread dots: Fizz remembers which conversations you have opened (in
  `~/.local/state/fizz/seen.json`), so they stay read across restarts.
  Marking them read on the Mac and your other devices needs the Private
  API helper; once it is connected Fizz does that too (`mark_read` in the
  settings turns it off).
- Shrunken copies of pictures are cached in `~/.cache/fizz/thumbs`; delete
  the folder any time. Failures are noted in `~/.cache/fizz/fizz.log`.
- Opened files (and videos fetched for their still) are kept in
  `~/.cache/fizz/files` for two weeks, then removed.
- Inside tmux, pictures show as coloured blocks rather than real pixels.
- A file the Mac itself never downloaded from iCloud can't be fetched;
  fizz says so when you try.

## Licence

[MIT](LICENSE). Fizz is an independent project, not affiliated with
BlueBubbles or Apple. iMessage is a trademark of Apple Inc.
