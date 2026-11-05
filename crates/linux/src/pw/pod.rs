//! SPA POD building and parsing (the parameter format PipeWire negotiates
//! with). The C builder is all inline functions, so it's reimplemented here;
//! the format is simple: `[u32 size][u32 type][body][pad to 8]`.

use super::layout as L;

/// An 8-byte aligned POD buffer, ready to pass as `const struct spa_pod *`.
pub struct Pod(Vec<u64>);

impl Pod {
    pub fn from_bytes(b: &[u8]) -> Self {
        let mut v = vec![0u64; b.len().div_ceil(8)];
        // SAFETY: v has at least b.len() bytes.
        unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), v.as_mut_ptr().cast(), b.len()) };
        Pod(v)
    }
    pub fn as_ptr(&self) -> *const std::ffi::c_void {
        self.0.as_ptr().cast()
    }
}

fn pad8(v: &mut Vec<u8>) {
    while !v.len().is_multiple_of(8) {
        v.push(0);
    }
}

pub fn pod(ty: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len() + 8);
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend_from_slice(&ty.to_le_bytes());
    v.extend_from_slice(body);
    pad8(&mut v);
    v
}

pub fn id(x: u32) -> Vec<u8> {
    pod(L::T_ID, &x.to_le_bytes())
}

pub fn int(x: i32) -> Vec<u8> {
    pod(L::T_INT, &x.to_le_bytes())
}

pub fn rect(w: u32, h: u32) -> Vec<u8> {
    let mut b = w.to_le_bytes().to_vec();
    b.extend_from_slice(&h.to_le_bytes());
    pod(L::T_RECTANGLE, &b)
}

pub fn frac(n: u32, d: u32) -> Vec<u8> {
    let mut b = n.to_le_bytes().to_vec();
    b.extend_from_slice(&d.to_le_bytes());
    pod(L::T_FRACTION, &b)
}

/// Choice pod: `kind` (range/enum/flags), child type and raw element bytes.
pub fn choice(kind: u32, child_ty: u32, elems: &[Vec<u8>]) -> Vec<u8> {
    let size = elems.first().map_or(0, |e| e.len()) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(&kind.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&child_ty.to_le_bytes());
    for e in elems {
        b.extend_from_slice(e);
    }
    pod(L::T_CHOICE, &b)
}

pub fn u32s(x: u32) -> Vec<u8> {
    x.to_le_bytes().to_vec()
}

pub fn i32s(x: i32) -> Vec<u8> {
    x.to_le_bytes().to_vec()
}

pub fn i64s(x: i64) -> Vec<u8> {
    x.to_le_bytes().to_vec()
}

pub fn pair(a: u32, b: u32) -> Vec<u8> {
    let mut v = a.to_le_bytes().to_vec();
    v.extend_from_slice(&b.to_le_bytes());
    v
}

pub fn prop(key: u32, flags: u32, value: Vec<u8>) -> Vec<u8> {
    let mut v = key.to_le_bytes().to_vec();
    v.extend_from_slice(&flags.to_le_bytes());
    v.extend_from_slice(&value);
    v
}

pub fn object(obj_type: u32, id: u32, props: &[Vec<u8>]) -> Vec<u8> {
    let mut b = obj_type.to_le_bytes().to_vec();
    b.extend_from_slice(&id.to_le_bytes());
    for p in props {
        b.extend_from_slice(p);
    }
    pod(L::T_OBJECT, &b)
}

/// A parsed property value: `(type, body)`; choices resolve to their default.
#[derive(Debug, Clone)]
pub struct PropValue {
    pub ty: u32,
    pub body: Vec<u8>,
    /// All values when the property was a choice (enum/range).
    pub alternatives: Vec<Vec<u8>>,
}

impl PropValue {
    pub fn as_u32(&self) -> Option<u32> {
        self.body.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn as_i64(&self) -> Option<i64> {
        self.body.get(..8).map(|b| i64::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn as_pair(&self) -> Option<(u32, u32)> {
        let b = self.body.get(..8)?;
        Some((u32::from_le_bytes(b[..4].try_into().unwrap()), u32::from_le_bytes(b[4..8].try_into().unwrap())))
    }
}

/// Parse an Object pod into `(object type, id, [(key, value)])`.
///
/// # Safety
/// `p` must point to a valid spa_pod.
pub unsafe fn parse_object(p: *const std::ffi::c_void) -> Option<(u32, u32, Vec<(u32, PropValue)>)> {
    if p.is_null() {
        return None;
    }
    let hdr = unsafe { std::slice::from_raw_parts(p as *const u8, 8) };
    let size = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
    let ty = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
    if ty != L::T_OBJECT || size < 8 {
        return None;
    }
    let body = unsafe { std::slice::from_raw_parts((p as *const u8).add(8), size) };
    parse_object_body(body)
}

pub fn parse_object_body(body: &[u8]) -> Option<(u32, u32, Vec<(u32, PropValue)>)> {
    let rd = |o: usize| -> Option<u32> { body.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())) };
    let obj_type = rd(0)?;
    let obj_id = rd(4)?;
    let mut off = 8;
    let mut props = Vec::new();
    while off + 16 <= body.len() {
        let key = rd(off)?;
        let vsize = rd(off + 8)? as usize;
        let vty = rd(off + 12)?;
        let vbody = body.get(off + 16..off + 16 + vsize)?;
        let value = if vty == L::T_CHOICE && vsize >= 16 {
            let csize = u32::from_le_bytes(vbody[8..12].try_into().unwrap()) as usize;
            let cty = u32::from_le_bytes(vbody[12..16].try_into().unwrap());
            let elems: Vec<Vec<u8>> =
                if csize == 0 { Vec::new() } else { vbody[16..].chunks_exact(csize).map(<[u8]>::to_vec).collect() };
            PropValue { ty: cty, body: elems.first().cloned().unwrap_or_default(), alternatives: elems }
        } else {
            PropValue { ty: vty, body: vbody.to_vec(), alternatives: Vec::new() }
        };
        props.push((key, value));
        off += 16 + vsize.div_ceil(8) * 8;
    }
    Some((obj_type, obj_id, props))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse() {
        let o = object(
            L::OBJ_FORMAT,
            L::PARAM_ENUM_FORMAT,
            &[
                prop(L::FORMAT_MEDIA_TYPE, 0, id(L::MEDIA_TYPE_VIDEO)),
                prop(L::FORMAT_VIDEO_FORMAT, 0, choice(L::CHOICE_ENUM, L::T_ID, &[u32s(8), u32s(8), u32s(12)])),
                prop(L::FORMAT_VIDEO_SIZE, 0, rect(2560, 1440)),
                prop(
                    L::FORMAT_VIDEO_MODIFIER,
                    L::PROP_FLAG_MANDATORY,
                    choice(L::CHOICE_ENUM, L::T_LONG, &[i64s(5), i64s(5), i64s(0)]),
                ),
            ],
        );
        assert_eq!(o.len() % 8, 0);
        let pod = Pod::from_bytes(&o);
        let (ty, idv, props) = unsafe { parse_object(pod.as_ptr()) }.unwrap();
        assert_eq!((ty, idv), (L::OBJ_FORMAT, L::PARAM_ENUM_FORMAT));
        assert_eq!(props.len(), 4);
        assert_eq!(props[0].1.as_u32(), Some(L::MEDIA_TYPE_VIDEO));
        assert_eq!(props[1].1.as_u32(), Some(8));
        assert_eq!(props[1].1.alternatives.len(), 3);
        assert_eq!(props[2].1.as_pair(), Some((2560, 1440)));
        assert_eq!(props[3].1.as_i64(), Some(5));
    }
}
