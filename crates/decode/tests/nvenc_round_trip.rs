//! The vendor's encoder on a D3D11 device, fed the synthetic source, and every
//! access unit it writes decoded by the library's own D3D11 decoder: each
//! picture must be the frame it was made from, in both codecs and at ten
//! bits. Needs the vendor's GPU and driver, so off by default: `cargo test -p
//! lowlat-decode --test nvenc_round_trip -- --ignored --nocapture`.

// The encoder is bound to a D3D11 device on Windows alone.
#![cfg(windows)]
#![allow(clippy::cast_possible_truncation)]

use std::sync::Arc;

use lowlat_capture::Frame;
use lowlat_capture::synthetic::Synthetic;
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_decode::d3d11::Backend;
use lowlat_decode::{Decoder, Format, Planes};
use lowlat_drivers::d3d11::D3d11;
use lowlat_encode::{Poll, nvenc};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
/// Long enough for drift to show: a reference chain that carried one wrong
/// picture carries it on.
const PICTURES: usize = 60;
/// The vendor whose encoder this is.
const NVIDIA: u32 = 0x10de;

fn header(codec: Codec, ten_bit: bool) -> VideoHeader {
    VideoHeader {
        frame_id: 1,
        width: 0,
        height: 0,
        codec,
        rotation: Rotation::None,
        ten_bit,
        locked: false,
        announced: false,
        metadata: false,
    }
}

/// The ratio of the largest sample to the error between two runs of eight-bit
/// samples, in decibels; infinite where they are equal.
fn psnr(got: &[u8], want: &[u8]) -> f64 {
    let squared: u64 = got
        .iter()
        .zip(want)
        .map(|(&g, &w)| (i64::from(g) - i64::from(w)).unsigned_abs().pow(2))
        .sum();
    if squared == 0 {
        return f64::INFINITY;
    }
    let mean = squared as f64 / got.len().min(want.len()).max(1) as f64;
    10.0 * (255.0 * 255.0 / mean).log10()
}

/// Luma and chroma of a picture read back, against the frame it was made from.
/// A ten-bit sample is compared by its top eight bits: the high byte of its
/// word, since the value sits high in the word and the source was eight bits
/// widened.
fn compare(want: &Frame<'_>, y: &[u8], uv: &[u8], pitch: usize, ten_bit: bool) -> (f64, f64) {
    let width = want.width as usize;
    let rows = want.height as usize;
    let at = |plane: &[u8], row: usize, column: usize| {
        if ten_bit {
            plane[row * pitch + column * 2 + 1]
        } else {
            plane[row * pitch + column]
        }
    };
    let mut got = Vec::with_capacity(width * rows);
    let mut wanted = Vec::with_capacity(width * rows);
    for row in 0..rows {
        for column in 0..width {
            got.push(at(y, row, column));
            wanted.push(want.luma.bytes[row * want.luma.stride + column]);
        }
    }
    let luma = psnr(&got, &wanted);
    got.clear();
    wanted.clear();
    // Half the rows, and on each the two components interleaved across the
    // picture's width.
    for row in 0..rows.div_ceil(2) {
        for column in 0..width {
            got.push(at(uv, row, column));
            wanted.push(want.chroma.bytes[row * want.chroma.stride + column]);
        }
    }
    (luma, psnr(&got, &wanted))
}

/// Encode the generator's pictures, decode each access unit as it comes, and
/// answer the worst luma and chroma either saw. `thorough` is the effort the
/// highest quality setting asks for: the most thorough preset, two passes.
fn round_trip(codec: Codec, ten_bit: bool, thorough: bool) -> (f64, f64) {
    let d3d11 = D3d11::load().expect("the system's libraries");
    let adapter = d3d11
        .adapters()
        .expect("the walk")
        .into_iter()
        .find(|a| a.decodes_here() && a.vendor == NVIDIA)
        .expect("an adapter of the encoder's vendor");
    let api = nvenc::Api::load().expect("encoder runtime");
    let session = api
        .open_session(Arc::new(d3d11.open(adapter.luid).expect("a device")))
        .expect("session");
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
            bitrate_bps: 20_000_000,
            min_qp: lowlat_encode::DEFAULT_MIN_QP,
            thorough,
            chroma: nvenc::Chroma::Yuv420,
            depth: if ten_bit {
                nvenc::Depth::Ten
            } else {
                nvenc::Depth::Eight
            },
        })
        .expect("initialize");
    // A device of the decoder's own, as a client's is.
    let decoding = d3d11.open(adapter.luid).expect("a second device");
    let mut decoder = Backend::new(&decoding, (4096, 4096));
    decoder.build(&header(codec, ten_bit)).expect("build");

    let mut source = Synthetic::new(WIDTH, HEIGHT);
    // The same pictures again, one behind the decoder at a time: the
    // generator's content is a function of the index alone.
    let mut reference = Synthetic::new(WIDTH, HEIGHT);
    // An odd pitch, so a pitch mistake shows.
    let pitch = WIDTH as usize * 2 + 64;
    let mut y = vec![0u8; pitch * HEIGHT as usize];
    let mut uv = vec![0u8; pitch * HEIGHT as usize];
    let mut v = vec![0u8; pitch * HEIGHT as usize];
    let (mut worst_luma, mut worst_chroma) = (f64::INFINITY, f64::INFINITY);
    let mut decoded = 0usize;
    for index in 0..PICTURES {
        encoder
            .submit(&source.acquire(), index == 0)
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
            assert_eq!(
                picture.format,
                if ten_bit { Format::P010 } else { Format::Nv12 }
            );
            let (luma, chroma) = compare(&reference.acquire(), &y, &uv, pitch, ten_bit);
            worst_luma = worst_luma.min(luma);
            worst_chroma = worst_chroma.min(chroma);
            decoded += 1;
        }
    }
    decoder.destroy();
    assert_eq!(decoded, PICTURES, "pictures went missing on the way");
    (worst_luma, worst_chroma)
}

fn check(name: &str, codec: Codec, ten_bit: bool, thorough: bool) {
    let (luma, chroma) = round_trip(codec, ten_bit, thorough);
    println!("{name}: {PICTURES} pictures, worst luma {luma:.1} dB, worst chroma {chroma:.1} dB");
    // **Far below what a right picture scores and far above a wrong one.** A
    // picture compared with its neighbour has the bar somewhere else, and a
    // lost colour plane leaves the marker grey: either is well under 30.
    assert!(
        luma > 40.0,
        "{name}: luma at {luma:.1} dB is not the source"
    );
    assert!(
        chroma > 40.0,
        "{name}: chroma at {chroma:.1} dB is not the source"
    );
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn h264_comes_back_as_it_went_in() {
    check("h264", Codec::H264, false, false);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn hevc_comes_back_as_it_went_in() {
    check("hevc", Codec::H265, false, false);
}

#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn ten_bit_hevc_comes_back_as_it_went_in() {
    check("hevc10", Codec::H265, true, false);
}

/// The highest quality setting's encoder: the most thorough preset and a
/// second pass, built and carrying the picture as the default's does.
#[test]
#[ignore = "requires the vendor's GPU and driver"]
fn a_thorough_encoder_comes_back_as_it_went_in() {
    check("hevc thorough", Codec::H265, false, true);
}
