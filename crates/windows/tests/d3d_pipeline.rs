//! Direct3D 11 parts of the pipeline that run on any adapter, including the
//! software one on CI runners (WARP / "Microsoft Basic Render Driver"):
//! HLSL compilation, the RGB → NV12 conversion shader, the NV12 → RGB present
//! shader, and Desktop Duplication of the first screen.

#![cfg(target_os = "windows")]

use sard_windows::com::Interface;
use sard_windows::d3d::{self, Device};
use sard_windows::gpu::{Nv12Target, Passes};
use std::time::Duration;

/// A device on the first adapter, hardware or not.
fn device() -> Option<Device> {
    let f = d3d::factory().ok()?;
    let list = d3d::adapters(&f);
    let a = list.iter().find(|a| !a.software).or(list.first())?;
    eprintln!("adapter: {} ({})", a.name, a.vendor_name());
    match Device::new(Some(&a.adapter)) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn read_back(dev: &Device, tex: &d3d::ID3D11Texture2D, w: u32, h: u32) -> Vec<u8> {
    let st = dev
        .texture(
            &d3d::D3D11_TEXTURE2D_DESC::new(
                w,
                h,
                d3d::DXGI_FORMAT_B8G8R8A8_UNORM,
                d3d::D3D11_USAGE_STAGING,
                0,
                d3d::D3D11_CPU_ACCESS_READ,
            ),
            None,
        )
        .unwrap();
    let mut out = vec![0u8; (w * h * 4) as usize];
    // SAFETY: same-size staging copy, mapped while read.
    unsafe {
        dev.context.CopyResource(st.as_raw(), tex.as_raw());
        let mut m = d3d::D3D11_MAPPED_SUBRESOURCE::default();
        assert!(dev.context.Map(st.as_raw(), 0, d3d::D3D11_MAP_READ, 0, &mut m) >= 0);
        for row in 0..h as usize {
            let src = (m.pData as *const u8).add(row * m.RowPitch as usize);
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(row * w as usize * 4), w as usize * 4);
        }
        dev.context.Unmap(st.as_raw(), 0);
    }
    out
}

/// Colour bars (BGRA): white, red, green, blue, black, grey, 32 px each.
const BARS: [[u8; 3]; 6] = [[255, 255, 255], [255, 0, 0], [0, 255, 0], [0, 0, 255], [0, 0, 0], [128, 128, 128]];

#[test]
fn rgb_nv12_rgb_round_trip() {
    let Some(dev) = device() else { return };
    let passes = Passes::new(&dev).expect("HLSL shaders compile");
    if !dev.can_render_to(d3d::DXGI_FORMAT_NV12) {
        eprintln!("skipping: this adapter can't render to NV12");
        return;
    }
    let (w, h) = (192u32, 64u32);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let [r, g, b] = BARS[x / 32];
            bgra[(y * w as usize + x) * 4..][..4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    let init = d3d::D3D11_SUBRESOURCE_DATA { pSysMem: bgra.as_ptr().cast(), SysMemPitch: w * 4, SysMemSlicePitch: 0 };
    let src = dev
        .texture(
            &d3d::D3D11_TEXTURE2D_DESC::new(
                w,
                h,
                d3d::DXGI_FORMAT_B8G8R8A8_UNORM,
                d3d::D3D11_USAGE_DEFAULT,
                d3d::D3D11_BIND_SHADER_RESOURCE,
                0,
            ),
            Some(&init),
        )
        .unwrap();
    let src_srv = dev.srv(&src, d3d::DXGI_FORMAT_B8G8R8A8_UNORM).unwrap();
    let nv12 = Nv12Target::new(&dev, w, h).expect("NV12 target");
    passes.rgb_to_nv12(&dev, &src_srv, &nv12);

    // The NV12 planes: BT.709 limited range.
    let (y, uv, pitch) = sard_windows::decode::read_nv12(&dev, &nv12.texture, w as usize, h as usize).unwrap();
    let expect = |rgb: [u8; 3]| {
        let (r, g, b) = (rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0);
        let yy = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        (16.0 + 219.0 * yy, 128.0 + 224.0 * (b - yy) / 1.8556, 128.0 + 224.0 * (r - yy) / 1.5748)
    };
    for (i, bar) in BARS.iter().enumerate() {
        let x = i * 32 + 16;
        let (ey, eu, ev) = expect(*bar);
        let (gy, gu, gv) =
            (y[8 * pitch + x] as f32, uv[4 * pitch + (x / 2) * 2] as f32, uv[4 * pitch + (x / 2) * 2 + 1] as f32);
        eprintln!("bar {i} {bar:?}: Y {gy} (want {ey:.1}) U {gu} ({eu:.1}) V {gv} ({ev:.1})");
        assert!((gy - ey).abs() <= 2.0 && (gu - eu).abs() <= 2.0 && (gv - ev).abs() <= 2.0, "bar {i}");
    }

    // And back to RGB with the viewer's shader, through R8 / R8G8 views of the NV12 texture.
    let y_srv = dev.srv(&nv12.texture, d3d::DXGI_FORMAT_R8_UNORM).unwrap();
    let c_srv = dev.srv(&nv12.texture, d3d::DXGI_FORMAT_R8G8_UNORM).unwrap();
    let out = dev
        .texture(
            &d3d::D3D11_TEXTURE2D_DESC::new(
                w,
                h,
                d3d::DXGI_FORMAT_B8G8R8A8_UNORM,
                d3d::D3D11_USAGE_DEFAULT,
                d3d::D3D11_BIND_RENDER_TARGET,
                0,
            ),
            None,
        )
        .unwrap();
    let rtv = dev.rtv(&out, d3d::DXGI_FORMAT_B8G8R8A8_UNORM).unwrap();
    let vp = d3d::viewport(0.0, 0.0, w as f32, h as f32);
    passes.present(&dev, &rtv, &vp, &[y_srv.as_raw(), c_srv.as_raw()], false, false).unwrap();
    let got = read_back(&dev, &out, w, h);
    for (i, bar) in BARS.iter().enumerate() {
        let p = &got[((32 * w as usize) + i * 32 + 16) * 4..][..4];
        let rgb = [p[2], p[1], p[0]];
        eprintln!("bar {i}: {bar:?} -> {rgb:?}");
        for c in 0..3 {
            assert!((rgb[c] as i32 - bar[c] as i32).abs() <= 4, "bar {i} channel {c}: {rgb:?} vs {bar:?}");
        }
    }
}

#[test]
fn desktop_duplication() {
    let screens = match sard_windows::capture::screens() {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            eprintln!("skipping: no screens");
            return;
        }
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let s = &screens[0];
    eprintln!("screen: {} {}x{} on {}", s.name, s.width(), s.height(), s.adapter.name);
    let dev = Device::new(Some(&s.adapter.adapter)).expect("device on the screen's adapter");
    let dupl = match sard_windows::capture::duplicate(&dev, &s.output) {
        Ok(d) => d,
        Err(e) => {
            // E.g. a service session without a desktop.
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut info = d3d::DXGI_OUTDUPL_FRAME_INFO::default();
    let mut res = std::ptr::null_mut();
    // SAFETY: valid out pointers; the frame is released below.
    let hr = unsafe { dupl.AcquireNextFrame(Duration::from_secs(2).as_millis() as u32, &mut info, &mut res) };
    eprintln!(
        "AcquireNextFrame: {hr:#010x}, present time {}, frames {}, pointer visible {}",
        info.LastPresentTime, info.AccumulatedFrames, info.PointerPosition.Visible
    );
    assert!(hr >= 0 || hr == d3d::DXGI_ERROR_WAIT_TIMEOUT, "AcquireNextFrame failed: {hr:#010x}");
    if hr >= 0 {
        // SAFETY: owned reference to the desktop image.
        let c = unsafe { sard_windows::com::Com::from_raw(res) }.expect("desktop resource");
        let tex: d3d::ID3D11Texture2D = c.query().expect("desktop image is a texture");
        let mut desc = d3d::D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out pointer.
        unsafe { tex.GetDesc(&mut desc) };
        eprintln!("desktop image {}x{} format {}", desc.Width, desc.Height, desc.Format);
        assert_eq!((desc.Width, desc.Height), (s.width(), s.height()));
        drop((tex, c));
        // SAFETY: matching release.
        unsafe { dupl.ReleaseFrame() };
    }
}
