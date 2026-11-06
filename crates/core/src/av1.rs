//! AV1 bitstream helpers: split a temporal unit (low-overhead OBU format, as
//! NVENC produces it) into the parts a decoder API wants, and build the
//! `av1C` configuration record (AV1-ISOBMFF §2.3) from the sequence header,
//! which VideoToolbox needs to set up an AV1 decoder.

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_PADDING: u8 = 15;

/// One OBU: its type and the complete bytes (header, size field, payload).
#[derive(Clone, Copy, Debug)]
pub struct Obu<'a> {
    pub kind: u8,
    pub bytes: &'a [u8],
    /// Payload only.
    pub payload: &'a [u8],
}

fn leb128(d: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, b) in d.iter().take(8).enumerate() {
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Iterate the OBUs of a temporal unit. Every OBU must carry a size field
/// (the low-overhead format); `None` if the data is malformed.
pub fn obus(mut d: &[u8]) -> Option<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    while !d.is_empty() {
        let h = d[0];
        if h & 0x80 != 0 {
            return None; // forbidden bit
        }
        let kind = (h >> 3) & 0x0f;
        let ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let hdr = 1 + ext as usize;
        if !has_size || d.len() < hdr {
            return None;
        }
        let (size, n) = leb128(&d[hdr..])?;
        let start = hdr + n;
        let end = start.checked_add(usize::try_from(size).ok()?)?;
        if end > d.len() {
            return None;
        }
        out.push(Obu { kind, bytes: &d[..end], payload: &d[start..end] });
        d = &d[end..];
    }
    Some(out)
}

/// A temporal unit split for a decoder: the sequence header OBU (if this
/// unit has one) and the sample data without temporal delimiters/padding.
pub struct TemporalUnit<'a> {
    pub sequence_header: Option<&'a [u8]>,
    pub sample: Vec<u8>,
}

pub fn split(d: &[u8]) -> Option<TemporalUnit<'_>> {
    let obus = obus(d)?;
    let mut sequence_header = None;
    let mut sample = Vec::with_capacity(d.len());
    for o in obus {
        match o.kind {
            OBU_TEMPORAL_DELIMITER | OBU_PADDING => {}
            OBU_SEQUENCE_HEADER => {
                sequence_header.get_or_insert(o.bytes);
                sample.extend_from_slice(o.bytes);
            }
            _ => sample.extend_from_slice(o.bytes),
        }
    }
    Some(TemporalUnit { sequence_header, sample })
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }
    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return None;
            }
        }
        Some(self.f(zeros)? + ((1u32 << zeros) - 1))
    }
}

/// The parts of a sequence header the `av1C` record and decoder setup need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceHeader {
    pub profile: u8,
    pub level: u8,
    pub tier: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub width: u32,
    pub height: u32,
    pub full_range: bool,
    /// (primaries, transfer, matrix) when signalled.
    pub color: Option<(u8, u8, u8)>,
}

/// Parse a sequence header OBU payload (AV1 spec §5.5).
pub fn parse_sequence_header(p: &[u8]) -> Option<SequenceHeader> {
    let mut b = Bits { d: p, pos: 0 };
    let profile = b.f(3)? as u8;
    let _still_picture = b.flag()?;
    let reduced = b.flag()?;
    let (level, tier);
    let mut decoder_model_info = false;
    let mut buffer_delay_len = 0;
    if reduced {
        level = b.f(5)? as u8;
        tier = 0;
    } else {
        if b.flag()? {
            // timing_info
            b.f(32)?;
            b.f(32)?;
            if b.flag()? {
                b.uvlc()?;
            }
            decoder_model_info = b.flag()?;
            if decoder_model_info {
                buffer_delay_len = b.f(5)? + 1;
                b.f(32)?;
                b.f(5)?;
                b.f(5)?;
            }
        }
        let initial_display_delay = b.flag()?;
        let ops = b.f(5)? + 1;
        let (mut l0, mut t0) = (0, 0);
        for i in 0..ops {
            b.f(12)?; // operating_point_idc
            let l = b.f(5)? as u8;
            let t = if l > 7 { b.f(1)? as u8 } else { 0 };
            if decoder_model_info && b.flag()? {
                b.f(buffer_delay_len)?;
                b.f(buffer_delay_len)?;
                b.f(1)?;
            }
            if initial_display_delay && b.flag()? {
                b.f(4)?;
            }
            if i == 0 {
                (l0, t0) = (l, t);
            }
        }
        level = l0;
        tier = t0;
    }
    let wbits = b.f(4)? + 1;
    let hbits = b.f(4)? + 1;
    let width = b.f(wbits)? + 1;
    let height = b.f(hbits)? + 1;
    if !reduced && b.flag()? {
        b.f(4)?;
        b.f(3)?;
    }
    b.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        b.f(4)?; // interintra_compound, masked_compound, warped_motion, dual_filter
        let order_hint = b.flag()?;
        if order_hint {
            b.f(2)?; // jnt_comp, ref_frame_mvs
        }
        let force_screen_content = if b.flag()? { 2 } else { b.f(1)? };
        if force_screen_content > 0 && !b.flag()? {
            b.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.f(3)?;
        }
    }
    b.f(3)?; // enable_superres, enable_cdef, enable_restoration
    // color_config (§5.5.2)
    let high_bitdepth = b.flag()?;
    let twelve_bit = profile == 2 && high_bitdepth && b.flag()?;
    let monochrome = profile != 1 && b.flag()?;
    let color = if b.flag()? { Some((b.f(8)? as u8, b.f(8)? as u8, b.f(8)? as u8)) } else { None };
    let (full_range, ss_x, ss_y, csp);
    if monochrome {
        full_range = b.flag()?;
        (ss_x, ss_y, csp) = (true, true, 0);
    } else if color == Some((1, 13, 0)) {
        // sRGB: 4:4:4, full range
        full_range = true;
        (ss_x, ss_y, csp) = (false, false, 0);
    } else {
        full_range = b.flag()?;
        let (x, y) = match profile {
            0 => (true, true),
            1 => (false, false),
            _ if twelve_bit => {
                let x = b.flag()?;
                (x, x && b.flag()?)
            }
            _ => (true, false),
        };
        let pos = if x && y { b.f(2)? as u8 } else { 0 };
        (ss_x, ss_y, csp) = (x, y, pos);
    }
    Some(SequenceHeader {
        profile,
        level,
        tier,
        high_bitdepth,
        twelve_bit,
        monochrome,
        subsampling_x: ss_x,
        subsampling_y: ss_y,
        chroma_sample_position: csp,
        width,
        height,
        full_range,
        color,
    })
}

/// `av1C` box contents (AV1CodecConfigurationRecord) for a sequence header
/// OBU (the whole OBU, as returned in [`TemporalUnit::sequence_header`]).
pub fn av1c(sequence_header_obu: &[u8]) -> Option<(Vec<u8>, SequenceHeader)> {
    let o = *obus(sequence_header_obu)?.first()?;
    if o.kind != OBU_SEQUENCE_HEADER {
        return None;
    }
    let s = parse_sequence_header(o.payload)?;
    let mut r = vec![
        0x81, // marker + version 1
        (s.profile << 5) | (s.level & 0x1f),
        (s.tier << 7)
            | (s.high_bitdepth as u8) << 6
            | (s.twelve_bit as u8) << 5
            | (s.monochrome as u8) << 4
            | (s.subsampling_x as u8) << 3
            | (s.subsampling_y as u8) << 2
            | (s.chroma_sample_position & 3),
        0, // no initial_presentation_delay
    ];
    r.extend_from_slice(o.bytes); // configOBUs
    Some((r, s))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920x1080, 8-bit 4:2:0, profile 0, level 4.0 (8) sequence header as
    /// encoders typically write it (no timing info), built bit by bit.
    fn seq_header_1080p() -> Vec<u8> {
        let mut bits = String::new();
        let mut put = |v: u32, n: u32| {
            for i in (0..n).rev() {
                bits.push(if v >> i & 1 == 1 { '1' } else { '0' });
            }
        };
        put(0, 3); // profile
        put(0, 1); // still
        put(0, 1); // reduced
        put(0, 1); // timing_info_present
        put(0, 1); // initial_display_delay_present
        put(0, 5); // operating_points_cnt_minus_1
        put(0, 12); // idc
        put(8, 5); // level 4.0
        put(0, 1); // tier (present because level > 7)
        put(15, 4); // frame_width_bits_minus_1 → 16
        put(15, 4);
        put(1919, 16);
        put(1079, 16);
        put(0, 1); // frame_id_numbers_present
        put(0b000, 3);
        put(0b0000, 4);
        put(1, 1); // enable_order_hint
        put(0b00, 2);
        put(1, 1); // seq_choose_screen_content_tools → 2
        put(1, 1); // seq_choose_integer_mv
        put(6, 3); // order_hint_bits_minus_1
        put(0b011, 3); // superres, cdef, restoration
        put(0, 1); // high_bitdepth
        put(0, 1); // mono
        put(1, 1); // color_description_present
        put(1, 8); // BT.709
        put(1, 8);
        put(1, 8);
        put(0, 1); // color_range
        put(0, 2); // chroma_sample_position
        put(0, 1); // separate_uv_delta_q
        put(0, 1); // film_grain
        put(1, 1); // trailing bit
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let payload: Vec<u8> =
            (0..bits.len() / 8).map(|i| u8::from_str_radix(&bits[i * 8..i * 8 + 8], 2).unwrap()).collect();
        let mut obu = vec![(OBU_SEQUENCE_HEADER << 3) | 0x02, payload.len() as u8];
        obu.extend(payload);
        obu
    }

    #[test]
    fn sequence_header_and_av1c() {
        let seq = seq_header_1080p();
        let (rec, s) = av1c(&seq).unwrap();
        assert_eq!((s.width, s.height, s.profile, s.level, s.tier), (1920, 1080, 0, 8, 0));
        assert!(s.subsampling_x && s.subsampling_y && !s.high_bitdepth && !s.full_range);
        assert_eq!(s.color, Some((1, 1, 1)));
        assert_eq!(&rec[..4], &[0x81, 0x08, 0x0c, 0x00]);
        assert_eq!(&rec[4..], &seq[..]);
    }

    #[test]
    fn split_temporal_unit() {
        let seq = seq_header_1080p();
        let mut tu = vec![(OBU_TEMPORAL_DELIMITER << 3) | 0x02, 0];
        tu.extend_from_slice(&seq);
        tu.extend_from_slice(&[(6 << 3) | 0x02, 3, 0xaa, 0xbb, 0xcc]); // a FRAME OBU
        let u = split(&tu).unwrap();
        assert_eq!(u.sequence_header, Some(&seq[..]));
        assert_eq!(u.sample.len(), seq.len() + 5);
        assert!(split(&[0x12, 0x05, 1]).is_none(), "truncated OBU");
    }
}
