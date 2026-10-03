//! The Photoshop features a document uses, for coverage reports.

use std::collections::BTreeSet;

use super::descriptor::{self, Descriptor, Value};
use super::{Document, Layer, LayerKind};
use crate::blend::BlendMode;

const EFFECTS: [(&str, &str); 15] = [
    ("DrSh", "effect: drop shadow"),
    ("dropShadowMulti", "effect: drop shadow"),
    ("IrSh", "effect: inner shadow"),
    ("innerShadowMulti", "effect: inner shadow"),
    ("OrGl", "effect: outer glow"),
    ("IrGl", "effect: inner glow"),
    ("ebbl", "effect: bevel and emboss"),
    ("ChFX", "effect: satin"),
    ("SoFi", "effect: color overlay"),
    ("solidFillMulti", "effect: color overlay"),
    ("GrFl", "effect: gradient overlay"),
    ("gradientFillMulti", "effect: gradient overlay"),
    ("patternFill", "effect: pattern overlay"),
    ("FrFX", "effect: stroke"),
    ("frameFXMulti", "effect: stroke"),
];

const ADJUSTMENTS: [(&[u8; 4], &str); 18] = [
    (b"levl", "levels"),
    (b"curv", "curves"),
    (b"brit", "brightness/contrast"),
    (b"CgEd", "brightness/contrast"),
    (b"hue2", "hue/saturation"),
    (b"hue ", "hue/saturation"),
    (b"blnc", "color balance"),
    (b"vibA", "vibrance"),
    (b"expA", "exposure"),
    (b"selc", "selective color"),
    (b"mixr", "channel mixer"),
    (b"grdm", "gradient map"),
    (b"phfl", "photo filter"),
    (b"nvrt", "invert"),
    (b"post", "posterize"),
    (b"thrs", "threshold"),
    (b"clrL", "color lookup"),
    (b"blwh", "black & white"),
];

fn enabled(d: &Descriptor) -> bool {
    d.bool("enab") != Some(false)
}

fn effects(l: &Layer, out: &mut BTreeSet<String>) {
    let Some(fx) =
        l.block(b"lmfx").or(l.block(b"lfx2")).or(l.block(b"lfxs")).and_then(|b| descriptor::parse_block(b, 8).ok())
    else {
        return;
    };
    if fx.bool("masterFXSwitch") == Some(false) {
        return;
    }
    for (key, name) in EFFECTS {
        let on = match fx.get(key) {
            Some(Value::Descriptor(d)) => enabled(d),
            Some(Value::List(items)) => items.iter().any(|v| matches!(v, Value::Descriptor(d) if enabled(d))),
            _ => false,
        };
        if on {
            out.insert(name.into());
        }
    }
}

impl Document {
    /// Names of the Photoshop features this document uses (color mode, depth, layer kinds, blend
    /// modes, masks, effects, adjustments...), for coverage reports.
    #[doc(hidden)]
    pub fn features(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        out.insert(format!("color mode: {:?}", self.color_mode));
        out.insert(format!("depth: {}", self.depth));
        for l in self.layers.iter().filter(|l| !l.hidden) {
            let mut add = |s: &str| {
                out.insert(s.to_owned());
            };
            match l.kind {
                LayerKind::Pixel => add("layer: pixel"),
                LayerKind::Text => {
                    add("layer: text");
                    if crate::text::TextLayer::parse(l.block(b"TySh").unwrap_or_default())
                        .is_ok_and(|t| !t.warp.is_identity())
                    {
                        add("text: warp");
                    }
                }
                LayerKind::Fill => {
                    let fill = if l.block(b"GdFl").is_some() {
                        "fill: gradient"
                    } else if l.block(b"PtFl").is_some() {
                        "fill: pattern"
                    } else {
                        "fill: solid"
                    };
                    add(fill);
                    if l.block(b"vmsk").or(l.block(b"vsms")).is_some() {
                        add("layer: shape");
                    }
                    if l.block(b"vstk").is_some() {
                        add("shape: stroke");
                    }
                }
                LayerKind::Adjustment => {
                    for (key, name) in ADJUSTMENTS {
                        if l.block(key).is_some() {
                            add(&format!("adjustment: {name}"));
                            break;
                        }
                    }
                }
                LayerKind::SmartObject => {
                    add("layer: smart object");
                    if let Some(p) = l.placed() {
                        if p.desc("warp").is_some_and(|w| w.enumerated("warpStyle").is_some_and(|s| s != "warpNone")) {
                            add("smart object: warp");
                        }
                        if p.desc("filterFX").is_some() {
                            add("smart object: smart filters");
                        }
                    }
                }
                LayerKind::Group => {
                    add("layer: group");
                    if l.blend_mode == BlendMode::PassThrough {
                        add("group: pass-through");
                    }
                    if [b"artb", b"artd", b"abdd"].iter().any(|k| l.block(k).is_some()) {
                        add("group: artboard");
                    }
                }
                LayerKind::GroupEnd => {}
            }
            if l.kind == LayerKind::GroupEnd {
                continue;
            }
            if !matches!(l.blend_mode, BlendMode::Normal | BlendMode::PassThrough) {
                add(&format!("blend: {:?}", l.blend_mode));
            }
            if l.opacity < 255 {
                add("opacity");
            }
            if l.fill_opacity < 255 {
                add("fill opacity");
            }
            if l.clipping {
                add("clipping mask");
            }
            if let Some(m) = l.mask.as_ref().filter(|m| !m.disabled) {
                add("layer mask");
                if m.feather > 0.0 || m.density < 1.0 {
                    add("mask: feather/density");
                }
            }
            if l.kind != LayerKind::Fill && l.block(b"vmsk").or(l.block(b"vsms")).is_some() {
                add("vector mask");
            }
            if l.block(b"knko").and_then(|b| b.first().copied()).unwrap_or(0) > 0 {
                add("knockout");
            }
            if l.blend_if {
                add("blend if");
            }
            if l.block(b"brst").is_some_and(|b| !b.is_empty()) {
                add("channel restrictions");
            }
            effects(l, &mut out);
        }
        out.into_iter().collect()
    }
}
