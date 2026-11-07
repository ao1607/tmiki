//! The warnings tmi prints to stderr while importing, with the
//! de-duplication of clip.py's `_warn_*` functions (once per process for an
//! offscreen, a tile size, a composite id, a filter id).

use std::collections::HashSet;

#[derive(Default)]
pub struct Warnings {
    /// Lines for stderr, in order (without the newline).
    pub lines: Vec<String>,
    offscreens: HashSet<String>,
    tile_sizes: HashSet<usize>,
    composites: HashSet<i64>,
    filters: HashSet<u32>,
}

const FILTER_NAMES: [&str; 10] = [
    "?",
    "brightness_contrast",
    "levels",
    "tone_curve",
    "hue_saturation",
    "color_balance",
    "invert",
    "posterize",
    "binarize",
    "gradient_map",
];

/// Marks a line of `Warnings::lines` that Python prints to stdout (repo.py
/// `_apply_layer_ignore`'s notes use `print()`).
pub const STDOUT_MARK: &str = "\u{0}stdout\u{0}";

impl Warnings {
    /// A line Python prints to stdout, in order with the stderr ones.
    pub fn stdout(&mut self, line: String) {
        self.lines.push(format!("{STDOUT_MARK}{line}"));
    }

    /// `_warn_offscreen_failure(outer_id, why)` (`why`: the text after the colon).
    pub fn offscreen(&mut self, outer_id: &str, why: &str) {
        if self.offscreens.insert(outer_id.to_string()) {
            self.lines.push(format!("tmiki: skipping unreadable CLIP offscreen {outer_id}: {why}"));
        }
    }

    /// `_warn_unknown_tile_size(bpt, pixels)`.
    pub fn tile_size(&mut self, bpt: usize, pixels: usize) {
        if self.tile_sizes.insert(bpt) {
            self.lines.push(format!(
                "tmiki: unknown CLIP tile size {bpt} for {pixels}-pixel block; rendering as transparent"
            ));
        }
    }

    /// `_warn_unknown_composite(id)`.
    pub fn composite(&mut self, id: i64) {
        if self.composites.insert(id) {
            self.lines.push(format!("tmiki: unknown CLIP composite id {id}; rendering as normal"));
        }
    }

    /// `_warn_unsupported_filter(id)`.
    pub fn filter(&mut self, id: u32) {
        if self.filters.insert(id) {
            let name = FILTER_NAMES.get(id as usize).filter(|_| id >= 1).copied().unwrap_or("?");
            self.lines.push(format!(
                "tmiki: CLIP correction-layer filter {id} ({name}) not yet rendered; passing the backdrop through unchanged"
            ));
        }
    }
}
