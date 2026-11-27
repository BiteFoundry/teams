//! Terminal and agent safe-output sanitization for untrusted external text.
//!
//! External Microsoft Teams messages, channels, chats, user display names, reactions, and
//! filenames may contain:
//! - ANSI / VT100 escape sequences (CSI cursor/screen manipulation, OSC 52 clipboard writes,
//!   OSC 8 deceptive hyperlinks, DCS/APC payloads, or 8-bit C1 equivalents),
//! - C0 / C1 control characters (including bare `\r` line-overwrite spoofing),
//! - Unicode bidirectional (Bidi) override/isolate controls (Trojan Source),
//! - Unicode Tag block characters (`U+E0000..=U+E007F`, invisible prompt-injection carriers).
//!
//! Call [`sanitize_terminal_text`] for multi-line bodies and [`sanitize_terminal_inline`] for
//! single-line headings, titles, names, previews, and table cells. Clean inputs return
//! `Cow::Borrowed` without allocating.

use std::borrow::Cow;

/// Returns `true` if `ch` is a Unicode Bidi override/isolate control character.
#[inline]
fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Returns `true` if `ch` belongs to the invisible Unicode Tag block (`U+E0000..=U+E007F`).
#[inline]
fn is_unicode_tag(ch: char) -> bool {
    matches!(ch, '\u{E0000}'..='\u{E007F}')
}

/// Returns `true` if `ch` needs sanitization in multi-line (`inline == false`) or single-line
/// (`inline == true`) terminal output.
#[inline]
fn needs_sanitization(ch: char, inline: bool) -> bool {
    if is_bidi_control(ch) || is_unicode_tag(ch) {
        return true;
    }
    let code = ch as u32;
    if inline {
        code < 0x20 || (0x7F..=0x9F).contains(&code)
    } else {
        (code < 0x20 && ch != '\n' && ch != '\t') || (0x7F..=0x9F).contains(&code)
    }
}

/// Sanitizes untrusted multi-line text for terminal and agent display.
///
/// - Strips ANSI/VT100 CSI, OSC, DCS/SOS/PM/APC, 2-byte ESC, and 8-bit C1 sequences.
/// - Normalizes `\r\n` to `\n` and removes standalone `\r`.
/// - Strips remaining C0/C1 control characters (preserving `\n` and `\t`).
/// - Strips Unicode Bidi override/isolate controls and Unicode Tag characters (`U+E0000..=U+E007F`).
pub fn sanitize_terminal_text(input: &str) -> Cow<'_, str> {
    sanitize_impl(input, false)
}

/// Sanitizes untrusted single-line text for terminal tables, headers, titles, and cards.
///
/// Applies the full sanitization of [`sanitize_terminal_text`] and replaces `\n`, `\r\n`, and
/// `\t` with a single ASCII space so untrusted fields cannot forge extra lines.
pub fn sanitize_terminal_inline(input: &str) -> Cow<'_, str> {
    sanitize_impl(input, true)
}

fn sanitize_impl(input: &str, inline: bool) -> Cow<'_, str> {
    if !input.chars().any(|ch| needs_sanitization(ch, inline)) {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // 7-bit ESC sequence
            '\x1B' => skip_esc_sequence(&mut chars),
            // 8-bit C1 CSI
            '\u{009B}' => skip_csi_body(&mut chars),
            // 8-bit C1 string sequences: DCS, SOS, OSC, PM, APC
            '\u{0090}' | '\u{0098}' | '\u{009D}' | '\u{009E}' | '\u{009F}' => {
                skip_st_or_bel_terminated(&mut chars, ch == '\u{009D}');
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    out.push(if inline { ' ' } else { '\n' });
                }
                // Bare '\r' is dropped to block carriage-return overwrite spoofing.
            }
            '\n' | '\t' => {
                out.push(if inline { ' ' } else { ch });
            }
            c if is_bidi_control(c) || is_unicode_tag(c) => {}
            c if (c as u32) < 0x20 || (0x7F..=0x9F).contains(&(c as u32)) => {}
            c => out.push(c),
        }
    }

    Cow::Owned(out)
}

fn skip_esc_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(&next) = chars.peek() else {
        return;
    };
    match next {
        // CSI: ESC [ ... <0x40..=0x7E>
        '[' => {
            chars.next();
            skip_csi_body(chars);
        }
        // OSC: ESC ] ... (BEL | ST)
        ']' => {
            chars.next();
            skip_st_or_bel_terminated(chars, true);
        }
        // DCS / SOS / PM / APC: ESC (P|X|^|_) ... ST
        'P' | 'X' | '^' | '_' => {
            chars.next();
            skip_st_or_bel_terminated(chars, false);
        }
        // nF sequences: ESC <0x20..=0x2F>+ <0x30..=0x7E>
        ' '..='/' => {
            while let Some(&c) = chars.peek() {
                if (' '..='/').contains(&c) {
                    chars.next();
                } else {
                    break;
                }
            }
            if let Some(&final_ch) = chars.peek() {
                if ('0'..='~').contains(&final_ch) {
                    chars.next();
                }
            }
        }
        // Standard 2-byte ESC sequence: ESC <0x30..=0x7E>
        '0'..='~' => {
            chars.next();
        }
        _ => {}
    }
}

fn skip_csi_body(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for c in chars.by_ref() {
        let code = c as u32;
        // Final byte of CSI sequence is in 0x40..=0x7E
        if (0x40..=0x7E).contains(&code) {
            break;
        }
        // Abort cleanly if an unexpected non-parameter/non-intermediate char appears
        if !(0x20..=0x3F).contains(&code) {
            break;
        }
    }
}

fn skip_st_or_bel_terminated(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    allow_bel: bool,
) {
    while let Some(c) = chars.next() {
        if allow_bel && c == '\x07' {
            break;
        }
        if c == '\u{009C}' {
            break;
        }
        if c == '\x1B' && chars.peek() == Some(&'\\') {
            chars.next();
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_borrows_without_allocating() {
        let s = "한국어 팀즈 메시지 🚀 — General\n\tindented line";
        let out = sanitize_terminal_text(s);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), s);

        let single = "한국어 팀즈 메시지 🚀 — General";
        let out_inline = sanitize_terminal_inline(single);
        assert!(matches!(out_inline, Cow::Borrowed(_)));
        assert_eq!(out_inline.as_ref(), single);
    }

    #[test]
    fn strips_csi_osc_dcs_and_c1_escape_sequences() {
        // CSI color + screen clear + cursor move
        assert_eq!(
            sanitize_terminal_text("\x1b[31mALERT\x1b[0m \x1b[2J\x1b[Hok"),
            "ALERT ok"
        );
        // OSC 52 clipboard exfiltration (BEL terminated) + OSC 8 hyperlink (ST terminated)
        assert_eq!(
            sanitize_terminal_text(
                "hi \x1b]52;c;c2VjcmV0\x07and \x1b]8;;https://evil.example\x1b\\click\x1b]8;;\x1b\\"
            ),
            "hi and click"
        );
        // DCS payload terminated by ST
        assert_eq!(
            sanitize_terminal_text("pre\x1bPq#0;2;0;0;0\x1b\\post"),
            "prepost"
        );
        // 8-bit C1 CSI & OSC
        assert_eq!(
            sanitize_terminal_text("a\u{009B}31mb\u{009D}0;spoof\u{009C}c"),
            "abc"
        );
    }

    #[test]
    fn strips_control_chars_bidi_overrides_and_unicode_tags() {
        // Bare CR overwrite spoofing + NUL/BEL/BS/DEL, while normalizing CRLF -> LF
        assert_eq!(
            sanitize_terminal_text("safe\rspoof\r\nline2\x00\x07\x08\x7f\tend"),
            "safespoof\nline2\tend"
        );
        // Trojan Source Bidi overrides & isolates
        assert_eq!(
            sanitize_terminal_text("file\u{202E}gpj.exe\u{202C}\u{2066}iso\u{2069}\u{200F}"),
            "filegpj.exeiso"
        );
        // Invisible Unicode Tag prompt-injection block (U+E0000..=U+E007F)
        let injected = format!(
            "Normal title{}{}{}!",
            '\u{E0001}', '\u{E0069}', '\u{E007F}'
        );
        assert_eq!(sanitize_terminal_text(&injected), "Normal title!");
    }

    #[test]
    fn inline_flattens_newlines_and_tabs_while_stripping_escapes() {
        assert_eq!(
            sanitize_terminal_inline("\x1b[31mTitle\x1b[0m\r\nForged: Approved\t\u{202E}ok"),
            "Title Forged: Approved ok"
        );
    }
}
