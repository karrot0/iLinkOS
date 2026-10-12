# iLinkOS

See and control your iPhone from a Windows PC, over Wi-Fi or USB.

- `ilinkos-app` — a desktop window that mirrors the phone and takes mouse and keyboard input.

## Requirements

- Windows 10 or 11
- An iPhone on iOS 27 or later for Wi-Fi (iOS 17.4 or later over USB)
- For USB only: Apple's "Apple Devices" app (or iTunes), which provides the iPhone USB driver

## Build

You need:

- [Rust](https://rustup.rs/) 1.85 or later, with the MSVC toolchain
- Visual Studio Build Tools (the "Desktop development with C++" workload)
- [CMake](https://cmake.org/) and [NASM](https://www.nasm.us/) on `PATH`, to build the TLS library

Then, from the project folder:

```
cargo build --release
```

This produces `target\release\ilinkos.exe` and `target\release\ilinkos-app.exe`. Copy them somewhere
permanent (this repository uses `bin\`): Windows locks a running program's file, so pointing an agent at
`target\release` would block the next build.

iLinkOS uses [ffmpeg](https://ffmpeg.org/) to decode the phone's screen. Releases ship `ffmpeg.exe` in the
same folder as the two programs, and iLinkOS runs that copy. When you build from source, put an `ffmpeg.exe`
beside the built executables; without one, iLinkOS falls back to the `ffmpeg` on `PATH`.

### Publishing a release

Pushing a tag that starts with `v` makes GitHub build iLinkOS and publish a release with a zip holding both
programs and ffmpeg:

```
git tag v0.1.0
git push origin v0.1.0
```

## Connect to your phone

Pairing is done once. After that iLinkOS finds the phone by itself whenever it is unlocked and on the same
Wi-Fi as the PC.

### Over Wi-Fi

1. Put the iPhone on the same Wi-Fi network as the PC.
2. Start pairing — either run `ilinkos pair`, or open `ilinkos-app` and click **Start pairing**.
3. On the iPhone, open **Settings › Privacy & Security › Developer Mode** and turn it on.
4. On that same screen, choose this PC (it is listed under its Windows computer name).
5. The PC shows a six-digit code. Type it into the phone.

If the phone never lists the PC, a firewall is probably blocking the incoming connection. Run `ilinkos scan`
to find the phone's address and connect out to it instead:

```
ilinkos pair --to <ip:port>
```

### Over USB

Plug the phone in, unlock it and tap **Trust**. `ilinkos devices` should list it. Add `--usb` to any command
to use the cable instead of Wi-Fi, for example `ilinkos --usb info`.

### The developer disk image

The screen and input services come from Apple's developer disk image, which has to be mounted on the phone.
iLinkOS mounts it for you whenever it is missing (the phone unmounts it on every restart), but you have to
supply the image once. Put these three files in `%APPDATA%\ilinkos\ddi`:

- `Image.dmg`
- `Image.dmg.trustcache`
- `BuildManifest.plist`

They are the personalized iOS developer disk image that ships with Xcode. To mount it by hand, run
`ilinkos mount`.

### Check the connection

```
ilinkos doctor
```

This prints the phone's model and iOS version and says whether the screen, touch and button services are
available. If they are missing, turn on Developer Mode and check the disk image files above.

## Install the MCP server

The MCP server is `ilinkos mcp`, speaking over stdio. Pair the phone first; the server starts the background
connection on its own the first time a tool is called.

**Claude Code**

```
claude mcp add --scope user ilinkos -- C:\path\to\ilinkos.exe mcp
```

**Any other agent** — add this to its MCP configuration:

```json
{
  "mcpServers": {
    "ilinkos": {
      "command": "C:\\path\\to\\ilinkos.exe",
      "args": ["mcp"]
    }
  }
}
```

Use the full path to `ilinkos.exe`; nothing adds it to `PATH`. To use a cable instead of Wi-Fi, make the
arguments `["--usb", "mcp"]`.

The agent gets tools to take screenshots, tap, swipe, pinch, type, press buttons, list and launch apps, read
the elements on screen, record the screen, simulate a location and read the front app's performance.

## Command line

`ilinkos --help` lists everything. The most useful commands:

| Command | What it does |
| --- | --- |
| `ilinkos info` | Name, model, iOS version, battery and appearance |
| `ilinkos apps` / `ilinkos launch <app>` | List installed apps, or open one |
| `ilinkos shot <file.png>` | Save a screenshot |
| `ilinkos record <seconds>` | Record the screen and sound to an MP4 |
| `ilinkos logs --filter <text>` | Stream the phone's system log |
| `ilinkos locate <lat> <lon>` / `ilinkos locate off` | Simulate a location, or stop |
| `ilinkos stop` | Stop the background connection |

## Where things are kept

- Pairing and settings: `%APPDATA%\ilinkos`
- Recordings: `Videos\iLinkOS`
- Screenshots from the app: `Pictures\iLinkOS`
- Saved crash reports: `Documents\iLinkOS`
