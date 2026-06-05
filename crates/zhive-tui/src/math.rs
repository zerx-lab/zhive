//! Lightweight LaTeX-math → Unicode approximation for terminal rendering.
//!
//! tui-markdown does not enable `pulldown-cmark`'s math extension, and a
//! terminal cannot render real LaTeX anyway, so [`render_math`] preprocesses the
//! source before it reaches the Markdown renderer: `$…$` / `$$…$$` spans are
//! stripped of their delimiters and their contents approximated with Unicode —
//! superscripts/subscripts (`x^2`→`x²`), Greek letters (`\alpha`→`α`), common
//! operators (`\sum`→`∑`, `\leq`→`≤`), and simple `\frac{a}{b}`→`a⁄b`.
//!
//! It is best-effort: constructs a terminal cannot show (matrices, integral
//! bounds, nested fractions) degrade to a readable linear form rather than
//! erroring.  Math inside code spans/fences is left untouched, and an unclosed
//! `$` mid-stream is kept literal so it heals once the closer arrives.

use std::borrow::Cow;

/// Approximates LaTeX math spans in `src` with Unicode, outside code regions.
pub(crate) fn render_math(src: &str) -> Cow<'_, str> {
    if !src.contains('$') {
        return Cow::Borrowed(src);
    }
    let mut out = String::with_capacity(src.len());
    let mut in_fence = false;
    let mut changed = false;
    for line in src.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if is_fence(body) {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        let (processed, c) = process_line(body);
        changed |= c;
        out.push_str(&processed);
        if line.ends_with('\n') {
            out.push('\n');
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(src)
    }
}

/// `true` when a line opens or closes a fenced code block (backtick or tilde fence).
fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Replaces `$…$` / `$$…$$` spans in one line; skips inline-code regions.
fn process_line(line: &str) -> (String, bool) {
    let b: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut changed = false;
    while i < b.len() {
        match b[i] {
            '`' => {
                out.push('`');
                i += 1;
                while i < b.len() && b[i] != '`' {
                    out.push(b[i]);
                    i += 1;
                }
                if i < b.len() {
                    out.push('`');
                    i += 1;
                }
            }
            '$' => {
                let display = i + 1 < b.len() && b[i + 1] == '$';
                let delim = if display { 2 } else { 1 };
                // Open flanking: char after the delimiter must be non-space, so
                // prose currency like `$5 and $10` is not parsed as a span.
                let opens = b.get(i + delim).is_some_and(|c| !c.is_whitespace());
                if opens && let Some(end) = find_close(&b, i + delim, display) {
                    let inner: String = b[i + delim..end].iter().collect();
                    out.push_str(&convert(&inner));
                    changed = true;
                    i = end + delim;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    (out, changed)
}

/// Finds the closing `$` (or `$$`) delimiter index at/after `from`.
fn find_close(b: &[char], from: usize, display: bool) -> Option<usize> {
    let mut i = from;
    while i < b.len() {
        if b[i] == '$' {
            if display {
                if i + 1 < b.len() && b[i + 1] == '$' {
                    return Some(i);
                }
            } else if i > from && !b[i - 1].is_whitespace() {
                // Inline close flanking: the char before `$` must be non-space.
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Converts the inner LaTeX of a math span to a Unicode approximation.
fn convert(s: &str) -> String {
    convert_depth(s, 0)
}

/// Inner conversion with a recursion-depth bound for nested `\frac`.
fn convert_depth(s: &str, depth: usize) -> String {
    // Bail to raw text past a sane nesting depth so a pathological `\frac` tower
    // cannot overflow the stack (the module contract is never-panic).
    if depth > 32 {
        return s.to_owned();
    }
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            '\\' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j].is_ascii_alphabetic() {
                    j += 1;
                }
                let cmd: String = b[start..j].iter().collect();
                if cmd == "frac" {
                    let (num, k1) = read_brace(&b, j);
                    let (den, k2) = read_brace(&b, k1);
                    out.push_str(&convert_depth(&num, depth + 1));
                    out.push('⁄');
                    out.push_str(&convert_depth(&den, depth + 1));
                    i = k2;
                } else if let Some(u) = symbol(&cmd) {
                    out.push_str(u);
                    i = j;
                } else if cmd.is_empty() {
                    // `\` before a non-letter: spacing commands (\, \; \: \!)
                    // become a thin space; other escapes drop the backslash.
                    if matches!(b.get(start), Some(',' | ';' | ':' | '!')) {
                        out.push(' ');
                        i = start + 1;
                    } else {
                        i += 1;
                    }
                } else {
                    // Unknown command: drop the backslash, keep the name.
                    out.push_str(&cmd);
                    i = j;
                }
            }
            '^' => {
                let (arg, k) = read_script(&b, i + 1);
                out.push_str(&map_script(&arg, true));
                i = k;
            }
            '_' => {
                let (arg, k) = read_script(&b, i + 1);
                out.push_str(&map_script(&arg, false));
                i = k;
            }
            '{' | '}' => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Reads a `{…}` group at `j` (if present), returning its content and next idx.
fn read_brace(b: &[char], j: usize) -> (String, usize) {
    if j >= b.len() || b[j] != '{' {
        return (String::new(), j);
    }
    let mut depth = 1;
    let mut k = j + 1;
    let mut content = String::new();
    while k < b.len() && depth > 0 {
        match b[k] {
            '{' => {
                depth += 1;
                content.push('{');
            }
            '}' => {
                depth -= 1;
                if depth > 0 {
                    content.push('}');
                }
            }
            c => content.push(c),
        }
        k += 1;
    }
    (content, k)
}

/// Reads a sub/superscript argument: a `{…}` group or a single char.
fn read_script(b: &[char], j: usize) -> (String, usize) {
    if j < b.len() && b[j] == '{' {
        read_brace(b, j)
    } else if j < b.len() {
        (b[j].to_string(), j + 1)
    } else {
        (String::new(), j)
    }
}

/// Maps a sub/superscript argument to Unicode, char by char.
///
/// Characters without a Unicode form fall back to a linear `^x` / `_x` so no
/// information is lost.
fn map_script(arg: &str, sup: bool) -> String {
    let mut out = String::new();
    let mut all_mapped = true;
    let mut linear = String::new();
    for ch in arg.chars() {
        linear.push(ch);
        match if sup { super_char(ch) } else { sub_char(ch) } {
            Some(u) => out.push(u),
            None => all_mapped = false,
        }
    }
    if all_mapped && !out.is_empty() {
        out
    } else {
        // Fall back to linear notation for unmappable scripts.
        format!("{}{}", if sup { "^" } else { "_" }, linear)
    }
}

/// Unicode superscript for a char, if one exists.
fn super_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        _ => return None,
    })
}

/// Unicode subscript for a char, if one exists.
fn sub_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'o' => 'ₒ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// Maps a LaTeX command name to its Unicode symbol, if known.
fn symbol(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        // lowercase Greek
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        // uppercase Greek
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        // operators & relations
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "neq" | "ne" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "cong" => "≅",
        "sim" => "∼",
        "propto" => "∝",
        "rightarrow" | "to" => "→",
        "leftarrow" | "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" | "implies" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "↦",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "emptyset" | "varnothing" => "∅",
        "setminus" => "∖",
        "sqrt" => "√",
        "angle" => "∠",
        "perp" => "⊥",
        "parallel" => "∥",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "neg" | "lnot" => "¬",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "prime" => "′",
        "circ" => "∘",
        "bullet" => "•",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "deg" | "degree" => "°",
        "quad" | "qquad" => " ",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> String {
        render_math(s).into_owned()
    }

    #[test]
    fn no_dollar_is_borrowed() {
        assert!(matches!(render_math("plain text"), Cow::Borrowed(_)));
    }

    #[test]
    fn inline_superscript() {
        assert_eq!(m("$E = mc^2$"), "E = mc²");
    }

    #[test]
    fn display_math_strips_double_dollar() {
        assert_eq!(m("$$x^2 + y^2$$"), "x² + y²");
    }

    #[test]
    fn greek_and_operators() {
        assert_eq!(m("$\\alpha + \\beta \\leq \\gamma$"), "α + β ≤ γ");
        assert_eq!(m("$\\sum x_i$"), "∑ xᵢ");
    }

    #[test]
    fn fraction_to_unicode_slash() {
        assert_eq!(m("$\\frac{a}{b}$"), "a⁄b");
    }

    #[test]
    fn braced_superscript() {
        assert_eq!(m("$x^{10}$"), "x¹⁰");
    }

    #[test]
    fn unmappable_script_falls_back_linearly() {
        // `^k` has no unicode superscript for k → keep linear `^k`.
        assert_eq!(m("$x^k$"), "x^k");
    }

    #[test]
    fn unclosed_dollar_stays_literal() {
        assert_eq!(m("cost is $5 and more"), "cost is $5 and more");
    }

    #[test]
    fn math_inside_code_is_untouched() {
        assert!(matches!(render_math("`$x^2$`"), Cow::Borrowed(_)));
    }

    #[test]
    fn math_inside_fence_is_untouched() {
        let src = "```\n$x^2$\n```\n";
        assert!(matches!(render_math(src), Cow::Borrowed(_)));
    }

    #[test]
    fn unknown_command_drops_backslash() {
        assert_eq!(m("$\\foo bar$"), "foo bar");
    }

    #[test]
    fn deeply_nested_frac_does_not_overflow() {
        let mut s = String::from("$");
        for _ in 0..200 {
            s.push_str("\\frac{");
        }
        s.push('1');
        for _ in 0..200 {
            s.push_str("}{2}");
        }
        s.push('$');
        // Depth-bounded conversion must return, not overflow the stack.
        let _ = render_math(&s);
    }

    #[test]
    fn currency_amounts_stay_literal() {
        assert_eq!(
            m("I paid $5 and then $10 more"),
            "I paid $5 and then $10 more"
        );
        assert_eq!(m("price is $42 today"), "price is $42 today");
    }

    #[test]
    fn tilde_fenced_math_untouched() {
        assert!(matches!(render_math("~~~\n$x^2$\n~~~\n"), Cow::Borrowed(_)));
    }
}
