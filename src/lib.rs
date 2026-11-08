// language: Rust, file: src/lib.rs, target: macOS 14+/15+ (Hackintosh / Mac)
pub mod audio;
pub mod codec;
pub mod input;
pub mod media;
pub mod protocol;
pub mod transport;
pub mod ui;

pub use protocol::display_client::DisplayClient;
pub use protocol::negotiator::MediaNegotiationOffer;
pub use transport::device_scanner::DeviceScanner;
