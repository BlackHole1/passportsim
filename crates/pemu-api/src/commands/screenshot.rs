//! `passportsim screenshot`: a PNG of the `raw`, `glass` or `perceived` view, optionally compared
//! with an expected image.
//!
//! | View | What it is | What it explains |
//! |---|---|---|
//! | `raw` | the frame memory as the guest wrote it | what the firmware drew |
//! | `glass` | `raw` after the panel's own state: INVON/INVOFF, DISPON/DISPOFF and sleep-in | why a correct drawing is invisible |
//! | `perceived` | `glass` scaled by the LEDC backlight duty | what a person in the room sees |
//!
//! Every result carries the panel state, so an all-black image is explained. The pixels and compare
//! rules are pure and here; the codec and file are the host's, since `pemu-api` may not name `png`.
//! `sha256` depends on the encoder; `pixels_sha256` is over the RGB888 view and depends only on the
//! guest. `inline` defaults to false: pixels are never inlined unless asked.

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, OnceLock};

use pemu_core::hostio::FramePort;

use crate::artifact_io::ArtifactIo;
use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_SECRET_REFUSED, E_STATE, E_USAGE};
use crate::output::{ArtifactRef, Output};
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use crate::args::{instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::pool::Pool;
use crate::session::Session;

pub const RGB888_BYTES: usize = 3;
/// The resolved brightness on the 10-bit scale with the LEDC duty's four fractional bits
/// (`Brightness::frame_duty`), so full is `1024 << 4`.
pub const BACKLIGHT_MAX: u32 = 1024 << 4;
pub const SCREENS_DIR: &str = "screens";

crate::matchers::str_enum! {
    pub enum View {
        Raw = "raw",
        Glass = "glass",
        Perceived = "perceived",
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Panel {
    pub width: usize,
    pub height: usize,
    /// The rail on, out of sleep and in DISPON.
    pub display_on: bool,
    pub sleep_in: bool,
    /// The command state: a status fact, never a drawing rule.
    pub inverted: bool,
    /// `inverted != invon_shows_ram`. This, not [`Panel::inverted`], is what `glass` and
    /// `perceived` apply.
    pub glass_complement: bool,
    pub backlight_duty: u32,
    pub frame_gen: u64,
}

impl Panel {
    pub fn of(frame: &FramePort) -> Panel {
        Panel {
            width: frame.width(),
            height: frame.height(),
            display_on: frame.powered() && !frame.sleeping() && frame.display_on(),
            sleep_in: frame.sleeping(),
            inverted: frame.inverted(),
            glass_complement: frame.glass_complement(),
            backlight_duty: u32::from(frame.backlight()),
            frame_gen: frame.generation(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "display_on": self.display_on,
            "sleep_in": self.sleep_in,
            "inverted": self.inverted,
            "backlight_duty": self.backlight_duty,
        })
    }
}

/// `pemu_host::png` uses it too, so the artifact and this crate's hash describe the same picture.
#[must_use]
pub fn rgb565_to_rgb888(pixel: u16) -> [u8; RGB888_BYTES] {
    let r5 = ((pixel >> 11) & 0x1f) as u8;
    let g6 = ((pixel >> 5) & 0x3f) as u8;
    let b5 = (pixel & 0x1f) as u8;
    [
        (r5 << 3) | (r5 >> 2),
        (g6 << 2) | (g6 >> 4),
        (b5 << 3) | (b5 >> 2),
    ]
}

/// `raw` ignores the panel state: a menu drawn into a sleeping panel still shows in `raw`.
#[must_use]
pub fn view_pixels(pixels: &[u16], panel: &Panel, view: View) -> Vec<u8> {
    let dark = matches!(view, View::Glass | View::Perceived) && !panel.display_on;
    let mut out = Vec::with_capacity(pixels.len() * RGB888_BYTES);
    for &pixel in pixels {
        let pixel = if matches!(view, View::Glass | View::Perceived) && panel.glass_complement {
            !pixel
        } else {
            pixel
        };
        let mut rgb = rgb565_to_rgb888(pixel);
        if dark {
            rgb = [0, 0, 0];
        } else if view == View::Perceived {
            for channel in &mut rgb {
                // Integer only: a float multiply could make two hosts differ in the last bit of a
                // compared image.
                let duty = panel.backlight_duty.min(BACKLIGHT_MAX);
                *channel =
                    u8::try_from((u32::from(*channel) * duty) / BACKLIGHT_MAX).unwrap_or(*channel);
            }
        }
        out.extend_from_slice(&rgb);
    }
    out
}

/// A mismatch count and a diff path, never inline pixels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    pub equal: bool,
    pub diff_pixels: u32,
    /// `(x, y, w, h)`; zero-sized when equal.
    pub bbox: (u32, u32, u32, u32),
}

impl Diff {
    pub fn to_json(&self, diff_path: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "equal": self.equal,
            "diff_pixels": self.diff_pixels,
            "bbox": {
                "x": self.bbox.0, "y": self.bbox.1, "w": self.bbox.2, "h": self.bbox.3
            },
            "diff_path": diff_path,
        })
    }
}

/// A pixel differs when any channel differs by more than `tolerance`: a rounded colour moves every
/// channel a little, a wrong glyph moves a few pixels a lot. Different sizes are `E_USAGE`, not a
/// diff.
pub fn compare(
    actual: &[u8],
    expected: &[u8],
    width: u32,
    tolerance: u8,
) -> Result<Diff, ApiError> {
    if actual.len() != expected.len() {
        return Err(usage(
            "compare_with",
            &format!(
                "the expected image is {} bytes and the screenshot {}",
                expected.len(),
                actual.len()
            ),
        ));
    }
    let mut diff = Diff {
        equal: true,
        ..Diff::default()
    };
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (index, (a, e)) in actual
        .chunks_exact(RGB888_BYTES)
        .zip(expected.chunks_exact(RGB888_BYTES))
        .enumerate()
    {
        let differs = a
            .iter()
            .zip(e.iter())
            .any(|(a, e)| a.abs_diff(*e) > tolerance);
        if !differs {
            continue;
        }
        diff.equal = false;
        diff.diff_pixels += 1;
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let (x, y) = (index % width.max(1), index / width.max(1));
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    if !diff.equal {
        diff.bbox = (x0, y0, x1 - x0 + 1, y1 - y0 + 1);
    }
    Ok(diff)
}

/// The expected image darkened, with differing pixels in magenta, which no test pattern here uses.
#[must_use]
pub fn diff_image(actual: &[u8], expected: &[u8], tolerance: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(actual.len());
    for (a, e) in actual
        .chunks_exact(RGB888_BYTES)
        .zip(expected.chunks_exact(RGB888_BYTES))
    {
        let differs = a
            .iter()
            .zip(e.iter())
            .any(|(a, e)| a.abs_diff(*e) > tolerance);
        if differs {
            out.extend_from_slice(&[0xFF, 0x00, 0xFF]);
        } else {
            out.extend(e.iter().map(|channel| channel / 3));
        }
    }
    out
}

pub type Encode = fn(u32, u32, &[u8]) -> Result<Vec<u8>, String>;

pub type Decode = fn(&[u8]) -> Result<(u32, u32, Vec<u8>), String>;

/// The host installs these over `pemu_host::png`; the file goes through [`crate::artifact_io`].
#[derive(Copy, Clone)]
pub struct ScreenshotCodec {
    pub encode: Encode,
    pub decode: Decode,
}

fn io_slot() -> &'static Mutex<Option<ScreenshotCodec>> {
    static IO: OnceLock<Mutex<Option<ScreenshotCodec>>> = OnceLock::new();
    IO.get_or_init(|| Mutex::new(None))
}

pub fn set_io(codec: ScreenshotCodec) {
    let mut guard: MutexGuard<'_, Option<ScreenshotCodec>> = match io_slot().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = Some(codec);
}

fn io() -> Result<(ScreenshotCodec, ArtifactIo), ApiError> {
    let codec = {
        let guard = match io_slot().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard
    };
    codec.zip(crate::artifact_io::installed()).ok_or_else(|| {
        ApiError::new(
            E_STATE,
            "this build cannot encode or write an image, so `screenshot` has nowhere to put one",
        )
        .with_hint(
            "a host installs the codec with `commands::screenshot::set_io` and the artifact \
                 access with `artifact_io::set`",
        )
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenshotArgs {
    pub instance: Option<String>,
    pub view: View,
    /// Becomes the file stem.
    pub save_as: Option<String>,
    /// A relative artifact path.
    pub compare_with: Option<String>,
    pub max_diff_pixels: u32,
    pub tolerance: u8,
    pub inline: bool,
}

impl Default for ScreenshotArgs {
    fn default() -> ScreenshotArgs {
        ScreenshotArgs {
            instance: None,
            view: View::Glass,
            save_as: None,
            compare_with: None,
            max_diff_pixels: 0,
            tolerance: 0,
            inline: false,
        }
    }
}

impl ScreenshotArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<ScreenshotArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "view",
                "save_as",
                "compare_with",
                "max_diff_pixels",
                "tolerance",
                "inline",
            ],
        )?;
        let view = match opt_str(args, "view")? {
            None => View::Glass,
            Some(text) => View::parse(text).ok_or_else(|| {
                usage(
                    "view",
                    &format!("`{text}` is not one of {}", View::vocabulary()),
                )
            })?,
        };
        let save_as = opt_str(args, "save_as")?.map(str::to_owned);
        if let Some(label) = &save_as
            && !crate::commands::snapshot::name_is_valid(label)
        {
            return Err(usage(
                "save_as",
                "expected 1 to 64 characters of A-Z, a-z, 0-9, `_`, `.` and `-`",
            ));
        }
        let compare_with = opt_str(args, "compare_with")?.map(str::to_owned);
        if let Some(path) = &compare_with {
            crate::output::check_artifact_path(path)
                .map_err(|err| usage("compare_with", &format!("{err}")))?;
        }
        let tolerance = match opt_u64(args, "tolerance")? {
            None => 0,
            Some(value) => u8::try_from(value)
                .map_err(|_| usage("tolerance", "expected 0..=255, one RGB888 channel step"))?,
        };
        Ok(ScreenshotArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            view,
            save_as,
            compare_with,
            max_diff_pixels: u32::try_from(opt_u64(args, "max_diff_pixels")?.unwrap_or(0))
                .map_err(|_| usage("max_diff_pixels", "does not fit"))?,
            tolerance,
            inline: opt_bool(args, "inline")?.unwrap_or(false),
        })
    }

    pub fn artifact_path(&self, frame_gen: u64) -> String {
        match &self.save_as {
            Some(label) => format!("{SCREENS_DIR}/{label}.png"),
            None => format!("{SCREENS_DIR}/{frame_gen:04}.png"),
        }
    }
}

/// A tainted instance is refused before the frame is read: a MAC rendered as pixels is nothing
/// value redaction can find. There is deliberately no `--include-secrets`.
pub fn screenshot_on(session: &mut Session, args: &ScreenshotArgs) -> Result<Output, ApiError> {
    let (codec, files) = io()?;
    let receipt = session.receipt();
    if receipt.tainted {
        return Err(ApiError::new(
            E_SECRET_REFUSED,
            "this instance is tainted, so its panel is not captured to an artifact",
        )
        .with_hint(
            "a tainted machine is loaded from a device's own inputs; start an instance on the \
             synthesized defaults to capture a screen",
        ));
    }
    let (panel, pixels) = {
        let frame = &session.machine().io().frame;
        (Panel::of(frame), frame.pixels().to_vec())
    };
    let rgb = view_pixels(&pixels, &panel, args.view);
    let width = u32::try_from(panel.width).unwrap_or(0);
    let height = u32::try_from(panel.height).unwrap_or(0);
    let encoded = (codec.encode)(width, height, &rgb).map_err(|err| {
        ApiError::new(E_INTERNAL, format!("the image could not be encoded: {err}"))
    })?;
    let path = args.artifact_path(panel.frame_gen);
    let written = (files.write)(&path, &encoded).map_err(|err| {
        ApiError::new(
            E_STATE,
            format!("the screenshot could not be written: {err}"),
        )
    })?;

    let mut compare_json = serde_json::Value::Null;
    let mut passed = true;
    if let Some(golden) = &args.compare_with {
        let bytes = (files.read)(golden).map_err(|err| {
            ApiError::new(E_STATE, format!("`{golden}` could not be read: {err}"))
        })?;
        let (gw, gh, expected) = (codec.decode)(&bytes).map_err(|err| {
            ApiError::new(E_USAGE, format!("`{golden}` could not be decoded: {err}"))
        })?;
        if gw != width || gh != height {
            return Err(usage(
                "compare_with",
                &format!("the expected image is {gw}x{gh} and the panel is {width}x{height}"),
            ));
        }
        let diff = compare(&rgb, &expected, width, args.tolerance)?;
        passed = diff.diff_pixels <= args.max_diff_pixels;
        let diff_path = if diff.equal {
            None
        } else {
            let image = diff_image(&rgb, &expected, args.tolerance);
            let encoded = (codec.encode)(width, height, &image).map_err(|err| {
                ApiError::new(E_INTERNAL, format!("the diff could not be encoded: {err}"))
            })?;
            let path = format!("{SCREENS_DIR}/{}-diff.png", stem(&written));
            Some((files.write)(&path, &encoded).map_err(|err| {
                ApiError::new(E_STATE, format!("the diff could not be written: {err}"))
            })?)
        };
        compare_json = diff.to_json(diff_path.as_deref());
    }

    // Reused from the top of the call: `Session::receipt` moves the ledger cursor, so a second call
    // would drain the delta twice.
    let mut json = serde_json::json!({
        "instance": session.id.to_string(),
        "view": args.view.as_str(),
        "w": width,
        "h": height,
        "path": written,
        "sha256": crate::commands::snapshot::sha256_hex(&encoded),
        "pixels_sha256": crate::commands::snapshot::sha256_hex(&rgb),
        "frame_gen": panel.frame_gen,
        "panel": panel.to_json(),
        "compare": compare_json,
        "result": if passed { "pass" } else { "fail" },
    });
    if args.inline {
        json["inline_rgb888_len"] = rgb.len().into();
    }
    let mut text = format!(
        "{} {} {width}x{height} {} frame_gen={}",
        session.id,
        args.view.as_str(),
        written,
        panel.frame_gen
    );
    if !panel.display_on {
        let _ = write!(
            text,
            "\npanel off (sleep_in={}), so `glass` and `perceived` are black",
            panel.sleep_in
        );
    }
    if let Some(diff) = json["compare"].as_object() {
        let _ = write!(
            text,
            "\ncompare {} diff_pixels={}",
            if passed { "pass" } else { "FAIL" },
            diff["diff_pixels"]
        );
    }
    let artifact = ArtifactRef {
        path: written,
        media_type: "image/png".to_owned(),
        bytes: encoded.len() as u64,
        sha256: json["sha256"].as_str().unwrap_or_default().to_owned(),
    };
    Output::new(json, text, receipt)
        .with_artifact(artifact)
        .map(|output| output.shaped(&ShapeLimits::DEFAULT))
        .map_err(|err| {
            ApiError::new(
                E_INTERNAL,
                format!("the artifact path is not usable: {err}"),
            )
        })
}

fn stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".png").unwrap_or(file)
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`screenshot` arguments.",
        "properties": {
            "instance": instance_schema(),
            "view": { "type": "string", "enum": ["raw", "glass", "perceived"], "description": "Which view (glass)." },
            "save_as": { "type": "string", "description": "Artifact label." },
            "compare_with": { "type": "string", "description": "Expected image, relative path." },
            "max_diff_pixels": { "type": "integer", "minimum": 0, "description": "Pixels that may differ (0)." },
            "tolerance": { "type": "integer", "minimum": 0, "maximum": 255, "description": "Per-channel tolerance (0)." },
            "inline": { "type": "boolean", "description": "Return the image inline (false)." }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "view": { "type": "string" },
            "w": { "type": "integer" },
            "h": { "type": "integer" },
            "path": { "type": "string" },
            "sha256": { "type": "string" },
            "pixels_sha256": { "type": "string" },
            "frame_gen": { "type": "integer" },
            "panel": { "type": "object" },
            "compare": { "type": ["object", "null"] },
            "result": { "type": "string", "enum": ["pass", "fail"] }
        }
    })
}

/// Capture the panel as a PNG artifact, optionally comparing it with an expected image.
#[command(
    api_crate = crate,
    name = "screenshot",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(read_only, idempotent, needs_instance),
    cli(positional = ["view"]),
    scenario_step = "screen.save",
    errors(E_USAGE, E_STATE, E_LEASE, E_SECRET_REFUSED, E_INTERNAL),
    example(
        title = "Capture what the panel shows",
        args = r#"{}"#,
    ),
    example(
        title = "Capture what the firmware drew, ignoring the panel state",
        args = r#"{"view":"raw","save_as":"menu"}"#,
    ),
    example(
        title = "Compare against a golden, allowing no differing pixel",
        args = r#"{"view":"raw","compare_with":"golden/menu.png"}"#,
    ),
)]
pub fn screenshot(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = ScreenshotArgs::from_json(&args)?;
    // On the checked-out session, outside the pool lock.
    crate::pool::with_session(
        |pool: &mut Pool| {
            let id = pool.bind(SPEC_SCREENSHOT.annotations, args.instance.as_deref())?;
            let now = pool
                .session(id)
                .map(Session::now)
                .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
            if let Some(state) = pool.table().get(id) {
                state.lease.check_call(
                    crate::lease::LeaseHolder::Agent,
                    SPEC_SCREENSHOT.annotations,
                    now,
                )?;
            }
            Ok(id)
        },
        |session| screenshot_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    use pemu_core::time::VTime;

    use crate::commands::start::{Boot, StartArgs};
    use crate::instance::Lifecycle;

    /// Small enough that a test can write the expected bytes down.
    const W: usize = 2;
    const H: usize = 2;

    const WHITE: u16 = 0xFFFF;
    const BLACK: u16 = 0x0000;
    const RED: u16 = 0xF800;
    const BLUE: u16 = 0x001F;

    static FILES: StdMutex<Option<BTreeMap<String, Vec<u8>>>> = StdMutex::new(None);

    /// The artifact seam is process-wide and shared with the snapshot tests.
    fn world() -> std::sync::MutexGuard<'static, ()> {
        crate::commands::snapshot::tests::world()
    }

    fn files<R>(f: impl FnOnce(&mut BTreeMap<String, Vec<u8>>) -> R) -> R {
        let mut guard = FILES.lock().expect("never poisoned");
        f(guard.get_or_insert_with(BTreeMap::new))
    }

    /// Not a PNG on purpose; the PNG encoder is tested in `pemu_host::png`.
    fn test_encode(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(8 + rgb.len());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(rgb);
        Ok(out)
    }

    fn test_decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
        if bytes.len() < 8 {
            return Err("too short".to_owned());
        }
        let width = u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| "width")?);
        let height = u32::from_le_bytes(bytes[4..8].try_into().map_err(|_| "height")?);
        Ok((width, height, bytes[8..].to_vec()))
    }

    fn test_write(path: &str, bytes: &[u8]) -> Result<String, String> {
        files(|map| map.insert(path.to_owned(), bytes.to_vec()));
        Ok(path.to_owned())
    }

    fn test_read(path: &str) -> Result<Vec<u8>, String> {
        files(|map| map.get(path).cloned()).ok_or_else(|| format!("no artifact `{path}`"))
    }

    fn panel() -> Panel {
        Panel {
            width: W,
            height: H,
            display_on: true,
            sleep_in: false,
            inverted: false,
            glass_complement: false,
            backlight_duty: BACKLIGHT_MAX,
            frame_gen: 7,
        }
    }

    #[test]
    fn rgb565_expands_by_bit_replication_so_white_stays_white() {
        assert_eq!(rgb565_to_rgb888(WHITE), [0xFF, 0xFF, 0xFF]);
        assert_eq!(rgb565_to_rgb888(BLACK), [0x00, 0x00, 0x00]);
        assert_eq!(rgb565_to_rgb888(RED), [0xFF, 0x00, 0x00]);
        assert_eq!(rgb565_to_rgb888(BLUE), [0x00, 0x00, 0xFF]);
    }

    #[test]
    fn the_raw_view_ignores_inversion_power_and_backlight() {
        let pixels = [WHITE, BLACK, RED, BLUE];
        let dark = Panel {
            display_on: false,
            inverted: true,
            backlight_duty: 0,
            ..panel()
        };
        assert_eq!(
            view_pixels(&pixels, &dark, View::Raw),
            view_pixels(&pixels, &panel(), View::Raw),
            "`raw` is the frame memory and nothing else"
        );
        assert_eq!(
            &view_pixels(&pixels, &panel(), View::Raw)[..3],
            &[0xFF, 0xFF, 0xFF]
        );
    }

    /// The complement is `inverted != invon_shows_ram`, not INVON itself.
    #[test]
    fn the_glass_view_applies_inversion_and_goes_black_when_the_panel_is_off() {
        let pixels = [WHITE, BLACK, RED, BLUE];
        let complement = Panel {
            glass_complement: true,
            ..panel()
        };
        assert_eq!(
            &view_pixels(&pixels, &complement, View::Glass)[..6],
            &[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF],
            "the glass complement swaps white and black"
        );
        let invon = Panel {
            inverted: true,
            ..panel()
        };
        assert_eq!(
            view_pixels(&pixels, &invon, View::Glass),
            view_pixels(&pixels, &panel(), View::Glass),
            "INVON alone is a status fact and draws nothing"
        );
        let asleep = Panel {
            display_on: false,
            sleep_in: true,
            ..panel()
        };
        assert!(
            view_pixels(&pixels, &asleep, View::Glass)
                .iter()
                .all(|&b| b == 0),
            "a sleeping panel shows nothing"
        );
    }

    #[test]
    fn the_perceived_view_scales_by_the_backlight_duty() {
        let pixels = [WHITE, WHITE, WHITE, WHITE];
        let half = Panel {
            backlight_duty: BACKLIGHT_MAX / 2,
            ..panel()
        };
        let bytes = view_pixels(&pixels, &half, View::Perceived);
        assert_eq!(bytes[0], 0x7F, "255 * 8192 / 16384");
        assert!(
            view_pixels(
                &pixels,
                &Panel {
                    backlight_duty: 0,
                    ..panel()
                },
                View::Perceived
            )
            .iter()
            .all(|&b| b == 0),
            "duty 0 is a dark room"
        );
        assert_eq!(
            view_pixels(&pixels, &panel(), View::Perceived),
            view_pixels(&pixels, &panel(), View::Glass),
            "full duty is the glass view"
        );
    }

    /// Powered, awake, DISPON, INVON with `invon_shows_ram = true`, and the backlight at 1023 of
    /// 1024 with four fractional bits.
    fn official_menu_port() -> FramePort {
        let mut frame = FramePort::new();
        frame.set_powered(true);
        frame.set_sleeping(false);
        frame.set_display_on(true);
        frame.set_inverted(true);
        frame.set_backlight(1023 << 4);
        frame
    }

    /// The menu's sky 0x145D shows on the glass as 0x145D, as the page draws it
    /// (`web/src/gl/frm1.ts`).
    #[test]
    fn the_glass_of_the_official_menu_is_panel_memory_as_drawn() {
        const SKY: u16 = 0x145D;
        let panel = Panel::of(&official_menu_port());
        assert_eq!(
            view_pixels(&[SKY], &panel, View::Glass),
            view_pixels(&[SKY], &panel, View::Raw),
            "INVON under invon_shows_ram shows memory unmodified"
        );
        let mut invoff = official_menu_port();
        invoff.set_inverted(false);
        assert_eq!(
            view_pixels(&[SKY], &Panel::of(&invoff), View::Glass),
            rgb565_to_rgb888(!SKY).to_vec(),
            "INVOFF under invon_shows_ram shows the complement"
        );
    }

    /// DISPOFF blanks the glass and keeps panel memory.
    #[test]
    fn dispoff_blanks_the_glass_of_a_powered_awake_panel() {
        let mut frame = official_menu_port();
        frame.set_display_on(false);
        let panel = Panel::of(&frame);
        assert!(!panel.display_on);
        for view in [View::Glass, View::Perceived] {
            assert!(
                view_pixels(&[WHITE], &panel, view).iter().all(|&b| b == 0),
                "{view:?} is black in DISPOFF"
            );
        }
        assert_eq!(view_pixels(&[WHITE], &panel, View::Raw), vec![0xFF; 3]);
    }

    /// 1023 of 1024 is full brightness, not a quarter.
    #[test]
    fn the_official_menu_backlight_is_full_in_the_perceived_view() {
        let panel = Panel::of(&official_menu_port());
        assert_eq!(panel.backlight_duty, 1023 << 4);
        assert_eq!(
            view_pixels(&[WHITE], &panel, View::Perceived),
            vec![254; 3],
            "255 * 16368 / 16384"
        );
    }

    #[test]
    fn a_compare_reports_the_count_and_the_bounding_box() {
        let expected = vec![0u8; 4 * 4 * RGB888_BYTES];
        let mut actual = expected.clone();
        // Pixels (1,1) and (2,2) of a 4x4 image.
        for index in [1 + 4, 2 + 8] {
            actual[index * RGB888_BYTES] = 0xFF;
        }
        let diff = compare(&actual, &expected, 4, 0).expect("same size");
        assert!(!diff.equal);
        assert_eq!(diff.diff_pixels, 2);
        assert_eq!(diff.bbox, (1, 1, 2, 2));
    }

    #[test]
    fn a_tolerance_forgives_a_rounded_channel_but_not_a_wrong_pixel() {
        let expected = vec![0x80u8; 3 * RGB888_BYTES];
        let mut actual = expected.clone();
        actual[0] = 0x82;
        actual[3] = 0xFF;
        assert_eq!(
            compare(&actual, &expected, 3, 0)
                .expect("same size")
                .diff_pixels,
            2
        );
        assert_eq!(
            compare(&actual, &expected, 3, 2)
                .expect("same size")
                .diff_pixels,
            1,
            "the 2-step channel is inside the tolerance and the 0x7F one is not"
        );
    }

    #[test]
    fn comparing_images_of_different_sizes_is_usage_and_not_a_diff() {
        let error = compare(&[0; 3], &[0; 6], 1, 0).expect_err("different sizes");
        assert_eq!(error.code, E_USAGE);
    }

    #[test]
    fn the_diff_image_marks_the_differing_pixels() {
        let expected = vec![0x90u8; 2 * RGB888_BYTES];
        let mut actual = expected.clone();
        actual[0] = 0x00;
        let image = diff_image(&actual, &expected, 0);
        assert_eq!(&image[..3], &[0xFF, 0x00, 0xFF], "magenta marks a mismatch");
        assert_eq!(&image[3..6], &[0x30, 0x30, 0x30], "the rest is darkened");
    }

    fn instance_with_frame(pixels: &[u16]) -> (Pool, crate::instance::InstanceId) {
        files(BTreeMap::clear);
        set_io(ScreenshotCodec {
            encode: test_encode,
            decode: test_decode,
        });
        crate::artifact_io::set(ArtifactIo {
            write: test_write,
            read: test_read,
        });
        let (machine, _) = crate::commands::env::tests::JournalMachine::new();
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        {
            let session = pool.session_mut(id).expect("the instance");
            let frame = &mut session.machine().io().frame;
            let target = frame.pixels_mut();
            for (slot, value) in target.iter_mut().zip(pixels.iter().cycle()) {
                *slot = *value;
            }
            // A fresh `FramePort` is unpowered; a booted instance has DISPON and INVON already.
            frame.set_powered(true);
            frame.set_sleeping(false);
            frame.set_display_on(true);
            frame.set_inverted(true);
            frame.set_backlight(1024 << 4);
            frame.present();
        }
        (pool, id)
    }

    fn args(json: serde_json::Value) -> ScreenshotArgs {
        ScreenshotArgs::from_json(&json).expect("inside the schema")
    }

    #[test]
    fn a_screenshot_returns_a_path_a_hash_and_the_panel_state() {
        let _world = world();
        let (mut pool, id) = instance_with_frame(&[WHITE]);
        let session = pool.session_mut(id).expect("the instance");
        let out = screenshot_on(session, &args(serde_json::json!({"save_as":"menu"})))
            .expect("the panel can always be captured");
        assert_eq!(out.json["path"], "screens/menu.png");
        assert_eq!(out.json["view"], "glass");
        assert_eq!(out.json["sha256"].as_str().expect("a hash").len(), 64);
        assert_eq!(
            out.json["pixels_sha256"].as_str().expect("a hash").len(),
            64
        );
        assert_eq!(out.json["panel"]["display_on"], true);
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].media_type, "image/png");
        assert!(
            !out.json
                .as_object()
                .expect("an object")
                .contains_key("pixels"),
            "never inline pixels unless asked"
        );
        assert!(files(|map| map.contains_key("screens/menu.png")));
    }

    #[test]
    fn the_pixel_hash_does_not_depend_on_the_encoder() {
        let _world = world();
        let (mut pool, id) = instance_with_frame(&[RED]);
        let first = {
            let session = pool.session_mut(id).expect("the instance");
            screenshot_on(session, &args(serde_json::json!({"view":"raw"}))).expect("captured")
        };
        fn other_encode(_w: u32, _h: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
            let mut out = vec![0xAA; 16];
            out.extend_from_slice(rgb);
            Ok(out)
        }
        set_io(ScreenshotCodec {
            encode: other_encode,
            decode: test_decode,
        });
        let second = {
            let session = pool.session_mut(id).expect("the instance");
            screenshot_on(session, &args(serde_json::json!({"view":"raw"}))).expect("captured")
        };
        assert_eq!(first.json["pixels_sha256"], second.json["pixels_sha256"]);
        assert_ne!(first.json["sha256"], second.json["sha256"]);
    }

    #[test]
    fn a_compare_against_a_golden_passes_and_fails_with_a_diff_artifact() {
        let _world = world();
        let (mut pool, id) = instance_with_frame(&[WHITE]);
        {
            let session = pool.session_mut(id).expect("the instance");
            let out = screenshot_on(
                session,
                &args(serde_json::json!({"view":"raw","save_as":"golden"})),
            )
            .expect("captured");
            assert_eq!(out.json["path"], "screens/golden.png");
        }
        {
            let session = pool.session_mut(id).expect("the instance");
            let out = screenshot_on(
                session,
                &args(serde_json::json!({"view":"raw","compare_with":"screens/golden.png"})),
            )
            .expect("captured");
            assert_eq!(out.json["compare"]["equal"], true);
            assert_eq!(out.json["result"], "pass");
            assert_eq!(out.json["compare"]["diff_path"], serde_json::Value::Null);
        }
        {
            let session = pool.session_mut(id).expect("the instance");
            session.machine().io().frame.pixels_mut()[0] = BLACK;
        }
        let session = pool.session_mut(id).expect("the instance");
        let out = screenshot_on(
            session,
            &args(serde_json::json!({"view":"raw","compare_with":"screens/golden.png"})),
        )
        .expect("captured");
        assert_eq!(out.json["compare"]["equal"], false);
        assert_eq!(out.json["compare"]["diff_pixels"], 1);
        assert_eq!(out.json["result"], "fail");
        let diff_path = out.json["compare"]["diff_path"]
            .as_str()
            .expect("a diff artifact")
            .to_owned();
        assert!(files(|map| map.contains_key(&diff_path)), "{diff_path}");
    }

    #[test]
    fn max_diff_pixels_decides_whether_a_difference_is_a_failure() {
        let _world = world();
        let (mut pool, id) = instance_with_frame(&[WHITE]);
        {
            let session = pool.session_mut(id).expect("the instance");
            screenshot_on(
                session,
                &args(serde_json::json!({"view":"raw","save_as":"g"})),
            )
            .expect("captured");
            session.machine().io().frame.pixels_mut()[0] = BLACK;
        }
        let session = pool.session_mut(id).expect("the instance");
        let out = screenshot_on(
            session,
            &args(serde_json::json!({
                "view":"raw","compare_with":"screens/g.png","max_diff_pixels":1
            })),
        )
        .expect("captured");
        assert_eq!(out.json["compare"]["diff_pixels"], 1);
        assert_eq!(out.json["result"], "pass");
    }

    #[test]
    fn a_dark_panel_is_explained_in_the_text() {
        let _world = world();
        let (mut pool, id) = instance_with_frame(&[WHITE]);
        {
            let session = pool.session_mut(id).expect("the instance");
            session.machine().io().frame.set_powered(false);
        }
        let session = pool.session_mut(id).expect("the instance");
        let out = screenshot_on(session, &args(serde_json::json!({}))).expect("captured");
        assert_eq!(out.json["panel"]["display_on"], false);
        assert!(out.text.contains("panel off"), "{}", out.text);
    }

    #[test]
    fn without_a_codec_the_command_names_who_installs_one() {
        let _world = world();
        let (mut pool, id) = instance_with_frame(&[WHITE]);
        {
            let mut guard = match io_slot().lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *guard = None;
        }
        let session = pool.session_mut(id).expect("the instance");
        let error = screenshot_on(session, &args(serde_json::json!({})))
            .expect_err("no codec is installed");
        assert_eq!(error.code, E_STATE);
        assert!(
            error
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("screenshot::set_io")),
            "{:?}",
            error.hint
        );
    }

    #[test]
    fn every_argument_outside_the_schema_is_usage() {
        for bad in [
            serde_json::json!({ "view": "thermal" }),
            serde_json::json!({ "save_as": "a/b" }),
            serde_json::json!({ "compare_with": "/etc/passwd" }),
            serde_json::json!({ "compare_with": "../up.png" }),
            serde_json::json!({ "tolerance": 256 }),
            serde_json::json!({ "nonsense": 1 }),
        ] {
            assert_eq!(
                ScreenshotArgs::from_json(&bad)
                    .expect_err("outside the schema")
                    .code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("screenshot").expect("#[command] registered screenshot");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            ScreenshotArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.read_only && spec.annotations.needs_instance);
        assert_eq!(spec.scenario_step, Some("screen.save"));
    }
}
