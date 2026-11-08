// Unit tests for RTP packet parser and RFC 7798 H.265 depacketizer
use iphone_mirror_rs::input::coordinate_mapper::CoordinateMapper;
use iphone_mirror_rs::media::nal_parser::{NalUnitType, ParameterSetCache};
use iphone_mirror_rs::media::rtp::RtpPacket;
use iphone_mirror_rs::media::{h265_depacketizer::H265Depacketizer, rtp};

fn rtp_packet(sequence: u16, payload: &[u8]) -> RtpPacket {
    RtpPacket {
        version: 2,
        padding: false,
        extension: false,
        marker: false,
        payload_type: 96,
        sequence_number: sequence,
        timestamp: 1,
        ssrc: 2,
        payload: payload.to_vec(),
    }
}

#[test]
fn test_rtp_header_parsing() {
    let raw_packet = vec![
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
    assert_eq!(
        ParameterSetCache::parse_nal_type(&vps),
        Some(NalUnitType::VpsNut)
    );

    // SPS (type 33): (33 << 1) = 66 = 0x42
    let sps = vec![0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01];
    assert_eq!(
        ParameterSetCache::parse_nal_type(&sps),
        Some(NalUnitType::SpsNut)
    );

    // PPS (type 34): (34 << 1) = 68 = 0x44
    let pps = vec![0x00, 0x00, 0x00, 0x01, 0x44, 0x01];
    assert_eq!(
        ParameterSetCache::parse_nal_type(&pps),
        Some(NalUnitType::PpsNut)
    );

    let mut cache = ParameterSetCache::new();
    cache.inspect_and_cache(&vps);
    cache.inspect_and_cache(&sps);
    cache.inspect_and_cache(&pps);
    assert!(cache.is_complete());
}

#[test]
fn test_single_nal_depacketization() {
    let mut depacketizer = H265Depacketizer::new();
    let packet = rtp_packet(1, &[0x40, 0x01, 0x0c, 0x01]);

    let nal = depacketizer
        .process_packet(&packet)
        .expect("single NAL should parse")
        .pop()
        .expect("single NAL should complete immediately");

    assert_eq!(nal, [vec![0, 0, 0, 1], packet.payload].concat());
}

#[test]
fn test_fragmented_nal_reassembly() {
    let mut depacketizer = H265Depacketizer::new();
    let start = rtp_packet(10, &[0x62, 0x01, 0x93, 0xaa, 0xbb]);
    let middle = rtp_packet(11, &[0x62, 0x01, 0x13, 0xcc]);
    let end = rtp_packet(12, &[0x62, 0x01, 0x53, 0xdd]);

    assert!(depacketizer.process_packet(&start).unwrap().is_empty());
    assert!(depacketizer.process_packet(&middle).unwrap().is_empty());
    assert_eq!(
        depacketizer.process_packet(&end).unwrap().pop().unwrap(),
        vec![0, 0, 0, 1, 0x26, 0x01, 0xaa, 0xbb, 0xcc, 0xdd]
    );
}

#[test]
fn test_sequence_gap_discards_fragment() {
    let mut depacketizer = H265Depacketizer::new();
    let start = rtp_packet(20, &[0x62, 0x01, 0x93, 0xaa]);
    let end_after_gap = rtp_packet(22, &[0x62, 0x01, 0x53, 0xbb]);

    assert!(depacketizer.process_packet(&start).unwrap().is_empty());
    assert!(depacketizer
        .process_packet(&end_after_gap)
        .unwrap()
        .is_empty());
}

#[test]
fn test_aggregation_packet_yields_multiple_nals() {
    let mut depacketizer = H265Depacketizer::new();
    let packet = rtp_packet(
        30,
        &[
            0x60, 0x01, // AP header (type 48)
            0x00, 0x03, 0x40, 0x01, 0xaa, // VPS
            0x00, 0x04, 0x42, 0x01, 0xbb, 0xcc, // SPS
        ],
    );

    let nals = depacketizer.process_packet(&packet).unwrap();
    assert_eq!(nals.len(), 2);
    assert_eq!(nals[0], vec![0, 0, 0, 1, 0x40, 0x01, 0xaa]);
    assert_eq!(nals[1], vec![0, 0, 0, 1, 0x42, 0x01, 0xbb, 0xcc]);
}

#[test]
fn test_rtp_extension_and_padding() {
    let raw = vec![
        0xb0, 0x60, 0, 1, 0, 0, 0, 1, 0, 0, 0, 2, // RTP header: extension + padding
        0xbe, 0xde, 0, 1, 1, 2, 3, 4, // one extension word
        0x40, 0x01, 0xaa, 0, 0, 3, // payload + three padding bytes
    ];

    let packet = rtp::RtpPacket::parse(&raw).expect("valid extension and padding");
    assert_eq!(packet.payload, vec![0x40, 0x01, 0xaa]);
}

#[test]
fn test_invalid_rtp_padding_is_rejected() {
    let raw = vec![
        0xa0, 0x60, 0, 1, 0, 0, 0, 1, 0, 0, 0, 2, // padding flag
        0x40, 0x01, 5, // claims five padding bytes, only three remain
    ];

    assert!(rtp::RtpPacket::parse(&raw).is_err());
}

#[test]
fn test_rtcp_is_detected_before_rtp_parsing() {
    let sender_report = [0x80, 200, 0, 6, 0, 0, 0, 1];
    assert!(RtpPacket::is_rtcp(&sender_report));
}

#[test]
fn test_aspect_fit_coordinate_mapping() {
    // 16:9 content centered in a square window: 218.75px bars top and bottom.
    assert_eq!(
        CoordinateMapper::map_aspect_fit(1000.0, 1000.0, 1600.0, 900.0, 500.0, 500.0),
        Some((0.5, 0.5))
    );
    assert!(
        CoordinateMapper::map_aspect_fit(1000.0, 1000.0, 1600.0, 900.0, 500.0, 100.0).is_none()
    );
}
