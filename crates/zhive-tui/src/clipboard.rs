//! Clipboard writes via OSC 52 with a best-effort native fallback.
//!
//! Mirrors opencode's `util/clipboard.ts`: emit the OSC 52 escape so the
//! terminal emulator sets the system clipboard itself — which is what makes
//! copy work over SSH, since the *local* terminal owns the clipboard — and
//! additionally shell out to a native tool (`wl-copy` / `xclip` / `xsel`) for
//! terminals that disable OSC 52 clipboard *writes* (a common security default,
//! and the rule rather than the exception inside `tmux`/`screen`).
//!
//! Both paths are best-effort by design: a terminal that ignores OSC 52 and a
//! host with no clipboard tool simply leave the clipboard untouched. There is
//! no error surface — a failed copy is a silent no-op, never a render-loop
//! failure.

use std::io::Write;

/// Argv for the Wayland clipboard writer.
const WL_COPY: &[&str] = &["wl-copy"];
/// Argv for the X11 `xclip` clipboard writer (clipboard, not primary).
const XCLIP: &[&str] = &["xclip", "-selection", "clipboard"];
/// Argv for the X11 `xsel` clipboard writer.
const XSEL: &[&str] = &["xsel", "--clipboard", "--input"];

/// Copies `text` to the system clipboard, best-effort.
///
/// Writes an OSC 52 escape to stdout (wrapped for `tmux`/`screen` passthrough
/// when running inside a multiplexer) and, on Linux, spawns a native clipboard
/// tool on a detached thread. Returns immediately; the native write never
/// blocks the caller.
pub(crate) fn copy(text: &str) {
    write_osc52(text);
    spawn_native(text);
}

/// Emits the OSC 52 clipboard-set escape for `text` to stdout.
///
/// OSC 52 produces no visible output, so writing it mid-frame (between ratatui
/// draws) cannot corrupt the screen. Inside `tmux`/`screen` the sequence is
/// wrapped in DCS passthrough; modern `tmux` additionally requires
/// `allow-passthrough on`, which is why the native fallback is the real backstop
/// for SSH-into-`tmux`.
fn write_osc52(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    // BEL (`\x07`) terminates the OSC so the passthrough wrap only has to double
    // a single leading ESC — matching opencode byte-for-byte.
    let osc = format!("\x1b]52;c;{encoded}\x07");
    let in_multiplexer = std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some();
    let sequence = if in_multiplexer {
        // tmux/screen DCS passthrough: `ESC P tmux ; <payload> ESC \`, where the
        // payload's own leading ESC is doubled so the multiplexer forwards it.
        format!("\x1bPtmux;\x1b{osc}\x1b\\")
    } else {
        osc
    };
    let mut out = std::io::stdout().lock();
    // Best-effort: a write/flush error leaves the clipboard unchanged.
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

/// Spawns a native clipboard writer on a detached thread (Linux only).
///
/// Tries tools in environment-appropriate order until one spawns; a host with
/// none installed simply relies on OSC 52. Detached so a slow spawn or a
/// clipboard daemon never stalls the render loop.
fn spawn_native(text: &str) {
    if !cfg!(target_os = "linux") {
        return;
    }
    // Prefer the display server's own tool; keep the others as fallbacks so an
    // X11-tool-only host under Wayland (or vice versa) still works.
    let order: [&[&str]; 3] = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        [WL_COPY, XCLIP, XSEL]
    } else {
        [XCLIP, XSEL, WL_COPY]
    };
    let text = text.to_owned();
    std::thread::spawn(move || {
        for argv in order {
            if run_native(argv, &text) {
                break;
            }
        }
    });
}

/// Runs one clipboard tool, piping `text` to its stdin. Returns whether it ran.
///
/// stdout/stderr are nulled so a tool's diagnostics can never print into the
/// alternate screen. `wl-copy`/`xclip` fork a daemon and exit promptly, so the
/// `wait` reaps the foreground process without holding the thread.
fn run_native(argv: &[&str], text: &str) -> bool {
    let Some((cmd, rest)) = argv.split_first() else {
        return false;
    };
    let Ok(mut child) = std::process::Command::new(cmd)
        .args(rest)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        // Dropping `stdin` here sends EOF so the tool reads the full payload.
    }
    let _ = child.wait();
    true
}

/// Encodes `input` as standard (RFC 4648) base64 with `=` padding.
///
/// Hand-rolled to keep the crate dependency-free; OSC 52 requires the standard
/// alphabet. Verified against the RFC 4648 §10 test vectors.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        // `chunks(3)` never yields an empty slice, so index 0 always exists.
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(char::from(ALPHABET[((n >> 18) & 0x3f) as usize]));
        out.push(char::from(ALPHABET[((n >> 12) & 0x3f) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((n >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(n & 0x3f) as usize])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64_encode;

    #[test]
    fn base64_rfc4648_vectors() {
        // RFC 4648 §10 canonical vectors — the cheapest proof the hand-roll is
        // correct across all three padding cases.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_non_ascii_bytes() {
        // UTF-8 multi-byte content must round-trip through the byte encoder.
        assert_eq!(base64_encode("✓".as_bytes()), "4pyT");
    }
}
