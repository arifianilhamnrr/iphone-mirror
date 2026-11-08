// language: Rust, file: src/media/nal_parser.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalUnitType {
    TrailN,
    TrailR,
    IdrWRadl,
    IdrNLp,
    CraNut,
    VpsNut,
    SpsNut,
    PpsNut,
    PrefixSei,
    SuffixSei,
    Other(u8),
}

impl From<u8> for NalUnitType {
    fn from(val: u8) -> Self {
        match val {
            0 => NalUnitType::TrailN,
            1 => NalUnitType::TrailR,
            19 => NalUnitType::IdrWRadl,
            20 => NalUnitType::IdrNLp,
            21 => NalUnitType::CraNut,
            32 => NalUnitType::VpsNut,
            33 => NalUnitType::SpsNut,
            34 => NalUnitType::PpsNut,
            39 => NalUnitType::PrefixSei,
            40 => NalUnitType::SuffixSei,
            other => NalUnitType::Other(other),
        }
    }
}

pub struct ParameterSetCache {
    pub vps: Option<Vec<u8>>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

impl ParameterSetCache {
    pub fn new() -> Self {
        Self {
            vps: None,
            sps: None,
            pps: None,
        }
    }

    /// Extract NAL type from a NAL buffer (with 4-byte start code)
    pub fn parse_nal_type(nal: &[u8]) -> Option<NalUnitType> {
        if nal.len() < 5 {
            return None;
        }
        let offset = if nal.starts_with(&[0, 0, 0, 1]) {
            4
        } else if nal.starts_with(&[0, 0, 1]) {
            3
        } else {
            0
        };

        if nal.len() <= offset {
            return None;
        }
        let hdr = nal[offset];
        let val = (hdr >> 1) & 0x3F;
        Some(NalUnitType::from(val))
    }

    /// Inspect NAL and cache if it's VPS, SPS, or PPS
    pub fn inspect_and_cache(&mut self, nal: &[u8]) {
        if let Some(nal_type) = Self::parse_nal_type(nal) {
            match nal_type {
                NalUnitType::VpsNut => self.vps = Some(nal.to_vec()),
                NalUnitType::SpsNut => self.sps = Some(nal.to_vec()),
                NalUnitType::PpsNut => self.pps = Some(nal.to_vec()),
                _ => {}
            }
        }
    }

    pub fn is_complete(&self) -> bool {
        self.vps.is_some() && self.sps.is_some() && self.pps.is_some()
    }
}

impl Default for ParameterSetCache {
    fn default() -> Self {
        Self::new()
    }
}
