//! H.264 / HEVC bitstream plumbing between the wire format (Annex-B, start
//! codes, parameter sets in-band before every IDR) and VideoToolbox (4-byte
//! length-prefixed NAL units, parameter sets in the format description).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NalCodec {
    H264,
    Hevc,
}

impl NalCodec {
    pub fn nal_type(self, nal: &[u8]) -> u8 {
        match (self, nal.first()) {
            (_, None) => 0xff,
            (NalCodec::H264, Some(b)) => b & 0x1f,
            (NalCodec::Hevc, Some(b)) => (b >> 1) & 0x3f,
        }
    }

    /// Parameter-set rank (VPS < SPS < PPS) or None for other NAL types.
    pub fn param_rank(self, t: u8) -> Option<u8> {
        match (self, t) {
            (NalCodec::H264, 7) => Some(1),
            (NalCodec::H264, 8) => Some(2),
            (NalCodec::Hevc, 32) => Some(0),
            (NalCodec::Hevc, 33) => Some(1),
            (NalCodec::Hevc, 34) => Some(2),
            _ => None,
        }
    }

    /// NAL types that must not go into a VideoToolbox sample (access-unit
    /// delimiters and filler data).
    pub fn is_dropped(self, t: u8) -> bool {
        match self {
            NalCodec::H264 => t == 9 || t == 12,
            NalCodec::Hevc => t == 35 || t == 38,
        }
    }

    /// IDR / IRAP picture.
    pub fn is_keyframe(self, t: u8) -> bool {
        match self {
            NalCodec::H264 => t == 5,
            NalCodec::Hevc => (16..=23).contains(&t),
        }
    }
}

/// Iterate the NAL units of an Annex-B stream (3- or 4-byte start codes).
pub fn nal_units(data: &[u8]) -> NalIter<'_> {
    let pos = find_start(data, 0).map_or(data.len(), |(_, after)| after);
    NalIter { data, pos }
}

pub struct NalIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for NalIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.data.len() {
            let start = self.pos;
            let (end, next) = find_start(self.data, start).unwrap_or((self.data.len(), self.data.len()));
            self.pos = next;
            // Trailing zero bytes belong to the next start code (a NAL unit
            // never ends in 0x00).
            let mut e = end;
            while e > start && self.data[e - 1] == 0 {
                e -= 1;
            }
            if e > start {
                return Some(&self.data[start..e]);
            }
        }
        None
    }
}

/// Find the next `00 00 01` at or after `from`: (index of the start code,
/// index just past it).
fn find_start(d: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= d.len() {
        if d[i + 2] > 1 {
            i += 3;
        } else if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            return Some((i, i + 3));
        } else {
            i += 1;
        }
    }
    None
}

/// Append `nal` as a 4-byte big-endian length-prefixed unit.
pub fn push_length_prefixed(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

/// Append `nal` with a 4-byte start code.
pub fn push_annexb(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Convert length-prefixed NAL units (`len_size` = 1, 2 or 4 bytes) to Annex-B.
pub fn length_prefixed_to_annexb(data: &[u8], len_size: usize, out: &mut Vec<u8>) -> Result<(), String> {
    if !matches!(len_size, 1 | 2 | 4) {
        return Err(format!("unsupported NAL length size {len_size}"));
    }
    let mut i = 0;
    while i < data.len() {
        if i + len_size > data.len() {
            return Err("truncated NAL length".into());
        }
        let n = data[i..i + len_size].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        i += len_size;
        if n > data.len() - i {
            return Err(format!("NAL length {n} exceeds sample ({} left)", data.len() - i));
        }
        if n > 0 {
            push_annexb(out, &data[i..i + n]);
        }
        i += n;
    }
    Ok(())
}

/// An Annex-B access unit split for VideoToolbox.
#[derive(Debug, Default)]
pub struct AccessUnit<'a> {
    /// Parameter sets in VPS, SPS, PPS order (duplicates removed).
    pub params: Vec<&'a [u8]>,
    /// Picture data as 4-byte length-prefixed NAL units.
    pub sample: Vec<u8>,
    pub keyframe: bool,
}

pub fn split_access_unit(codec: NalCodec, data: &[u8]) -> AccessUnit<'_> {
    let mut au = AccessUnit { sample: Vec::with_capacity(data.len() + 64), ..Default::default() };
    let mut ranked: Vec<(u8, &[u8])> = Vec::new();
    for nal in nal_units(data) {
        let t = codec.nal_type(nal);
        if let Some(rank) = codec.param_rank(t) {
            if !ranked.iter().any(|(_, p)| *p == nal) {
                ranked.push((rank, nal));
            }
        } else if !codec.is_dropped(t) {
            au.keyframe |= codec.is_keyframe(t);
            push_length_prefixed(&mut au.sample, nal);
        }
    }
    ranked.sort_by_key(|(r, _)| *r); // stable: keeps stream order within a type
    au.params = ranked.into_iter().map(|(_, p)| p).collect();
    au
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let s = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4, 5, 0];
        let v: Vec<&[u8]> = nal_units(&s).collect();
        assert_eq!(v, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }

    #[test]
    fn ignores_leading_garbage_and_empty() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 0, 1]).count(), 0);
        let v: Vec<&[u8]> = nal_units(&[9, 9, 0, 0, 1, 0x41, 7]).collect();
        assert_eq!(v, vec![&[0x41, 7][..]]);
    }

    #[test]
    fn emulation_prevention_bytes_are_not_start_codes() {
        // 00 00 03 01 inside a NAL must not split it.
        let s = [0, 0, 1, 0x41, 0, 0, 3, 1, 0xff];
        let v: Vec<&[u8]> = nal_units(&s).collect();
        assert_eq!(v, vec![&[0x41, 0, 0, 3, 1, 0xff][..]]);
    }

    #[test]
    fn length_prefixed_roundtrip() {
        let nals: [&[u8]; 3] = [&[0x40, 1, 1], &[0x42, 1, 2, 3], &[0x26, 1, 0xaa, 0, 0, 3, 0xbb]];
        let mut lp = Vec::new();
        let mut ab = Vec::new();
        for n in nals {
            push_length_prefixed(&mut lp, n);
            push_annexb(&mut ab, n);
        }
        let mut out = Vec::new();
        length_prefixed_to_annexb(&lp, 4, &mut out).unwrap();
        assert_eq!(out, ab);
        let back: Vec<&[u8]> = nal_units(&out).collect();
        assert_eq!(back, nals.to_vec());

        // 2-byte lengths
        let mut lp2 = Vec::new();
        for n in nals {
            lp2.extend_from_slice(&(n.len() as u16).to_be_bytes());
            lp2.extend_from_slice(n);
        }
        let mut out2 = Vec::new();
        length_prefixed_to_annexb(&lp2, 2, &mut out2).unwrap();
        assert_eq!(out2, ab);
    }

    #[test]
    fn length_prefixed_rejects_truncation() {
        let mut out = Vec::new();
        assert!(length_prefixed_to_annexb(&[0, 0, 0, 9, 1, 2], 4, &mut out).is_err());
        assert!(length_prefixed_to_annexb(&[0, 0], 4, &mut out).is_err());
        assert!(length_prefixed_to_annexb(&[1, 2], 3, &mut out).is_err());
    }

    #[test]
    fn hevc_access_unit() {
        // AUD, VPS, SPS, PPS, prefix SEI, IDR_W_RADL slice, with PPS repeated.
        let mut s = Vec::new();
        for n in [
            &[0x46, 0x01, 0x50][..],
            &[0x40, 0x01, 0x0c],
            &[0x42, 0x01, 0x01],
            &[0x44, 0x01, 0xc1],
            &[0x4e, 0x01, 0x05],
            &[0x26, 0x01, 0xaf, 0x10],
            &[0x44, 0x01, 0xc1],
        ] {
            push_annexb(&mut s, n);
        }
        let au = split_access_unit(NalCodec::Hevc, &s);
        assert!(au.keyframe);
        assert_eq!(au.params, vec![&[0x40, 0x01, 0x0c][..], &[0x42, 0x01, 0x01], &[0x44, 0x01, 0xc1]]);
        let mut expect = Vec::new();
        push_length_prefixed(&mut expect, &[0x4e, 0x01, 0x05]);
        push_length_prefixed(&mut expect, &[0x26, 0x01, 0xaf, 0x10]);
        assert_eq!(au.sample, expect);

        // TRAIL_R (type 1) P-frame: not a keyframe, no params.
        let mut p = Vec::new();
        push_annexb(&mut p, &[0x02, 0x01, 0xd0]);
        let au = split_access_unit(NalCodec::Hevc, &p);
        assert!(!au.keyframe && au.params.is_empty() && au.sample.len() == 7);
    }

    #[test]
    fn h264_access_unit() {
        let mut s = Vec::new();
        for n in [&[0x09, 0xf0][..], &[0x67, 0x64, 0x00, 0x28], &[0x68, 0xee, 0x3c], &[0x65, 0x88, 0x84]] {
            push_annexb(&mut s, n);
        }
        let au = split_access_unit(NalCodec::H264, &s);
        assert!(au.keyframe);
        assert_eq!(au.params, vec![&[0x67, 0x64, 0x00, 0x28][..], &[0x68, 0xee, 0x3c]]);
        assert_eq!(au.sample, [0, 0, 0, 3, 0x65, 0x88, 0x84]);
        let mut p = Vec::new();
        push_annexb(&mut p, &[0x41, 0x9a]);
        assert!(!split_access_unit(NalCodec::H264, &p).keyframe);
    }
}
