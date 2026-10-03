//! Layer styles: drop shadow, outer glow, stroke and color overlay.

use super::distance::{downsample, Distances, SS};
use crate::blend::BlendMode;
use crate::psd::descriptor::{Descriptor, Value};

/// Gaussian sigma per pixel of effect size, matching Photoshop's soft shadows and glows.
pub(crate) const SIGMA_PER_SIZE: f64 = 0.45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StrokePosition {
    Outside,
    Inside,
    Center,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shadow {
    pub color: [f32; 3],
    pub opacity: f32,
    pub mode: BlendMode,
    pub angle: f64,
    pub distance: f64,
    pub spread: f64,
    pub size: f64,
    pub knocked_out: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Glow {
    pub color: [f32; 3],
    pub opacity: f32,
    pub mode: BlendMode,
    pub spread: f64,
    pub size: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stroke {
    pub color: [f32; 3],
    pub opacity: f32,
    pub mode: BlendMode,
    pub size: f64,
    pub position: StrokePosition,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Overlay {
    pub color: [f32; 3],
    pub opacity: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Effects {
    pub shadows: Vec<Shadow>,
    pub glows: Vec<Glow>,
    pub strokes: Vec<Stroke>,
    pub overlays: Vec<Overlay>,
    pub unsupported: Vec<&'static str>,
}

impl Effects {
    /// How far effects can reach outside the layer's own pixels.
    pub fn reach(&self) -> f64 {
        let shadows = self.shadows.iter().map(|s| s.size + s.distance);
        let glows = self.glows.iter().map(|g| g.size);
        let strokes = self.strokes.iter().map(|s| s.size);
        shadows.chain(glows).chain(strokes).map(|r| r + 2.0).fold(0.0, f64::max)
    }

    pub fn needs_distances(&self) -> bool {
        !self.strokes.is_empty()
            || self.shadows.iter().any(|s| s.spread > 0.0)
            || self.glows.iter().any(|g| g.spread > 0.0)
    }

    pub fn needs_inward(&self) -> bool {
        self.strokes.iter().any(|s| s.position != StrokePosition::Outside)
    }

    /// Largest distance any effect queries.
    pub fn max_distance(&self) -> f64 {
        let strokes = self.strokes.iter().map(|s| s.size);
        let shadows = self.shadows.iter().map(|s| s.spread);
        let glows = self.glows.iter().map(|g| g.spread);
        strokes.chain(shadows).chain(glows).fold(0.0, f64::max)
    }
}

fn hsb_to_rgb(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let i = h.floor();
    let f = h - i;
    let (p, q, t) = (v * (1.0 - s), v * (1.0 - f * s), v * (1.0 - (1.0 - f) * s));
    match i as i32 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

fn color(d: &Descriptor) -> [f32; 3] {
    let Some(c) = d.desc("Clr ") else { return [0.0; 3] };
    let rgb = if let (Some(r), Some(g), Some(b)) = (c.num("Rd  "), c.num("Grn "), c.num("Bl  ")) {
        [r / 255.0, g / 255.0, b / 255.0]
    } else if let Some(g) = c.num("Gry ") {
        [1.0 - g / 100.0; 3]
    } else if let (Some(h), Some(s), Some(b)) = (c.num("H   "), c.num("Strt"), c.num("Brgh")) {
        hsb_to_rgb(h / 360.0, s / 100.0, b / 100.0)
    } else if let (Some(cy), Some(m), Some(y), Some(k)) = (c.num("Cyn "), c.num("Mgnt"), c.num("Ylw "), c.num("Blck")) {
        let k = 1.0 - k / 100.0;
        [(1.0 - cy / 100.0) * k, (1.0 - m / 100.0) * k, (1.0 - y / 100.0) * k]
    } else {
        [0.0; 3]
    };
    rgb.map(|v| v.clamp(0.0, 1.0) as f32)
}

fn mode(d: &Descriptor) -> BlendMode {
    BlendMode::from_key(d.enumerated("Md  ").unwrap_or("Nrml").as_bytes())
}

fn opacity(d: &Descriptor, default: f64) -> f32 {
    (d.num("Opct").unwrap_or(default) / 100.0) as f32
}

/// Enabled instances of an effect: the multi-instance list when present, else the single entry.
fn enabled<'a>(fx: &'a Descriptor, single: &str, multi: Option<&str>) -> Vec<&'a Descriptor> {
    let all: Vec<&Descriptor> = match multi.and_then(|m| fx.list(m)) {
        Some(list) => list
            .iter()
            .filter_map(|v| match v {
                Value::Descriptor(d) => Some(d),
                _ => None,
            })
            .collect(),
        None => fx.desc(single).into_iter().collect(),
    };
    all.into_iter().filter(|d| d.bool("enab").unwrap_or(false)).collect()
}

const UNSUPPORTED: [(&str, Option<&str>, &str); 6] = [
    ("IrSh", Some("innerShadowMulti"), "inner shadow"),
    ("IrGl", None, "inner glow"),
    ("ebbl", None, "bevel and emboss"),
    ("ChFX", None, "satin"),
    ("GrFl", Some("gradientFillMulti"), "gradient overlay"),
    ("patternFill", None, "pattern overlay"),
];

/// Reads an `lfx2`/`lmfx` descriptor. `global_angle` is the document's global light angle.
pub(crate) fn parse(fx: &Descriptor, global_angle: f64) -> Effects {
    let mut e = Effects::default();
    if fx.bool("masterFXSwitch") == Some(false) {
        return e;
    }
    let scale = fx.num("Scl ").unwrap_or(100.0) / 100.0;
    for d in enabled(fx, "DrSh", Some("dropShadowMulti")) {
        let size = d.num("blur").unwrap_or(5.0) * scale;
        let angle = if d.bool("uglg").unwrap_or(true) { global_angle } else { d.num("lagl").unwrap_or(120.0) };
        e.shadows.push(Shadow {
            color: color(d),
            opacity: opacity(d, 75.0),
            mode: mode(d),
            angle,
            distance: d.num("Dstn").unwrap_or(0.0) * scale,
            spread: (d.num("Ckmt").unwrap_or(0.0) / 100.0 * size).min(size),
            size,
            knocked_out: d.bool("layerConceals").unwrap_or(true),
        });
    }
    for d in enabled(fx, "OrGl", Some("outerGlowMulti")) {
        let size = d.num("blur").unwrap_or(5.0) * scale;
        e.glows.push(Glow {
            color: color(d),
            opacity: opacity(d, 75.0),
            mode: mode(d),
            spread: d.num("Ckmt").unwrap_or(0.0) / 100.0 * size,
            size,
        });
    }
    for d in enabled(fx, "FrFX", Some("frameFXMulti")) {
        if d.enumerated("PntT").is_some_and(|p| p != "SClr") {
            e.unsupported.push("gradient or pattern stroke (drawn with its color)");
        }
        e.strokes.push(Stroke {
            color: color(d),
            opacity: opacity(d, 100.0),
            mode: mode(d),
            size: d.num("Sz  ").unwrap_or(3.0) * scale,
            position: match d.enumerated("Styl") {
                Some("InsF") => StrokePosition::Inside,
                Some("CtrF") => StrokePosition::Center,
                _ => StrokePosition::Outside,
            },
        });
    }
    for d in enabled(fx, "SoFi", Some("solidFillMulti")) {
        e.overlays.push(Overlay { color: color(d), opacity: opacity(d, 100.0) });
    }
    for (single, multi, name) in UNSUPPORTED {
        if !enabled(fx, single, multi).is_empty() {
            e.unsupported.push(name);
        }
    }
    e
}

/// Coverage of a stroke of `size` pixels at `position` around the shape.
pub(crate) fn stroke_coverage(d: &Distances, w: usize, h: usize, size: f64, position: StrokePosition) -> Vec<f32> {
    let s = (size * SS as f64) as f32;
    match position {
        StrokePosition::Outside => downsample(|i| d.inside[i] || d.outside[i] <= s * s, w, h),
        StrokePosition::Inside => downsample(|i| d.inside[i] && d.inward[i] <= s * s, w, h),
        StrokePosition::Center => {
            let half = s * s / 4.0;
            downsample(|i| if d.inside[i] { d.inward[i] <= half } else { d.outside[i] <= half }, w, h)
        }
    }
}

/// Coverage of the shape grown by `radius` pixels.
pub(crate) fn dilate(d: &Distances, w: usize, h: usize, radius: f64) -> Vec<f32> {
    let s = (radius * SS as f64) as f32;
    downsample(|i| d.inside[i] || d.outside[i] <= s * s, w, h)
}

#[cfg(test)]
mod tests {
    use super::super::distance::distances;
    use super::*;

    fn unit(v: f64) -> Value {
        Value::Unit("#Pxl".into(), v)
    }

    fn desc(items: Vec<(&str, Value)>) -> Descriptor {
        Descriptor { class: String::new(), items: items.into_iter().map(|(k, v)| (k.to_string(), v)).collect() }
    }

    fn rgb(r: f64, g: f64, b: f64) -> Value {
        Value::Descriptor(desc(vec![
            ("Rd  ", Value::Number(r)),
            ("Grn ", Value::Number(g)),
            ("Bl  ", Value::Number(b)),
        ]))
    }

    #[test]
    fn parses_shadow_and_stroke() {
        let shadow = desc(vec![
            ("enab", Value::Bool(true)),
            ("Md  ", Value::Enum("BlnM".into(), "Mltp".into())),
            ("Clr ", rgb(255.0, 0.0, 0.0)),
            ("Opct", Value::Unit("#Prc".into(), 50.0)),
            ("uglg", Value::Bool(false)),
            ("lagl", Value::Unit("#Ang".into(), 90.0)),
            ("Dstn", unit(4.0)),
            ("Ckmt", Value::Unit("#Prc".into(), 25.0)),
            ("blur", unit(8.0)),
        ]);
        let stroke = desc(vec![
            ("enab", Value::Bool(true)),
            ("Styl", Value::Enum("FStl".into(), "InsF".into())),
            ("Sz  ", unit(3.0)),
            ("Clr ", rgb(0.0, 0.0, 255.0)),
        ]);
        let disabled_glow = desc(vec![("enab", Value::Bool(false))]);
        let fx = desc(vec![
            ("Scl ", Value::Unit("#Prc".into(), 200.0)),
            ("DrSh", Value::Descriptor(shadow)),
            ("FrFX", Value::Descriptor(stroke)),
            ("OrGl", Value::Descriptor(disabled_glow)),
        ]);
        let e = parse(&fx, 120.0);
        assert_eq!(e.shadows.len(), 1);
        let s = &e.shadows[0];
        assert_eq!(s.color, [1.0, 0.0, 0.0]);
        assert_eq!(s.mode, BlendMode::Multiply);
        assert_eq!((s.angle, s.distance, s.size, s.spread), (90.0, 8.0, 16.0, 4.0));
        assert_eq!(s.opacity, 0.5);
        assert_eq!(e.strokes[0].position, StrokePosition::Inside);
        assert_eq!(e.strokes[0].size, 6.0);
        assert!(e.glows.is_empty());
        assert!(e.needs_distances() && e.needs_inward());
        assert_eq!(e.reach(), 26.0);
        assert_eq!(e.max_distance(), 6.0);
    }

    #[test]
    fn master_switch_disables_everything() {
        let fx = desc(vec![("masterFXSwitch", Value::Bool(false)), ("SoFi", Value::Descriptor(desc(vec![])))]);
        assert_eq!(parse(&fx, 120.0), Effects::default());
    }

    #[test]
    fn reports_unsupported_effects() {
        let bevel = desc(vec![("enab", Value::Bool(true))]);
        let e = parse(&desc(vec![("ebbl", Value::Descriptor(bevel))]), 120.0);
        assert_eq!(e.unsupported, ["bevel and emboss"]);
    }

    #[test]
    fn colors_from_other_models() {
        let gray = desc(vec![("Clr ", Value::Descriptor(desc(vec![("Gry ", Value::Number(100.0))])))]);
        assert_eq!(color(&gray), [0.0; 3]);
        let hsb = desc(vec![(
            "Clr ",
            Value::Descriptor(desc(vec![
                ("H   ", Value::Number(120.0)),
                ("Strt", Value::Number(100.0)),
                ("Brgh", Value::Number(100.0)),
            ])),
        )]);
        assert_eq!(color(&hsb), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn stroke_rings_have_expected_area() {
        let (w, h) = (21, 21);
        let mut a = vec![0f32; w * h];
        for y in 8..13 {
            for x in 8..13 {
                a[y * w + x] = 1.0;
            }
        }
        let d = distances(&a, w, h, true, 3.0);
        let area = |v: Vec<f32>| v.iter().sum::<f32>();
        let outside = area(stroke_coverage(&d, w, h, 2.0, StrokePosition::Outside));
        let inside = area(stroke_coverage(&d, w, h, 1.0, StrokePosition::Inside));
        let grown = area(dilate(&d, w, h, 2.0));
        assert!((outside - grown).abs() < 1e-3);
        assert!(outside > 25.0 + 4.0 * 5.0 * 2.0 && outside < 81.0);
        assert!((inside - (25.0 - 9.0)).abs() < 1.0);
    }
}
