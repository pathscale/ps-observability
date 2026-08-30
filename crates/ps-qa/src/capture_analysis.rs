//! Decode and compare renderer captures.
//!
//! The runner owns interaction ordering and transport. This module owns the
//! pixel contract: buffer validation, tolerance, visible-ink measurement and
//! diagnostic artifacts. Keeping those concerns separate makes a paint verdict
//! testable without a live application or control socket.

use std::collections::HashMap;
use std::io::Write as _;

use base64::Engine as _;
use blitz_control_protocol::CapturedImage;
use eyre::{Context, Result};

/// Subpixel edge coverage can vary by a few 8-bit levels between equivalent
/// GPU captures. Four levels is the first difference treated as authored ink.
pub(crate) const CHANNEL_TOLERANCE: u8 = 3;

fn decode(image: &CapturedImage) -> std::result::Result<Vec<u8>, String> {
    let rgba = base64::engine::general_purpose::STANDARD
        .decode(&image.rgba_base64)
        .map_err(|error| format!("captured frame is not valid base64: {error}"))?;
    let expected = (image.width as usize)
        .checked_mul(image.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "captured frame dimensions overflowed".to_owned())?;
    if rgba.len() != expected {
        return Err(format!(
            "captured {}x{} frame has {} RGBA bytes; expected {expected}",
            image.width,
            image.height,
            rgba.len()
        ));
    }
    Ok(rgba)
}

pub(crate) fn pixel_delta(
    before: &CapturedImage,
    after: &CapturedImage,
) -> std::result::Result<(usize, u8), String> {
    let before_rgba = decode(before)?;
    let after_rgba = decode(after)?;
    if before_rgba.len() != after_rgba.len() {
        return Err(format!(
            "last frame buffer changed length from {} to {} bytes",
            before_rgba.len(),
            after_rgba.len()
        ));
    }
    let mut changed = 0;
    let mut max_delta = 0;
    for (before, after) in before_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_rgba.as_chunks::<4>().0.iter())
    {
        let pixel_delta = before
            .iter()
            .zip(after.iter())
            .map(|(left, right)| left.abs_diff(*right))
            .max()
            .unwrap_or_default();
        if pixel_delta > 0 {
            changed += 1;
            max_delta = max_delta.max(pixel_delta);
        }
    }
    Ok((changed, max_delta))
}

/// Whether same-sized captures differ only by bounded raster rounding.
pub(crate) fn pixels_hold_with_tolerance(
    before: &CapturedImage,
    after: &CapturedImage,
    channel_tolerance: u8,
) -> std::result::Result<bool, String> {
    if before.width != after.width || before.height != after.height {
        return Ok(false);
    }
    if before.rgba_base64 == after.rgba_base64 {
        return Ok(true);
    }
    let before_rgba = decode(before)?;
    let after_rgba = decode(after)?;
    Ok(before_rgba
        .iter()
        .zip(&after_rgba)
        .all(|(left, right)| left.abs_diff(*right) <= channel_tolerance))
}

pub(crate) fn pixels_hold(
    before: &CapturedImage,
    after: &CapturedImage,
) -> std::result::Result<(), String> {
    if before.width != after.width || before.height != after.height {
        return Err(format!(
            "rendered frame changed size from {}x{} to {}x{}",
            before.width, before.height, after.width, after.height
        ));
    }
    if pixels_hold_with_tolerance(before, after, CHANNEL_TOLERANCE)? {
        return Ok(());
    }

    let before_rgba = decode(before)?;
    let after_rgba = decode(after)?;
    let changed = before_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_rgba.as_chunks::<4>().0.iter())
        .filter(|(before, after)| {
            before
                .iter()
                .zip(after.iter())
                .any(|(left, right)| left.abs_diff(*right) > CHANNEL_TOLERANCE)
        })
        .count();
    Err(format!(
        "{changed} rendered pixel(s) changed after the pointer returned to the same state"
    ))
}

/// Compare visible RGB placement while ignoring alpha-only edge coverage.
pub(crate) fn rgb_pixels_hold(
    before: &CapturedImage,
    after: &CapturedImage,
) -> std::result::Result<(), String> {
    if before.width != after.width || before.height != after.height {
        return Err(format!(
            "rendered frame changed size from {}x{} to {}x{}",
            before.width, before.height, after.width, after.height
        ));
    }
    let before_rgba = decode(before)?;
    let after_rgba = decode(after)?;
    const RGB_TOLERANCE: u8 = 8;
    let changed = before_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_rgba.as_chunks::<4>().0.iter())
        .filter(|(before, after)| {
            before[..3]
                .iter()
                .zip(after[..3].iter())
                .any(|(left, right)| left.abs_diff(*right) > RGB_TOLERANCE)
        })
        .count();
    if changed == 0 {
        Ok(())
    } else {
        Err(format!(
            "{changed} visibly coloured pixel(s) changed after the first hover"
        ))
    }
}

/// Require an actual pixel change, including visible growth or shrinkage.
pub(crate) fn pixels_change(
    before: &CapturedImage,
    after: &CapturedImage,
) -> std::result::Result<(), String> {
    let before_rgba = decode(before)?;
    let after_rgba = decode(after)?;
    let canvas_width = before.width.max(after.width) as usize;
    let canvas_height = before.height.max(after.height) as usize;
    let pixel_at = |rgba: &[u8], capture: &CapturedImage, x: usize, y: usize| -> [u8; 4] {
        let offset_x = (canvas_width - capture.width as usize) / 2;
        let offset_y = (canvas_height - capture.height as usize) / 2;
        let Some(local_x) = x.checked_sub(offset_x) else {
            return [0; 4];
        };
        let Some(local_y) = y.checked_sub(offset_y) else {
            return [0; 4];
        };
        if local_x >= capture.width as usize || local_y >= capture.height as usize {
            return [0; 4];
        }
        let index = (local_y * capture.width as usize + local_x) * 4;
        rgba[index..index + 4]
            .try_into()
            .expect("validated RGBA frame")
    };

    for y in 0..canvas_height {
        for x in 0..canvas_width {
            let left = pixel_at(&before_rgba, before, x, y);
            let right = pixel_at(&after_rgba, after, x, y);
            if left
                .iter()
                .zip(right)
                .any(|(left, right)| left.abs_diff(right) > CHANNEL_TOLERANCE)
            {
                return Ok(());
            }
        }
    }
    Err("hover left every rendered pixel unchanged".to_owned())
}

/// What a captured frame contains, in the terms a person would use.
pub(crate) struct Ink {
    pub(crate) visible: usize,
    pub(crate) total: usize,
    pub(crate) background: (u8, u8, u8),
}

impl Ink {
    pub(crate) fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.visible as f64 / self.total as f64
        }
    }
}

/// Discover the modal background and count pixels with visible contrast.
fn measure_ink_bounds(
    image: &CapturedImage,
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
) -> Result<Ink> {
    let rgba = decode(image).map_err(eyre::Report::msg)?;
    let mut histogram: HashMap<(u8, u8, u8), usize> = HashMap::new();
    let pixel = |x: u32, y: u32| {
        let start = ((y * image.width + x) * 4) as usize;
        [rgba[start], rgba[start + 1], rgba[start + 2], rgba[start + 3]]
    };
    for y in top..bottom {
        for x in left..right {
            let pixel = pixel(x, y);
            *histogram.entry((pixel[0], pixel[1], pixel[2])).or_default() += 1;
        }
    }
    let background = histogram
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(colour, _)| *colour)
        .unwrap_or((0, 0, 0));

    let luminance = |(r, g, b): (u8, u8, u8)| {
        0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
    };
    let background_luminance = luminance(background);
    let mut visible = 0;
    for y in top..bottom {
        for x in left..right {
            let pixel = pixel(x, y);
            if pixel[3] >= 32
                && (luminance((pixel[0], pixel[1], pixel[2])) - background_luminance).abs() > 24.0
            {
                visible += 1;
            }
        }
    }

    Ok(Ink {
        visible,
        total: ((right - left) as usize) * ((bottom - top) as usize),
        background,
    })
}

/// Discover visible ink anywhere in the captured region.
pub(crate) fn measure_ink(image: &CapturedImage) -> Result<Ink> {
    measure_ink_bounds(image, 0, 0, image.width, image.height)
}

/// Discover visible ink inside the control, excluding its border and focus ring.
pub(crate) fn measure_interior_ink(image: &CapturedImage) -> Result<Ink> {
    let inset_x = image.width / 5;
    let inset_y = image.height / 5;
    let right = image.width.saturating_sub(inset_x);
    let bottom = image.height.saturating_sub(inset_y);
    if right <= inset_x || bottom <= inset_y {
        return Err(eyre::eyre!(
            "captured {}x{} region has no measurable interior",
            image.width,
            image.height
        ));
    }
    measure_ink_bounds(image, inset_x, inset_y, right, bottom)
}

pub(crate) fn write_ppm(image: &CapturedImage, output: &std::path::Path) -> Result<()> {
    let rgba = decode(image).map_err(eyre::Report::msg)?;
    let mut file = std::fs::File::create(output)
        .wrap_err_with(|| format!("could not create {}", output.display()))?;
    write!(file, "P6\n{} {}\n255\n", image.width, image.height)?;
    for pixel in rgba.as_chunks::<4>().0 {
        file.write_all(&pixel[..3])?;
    }
    println!("saved {}", output.display());
    Ok(())
}

pub(crate) fn save_artifacts(
    check_id: &str,
    before: &CapturedImage,
    after: &CapturedImage,
) -> Result<()> {
    let Some(directory) = std::env::var_os("PS_QA_PIXEL_ARTIFACT_DIR") else {
        return Ok(());
    };
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory)
        .wrap_err_with(|| format!("could not create {}", directory.display()))?;
    let safe_id: String = check_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    write_ppm(before, &directory.join(format!("{safe_id}-before.ppm")))?;
    write_ppm(after, &directory.join(format!("{safe_id}-after.ppm")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(rgba: &[u8]) -> CapturedImage {
        CapturedImage {
            width: 1,
            height: 1,
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(rgba),
            node_id: Some(7),
        }
    }

    #[test]
    fn change_requires_pixels_not_only_dimensions() {
        let transparent = capture(&[0, 0, 0, 0]);
        let resized_transparent = CapturedImage {
            width: 2,
            height: 1,
            rgba_base64: base64::engine::general_purpose::STANDARD.encode([0, 0, 0, 0, 0, 0, 0, 0]),
            node_id: Some(7),
        };
        assert_eq!(
            pixels_change(&transparent, &resized_transparent).unwrap_err(),
            "hover left every rendered pixel unchanged"
        );
    }

    #[test]
    fn visible_ink_is_contrast_not_alpha_alone() {
        let background = [20, 20, 20, 255];
        let empty = CapturedImage {
            width: 2,
            height: 2,
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(background.repeat(4)),
            node_id: Some(7),
        };
        assert_eq!(measure_ink(&empty).unwrap().visible, 0);

        let mut marked = background.repeat(4);
        marked[0..4].copy_from_slice(&[240, 240, 240, 255]);
        let marked = CapturedImage {
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(marked),
            ..empty
        };
        assert_eq!(measure_ink(&marked).unwrap().visible, 1);
    }

    #[test]
    fn interior_ink_ignores_a_control_border() {
        let background = [20, 20, 20, 255];
        let border = [240, 240, 240, 255];
        let mut pixels = background.repeat(25);
        for y in 0..5 {
            for x in 0..5 {
                if x == 0 || x == 4 || y == 0 || y == 4 {
                    let start = (y * 5 + x) * 4;
                    pixels[start..start + 4].copy_from_slice(&border);
                }
            }
        }
        let bordered = CapturedImage {
            width: 5,
            height: 5,
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(&pixels),
            node_id: Some(7),
        };
        assert!(measure_ink(&bordered).unwrap().visible > 0);
        assert_eq!(measure_interior_ink(&bordered).unwrap().visible, 0);

        pixels[(2 * 5 + 2) * 4..(2 * 5 + 2) * 4 + 4].copy_from_slice(&border);
        let labeled = CapturedImage {
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(pixels),
            ..bordered
        };
        assert_eq!(measure_interior_ink(&labeled).unwrap().visible, 1);
    }
}
