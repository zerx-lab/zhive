//! Workspace file indexing and fuzzy matching for the `@`-mention picker.
//!
//! [`scan`] walks the working directory once (skipping hidden and known-heavy
//! directories) into a flat list of project-relative paths; [`fuzzy_filter`]
//! ranks that list against a query so the composer's `@` popup can surface the
//! closest files and folders as the user types. Kept dependency-free: the walk
//! uses [`std::fs`] and the scorer is a small subsequence matcher, so the TUI
//! still depends only on `zhive-proto` and the native client (D-002).

use std::path::Path;

/// Upper bound on indexed entries, so a huge tree cannot stall the first `@`.
const MAX_ENTRIES: usize = 20_000;

/// Directory names skipped wholesale during the walk (build/vendor caches).
///
/// Hidden entries (names starting with `.`, e.g. `.git`) are skipped
/// separately, so they are intentionally absent here.
const SKIP_DIRS: &[&str] = &["target", "node_modules"];

/// Walks `root` into a sorted list of project-relative file and folder paths.
///
/// Directories are suffixed with `/` so a folder mention is visually distinct
/// and selectable. Hidden entries and [`SKIP_DIRS`] are pruned, and the walk
/// stops once [`MAX_ENTRIES`] paths are collected. Unreadable directories are
/// skipped rather than failing the whole scan. Path separators are normalized
/// to `/` so the same index works regardless of platform.
#[must_use]
pub(crate) fn scan(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.len() >= MAX_ENTRIES {
                break;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Hidden dotfiles/dirs stay out of the picker by default.
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if is_dir && SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let mut rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.is_empty() {
                continue;
            }
            if is_dir {
                rel_str.push('/');
                out.push(rel_str);
                stack.push(path);
            } else {
                out.push(rel_str);
            }
        }
    }
    out.sort();
    out
}

/// Ranks `files` against `query`, best match first; empty `query` keeps all.
///
/// Each path is scored by [`fuzzy_score`]; non-matches are dropped. Ties break
/// toward shorter paths, then lexically, so results stay stable between the key
/// handler and the renderer (both call this with identical inputs).
#[must_use]
pub(crate) fn fuzzy_filter<'a>(files: &'a [String], query: &str) -> Vec<&'a str> {
    if query.is_empty() {
        return files.iter().map(String::as_str).collect();
    }
    let mut scored: Vec<(i32, &str)> = files
        .iter()
        .filter_map(|f| fuzzy_score(f, query).map(|s| (s, f.as_str())))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.len().cmp(&b.1.len()))
            .then_with(|| a.1.cmp(b.1))
    });
    scored.into_iter().map(|(_, f)| f).collect()
}

/// Scores `needle` as a subsequence of `haystack`, or `None` if it is not one.
///
/// Matching is case-insensitive. The score rewards matches at path boundaries
/// (start, or just after `/`, `_`, `-`, `.`, space) and consecutive runs, and
/// lightly penalizes longer paths so a tight match on a short path outranks a
/// scattered match on a long one.
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    let hay: Vec<char> = haystack.chars().collect();
    let mut score: i32 = 0;
    let mut hi: usize = 0;
    let mut prev_match = false;
    for nc in needle.chars() {
        let target = nc.to_ascii_lowercase();
        let mut found = false;
        while hi < hay.len() {
            let hc = hay[hi];
            if hc.to_ascii_lowercase() == target {
                let boundary = hi == 0 || matches!(hay[hi - 1], '/' | '_' | '-' | '.' | ' ');
                score += 1;
                if prev_match {
                    score += 5;
                }
                if boundary {
                    score += 10;
                }
                hi += 1;
                prev_match = true;
                found = true;
                break;
            }
            prev_match = false;
            hi += 1;
        }
        if !found {
            return None;
        }
    }
    // Favor shorter paths among otherwise comparable matches.
    score -= i32::try_from(hay.len()).unwrap_or(i32::MAX) / 8;
    Some(score)
}

/// Largest file slice inlined for a single `@file` mention.
const MAX_FILE_BYTES: usize = 64 * 1024;

/// Largest number of directory entries listed for a single `@dir/` mention.
const MAX_DIR_ENTRIES: usize = 200;

/// Extracts the path of every whitespace-delimited `@<path>` token in `text`.
///
/// A mention is a standalone word starting with `@`, so an embedded `@` (such
/// as an email like `name@host`) is not treated as one. Trailing `/` and other
/// characters are kept verbatim; resolution is left to [`expand_mentions`].
pub(crate) fn mention_paths(text: &str) -> Vec<&str> {
    text.split(char::is_whitespace)
        .filter_map(|word| word.strip_prefix('@'))
        .filter(|path| !path.is_empty())
        .collect()
}

/// Inlines the contents of every resolvable `@<path>` mention after `text`.
///
/// For each unique mention that resolves to a real file or directory **inside**
/// `root`, appends a tagged block: a file's (capped) text or a directory's
/// immediate entries. Mentions that do not resolve, or that escape `root` via
/// `..`/symlinks, are left untouched so a stray `@word` is never leaked or
/// fabricated. Returns `text` unchanged when nothing resolves.
///
/// This is what gives zhive's `@`-mentions opencode-style behavior while keeping
/// its what-you-see-is-what-is-sent contract: the resolved content is both shown
/// in the transcript and sent to the model.
pub(crate) fn expand_mentions(text: &str, root: &Path) -> String {
    // Resolve the root once; if it cannot be canonicalized we cannot safely
    // bound traversal, so decline to expand anything.
    let Ok(canon_root) = root.canonicalize() else {
        return text.to_owned();
    };
    let mut seen = std::collections::BTreeSet::new();
    let mut blocks = String::new();
    for raw in mention_paths(text) {
        let rel = raw.trim_end_matches('/');
        if rel.is_empty() || !seen.insert(rel.to_owned()) {
            continue;
        }
        let Ok(canon) = root.join(rel).canonicalize() else {
            continue;
        };
        // Keep the reference within the workspace (block `../` / symlink escapes).
        if !canon.starts_with(&canon_root) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&canon) else {
            continue;
        };
        if meta.is_dir() {
            blocks.push_str(&render_dir_block(rel, &canon));
        } else if meta.is_file() {
            blocks.push_str(&render_file_block(rel, &canon));
        }
    }
    if blocks.is_empty() {
        text.to_owned()
    } else {
        format!("{}\n\n{}", text, blocks.trim_end())
    }
}

/// Renders a `<file type="file">` block holding `path`'s capped text contents.
fn render_file_block(rel: &str, abs: &Path) -> String {
    let body =
        std::fs::read_to_string(abs).unwrap_or_else(|_| "[binary or unreadable file]".to_owned());
    let (body, truncated) = if body.len() > MAX_FILE_BYTES {
        let mut end = MAX_FILE_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        (&body[..end], true)
    } else {
        (body.as_str(), false)
    };
    let suffix = if truncated {
        "\n\u{2026} [truncated]"
    } else {
        ""
    };
    format!("<file path=\"{rel}\" type=\"file\">\n{body}{suffix}\n</file>\n")
}

/// Renders a `<file type="directory">` block listing `path`'s immediate entries.
fn render_dir_block(rel: &str, abs: &Path) -> String {
    let mut names: Vec<String> = Vec::new();
    if let Ok(read) = std::fs::read_dir(abs) {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                names.push(format!("{name}/"));
            } else {
                names.push(name);
            }
        }
    }
    names.sort();
    let total = names.len();
    let shown = total.min(MAX_DIR_ENTRIES);
    let mut listing = names[..shown].join("\n");
    if total > shown {
        use std::fmt::Write as _;
        let _ = write!(listing, "\n\u{2026} ({} more)", total - shown);
    }
    format!("<file path=\"{rel}/\" type=\"directory\">\n{listing}\n</file>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_keeps_every_file_in_order() {
        let files = vec!["a.rs".to_owned(), "b.rs".to_owned()];
        assert_eq!(fuzzy_filter(&files, ""), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn fuzzy_matches_subsequence_across_separators() {
        let files = vec!["src/main.rs".to_owned(), "README.md".to_owned()];
        // "smr" hits s(rc)/m(ain).r(s) as a subsequence; README does not.
        assert_eq!(fuzzy_filter(&files, "smr"), vec!["src/main.rs"]);
    }

    #[test]
    fn non_subsequence_is_rejected() {
        let files = vec!["src/main.rs".to_owned()];
        assert!(fuzzy_filter(&files, "zzz").is_empty());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let files = vec!["Cargo.toml".to_owned()];
        assert_eq!(fuzzy_filter(&files, "cargo"), vec!["Cargo.toml"]);
    }

    #[test]
    fn boundary_match_outranks_scattered_match() {
        let files = vec!["abc/config.rs".to_owned(), "cfg.rs".to_owned()];
        // "cfg" is contiguous + boundary-anchored in cfg.rs, so it ranks first.
        let ranked = fuzzy_filter(&files, "cfg");
        assert_eq!(ranked.first(), Some(&"cfg.rs"));
    }

    #[test]
    fn scan_lists_files_and_dirs_skipping_noise() {
        // Index this crate's own `src/` tree (always present in-repo) to avoid a
        // tempdir dependency: it must surface `files.rs` and the `src/` folder
        // entry while pruning the sibling `target/` build cache.
        let listed = scan(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(listed.iter().any(|p| p == "src/files.rs"));
        assert!(listed.iter().any(|p| p == "src/"));
        assert!(listed.iter().all(|p| !p.starts_with("target")));
    }

    #[test]
    fn mention_paths_extracts_boundary_tokens_only() {
        let found = mention_paths("see @plans/ and @src/main.rs not mail@host");
        assert_eq!(found, vec!["plans/", "src/main.rs"]);
    }

    #[test]
    fn expand_inlines_dir_listing_and_file_contents() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = expand_mentions("look @src/ and @Cargo.toml", root);
        // Original message text is preserved verbatim at the front.
        assert!(out.starts_with("look @src/ and @Cargo.toml"));
        // Directory mention inlines an entry listing.
        assert!(out.contains("type=\"directory\""));
        assert!(out.contains("files.rs"));
        // File mention inlines the file's actual contents.
        assert!(out.contains("type=\"file\""));
        assert!(out.contains("zhive-tui"));
    }

    #[test]
    fn expand_is_noop_without_resolvable_mentions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = "a plain message · email name@host · @does_not_exist_xyz";
        assert_eq!(expand_mentions(text, root), text);
    }

    #[test]
    fn expand_blocks_path_traversal_outside_root() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // `../../Cargo.toml` resolves to the workspace root manifest, which lies
        // outside the crate root and must not be inlined.
        let text = "@../../Cargo.toml";
        assert_eq!(expand_mentions(text, root), text);
    }
}

// Rust guideline compliant 2026-02-21
