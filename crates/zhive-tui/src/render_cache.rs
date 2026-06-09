//! Per-message render memoization for finalized transcript items.
//!
//! The transcript is rebuilt on every `terminal.draw`, and once code blocks
//! carry `syntect` highlighting a full re-render per frame is expensive
//! (re-parsing + re-highlighting every historical code block).  This cache
//! keys [`markdown::render`](crate::markdown::render) output by a content hash
//! plus the layout width (tables are laid out to width during render) so an
//! unchanged message at an unchanged width costs a clone instead of a
//! re-highlight; it is cleared whenever the palette changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::markdown;
use crate::theme::Palette;

/// Memoizes finalized-message markdown rendering, keyed by content hash + width.
///
/// Keying on content (not item id) is deliberate: turn-internal item-id reuse
/// has caused stale-render bugs before, and a content hash is self-correcting.
/// The render width is part of the key because tables are laid out to the
/// available width during rendering, so a resize must not reuse a stale layout.
#[derive(Debug, Default)]
pub(crate) struct MarkdownCache {
    map: RefCell<HashMap<(u64, u16), Vec<Line<'static>>>>,
}

impl MarkdownCache {
    /// Returns the rendered lines for `text` at `width`, rendering and caching
    /// on a miss.
    ///
    /// The returned `Vec` is owned (cloned from the cache) because the caller
    /// re-wraps non-table text to the current width every frame; table layout,
    /// however, depends on `width`, so it is folded into the cache key.
    pub(crate) fn render(&self, text: &str, palette: &Palette, width: u16) -> Vec<Line<'static>> {
        let key = (hash_text(text), width);
        if let Some(cached) = self.map.borrow().get(&key) {
            return cached.clone();
        }
        let lines = markdown::render(text, palette, width);
        self.map.borrow_mut().insert(key, lines.clone());
        lines
    }

    /// Drops all cached renders (call when the palette changes).
    pub(crate) fn clear(&self) {
        self.map.borrow_mut().clear();
    }
}

/// Hashes `text` with the default `SipHasher` for use as a cache key.
fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_render_hits_cache_and_matches() {
        let cache = MarkdownCache::default();
        let p = Palette::default();
        let first = cache.render("hello **world**", &p, 80);
        let second = cache.render("hello **world**", &p, 80);
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn clear_drops_entries() {
        let cache = MarkdownCache::default();
        let p = Palette::default();
        let _ = cache.render("x", &p, 80);
        cache.clear();
        assert!(cache.map.borrow().is_empty());
    }

    #[test]
    fn distinct_text_renders_independently() {
        let cache = MarkdownCache::default();
        let p = Palette::default();
        let a = cache.render("# Heading", &p, 80);
        let b = cache.render("plain", &p, 80);
        assert!(!a.is_empty() && !b.is_empty());
        assert_eq!(cache.map.borrow().len(), 2);
    }

    #[test]
    fn same_text_at_different_widths_caches_separately() {
        // Table layout depends on width, so the same source at two widths must
        // produce two distinct cache entries rather than reusing a stale layout.
        let cache = MarkdownCache::default();
        let p = Palette::default();
        let src = "| a | b |\n|---|---|\n| 1 | 2 |";
        let _ = cache.render(src, &p, 80);
        let _ = cache.render(src, &p, 30);
        assert_eq!(cache.map.borrow().len(), 2);
    }

    /// Perf gate: a cached render of a large code block must cost a clone, not a
    /// full `syntect` re-highlight, so the per-tick transcript rebuild stays
    /// well under the 90ms tick even with several historical blocks.
    #[test]
    #[ignore = "perf gate; run manually with --ignored --nocapture"]
    fn perf_gate_cached_render_far_under_tick() {
        use std::fmt::Write as _;
        use std::time::Instant;
        let mut src = String::from("Here is some code:\n\n```rust\n");
        for i in 0..200 {
            let _ = writeln!(
                src,
                "fn function_{i}(x: u64) -> u64 {{ let y = x * 2; y + {i} }}"
            );
        }
        src.push_str("```\n");
        let p = Palette::default();
        let cache = MarkdownCache::default();

        // Warm up the global highlighter so its one-time asset load is excluded.
        let _ = markdown::render(&src, &p, 80);

        let t0 = Instant::now();
        for _ in 0..10 {
            let _ = markdown::render(&src, &p, 80);
        }
        let cold = t0.elapsed() / 10;

        let _ = cache.render(&src, &p, 80);
        let t1 = Instant::now();
        for _ in 0..10 {
            let _ = cache.render(&src, &p, 80);
        }
        let warm = t1.elapsed() / 10;

        println!("[perf-gate] cold(full render)={cold:?}  warm(cached)={warm:?}");
        assert!(
            warm.as_millis() < 9,
            "cached per-block render must be <9ms (≈10 blocks under 90ms tick), got {warm:?}"
        );
    }
}
