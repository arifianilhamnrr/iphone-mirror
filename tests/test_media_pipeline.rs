// Unit tests for RTP packet parser and RFC 7798 H.265 depacketizer
use iphone_mirror_rs::media::h265_depacketizer::H265Depacketizer;
use iphone_mirror_rs::media::nal_parser::{NalUnitType, ParameterSetCache};
use iphone_mirror_rs::media::rtp::RtpPacket;

#[test]
fn test_rtp_header_parsing() {
    let mut raw_packet = vec![
        0x80, 0x60, 0x00, 0x01, // V=2, P=0, X=0, CC=0, M=0, PT=96, Seq=1
        0x00, 0x01, 0x02, 0x03, // Timestamp
        0x12, 0x34, 0x56, 0x78, // SSRC
        0x40, 0x01, 0x0C, 0x01, // Payload: VPS NAL header (type 32)
    ];

    let packet = RtpPacket::parse(&raw_packet).expect("Failed to parse RTP");
    assert_eq!(packet.version, 2);
    assert_eq!(packet.payload_type, 96);
    assert_eq!(packet.sequence_number, 1);
    assert_eq!(packet.payload.len(), 4);
}

#[test]
fn test_parameter_set_identification() {
    // VPS (type 32): (32 << 1) = 64 = 0x40
    let vps = vec![0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0C, 0x01];
    assert_eq!(ParameterSetCache::parse_nal_type(&vps), Some(NalUnitType::VpsNut));

    // SPS (type 33): (33 << 1) = 66 = 0x42
    let sps = vec![0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01];
    assert_eq!(ParameterSetCache::parse_nal_type(&sps), Some(NalUnitType::SpsNut));

    // PPS (type 34): (34 << 1) = 68 = 0x44
    let pps = vec![0x00, 0x00, 0x00, 0x01, 0x44, 0x01];
    assert_eq!(ParameterSetCache::parse_nal_type(&pps), Some(NalUnitType::PpsNut));

    let mut cache = ParameterSetCache::new();
    cache.inspect_and_cache(&vps);
    cache.inspect_and_cache(&sps);
    cache.inspect_and_cache(&pps);
    assert!(cache.is_complete());
}
