# iphone-mirror-rs

Experimental native iPhone screen mirroring client in Rust for **macOS 26 Tahoe** and **macOS 15 Sequoia**, with Intel Mac/Hackintosh support as a primary goal.

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

If the cable, tunnel, or display session drops, the supervisor waits two seconds and starts a fresh session automatically. The mirror window closes while disconnected and reappears after reconnection. Closing the window normally stops the supervisor.

The app bundle opens a native SwiftUI launcher instead of starting with an empty mirror window. It continuously scans for ready USB/Finder Wi-Fi devices, shows nearby Bonjour iPhones that still need pairing, lets you select a device and transport, and starts the Rust media backend only after **Start Mirroring**. Settings use the standard macOS `Cmd+,` window and persist through `UserDefaults`.

Connection mode can be **Auto**, **USB**, or **Wi-Fi** from the launcher sidebar or Settings. Auto treats USB and Network records with the same UDID as one phone and prefers USB. Bonjour scans `_remotepairing._tcp` and `_apple-mobdev2._tcp`; a Bonjour-only result is labeled as nearby and must be paired/enabled in Finder before it becomes connectable. Finder-ready Network records can be mirrored directly over Wi-Fi.

Click or drag inside the displayed phone image to send touchscreen input. Clicks in letterbox/pillarbox regions are ignored.

Use a two-finger trackpad gesture (or a mouse wheel) while the pointer is over the mirrored phone to scroll. The wheel stream is translated into a continuous touchscreen drag beginning at the pointer position, so it works with ordinary iOS lists and web views rather than relying on a digital-crown HID event.

Text and image clipboard contents synchronize automatically in both directions while a mirror session is active. PNG and TIFF are accepted from macOS; PNG, JPEG, and TIFF snapshots are accepted from iPhone. Images larger than 20 MiB are ignored to keep memory and tunnel usage bounded. File clipboard payloads are not synchronized yet.

Focus the mirror window and type while an editable field is active on the iPhone. Keyboard input uses physical USB HID usages, including Shift, Control, Option, and Command modifiers. The current implementation follows the physical key layout rather than macOS text composition, so IME/dead-key input is not supported yet.

While a mirror session is active, the iPhone's system audio is decoded (AAC-ELD, 48 kHz stereo) and played through the host Mac's current output device. Audio starts automatically and is kept alive with RTCP receiver reports; press `Ctrl+M` to mute playback locally. While muted, the queue receives 10 ms silent PCM frames instead of changing native AudioQueue volume/state. On unmute the host output queue is recreated before live PCM resumes, avoiding output units that remain silent on some Hackintosh audio drivers without disturbing RTP or AAC decoder state. Set `IPHONE_MIRROR_AUDIO_DUMP=/some/directory` to write `au.bin` (length-prefixed access units) and `pcm.raw` for diagnostics.

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
