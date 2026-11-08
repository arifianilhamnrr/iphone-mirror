# iphone-mirror-rs

Experimental native iPhone screen mirroring client in Rust for **macOS 26 Tahoe** and **macOS 15 Sequoia**, with Intel Mac/Hackintosh support as a primary goal.

> [!IMPORTANT]
> This project is under active development. End-to-end display streaming, Metal rendering, touch forwarding, and text clipboard sync work on tested hardware. Reconnect and release hardening are not complete yet.

---

## Current status

- Working and hardware-tested: usbmuxd discovery, CoreDeviceProxy/RSD, display-service negotiation, tunneled RTP, HEVC depacketization, VideoToolbox decoding, and Metal presentation.
- Implemented: aspect-fit mouse mapping and CoreDevice touchscreen down/move/up forwarding.
- Hardware-tested: bidirectional text clipboard sharing between macOS and iPhone.
- Implemented: physical laptop keyboard forwarding (letters, numbers, punctuation, navigation, function keys, and modifiers).
- In progress: image/file clipboard payloads and hardware-button shortcuts.
- Planned: reconnect, app-bundle polish, and performance hardening.
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

The default `mirror` command establishes the CoreDevice tunnel and display-service session, decodes HEVC with VideoToolbox, and displays it in a native Metal window.

Click or drag inside the displayed phone image to send touchscreen input. Clicks in letterbox/pillarbox regions are ignored.

Plain-text clipboard contents synchronize automatically in both directions while a mirror session is active. Image and file clipboard payloads are not synchronized yet.

Focus the mirror window and type while an editable field is active on the iPhone. Keyboard input uses physical USB HID usages, including Shift, Control, Option, and Command modifiers. The current implementation follows the physical key layout rather than macOS text composition, so IME/dead-key input is not supported yet.

---

## Architecture

```
src/
├── codec/           # Hardware VideoToolbox FFI & CMFormatDescription bindings
├── input/           # CoreDevice HID Touch serialization & coordinate mapper
├── media/           # RFC 7798 RTP parser, H.265 depacketizer & UDP receiver
├── protocol/        # displayservice client & AVConference zlib offer negotiator
├── transport/       # usbmuxd device scanner & RemoteXPC RSD software tunnel
├── ui/              # macOS windowing & Metal rendering surface
├── lib.rs
└── main.rs
```
