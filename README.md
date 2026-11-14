<p align="center">
  <img src="assets/logo-128.png" width="96" alt="clipkeep logo">
  &nbsp;&nbsp;&nbsp;
  <a href="https://github.com/mrevds"><img src="assets/mrevds-128.png" width="96" alt="mrevds"></a>
</p>

# clipkeep

A fast clipboard history manager (like Win+V) for Linux, Windows and macOS.
One native Rust binary: unlimited history stored compressed and encrypted, search in milliseconds,
images, pins and groups, and optional end-to-end encrypted sync between your devices.

## Install

No Rust needed: the script downloads a ready-made build, installs it into your home folder
(no admin rights) and runs `clipkeep setup` (autostart, hotkey, app icon).

**Linux / macOS**

```bash
curl -fsSL https://raw.githubusercontent.com/mrevds/clipkeep/main/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://raw.githubusercontent.com/mrevds/clipkeep/main/install.ps1 | iex
```

Run the same command again to update. `clipkeep uninstall` removes autostart and the hotkey;
your history stays on disk.

Ready-made builds: Linux x86_64 (glibc 2.35+), Windows x86_64, macOS Apple Silicon and Intel.
The binaries are not code-signed yet: if you download an archive by hand in a browser, Windows
SmartScreen asks for confirmation and macOS needs `xattr -d com.apple.quarantine clipkeep clipkeepw`.

## Use

| OS | Hotkey |
|---|---|
| Linux (GNOME) | `Super+V` (other desktops: bind a key to `clipkeep show`) |
| Windows | `Win+Alt+V`, or `Ctrl+Alt+V` if taken |
| macOS | `Ctrl+Cmd+V`, or `Ctrl+Option+V` if taken |

In the window:

- type to search (all words, case-insensitive, any language)
- `↑` `↓` select, `Enter` copy and paste, `Ctrl+1…9` pick directly
- `Tab` switch tabs: All, Pinned, Links, Images and your groups
- `Ctrl+P` or right click: pin · `Ctrl+G`: put in a group · `Shift+Del`: delete
- `Esc` or a click outside closes the window

After you pick a clip, clipkeep pastes it into the app you came from (Linux and Windows;
on macOS the clip is just copied). In terminals press `Ctrl+Shift+V` yourself.

```
clipkeep [show] [TEXT]   open the window (optionally searching for TEXT)
clipkeep status          clips, disk usage, active hotkey
clipkeep list [N]        last N clips
clipkeep search WORDS    clips containing all WORDS
clipkeep clear           delete everything except pinned clips
clipkeep stop            stop the background daemon
clipkeep sync ...        see Sync
```

## Sync

Open the window, click **Sync**, sign in with Google and choose a sync password. On another
device, sign in with the same Google account and enter the same password. Save the recovery code
shown once: it is the only way back if you forget the password.

- Everything is encrypted on your device before upload. Google only stores unreadable files in a
  hidden app folder (scope `drive.appdata`) and never sees your password.
- Your password protects a random master key (Argon2id). Changing the password does not
  re-encrypt your history.
- Without Google, any shared folder works (Syncthing, Dropbox, a network drive):
  `clipkeep sync setup folder ~/Sync`.

```
clipkeep sync setup google | folder PATH   turn sync on
clipkeep sync status | now | off           check, sync now, turn off on this device
clipkeep sync password                     change the password
```

## Privacy and storage

- History, index and images are encrypted at rest (ChaCha20-Poly1305) with a random per-device
  key kept outside the data folder. There is no shared key in the code.
- Lose the key file and the local history cannot be opened, so back it up:
  Linux `~/.config/clipkeep/key.bin`, Windows `%LOCALAPPDATA%\clipkeep\key.bin`,
  macOS `~/Library/Application Support/clipkeep-key/key.bin`.
- Text is stored in compressed 64 KB blocks; duplicates are not stored twice.
  Search over 100,000 clips takes about 1 ms.

## Passwords

Passwords and keys are never kept. clipkeep holds them in memory for 60 seconds (shown masked in
the window), then forgets them and wipes the clipboard if it still holds one. They are never
written to disk and never synced. A clip counts as a secret when:

- the app that copied it marks it as private, as password managers do (KeePassXC, Bitwarden,
  1Password and others), or
- it looks like one: API tokens (GitHub, OpenAI, AWS, Google, Slack...), private keys, JWTs,
  or a single random-looking word with mixed letters, digits and symbols.

`secret_seconds = 0` in `config.txt` turns this off.

## Settings

`config.txt` in the data folder (Linux `~/.local/share/clipkeep`, Windows `%APPDATA%\clipkeep`,
macOS `~/Library/Application Support/clipkeep`):

```text
hotkey = Ctrl+Alt+V     # Windows/macOS
max_entries = 0         # 0 = unlimited
auto_paste = true       # paste the chosen clip automatically
image_limit_mb = 1024   # disk space for images; 0 = unlimited
secret_seconds = 60     # how long passwords stay; 0 = keep them like other clips
```

Restart after editing: `clipkeep stop`, then `clipkeep show`.

## Build from source

```bash
cargo install --git https://github.com/mrevds/clipkeep
clipkeep setup
```

Google sign-in is built into release binaries only. For your own build, create an OAuth client
(Google Cloud: enable Drive API, consent screen with scope `drive.appdata`, client type
Desktop app) and either put `google_client_id` and `google_client_secret` into `config.txt`
or set `CLIPKEEP_GOOGLE_CLIENT_ID` and `CLIPKEEP_GOOGLE_CLIENT_SECRET` at build time.
Folder sync works without it. `--no-default-features` builds without Google support at all.

Releases: push a tag (`git tag v0.1.0 && git push --tags`). GitHub Actions builds all platforms
and publishes the archives with the install scripts. The OAuth client comes from the repository
secrets `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET`.

Status: Linux (GNOME, Wayland) is tested on real hardware; Windows and macOS are compile-checked only.

## License

MIT
