//! PNG encoding of the 240x320 ST7789 panel for screenshots, and decoding of screenshot goldens.
//!
//! RGB565 expands to RGB888 by bit replication (`r8 = r5 << 3 | r5 >> 2`) so full intensity is
//! 255, not 248. Scaling is integer nearest-neighbour, because interpolation would invent colours
//! a matcher could match on.

use std::fmt;

pub const RGB888_BYTES: usize = 3;

/// Panel width in pixels (ST7789P3, 240x320).
pub const PANEL_WIDTH: u32 = 240;

pub const PANEL_HEIGHT: u32 = 320;

/// Expands one RGB565 pixel to RGB888: the command API's expansion, so a PNG artifact and the
/// hash `screenshot` reports describe the same picture.
pub use pemu_api::commands::screenshot::rgb565_to_rgb888;

pub fn expand(pixels: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * RGB888_BYTES);
    for &pixel in pixels {
        out.extend_from_slice(&rgb565_to_rgb888(pixel));
    }
    out
}

/// Repeats every pixel `scale` times in each direction. `scale` 0 and 1 return the buffer as is.
pub fn scale(rgb888: &[u8], width: u32, height: u32, scale: u32) -> Vec<u8> {
    if scale <= 1 {
        return rgb888.to_vec();
    }
    let width = width as usize;
    let height = height as usize;
    let factor = scale as usize;
    let mut out = Vec::with_capacity(rgb888.len() * factor * factor);
    for y in 0..height {
        let row = &rgb888[y * width * RGB888_BYTES..(y + 1) * width * RGB888_BYTES];
        let mut wide = Vec::with_capacity(row.len() * factor);
        for x in 0..width {
            let pixel = &row[x * RGB888_BYTES..(x + 1) * RGB888_BYTES];
            for _ in 0..factor {
                wide.extend_from_slice(pixel);
            }
        }
        for _ in 0..factor {
            out.extend_from_slice(&wide);
        }
    }
    out
}

#[derive(Debug)]
pub enum PngError {
    WrongSize {
        got: usize,
        /// Pixels `width * height` asks for.
        want: usize,
    },
    Encode(png::EncodingError),
}

impl fmt::Display for PngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PngError::WrongSize { got, want } => {
                write!(
                    f,
                    "the frame holds {got} pixels, not the {want} its size asks for"
                )
            }
            PngError::Encode(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PngError {}

impl From<png::EncodingError> for PngError {
    fn from(e: png::EncodingError) -> PngError {
        PngError::Encode(e)
    }
}

/// Largest image [`decode_rgb888`] accepts, in pixels: a 4x scaled panel, so a hostile PNG header
/// cannot ask for gigabytes.
pub const MAX_DECODE_PIXELS: u64 = (PANEL_WIDTH as u64 * 4) * (PANEL_HEIGHT as u64 * 4);

/// Decodes a PNG into `(width, height, RGB888 bytes)`, the golden side of `screenshot --compare`.
/// 8-bit RGBA and grey are converted so a golden saved by an image editor still compares; other
/// layouts are an error whose text never carries a path.
pub fn decode_rgb888(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("not a PNG: {e}"))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS {
        return Err(format!("{width}x{height} is larger than any screenshot"));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "the PNG does not fit in memory".to_owned())?;
    let mut buf = vec![0u8; size];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("the PNG does not decode: {e}"))?;
    let data = &buf[..frame.buffer_size()];
    let rgb = match frame.color_type {
        png::ColorType::Rgb => data.to_vec(),
        png::ColorType::Rgba => data
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0]])
            .collect(),
        other => return Err(format!("a {other:?} PNG is not an RGB image")),
    };
    Ok((width, height, rgb))
}

pub fn encode_rgb888(width: u32, height: u32, rgb888: &[u8]) -> Result<Vec<u8>, PngError> {
    let want = width as usize * height as usize;
    let got = rgb888.len() / RGB888_BYTES;
    if got != want || !rgb888.len().is_multiple_of(RGB888_BYTES) {
        return Err(PngError::WrongSize { got, want });
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgb888)?;
        writer.finish()?;
    }
    Ok(out)
}

/// Encodes an RGB565 frame as a PNG, optionally scaled.
pub fn encode_rgb565(
    width: u32,
    height: u32,
    pixels: &[u16],
    factor: u32,
) -> Result<Vec<u8>, PngError> {
    let want = width as usize * height as usize;
    if pixels.len() != want {
        return Err(PngError::WrongSize {
            got: pixels.len(),
            want,
        });
    }
    let rgb888 = expand(pixels);
    if factor <= 1 {
        return encode_rgb888(width, height, &rgb888);
    }
    let scaled = scale(&rgb888, width, height, factor);
    encode_rgb888(width * factor, height * factor, &scaled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_endpoints_expand_exactly() {
        assert_eq!(rgb565_to_rgb888(0xffff), [0xff, 0xff, 0xff]);
        assert_eq!(rgb565_to_rgb888(0x0000), [0x00, 0x00, 0x00]);
        assert_eq!(rgb565_to_rgb888(0xf800), [0xff, 0x00, 0x00]);
        assert_eq!(rgb565_to_rgb888(0x07e0), [0x00, 0xff, 0x00]);
        assert_eq!(rgb565_to_rgb888(0x001f), [0x00, 0x00, 0xff]);
    }

    #[test]
    fn the_expansion_is_monotonic_in_every_channel() {
        let mut previous = 0u8;
        for r5 in 0..32u16 {
            let [r, _, _] = rgb565_to_rgb888(r5 << 11);
            assert!(r >= previous, "red must not go backwards at {r5}");
            previous = r;
        }
        assert_eq!(previous, 0xff);
        let mut previous = 0u8;
        for g6 in 0..64u16 {
            let [_, g, _] = rgb565_to_rgb888(g6 << 5);
            assert!(g >= previous, "green must not go backwards at {g6}");
            previous = g;
        }
        assert_eq!(previous, 0xff);
    }

    #[test]
    fn a_panel_frame_encodes_to_a_png_of_the_panel_size() {
        let pixels = vec![0x07e0u16; (PANEL_WIDTH * PANEL_HEIGHT) as usize];
        let png = encode_rgb565(PANEL_WIDTH, PANEL_HEIGHT, &pixels, 1).expect("encode");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "the PNG signature");
        // The IHDR payload begins at byte 16: width, height, depth, colour type.
        assert_eq!(&png[16..20], &PANEL_WIDTH.to_be_bytes());
        assert_eq!(&png[20..24], &PANEL_HEIGHT.to_be_bytes());
        assert_eq!(png[24], 8, "8 bits per channel");
        assert_eq!(png[25], 2, "colour type 2 is RGB");
    }

    #[test]
    fn scaling_repeats_whole_pixels_and_invents_no_colour() {
        let pixels = [0xf800u16, 0x001f];
        let png = encode_rgb565(2, 1, &pixels, 2).expect("encode");
        assert_eq!(&png[16..20], &4u32.to_be_bytes());
        assert_eq!(&png[20..24], &2u32.to_be_bytes());

        let rgb888 = expand(&pixels);
        let scaled = scale(&rgb888, 2, 1, 2);
        assert_eq!(scaled.len(), 4 * 2 * RGB888_BYTES);
        let colours: std::collections::BTreeSet<&[u8]> = scaled.chunks(RGB888_BYTES).collect();
        assert_eq!(
            colours.len(),
            2,
            "nearest neighbour must not blend the two colours"
        );
        assert_eq!(&scaled[..4 * RGB888_BYTES], &scaled[4 * RGB888_BYTES..]);
    }

    #[test]
    fn scale_one_and_scale_zero_leave_the_frame_alone() {
        let rgb888 = expand(&[0x1234u16, 0x5678]);
        assert_eq!(scale(&rgb888, 2, 1, 1), rgb888);
        assert_eq!(scale(&rgb888, 2, 1, 0), rgb888);
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_refused_rather_than_encoded() {
        let err = encode_rgb565(PANEL_WIDTH, PANEL_HEIGHT, &[0u16; 4], 1).expect_err("refused");
        match err {
            PngError::WrongSize { got, want } => {
                assert_eq!(got, 4);
                assert_eq!(want, (PANEL_WIDTH * PANEL_HEIGHT) as usize);
            }
            other => panic!("expected a size refusal, got {other}"),
        }
        assert!(
            encode_rgb888(2, 2, &[0; 11]).is_err(),
            "a partial pixel is refused"
        );
    }

    #[test]
    fn the_same_frame_encodes_to_the_same_bytes() {
        // A screenshot golden compares bytes, so the encoder must be deterministic.
        let pixels: Vec<u16> = (0..64u16).map(|i| i.wrapping_mul(1031)).collect();
        let a = encode_rgb565(8, 8, &pixels, 1).expect("encode");
        let b = encode_rgb565(8, 8, &pixels, 1).expect("encode");
        assert_eq!(a, b);
    }
    #[test]
    fn a_png_decodes_back_to_its_rgb888_bytes() {
        let rgb: Vec<u8> = (0..(3 * 2 * 3)).map(|i| (i * 13) as u8).collect();
        let png = encode_rgb888(3, 2, &rgb).expect("encodes");
        assert_eq!(decode_rgb888(&png), Ok((3, 2, rgb.clone())));

        let mut rgba = Vec::new();
        {
            let mut encoder = ::png::Encoder::new(&mut rgba, 3, 2);
            encoder.set_color(::png::ColorType::Rgba);
            encoder.set_depth(::png::BitDepth::Eight);
            let data: Vec<u8> = rgb
                .chunks_exact(3)
                .flat_map(|p| [p[0], p[1], p[2], 0x80])
                .collect();
            let mut writer = encoder.write_header().expect("header");
            writer.write_image_data(&data).expect("data");
        }
        assert_eq!(decode_rgb888(&rgba), Ok((3, 2, rgb)));
        assert!(decode_rgb888(b"not a png").is_err());
    }
}
