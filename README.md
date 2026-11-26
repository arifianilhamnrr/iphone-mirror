# iPhone Mirror

Native iPhone screen mirroring for macOS, written in Rust.

- Version: **0.2.0 (build 17)**
- Host: **macOS 12 Monterey** or newer (tested up to macOS 26 Tahoe)
- Architecture: **x86_64** (Intel Mac and Hackintosh)
- Tested device: **iPhone SE on iOS 27.2.1**
- Connections: **USB** and direct **Wi-Fi**
- Status: stable

> [!IMPORTANT]
> This project uses private and undocumented Apple services. Compatibility may change after an iOS or macOS update.

## Features

- HEVC screen streaming through CoreDevice DisplayService.
- VideoToolbox hardware decoding and native Metal rendering.
- iPhone system audio playback through AudioToolbox.
- Mouse click and drag translated to iPhone touchscreen input.
- Two-finger trackpad and mouse-wheel scrolling.
- Physical keyboard forwarding.
- Text and image clipboard synchronization in both directions.
- Finder drag-and-drop from Mac to iPhone.
- Direct Wi-Fi mirroring through Bonjour and RemotePairing TLS-PSK.
- Automatic compatible DeveloperDiskImage installation.
- Automatic recovery after media stalls or RTP packet loss.
- Full-resolution PNG screenshots.
- Fullscreen, auto-rotate, and always-on-top controls.
- Native SwiftUI connection launcher.
- Hardware-clocked AudioUnit output with a lock-free PCM ring buffer, replacing the timer-driven AudioQueue path.
- Audio RTP reorder buffer with silence concealment for lost packets.
- Output-device change detection with automatic backend recreation.
- Accurate RTCP receiver reports (loss fraction, cumulative loss, jitter, LSR/DLSR).
- Selectable audio backend (`IPHONE_MIRROR_AUDIO_BACKEND=audiounit|audioqueue`) and opt-in diagnostics (`IPHONE_MIRROR_AUDIO_DIAGNOSTICS=1`).

## Installation

1. Open the [Releases](https://github.com/arifianilhamnrr/iphone-mirror/releases) page.
2. Download `iPhone-Mirror-v0.2.0-macOS-x86_64.dmg`.
3. Open the DMG.
4. Drag **iPhone Mirror.app** into **Applications**.
5. Eject the DMG.
6. Open `/Applications/iPhone Mirror.app`.

The app is ad-hoc signed and not notarized. If Gatekeeper blocks the first launch:

1. Open Finder and go to **Applications**.
2. Right-click **iPhone Mirror**.
3. Select **Open**.
4. Confirm **Open** in the dialog.

Install only one copy of the app. Running another copy from the DMG or a build directory can create duplicate Launchpad entries.

## iPhone preparation

- Enable **Developer Mode** on the iPhone.
- Unlock the iPhone before the first connection.
- Connect it to the Mac with a USB cable.
- Accept the trust or pairing prompt when it appears.
- Keep internet access available on first use so the app can download and personalize a compatible DeveloperDiskImage.

## Using the app over USB

1. Connect and unlock the iPhone.
2. Open **iPhone Mirror**.
3. Select **USB** or **Auto** in the launcher.
4. Select the iPhone from the device list.
5. Click **Start Mirroring**.
6. The launcher hides and the mirror window opens.
7. Close the mirror window to end the session; the launcher reappears automatically.

## Enabling direct Wi-Fi mirroring

Wi-Fi mirroring does not require Finder Wi-Fi Sync. One initial cable pairing is required to create the RemotePairing identity.

1. Connect the iPhone by USB and unlock it.
2. Open Terminal.
3. Run:

```bash
"/Applications/iPhone Mirror.app/Contents/Helpers/iPhone Mirror Stream.app/Contents/MacOS/iphone-mirror-backend" pair
```

4. Accept any pairing prompt on the iPhone.
5. Wait for `Paired. Wi-Fi mirroring is now available for this Mac.`
6. Disconnect the USB cable.
7. Keep the Mac and iPhone on the same network.
8. Open **iPhone Mirror**.
9. Select **Wi-Fi** or **Auto**.
10. Select the discovered iPhone and click **Start Mirroring**.

The pairing identity is stored at:

```text
~/Library/Application Support/iPhone Mirror/rppairing.plist
```

Deleting that file requires pairing again by cable.

## Controls

### Pointer and trackpad

- Click: tap the iPhone screen.
- Click and drag: touchscreen drag.
- Two-finger trackpad gesture: scroll.
- Mouse wheel: scroll.
- `↑` / `↓`: keyboard scrolling.
- Clicks outside the displayed phone image are ignored.

### Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl+H` | Home |
| `Ctrl+L` | Lock or sleep |
| `Ctrl+[` | Volume down |
| `Ctrl+]` | Volume up |
| `Ctrl+\` | Mute the iPhone |
| `Ctrl+M` | Mute or unmute Mac audio playback |
| `Ctrl+S` | Save a PNG screenshot |
| `Ctrl+T` | Toggle always-on-top |
| `Ctrl+R` | Rotate display manually |
| `Ctrl+Cmd+F` | Toggle borderless fullscreen |
| `Cmd+,` | Open settings |
| `↑` / `↓` | Scroll the mirrored screen |
| `Esc` | Back (left-edge swipe gesture) |

Screenshots are saved to:

```text
~/Pictures/iPhone Mirror
```

## Clipboard

- Text synchronizes automatically in both directions.
- Images synchronize automatically in both directions.
- macOS input formats: PNG and TIFF.
- iPhone input formats: PNG, JPEG, and TIFF.
- Clipboard images larger than 20 MiB are ignored.
- IME composition and dead-key input are not currently supported.

## Drag-and-drop files

1. Open the destination app on the mirrored iPhone.
2. Focus the destination field or conversation.
3. Drag one or more files from Finder onto the mirror window.
4. Wait for the transfer progress shown in the window title.
5. The app stages the files on the iPhone and sends `Cmd+V` automatically.

Transfer limits:

- Maximum 10 files per batch.
- Maximum 100 MiB per file.
- Maximum 250 MiB per batch.
- Directories are not supported.
- Symbolic links are not supported.
- Transfer currently works from Mac to iPhone only.
- Some iPhone apps may not accept every pasteboard representation.
- Unknown or large file types may fall back to a file URL.

Disable automatic paste with:

```bash
IPHONE_MIRROR_AUTO_PASTE=0 \
  "/Applications/iPhone Mirror.app/Contents/Helpers/iPhone Mirror Stream.app/Contents/MacOS/iphone-mirror-backend" mirror
```

Transferred files are staged under:

```text
/var/mobile/Media/Downloads/iphone-mirror-transfers
```

## Settings

Open settings with `Cmd+,`.

- **Automatic**: prefer USB when both USB and Wi-Fi are available.
- **USB only**: list and use USB devices only.
- **Wi-Fi only**: list and use direct RemotePairing devices only.
- **Auto Connect**: start when a matching iPhone appears.
- **Auto Rotate**: resize the mirror window when the incoming orientation changes.

## Automatic DeveloperDiskImage repair

- DisplayService is provided by the mounted DeveloperDiskImage, not directly by iOS.
- If DisplayService is missing, the app downloads the pinned Cryptex1 DDI build `27B52`.
- A stale image at `/System/Developer` is unmounted automatically.
- The DDI is personalized through Apple's TSS service and installed through `cryptexd`.
- The RSD tunnel is reopened after installation.
- Downloaded assets are cached under:

```text
~/Library/Application Support/iPhone Mirror/DDI
```

Developer Mode and internet access are required for the first repair.

## Diagnostics

Build the project first, then use these commands from the repository root:

```bash
# List USB/usbmuxd and Bonjour Wi-Fi devices
./target/release/iphone-mirror devices

# Verify CoreDevice, RSD, and DisplayService
./target/release/iphone-mirror diagnose

# Diagnose a specific USB device
./target/release/iphone-mirror diagnose --udid <UDID>

# Create the one-time Wi-Fi pairing identity
./target/release/iphone-mirror pair

# Test AFC upload, readback, and device pasteboard publication
./target/release/iphone-mirror probe-files

# Send files through the drag-and-drop transfer pipeline
./target/release/iphone-mirror send-files /path/to/file ...
```

Useful environment variables:

```bash
# Detailed logging
RUST_LOG=debug

# Include the full RSD service catalog in diagnose output
IPHONE_MIRROR_LIST_SERVICES=1

# Dump decoded audio data
IPHONE_MIRROR_AUDIO_DUMP=/path/to/directory

# Dump media negotiation replies
IPHONE_MIRROR_DUMP_NEGOTIATION=/path/to/directory

# Disable Cmd+V after file transfer
IPHONE_MIRROR_AUTO_PASTE=0

# Select audio output backend (default: audiounit)
IPHONE_MIRROR_AUDIO_BACKEND=audiounit
IPHONE_MIRROR_AUDIO_BACKEND=audioqueue

# Per-second audio sink diagnostics (RTP, ring depth, callbacks, restarts)
IPHONE_MIRROR_AUDIO_DIAGNOSTICS=1

# Opt-in: recreate audio backend on default output device change
IPHONE_MIRROR_AUDIO_DEVICE_SWITCH=1
```

The app-bundle backend log is written to the macOS temporary directory as:

```text
iphone-mirror-backend.log
```

## Troubleshooting

### No device appears

- Unlock and reconnect the iPhone.
- Accept the trust prompt.
- Confirm Apple Mobile Device services are running.
- For Wi-Fi, run the one-time `pair` command again.
- Confirm the Mac and iPhone are on the same network.

### DisplayService is unavailable

- Confirm Developer Mode is enabled.
- Connect to the internet.
- Restart the app and allow automatic DDI repair to finish.
- Run `diagnose` and check `Display service: available`.

### Video freezes

- The app sends RTCP feedback and restarts a stalled media session automatically.
- Wi-Fi RTP packet loss can still cause a short interruption before recovery.
- Use USB for the lowest latency and highest stability.

### Audio stops

- Audio now runs through a hardware-clocked AudioUnit with a lock-free PCM ring buffer; short network gaps produce silence instead of stopping output.
- Press `Ctrl+M` twice if a driver still needs a manual reset.
- A wedged output backend is recreated automatically (with a cooldown).
- Run with `IPHONE_MIRROR_AUDIO_DIAGNOSTICS=1` and check `callbacks`, `underruns`, and `restarts` in the log.
- To compare against the legacy path, use `IPHONE_MIRROR_AUDIO_BACKEND=audioqueue`.

### Duplicate Launchpad icon

- Keep only `/Applications/iPhone Mirror.app` installed.
- Eject mounted installer DMGs.
- Do not run a loose `.app` copy from another directory.
- Restart Dock after removing duplicate copies:

```bash
killall Dock
```

## Building from source

Requirements:

- Xcode command-line tools.
- Rust 1.85 or newer.
- Python and Pillow only when regenerating the icon.

```bash
git clone https://github.com/arifianilhamnrr/iphone-mirror.git
cd iphone-mirror
cargo build --release
```

Create and verify an app bundle:

```bash
make verify
open "dist/iPhone Mirror.app"
```

Create release packages:

```bash
make package
```

Generated artifacts:

```text
dist/iPhone-Mirror-v0.2.0-macOS-x86_64.dmg
dist/iPhone-Mirror-v0.2.0-macOS-x86_64.zip
dist/SHA256SUMS.txt
```

`make package` removes the loose app bundle from `dist/` after packaging to prevent duplicate Launchpad entries.

## Architecture

```text
src/
├── audio/       # AAC-ELD decoding and AudioQueue playback
├── codec/       # VideoToolbox decoding and screenshots
├── input/       # Touch, keyboard, shortcuts, and clipboard
├── media/       # RTP parsing and HEVC depacketization
├── protocol/    # DisplayService negotiation and RTCP feedback
├── transfer/    # AFC staging, pasteboard publication, and file limits
├── transport/   # usbmuxd, CoreDeviceProxy, RSD, DDI, and Wi-Fi pairing
├── ui/          # macOS window and Metal renderer
├── lib.rs
└── main.rs
```

## Known limitations

- Tested hardware coverage is currently limited.
- Release artifacts are x86_64 only.
- The app is not notarized.
- The software TCP tunnel may lose RTP bursts over Wi-Fi.
- File transfer is Mac to iPhone only.
- Folder transfer is not implemented.
- Performance and zero-copy behavior have not been benchmarked.
- Sustained throughput, roaming, and sleep/wake reliability need more testing.
