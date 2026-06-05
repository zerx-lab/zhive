//! Per-message render memoization for finalized transcript items.
//!
//! The transcript is rebuilt on every `terminal.draw`, and once code blocks
//! carry `syntect` highlighting a full re-render per frame is expensive
//! (re-parsing + re-highlighting every historical code block).  This cache
//! keys [`markdown::render`](crate::markdown::render) output by a content hash
//! so an unchanged message costs a clone instead of a re-highlight; it is
//! cleared whenever the palette changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::markdown;
use crate::theme::Palette;

/// Memoizes finalized-message markdown rendering, keyed by content hash.
///
/// Keying on content (not item id) is deliberate: turn-internal item-id reuse
/// has caused stale-render bugs before, and a content hash is self-correcting.
#[derive(Debug, Default)]
pub(crate) struct MarkdownCache {
    map: RefCell<HashMap<u64, Vec<Line<'static>>>>,
}

impl MarkdownCache {
    /// Returns the rendered lines for `text`, rendering and caching on a miss.
    ///
    /// The returned `Vec` is owned (cloned from the cache) because the caller
    /// re-wraps it to the current width every frame; wrapping happens after
    /// render, so the cache is width-independent.
    pub(crate) fn render(&self, text: &str, palette: &Palette) -> Vec<Line<'static>> {
        let key = hash_text(text);
        if let Some(cached) = self.map.borrow().get(&key) {
            return cached.clone();
        }
        let lines = markdown::render(text, palette);
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
        let first = cache.render("hello **world**", &p);
        let second = cache.render("hello **world**", &p);
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn clear_drops_entries() {
        let cache = MarkdownCache::default();
        let p = Palette::default();
        let _ = cache.render("x", &p);
        cache.clear();
        assert!(cache.map.borrow().is_empty());
    }

    #[test]
    fn distinct_text_renders_independently() {
        let cache = MarkdownCache::default();
        let p = Palette::default();
        let a = cache.render("# Heading", &p);
        let b = cache.render("plain", &p);
        assert!(!a.is_empty() && !b.is_empty());
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
        let _ = markdown::render(&src, &p);

        let t0 = Instant::now();
        for _ in 0..10 {
            let _ = markdown::render(&src, &p);
        }
        let cold = t0.elapsed() / 10;

        let _ = cache.render(&src, &p);
        let t1 = Instant::now();
        for _ in 0..10 {
            let _ = cache.render(&src, &p);
        }
        let warm = t1.elapsed() / 10;

        println!("[perf-gate] cold(full render)={cold:?}  warm(cached)={warm:?}");
        assert!(
            warm.as_millis() < 9,
            "cached per-block render must be <9ms (≈10 blocks under 90ms tick), got {warm:?}"
        );
    }
}
