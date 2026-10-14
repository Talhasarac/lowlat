//! The colour transform computed on the processor from its definitions, which
//! every platform's conversion is checked against, so the platforms cannot
//! drift apart where nothing else compares them.

/// Saturated colours, each filling a whole 2x2 block so subsampling has
/// nothing to average and the answer is exact.
///
/// **Grey is deliberately not the whole list.** A grey pixel has equal
/// channels, so every luma matrix returns the same luma for it and it
/// carries no chroma at all; a test made of greys passes with the wrong
/// coefficients, which is exactly how the desktop comparison nearly fooled
/// us.
pub(crate) const PATTERN: [[u8; 3]; 8] = [
    [255, 0, 0],
    [0, 255, 0],
    [0, 0, 255],
    [255, 255, 0],
    [0, 255, 255],
    [255, 0, 255],
    [255, 255, 255],
    [0, 0, 0],
];

/// The colour transform, computed on the processor from the same rules the
/// shader implements, written from the definitions rather than transcribed
/// from the shader so a mistake cannot be shared by both.
pub(crate) fn reference(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (rf, gf, bf) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let kr = 0.2126_f64;
    let kb = 0.0722_f64;
    let kg = 1.0 - kr - kb;
    let y = kr * rf + kg * gf + kb * bf;
    let u = (bf - y) / (2.0 - 2.0 * kb);
    let v = (rf - y) / (2.0 - 2.0 * kr);
    // Walked rather than cast, because a cast here is exactly the lint the
    // data path forbids and this is eight comparisons in a test.
    let quantise = |value: f64| -> u8 {
        let scaled = (value * 255.0).round().clamp(0.0, 255.0);
        (0..=u8::MAX)
            .find(|candidate| f64::from(*candidate) >= scaled)
            .unwrap_or(u8::MAX)
    };
    (
        quantise(y * (219.0 / 255.0) + 16.0 / 255.0),
        quantise(u * (224.0 / 255.0) + 128.0 / 255.0),
        quantise(v * (224.0 / 255.0) + 128.0 / 255.0),
    )
}

/// The same transform at ten bits, and **not the eight-bit answer scaled**.
///
/// The range constants are different numbers rather than one set
/// multiplied out -- 235 of 255 is 0.92157 and 940 of 1023 is 0.91887 --
/// so a reference that scaled would agree with a conversion that made the
/// same mistake and prove nothing.
pub(crate) fn reference_ten(r: u8, g: u8, b: u8) -> (u16, u16, u16) {
    let (rf, gf, bf) = (
        f64::from(r) / 255.0,
        f64::from(g) / 255.0,
        f64::from(b) / 255.0,
    );
    let kr = 0.2126_f64;
    let kb = 0.0722_f64;
    let kg = 1.0 - kr - kb;
    let y = kr * rf + kg * gf + kb * bf;
    let u = (bf - y) / (2.0 - 2.0 * kb);
    let v = (rf - y) / (2.0 - 2.0 * kr);
    let quantise = |value: f64| -> u16 {
        let scaled = (value * 1023.0).round().clamp(0.0, 1023.0);
        (0..=1023u16)
            .find(|candidate| f64::from(*candidate) >= scaled)
            .unwrap_or(1023)
    };
    (
        quantise(y * (876.0 / 1023.0) + 64.0 / 1023.0),
        quantise(u * (896.0 / 1023.0) + 512.0 / 1023.0),
        quantise(v * (896.0 / 1023.0) + 512.0 / 1023.0),
    )
}
