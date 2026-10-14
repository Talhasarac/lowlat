//! The display's path on Windows without a display: a desktop-like picture,
//! eight-bit BGRA on the device, converted by the host's conversion into the
//! encoder's own inputs -- textures registered once -- encoded by the
//! vendor's encoder and decoded by the library's own D3D11 decoder, each
//! picture compared with the colour transform's definition. In every layout
//! the conversion writes, the packed full-chroma ones included, whose
//! pictures the encoder's registration alone cannot vouch for. Needs the
//! vendor's GPU and driver, so off by default: `cargo test -p lowlat-decode
//! --test conversion_round_trip -- --ignored --nocapture`.

// The encoder and the conversion are bound to a D3D11 device on Windows alone.
#![cfg(windows)]
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::sync::Arc;

use lowlat_capture::d3d11::{Converter, Layout, Target};
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::Backend;
use lowlat_decode::{Decoder, Format, Planes};
use lowlat_drivers::d3d11::{Com, D3d11, Device};
use lowlat_drivers::ffi::d3d11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC, ID3D11Resource, ID3D11Texture2D,
};
use lowlat_drivers::vcall;
use lowlat_encode::{Poll, nvenc};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
/// Long enough for drift to show: a reference chain that carried one wrong
/// picture carries it on.
const PICTURES: usize = 60;
/// The vendor whose encoder this is.
const NVIDIA: u32 = 0x10de;
/// Targets in rotation, one per picture the encoder may hold.
const TARGETS: usize = nvenc::IN_FLIGHT;

/// Saturated colours that tell one matrix from another.
const COLOURS: [[u8; 3]; 8] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 255, 255],
    [0, 0, 0],
];

/// Where, with `fine`, the colour changes every pixel across: red and blue
/// alternating, which only full chroma carries -- subsampled, each pair
/// averages to one purple -- as columns `x0..x1` of rows `y0..y1`.
const DETAIL: (usize, usize, usize, usize) = (256, 384, 128, 192);

fn in_detail(x: usize, y: usize) -> bool {
    (DETAIL.0..DETAIL.1).contains(&x) && (DETAIL.2..DETAIL.3).contains(&y)
}

/// Picture `index`'s colour at a pixel: blocks of one colour, which the
/// encoder carries nearly exactly, moving eight pixels a picture, so a
/// picture handed back out of order is not the one compared with it; and
/// every 2x2 block one colour, so subsampling has nothing to average outside
/// the detail.
fn colour(index: usize, x: usize, y: usize, fine: bool) -> [u8; 3] {
    if fine && in_detail(x, y) {
        return COLOURS[(x % 2) * 2];
    }
    let (column, row) = ((x + index * 8) / 64, y / 64);
    COLOURS[(column + row) % COLOURS.len()]
}

/// The colour transform's definition at eight and ten bits: luma, blue
/// difference, red difference.
fn yuv(colour: [u8; 3], ten_bit: bool) -> [u16; 3] {
    let [r, g, b] = colour.map(|c| f64::from(c) / 255.0);
    let (kr, kb) = (0.2126, 0.0722);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    let u = (b - y) / (2.0 - 2.0 * kb);
    let v = (r - y) / (2.0 - 2.0 * kr);
    let (top, ys, yo, cs, co) = if ten_bit {
        (1023.0, 876.0, 64.0, 896.0, 512.0)
    } else {
        (255.0, 219.0, 16.0, 224.0, 128.0)
    };
    let quantise = |value: f64| (value * top).round().clamp(0.0, top) as u16;
    [
        quantise(y * ys / top + yo / top),
        quantise(u * cs / top + co / top),
        quantise(v * cs / top + co / top),
    ]
}

/// The ratio of the largest sample to the error between two runs of samples
/// at a depth, in decibels; infinite where they are equal.
fn psnr(got: &[u16], want: &[u16], top: f64) -> f64 {
    let squared: u64 = got
        .iter()
        .zip(want)
        .map(|(&g, &w)| u64::from(g.abs_diff(w)).pow(2))
        .sum();
    if squared == 0 {
        return f64::INFINITY;
    }
    let mean = squared as f64 / got.len().min(want.len()).max(1) as f64;
    10.0 * (top * top / mean).log10()
}

/// Write picture `index` into the source texture, as the duplication would
/// have handed it over.
fn draw(device: &Device, source: &Com<ID3D11Texture2D>, index: usize, fine: bool) {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let mut pixels = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = colour(index, x, y, fine);
            pixels[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[b, g, r, 0xFF]);
        }
    }
    // SAFETY: the device's context on this thread, a live texture of this
    // size, and the pixels live for the call.
    unsafe {
        vcall!(
            device.context(),
            UpdateSubresource,
            source.as_ptr().cast::<ID3D11Resource>(),
            0,
            core::ptr::null(),
            pixels.as_ptr().cast(),
            WIDTH * 4,
            0
        )
    };
}

/// One sample of a plane the decoder handed out, at its depth: ten bits sit
/// in the high bits of a sixteen-bit word.
fn sample(plane: &[u8], pitch: usize, x: usize, y: usize, ten_bit: bool) -> u16 {
    if ten_bit {
        u16::from_le_bytes([plane[y * pitch + x * 2], plane[y * pitch + x * 2 + 1]]) >> 6
    } else {
        u16::from(plane[y * pitch + x])
    }
}

/// Convert, encode and decode `PICTURES` pictures in `layout`, and answer the
/// worst luma and chroma of any against the definition, and with full chroma
/// the worst chroma of the detail alone.
fn round_trip(codec: Codec, layout: Layout) -> (f64, f64, f64) {
    let ten_bit = matches!(layout, Layout::P010 | Layout::Y410);
    let full = layout.full_chroma();
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.vendor == NVIDIA)
        .expect("an adapter of the encoder's vendor");
    let device = Arc::new(d3d11.open(adapter.luid).expect("a device"));
    let source = device
        .texture(&D3D11_TEXTURE2D_DESC {
            Width: WIDTH,
            Height: HEIGHT,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        })
        .expect("a source texture");
    let mut converter = Converter::new(&device, layout).expect("the conversion");
    // **Made before the encoder, so they outlive it**: they are registered
    // with its session, which takes the registrations back as it goes.
    let targets: Vec<Target> = (0..TARGETS)
        .map(|_| Target::new(&device, layout, WIDTH, HEIGHT).expect("a target"))
        .collect();
    let api = nvenc::Api::load().expect("encoder runtime");
    let session = api.open_session(Arc::clone(&device)).expect("session");
    let mut encoder = session
        .initialize(nvenc::Config {
            codec: if codec == Codec::H264 {
                nvenc::Codec::H264
            } else {
                nvenc::Codec::H265
            },
            width: WIDTH,
            height: HEIGHT,
            fps: 60,
            bitrate_bps: 40_000_000,
            min_qp: lowlat_encode::DEFAULT_MIN_QP,
            thorough: false,
            chroma: if full {
                nvenc::Chroma::Yuv444
            } else {
                nvenc::Chroma::Yuv420
            },
            depth: if ten_bit {
                nvenc::Depth::Ten
            } else {
                nvenc::Depth::Eight
            },
        })
        .expect("initialize");
    let inputs: Vec<nvenc::Input> = targets
        .iter()
        .map(|target| {
            encoder
                .register_texture(target.texture())
                .expect("the target registered")
        })
        .collect();

    // A device of the decoder's own, as a client's is.
    let decoding = d3d11.open(adapter.luid).expect("a second device");
    let mut decoder = Backend::new(&decoding, (4096, 4096));
    decoder
        .build(&VideoHeader {
            frame_id: 1,
            width: 0,
            height: 0,
            codec,
            rotation: Rotation::None,
            ten_bit,
            locked: false,
            announced: false,
            metadata: false,
        })
        .expect("build");
    // An odd pitch, so a pitch mistake shows.
    let pitch = WIDTH as usize * 2 + 64;
    let mut y = vec![0u8; pitch * HEIGHT as usize];
    let mut uv = vec![0u8; pitch * HEIGHT as usize];
    let mut v = vec![0u8; pitch * HEIGHT as usize];
    let (mut worst_luma, mut worst_chroma) = (f64::INFINITY, f64::INFINITY);
    let mut worst_detail = f64::INFINITY;
    let top = if ten_bit { 1023.0 } else { 255.0 };
    let mut decoded = 0usize;
    for index in 0..PICTURES {
        draw(&device, &source, index, full);
        let slot = index % TARGETS;
        converter
            .convert(&device, source.as_ptr(), &targets[slot])
            .expect("convert");
        encoder
            .submit_registered(&inputs[slot], index == 0)
            .expect("submit");
        let unit = loop {
            match encoder.poll().expect("poll") {
                Poll::Ready { bitstream, .. } => break bitstream.to_vec(),
                Poll::Pending => std::hint::spin_loop(),
            }
        };
        decoder.feed(&unit).expect("feed");
        if index + 1 == PICTURES {
            decoder.drain();
        }
        loop {
            let mut planes = Planes {
                y: &mut y,
                y_pitch: pitch,
                uv: &mut uv,
                uv_pitch: pitch,
                v: &mut v,
                v_pitch: pitch,
            };
            let Some(picture) = decoder.take(&mut planes).expect("take") else {
                break;
            };
            assert_eq!((picture.width, picture.height), (WIDTH, HEIGHT));
            let want = match layout {
                Layout::Nv12 => Format::Nv12,
                Layout::P010 => Format::P010,
                Layout::Ayuv => Format::Yuv444,
                Layout::Y410 => Format::Yuv444_16,
            };
            assert_eq!(picture.format, want, "{layout:?}");
            // The picture this one is, in decode order: each is decoded in the
            // order it went in.
            let shown = decoded;
            let (mut got_luma, mut want_luma) = (Vec::new(), Vec::new());
            let (mut got_chroma, mut want_chroma) = (Vec::new(), Vec::new());
            let (mut got_detail, mut want_detail) = (Vec::new(), Vec::new());
            for row in 0..HEIGHT as usize {
                for column in 0..WIDTH as usize {
                    let [wy, wu, wv] = yuv(colour(shown, column, row, full), ten_bit);
                    got_luma.push(sample(&y, pitch, column, row, ten_bit));
                    want_luma.push(wy);
                    if full {
                        let (u, v) = (
                            sample(&uv, pitch, column, row, ten_bit),
                            sample(&v, pitch, column, row, ten_bit),
                        );
                        got_chroma.extend([u, v]);
                        want_chroma.extend([wu, wv]);
                        if in_detail(column, row) {
                            got_detail.extend([u, v]);
                            want_detail.extend([wu, wv]);
                        }
                    } else if row % 2 == 0 && column % 2 == 0 {
                        got_chroma.push(sample(&uv, pitch, column, row / 2, ten_bit));
                        want_chroma.push(wu);
                        got_chroma.push(sample(&uv, pitch, column + 1, row / 2, ten_bit));
                        want_chroma.push(wv);
                    }
                }
            }
            worst_luma = worst_luma.min(psnr(&got_luma, &want_luma, top));
            worst_chroma = worst_chroma.min(psnr(&got_chroma, &want_chroma, top));
            if full {
                worst_detail = worst_detail.min(psnr(&got_detail, &want_detail, top));
            }
            decoded += 1;
        }
    }
    decoder.destroy();
    assert_eq!(decoded, PICTURES, "pictures went missing on the way");
    (worst_luma, worst_chroma, worst_detail)
}

fn check(name: &str, codec: Codec, layout: Layout) {
    let (luma, chroma, detail) = round_trip(codec, layout);
    println!(
        "{name}: {PICTURES} pictures, worst luma {luma:.1} dB, worst chroma {chroma:.1} dB, \
         worst chroma of the detail {detail:.1} dB"
    );
    // **Far below what a right picture scores and far above a wrong one.** A
    // picture compared with its neighbour and a chroma plane read as another
    // layout both land well under it.
    assert!(
        luma > 40.0,
        "{name}: luma at {luma:.1} dB is not the source"
    );
    assert!(
        chroma > 40.0,
        "{name}: chroma at {chroma:.1} dB is not the source"
    );
    // **Full chroma is the detail coming back**: subsampled, each red and
    // blue pair comes back one purple, near 11 dB.
    if layout.full_chroma() {
        assert!(
            detail > 30.0,
            "{name}: the detail's chroma at {detail:.1} dB is not full chroma"
        );
    }
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn h264_from_the_conversion_comes_back_as_it_went_in() {
    check("h264 nv12", Codec::H264, Layout::Nv12);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn hevc_from_the_conversion_comes_back_as_it_went_in() {
    check("hevc nv12", Codec::H265, Layout::Nv12);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn ten_bit_hevc_from_the_conversion_comes_back_as_it_went_in() {
    check("hevc p010", Codec::H265, Layout::P010);
}

/// **Full chroma at eight bits, from the packed word**: colour that changes
/// every pixel comes back changing every pixel.
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn full_chroma_hevc_from_the_packed_word_comes_back_as_it_went_in() {
    check("hevc ayuv", Codec::H265, Layout::Ayuv);
}

/// **Full chroma at ten bits, from the packed ten-bit word**, which the
/// encoder registers under its planar name: whether it reads the word as the
/// texture's own format is what this says.
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn ten_bit_full_chroma_hevc_from_the_packed_word_comes_back_as_it_went_in() {
    check("hevc y410", Codec::H265, Layout::Y410);
}
