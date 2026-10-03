//! Smart objects: re-rendered from their embedded document where possible.

use super::canvas::Raster;
use super::Ctx;
use crate::psd::Layer;

/// Renders the embedded source of smart object `l`, or `None` to use its cached pixels.
pub(crate) fn render(_ctx: &Ctx, _l: &Layer, _warnings: &mut Vec<String>) -> Option<Raster> {
    None
}
