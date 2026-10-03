/// Photoshop layer blend modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[allow(missing_docs)]
pub enum BlendMode {
    #[default]
    Normal,
    Dissolve,
    Darken,
    Multiply,
    ColorBurn,
    LinearBurn,
    DarkerColor,
    Lighten,
    Screen,
    ColorDodge,
    LinearDodge,
    LighterColor,
    Overlay,
    SoftLight,
    HardLight,
    VividLight,
    LinearLight,
    PinLight,
    HardMix,
    Difference,
    Exclusion,
    Subtract,
    Divide,
    Hue,
    Saturation,
    Color,
    Luminosity,
    PassThrough,
    /// Keeps the backdrop color (internal: what Hue, Saturation and Color do to grays).
    #[doc(hidden)]
    Backdrop,
}

impl BlendMode {
    /// Parses a layer record key (`mul `) or an effect descriptor enum (`Mltp`, or `multiply` in
    /// recent files).
    pub fn from_key(key: &[u8]) -> BlendMode {
        let key = std::str::from_utf8(key).unwrap_or("").trim_end();
        match key {
            "diss" | "Dslv" | "dissolve" => BlendMode::Dissolve,
            "dark" | "Drkn" | "darken" => BlendMode::Darken,
            "mul" | "Mltp" | "multiply" => BlendMode::Multiply,
            "idiv" | "CBrn" | "colorBurn" => BlendMode::ColorBurn,
            "lbrn" | "linearBurn" => BlendMode::LinearBurn,
            "dkCl" | "darkerColor" => BlendMode::DarkerColor,
            "lite" | "Lghn" | "lighten" => BlendMode::Lighten,
            "scrn" | "Scrn" | "screen" => BlendMode::Screen,
            "div" | "CDdg" | "colorDodge" => BlendMode::ColorDodge,
            "lddg" | "linearDodge" => BlendMode::LinearDodge,
            "lgCl" | "lighterColor" => BlendMode::LighterColor,
            "over" | "Ovrl" | "overlay" => BlendMode::Overlay,
            "sLit" | "SftL" | "softLight" => BlendMode::SoftLight,
            "hLit" | "HrdL" | "hardLight" => BlendMode::HardLight,
            "vLit" | "vividLight" => BlendMode::VividLight,
            "lLit" | "linearLight" => BlendMode::LinearLight,
            "pLit" | "pinLight" => BlendMode::PinLight,
            "hMix" | "hardMix" => BlendMode::HardMix,
            "diff" | "Dfrn" | "difference" => BlendMode::Difference,
            "smud" | "Xclu" | "exclusion" => BlendMode::Exclusion,
            "fsub" | "blendSubtraction" => BlendMode::Subtract,
            "fdiv" | "blendDivide" => BlendMode::Divide,
            "hue" | "H" => BlendMode::Hue,
            "sat" | "Strt" | "saturation" => BlendMode::Saturation,
            "colr" | "Clr" | "color" => BlendMode::Color,
            "lum" | "Lmns" | "luminosity" => BlendMode::Luminosity,
            "pass" => BlendMode::PassThrough,
            _ => BlendMode::Normal,
        }
    }

    /// The separable mode that acts like `self` on gray source and backdrop, for passes that
    /// carry independent grays in each channel (the black plate of CMYK documents).
    pub(crate) fn on_grays(self) -> BlendMode {
        use BlendMode::*;
        match self {
            Hue | Saturation | Color => Backdrop,
            Luminosity => Normal,
            DarkerColor => Darken,
            LighterColor => Lighten,
            m => m,
        }
    }

    /// For modes where fill opacity fades the color toward a neutral one instead of thinning it
    /// (effects fade by their opacity and coverage the same way), that neutral value.
    pub(crate) fn neutral(self) -> Option<f32> {
        use BlendMode::*;
        match self {
            ColorDodge | LinearDodge | Difference => Some(0.0),
            ColorBurn | LinearBurn => Some(1.0),
            VividLight | LinearLight | HardMix => Some(0.5),
            _ => None,
        }
    }

    pub(crate) fn is_normal(self) -> bool {
        matches!(self, BlendMode::Normal | BlendMode::PassThrough | BlendMode::Dissolve)
    }

    fn separable(self, b: f32, s: f32) -> f32 {
        use BlendMode::*;
        match self {
            Darken => b.min(s),
            Multiply => b * s,
            ColorBurn if b >= 1.0 => 1.0,
            ColorBurn if s <= 0.0 => 0.0,
            ColorBurn => 1.0 - ((1.0 - b) / s).min(1.0),
            LinearBurn => (b + s - 1.0).max(0.0),
            Lighten => b.max(s),
            Screen => b + s - b * s,
            ColorDodge if b <= 0.0 => 0.0,
            ColorDodge if s >= 1.0 => 1.0,
            ColorDodge => (b / (1.0 - s)).min(1.0),
            LinearDodge => (b + s).min(1.0),
            Overlay => HardLight.separable(s, b),
            HardLight if s <= 0.5 => b * 2.0 * s,
            HardLight => Screen.separable(b, 2.0 * s - 1.0),
            SoftLight if s <= 0.5 => b - (1.0 - 2.0 * s) * b * (1.0 - b),
            SoftLight => {
                let d = if b <= 0.25 { ((16.0 * b - 12.0) * b + 4.0) * b } else { b.sqrt() };
                b + (2.0 * s - 1.0) * (d - b)
            }
            // Inside vivid light the source extremes win over the backdrop special cases.
            VividLight if s <= 0.0 => 0.0,
            VividLight if s >= 1.0 => 1.0,
            VividLight if s <= 0.5 => 1.0 - ((1.0 - b) / (2.0 * s)).min(1.0),
            VividLight => (b / (2.0 * (1.0 - s))).min(1.0),
            LinearLight => (b + 2.0 * s - 1.0).clamp(0.0, 1.0),
            PinLight if s <= 0.5 => b.min(2.0 * s),
            PinLight => b.max(2.0 * s - 1.0),
            HardMix => (b + s > 1.0 || (b + s == 1.0 && b > 0.5)) as u8 as f32,
            Difference => (b - s).abs(),
            Exclusion => b + s - 2.0 * b * s,
            Subtract => (b - s).max(0.0),
            Divide if b <= 0.0 => 0.0,
            Divide if s <= 0.0 => 1.0,
            Divide => (b / s).min(1.0),
            Backdrop => b,
            _ => s,
        }
    }

    /// Blends straight (non-premultiplied) source color `s` onto backdrop `b`.
    pub(crate) fn apply(self, b: [f32; 3], s: [f32; 3]) -> [f32; 3] {
        use BlendMode::*;
        match self {
            Hue => set_lum(set_sat(s, sat(b)), lum(b)),
            Saturation => set_lum(set_sat(b, sat(s)), lum(b)),
            Color => set_lum(s, lum(b)),
            Luminosity => set_lum(b, lum(s)),
            DarkerColor if lum(s) < lum(b) => s,
            DarkerColor => b,
            LighterColor if lum(s) > lum(b) => s,
            LighterColor => b,
            _ => [self.separable(b[0], s[0]), self.separable(b[1], s[1]), self.separable(b[2], s[2])],
        }
    }
}

pub(crate) fn lum(c: [f32; 3]) -> f32 {
    0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2]
}

fn clip_color(c: [f32; 3]) -> [f32; 3] {
    let l = lum(c);
    let lo = c[0].min(c[1]).min(c[2]);
    let hi = c[0].max(c[1]).max(c[2]);
    let mut o = c;
    if lo < 0.0 {
        o = o.map(|v| l + (v - l) * l / (l - lo).max(1e-6));
    }
    if hi > 1.0 {
        o = o.map(|v| l + (v - l) * (1.0 - l) / (hi - l).max(1e-6));
    }
    o
}

pub(crate) fn set_lum(c: [f32; 3], l: f32) -> [f32; 3] {
    let d = l - lum(c);
    clip_color(c.map(|v| v + d))
}

fn sat(c: [f32; 3]) -> f32 {
    c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])
}

fn set_sat(c: [f32; 3], s: f32) -> [f32; 3] {
    let hi = c[0].max(c[1]).max(c[2]);
    let lo = c[0].min(c[1]).min(c[2]);
    if hi - lo <= 1e-6 {
        return [0.0; 3];
    }
    c.map(|v| (v - lo) * s / (hi - lo))
}

#[cfg(test)]
mod tests {
    use super::BlendMode::{self, *};

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn parses_record_and_descriptor_keys() {
        assert_eq!(BlendMode::from_key(b"mul "), Multiply);
        assert_eq!(BlendMode::from_key(b"Mltp"), Multiply);
        assert_eq!(BlendMode::from_key(b"colorBurn"), ColorBurn);
        assert_eq!(BlendMode::from_key(b"pass"), PassThrough);
        assert_eq!(BlendMode::from_key(b"norm"), Normal);
        assert_eq!(BlendMode::from_key(b"\xff\xff"), Normal);
    }

    #[test]
    fn separable_modes_match_reference_values() {
        let b = [0.25, 0.5, 0.75];
        let s = [0.5, 0.5, 0.5];
        assert!(close(Multiply.apply(b, s), [0.125, 0.25, 0.375]));
        assert!(close(Screen.apply(b, s), [0.625, 0.75, 0.875]));
        assert!(close(Darken.apply(b, s), [0.25, 0.5, 0.5]));
        assert!(close(Lighten.apply(b, s), [0.5, 0.5, 0.75]));
        assert!(close(Difference.apply(b, s), [0.25, 0.0, 0.25]));
        assert!(close(Exclusion.apply(b, s), [0.5, 0.5, 0.5]));
        assert!(close(LinearDodge.apply(b, s), [0.75, 1.0, 1.0]));
        assert!(close(LinearBurn.apply(b, s), [0.0, 0.0, 0.25]));
        assert!(close(Subtract.apply(b, s), [0.0, 0.0, 0.25]));
        assert!(close(Overlay.apply(b, s), b));
        assert!(close(HardLight.apply(b, s), b));
        assert!(close(SoftLight.apply(b, s), b));
        assert!(close(HardMix.apply(b, s), [0.0, 0.0, 1.0]));
    }

    #[test]
    fn dodge_and_burn_handle_extremes() {
        assert!(close(ColorDodge.apply([0.0; 3], [1.0; 3]), [0.0; 3]));
        assert!(close(ColorDodge.apply([0.5; 3], [1.0; 3]), [1.0; 3]));
        assert!(close(ColorBurn.apply([1.0; 3], [0.0; 3]), [1.0; 3]));
        assert!(close(ColorBurn.apply([0.5; 3], [0.0; 3]), [0.0; 3]));
        assert!(close(Divide.apply([0.5; 3], [0.0; 3]), [1.0; 3]));
    }

    #[test]
    fn non_separable_modes_preserve_their_component() {
        let b = [0.2, 0.4, 0.6];
        let s = [0.9, 0.1, 0.1];
        let lum = |c: [f32; 3]| 0.3 * c[0] + 0.59 * c[1] + 0.11 * c[2];
        assert!((lum(Color.apply(b, s)) - lum(b)).abs() < 1e-4);
        assert!((lum(Luminosity.apply(b, s)) - lum(s)).abs() < 1e-4);
        assert!(close(Saturation.apply([0.5; 3], s), [0.5; 3]));
        assert!(close(DarkerColor.apply(b, s), s));
        assert!(close(LighterColor.apply(b, s), b));
    }

    #[test]
    fn normal_returns_source() {
        assert!(close(Normal.apply([0.1; 3], [0.7; 3]), [0.7; 3]));
    }
}
