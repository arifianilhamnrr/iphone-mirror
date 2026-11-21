# iphone-mirror-rs

Experimental native iPhone screen mirroring client in Rust for **macOS 26 Tahoe** and **macOS 15 Sequoia**, with Intel Mac/Hackintosh support as a primary goal.

Current build: **0.1.0-beta.2 (build 8)**. The launcher shows this in the sidebar footer and in Settings → General, so you can tell which experimental build is running.

### Drag-and-drop: Phase 0/1 (file-transfer foundation)

An early transfer probe is available from the CLI:

```bash
./target/release/iphone-mirror probe-files
```

It stages a small text file on the iPhone over AFC, verifies the bytes, publishes its `file://` URL on the device pasteboard, then removes the temporary file. Open Files or Notes on the iPhone and paste to check whether the URL is accepted.

Real files can be sent with `iphone-mirror send-files <path>...`. The same pipeline is used when you drag files from Finder onto the mirror window: the window title shows progress, and the file is staged under `/var/mobile/Media/Downloads/iphone-mirror-transfers` on the iPhone. The device pasteboard then gets **two representations** of the file: a `file://` URL and the file's inline content under its content UTI (for example `public.png` or `public.data`). After the drop, the app automatically sends ⌘V to the iPhone so the focused app receives the file without a manual paste. Set `IPHONE_MIRROR_AUTO_PASTE=0` to keep the manual paste step. Limits for this beta: 10 files, 100 MiB per file, 250 MiB per batch. If the device rejects a large inline payload, the transfer falls back to publishing the file URL only. Directories and symlinks are rejected.

> [!IMPORTANT]
> This project is under active development. End-to-end display streaming, Metal rendering, touch forwarding, text clipboard sync, keyboard input, and iPhone system audio playback work on tested hardware. Reconnect and release hardening are not complete yet.

---

## Current status

- Working and hardware-tested: usbmuxd discovery, CoreDeviceProxy/RSD, display-service negotiation, tunneled RTP, HEVC depacketization, VideoToolbox decoding, and Metal presentation.
- Implemented: aspect-fit mouse mapping and CoreDevice touchscreen down/move/up forwarding.
- Hardware-tested: bidirectional text and image clipboard sharing between macOS and iPhone.
- Implemented: physical laptop keyboard forwarding (letters, numbers, punctuation, navigation, function keys, and modifiers).
- Hardware-tested: iPhone system audio playback on the host Mac (AAC-ELD, 48 kHz stereo, via AudioToolbox/AudioQueue), bounded 120 ms burst buffering, and RTCP receiver reports keeping the audio session alive.
- Implemented: hardware-button shortcuts (Home, Lock, volume, mute) through the Indigo HID service.
- Hardware-tested: full-resolution PNG screenshots, fullscreen, and always-on-top window controls.
- Implemented: automatic session restart after a stream/tunnel failure; each retry recreates CoreDevice, VideoToolbox, AudioToolbox, HID, and clipboard state cleanly.
- Implemented: automatic DeveloperDiskImage repair when `com.apple.coredevice.displayservice` is missing from the RSD catalog. iOS 27+ devices get a compatible Cryptex1 DDI downloaded and installed through `cryptexd` (including unmounting a stale Xcode 26 image), so an iOS update or a reboot no longer leaves mirroring broken.
- Implemented: direct Wi-Fi mirroring over a RemotePairing TLS-PSK tunnel with Bonjour discovery and one-time USB pairing.
- Implemented: RTCP receiver reports and Picture Loss Indication for both media streams, plus automatic media-session restart when the decoded video stalls, so a burst of RTP loss no longer leaves the picture frozen permanently.
- In progress: file clipboard payloads and drag-and-drop transfer.
- Planned: app-bundle polish and performance hardening.
- Performance and zero-copy behavior have not been benchmarked yet.

---

## Build and diagnostics

Requirements: Xcode command-line tools and Rust 1.85 or newer.

```bash
# 1. Clone repository
git clone <your-repo-url>
cd iphone-mirror-rs

# 2. Build in release mode
cargo build --release

# 3. Connect and trust an iPhone, then list devices
./target/release/iphone-mirror devices

# 4. Verify CoreDevice/RSD and display-service availability
./target/release/iphone-mirror diagnose

# Select a device when multiple iPhones are connected
./target/release/iphone-mirror diagnose --udid <UDID>

# Pair once over USB so Wi-Fi mirroring works without a cable afterwards
./target/release/iphone-mirror pair
```

Set `IPHONE_MIRROR_LIST_SERVICES=1` to include the full RSD service catalog in diagnostic output. Use `RUST_LOG=debug` for detailed logs.

To create a LaunchServices-ready, ad-hoc-signed app bundle and distributable ZIP:

```bash
# Regenerate the checked-in icon only when its artwork changes (requires Pillow).
make icon

# Build, bundle, verify the signature, and create ZIP + DMG installer artifacts.
make package

open "dist/iPhone Mirror.app"
```

The DMG presents `iPhone Mirror.app` next to an Applications shortcut: drag the app onto Applications to install it. `make package` also writes `dist/SHA256SUMS.txt`. Packaging itself only needs the Xcode command-line tools and Rust; the generated `resources/AppIcon.icns` is tracked so end users do not need Python/Pillow.

Current release artifacts are built for Intel (`x86_64`) macOS. The app is ad-hoc signed but not Apple-notarized, so first launch may require right-clicking the app and choosing **Open**.

The default `mirror` command establishes the CoreDevice tunnel and display-service session, decodes HEVC with VideoToolbox, and displays it in a native Metal window.

The display service is provided by the mounted DeveloperDiskImage, not by iOS itself. If it is missing from the RSD catalog, the app automatically downloads the pinned Cryptex1 DDI (`27B52`, cached under `~/Library/Application Support/iPhone Mirror/DDI`), unmounts any stale image at `/System/Developer`, installs the new cryptex through `cryptexd`, and reconnects the tunnel. This needs internet access on first use (for the DDI download and Apple's TSS signing service) and Developer Mode enabled.

If the cable, tunnel, or display session drops, the supervisor waits two seconds and starts a fresh session automatically. The mirror window closes while disconnected and reappears after reconnection. Closing the window normally stops the supervisor.

Disconnecting from the SwiftUI launcher, closing the mirror window, sending SIGTERM, or pressing Ctrl-C all use the same graceful shutdown path. The backend releases the virtual keyboard and waits for the iPhone to acknowledge `stop_media_stream` before exiting, so protected camera previews and view-once messages are no longer left blocked by a stale mirroring session.

The app bundle opens a native SwiftUI launcher instead of starting with an empty mirror window. It continuously scans for ready USB/Finder Wi-Fi devices, shows nearby Bonjour iPhones that still need pairing, lets you select a device and transport, and starts the Rust media backend only after **Start Mirroring**. Settings use the standard macOS `Cmd+,` window and persist through `UserDefaults`.

Connection mode can be **Auto**, **USB**, or **Wi-Fi** from the launcher sidebar or Settings. Auto treats USB and Network records with the same UDID as one phone and prefers USB. Wi-Fi uses a direct RemotePairing TLS-PSK tunnel: the backend discovers `_remotepairing._tcp` with `dns-sd`, pairs once over USB (`iphone-mirror pair`, or accept the on-device prompt on first connection), and stores the identity in `~/Library/Application Support/iPhone Mirror/rppairing.plist`. After that, mirroring over Wi-Fi needs no cable and no usbmuxd Network record. Finder Wi-Fi sync still works as before for the USB/Auto path.

Click or drag inside the displayed phone image to send touchscreen input. Clicks in letterbox/pillarbox regions are ignored.

Use a two-finger trackpad gesture (or a mouse wheel) while the pointer is over the mirrored phone to scroll. The wheel stream is translated into a continuous touchscreen drag beginning at the pointer position, so it works with ordinary iOS lists and web views rather than relying on a digital-crown HID event. The `↑` / `↓` arrow keys drive the same synthetic drag from the pointer position (or the screen centre when the pointer is outside the image), including OS key repeat. `Esc` sends the consumer-page "AC Back" hardware button.

Text and image clipboard contents synchronize automatically in both directions while a mirror session is active. PNG and TIFF are accepted from macOS; PNG, JPEG, and TIFF snapshots are accepted from iPhone. Images larger than 20 MiB are ignored to keep memory and tunnel usage bounded. File clipboard payloads are not synchronized yet.

Focus the mirror window and type while an editable field is active on the iPhone. Keyboard input uses physical USB HID usages, including Shift, Control, Option, and Command modifiers. The current implementation follows the physical key layout rather than macOS text composition, so IME/dead-key input is not supported yet.

While a mirror session is active, the iPhone's system audio is decoded (AAC-ELD, 48 kHz stereo) and played through the host Mac's current output device. Audio starts automatically and is kept alive with RTCP receiver reports sent to the `RTCPRemotePort` from the negotiation answer; press `Ctrl+M` to mute playback locally. While muted, the queue receives 10 ms silent PCM frames instead of changing native AudioQueue volume/state. On unmute the host output queue is recreated before live PCM resumes, avoiding output units that remain silent on some Hackintosh audio drivers without disturbing RTP or AAC decoder state. The same recreation now happens automatically if AudioToolbox stops returning consumed buffers for three seconds, so the output no longer has to be muted and unmuted manually to recover. Set `IPHONE_MIRROR_AUDIO_DUMP=/some/directory` to write `au.bin` (length-prefixed access units) and `pcm.raw` for diagnostics.

Keyboard shortcuts (hold Control):

| Shortcut | Action |
| --- | --- |
| `Ctrl+H` | Home |
| `Ctrl+L` | Lock / sleep |
| `Ctrl+[` / `Ctrl+]` | Volume down / up |
| `Ctrl+\` | Mute the iPhone |
| `Ctrl+M` | Mute or unmute audio playback on the Mac (local only) |
| `Ctrl+S` | Save a PNG to `~/Pictures/iPhone Mirror` |
| `Ctrl+T` | Toggle always-on-top |
| `Ctrl+Cmd+F` | Toggle borderless fullscreen |
| `Cmd+,` | Open connection and auto-rotate settings |
| `↑` / `↓` | Scroll the mirrored screen (synthetic touch drag, works with key repeat) |
| `Esc` | Back (hardware "AC Back" button) |

Auto-Rotate is enabled by default. When the incoming frame changes between portrait and landscape, the window swaps its dimensions while preserving aspect-fit rendering and touch/scroll mapping. Fullscreen size is not changed.

---

## Architecture

```
src/
├── audio/           # AAC-ELD decode (AudioConverter) & PCM playback (AudioQueue)
├── codec/           # Hardware VideoToolbox FFI & CMFormatDescription bindings
├── input/           # CoreDevice HID Touch serialization & coordinate mapper
├── media/           # RFC 7798 RTP parser, H.265 depacketizer & UDP receiver
├── protocol/        # displayservice client & AVConference zlib offer negotiator
├── transport/       # usbmuxd device scanner & RemoteXPC RSD software tunnel
├── ui/              # macOS windowing & Metal rendering surface
├── lib.rs
└── main.rs
```
