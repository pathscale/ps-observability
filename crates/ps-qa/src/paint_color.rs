//! Colour math for audits of the renderer's resolved paint.
//!
//! Blitz reports colours as `#rrggbbaa`. These helpers reconstruct translucent
//! paint stacks and calculate relative contrast from those resolved values.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Rgba {
    pub(crate) red: f64,
    pub(crate) green: f64,
    pub(crate) blue: f64,
    pub(crate) alpha: f64,
}

impl Rgba {
    pub(crate) const BLACK: Self = Self {
        red: 0.0,
        green: 0.0,
        blue: 0.0,
        alpha: 1.0,
    };

    pub(crate) const WHITE: Self = Self {
        red: 1.0,
        green: 1.0,
        blue: 1.0,
        alpha: 1.0,
    };
}

pub(crate) fn parse(value: &str) -> Option<Rgba> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 8 {
        return None;
    }
    let byte = |at| {
        u8::from_str_radix(&hex[at..at + 2], 16)
            .ok()
            .map(|value| f64::from(value) / 255.0)
    };
    Some(Rgba {
        red: byte(0)?,
        green: byte(2)?,
        blue: byte(4)?,
        alpha: byte(6)?,
    })
}

pub(crate) fn composite(top: Rgba, bottom: Rgba) -> Rgba {
    let alpha = top.alpha + bottom.alpha * (1.0 - top.alpha);
    if alpha <= f64::EPSILON {
        return Rgba {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        };
    }
    let channel = |top_channel, bottom_channel| {
        (top_channel * top.alpha + bottom_channel * bottom.alpha * (1.0 - top.alpha)) / alpha
    };
    Rgba {
        red: channel(top.red, bottom.red),
        green: channel(top.green, bottom.green),
        blue: channel(top.blue, bottom.blue),
        alpha,
    }
}

pub(crate) fn luminance(color: Rgba) -> f64 {
    let linear = |channel: f64| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.red) + 0.7152 * linear(color.green) + 0.0722 * linear(color.blue)
}

pub(crate) fn contrast_ratio(a: Rgba, b: Rgba) -> f64 {
    let a = luminance(a);
    let b = luminance(b);
    let (lighter, darker) = if a >= b { (a, b) } else { (b, a) };
    (lighter + 0.05) / (darker + 0.05)
}

#[cfg(test)]
mod tests {
    use super::{Rgba, composite, contrast_ratio, parse};

    #[test]
    fn parses_the_resolved_blitz_format() {
        assert_eq!(
            parse("#ff008080"),
            Some(Rgba {
                red: 1.0,
                green: 0.0,
                blue: 128.0 / 255.0,
                alpha: 128.0 / 255.0,
            })
        );
        assert_eq!(parse("#fff"), None);
        assert_eq!(parse("transparent"), None);
    }

    #[test]
    fn opaque_paint_replaces_what_is_beneath_it() {
        assert_eq!(composite(Rgba::WHITE, Rgba::BLACK), Rgba::WHITE);
    }

    #[test]
    fn black_and_white_have_the_wcag_maximum_contrast() {
        assert!((contrast_ratio(Rgba::BLACK, Rgba::WHITE) - 21.0).abs() < f64::EPSILON);
    }
}
