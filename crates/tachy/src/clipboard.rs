//! Copying to the system clipboard with the OSC 52 escape sequence (spec §13
//! `y` / `Y`, M6-05). It works over SSH and needs no external tool, but the
//! terminal must support OSC 52: kitty, alacritty, foot, WezTerm, iTerm2 and
//! Windows Terminal do; xterm needs `allowWindowOps`.
//!
//! - The sequence is `ESC ] 52 ; c ; <base64> ESC \` (ST terminator; BEL is
//!   less portable inside tmux).
//! - Inside tmux (`$TMUX` set) it is wrapped in a DCS passthrough,
//!   `ESC P tmux ; <seq, every ESC doubled> ESC \`. tmux needs
//!   `set -g allow-passthrough on`.
//! - Inside GNU screen (`$STY` set) it is wrapped in `ESC P … ESC \` pieces of
//!   at most [`SCREEN_CHUNK`] bytes (screen's string limit). The inner
//!   sequence ends with BEL there, because an inner `ESC \` would end
//!   screen's DCS string early.
//! - A base64 payload over [`MAX_PAYLOAD`] is not sent: many terminals cap
//!   OSC 52 (xterm ~100 KB, others 1 MB) and would drop it silently.
//!
//! The sequence must never interleave with a ratatui frame: the app queues
//! it and writes it to the same stdout handle after `terminal.draw` returns
//! ([`write_sequence`]).
//!
//! What is copied: `y` copies the cell's **full** value (the unescaped field
//! bytes, not the truncated display text), decoded to UTF-8 with lossy
//! replacement ([`cell_text`]). `Y` copies the record **as stored**, not as
//! displayed: every column in source order (synthetic `_extraN` included),
//! with the source delimiter and minimal quoting, no trailing newline
//! ([`row_text`]). Control characters are kept: this is data, not display.

use std::io::{self, Write};

use tachy_core::{
    dialect::Encoding,
    export::{Quoting, format_record},
    parse::decode_field,
    size::format_size,
};

use crate::toast::{Toast, ToastLevel};

/// Largest base64 payload sent: 1 MB.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// Largest DCS string GNU screen passes through.
pub const SCREEN_CHUNK: usize = 768;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// The terminal multiplexer between tachy and the terminal, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Multiplexer {
    None,
    Tmux,
    Screen,
}

impl Multiplexer {
    /// From `$TMUX` and `$STY`. tmux wins when both are set (tmux started
    /// inside screen is the innermost layer).
    pub fn detect() -> Self {
        Self::from_env(
            std::env::var_os("TMUX").is_some_and(|v| !v.is_empty()),
            std::env::var_os("STY").is_some_and(|v| !v.is_empty()),
        )
    }

    fn from_env(tmux: bool, screen: bool) -> Self {
        if tmux {
            Multiplexer::Tmux
        } else if screen {
            Multiplexer::Screen
        } else {
            Multiplexer::None
        }
    }
}

/// Why a copy was not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    /// The base64 payload is over [`MAX_PAYLOAD`] bytes.
    TooLarge { payload: usize },
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::TooLarge { payload } => {
                write!(
                    f,
                    "value too large to copy ({})",
                    format_size(*payload as u64)
                )
            }
        }
    }
}

impl CopyError {
    /// The error toast.
    pub fn toast(&self) -> Toast {
        Toast::new(ToastLevel::Error, self.to_string())
    }
}

/// Standard base64 (RFC 4648 §4) with `=` padding.
pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        let sextet = |shift: u32| ALPHABET[(n >> shift & 0x3f) as usize] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if chunk.len() > 1 { sextet(6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

/// `ESC ] 52 ; c ; <payload> <terminator>`.
fn osc52(payload: &str, terminator: &[u8]) -> Vec<u8> {
    let mut seq = Vec::with_capacity(payload.len() + 10);
    seq.extend_from_slice(b"\x1b]52;c;");
    seq.extend_from_slice(payload.as_bytes());
    seq.extend_from_slice(terminator);
    seq
}

/// The bytes to write to the terminal to put `text` on the clipboard,
/// wrapped for `mux`. Fails when the payload is too large.
pub fn copy_sequence(text: &str, mux: Multiplexer) -> Result<Vec<u8>, CopyError> {
    let payload = base64_encode(text.as_bytes());
    if payload.len() > MAX_PAYLOAD {
        return Err(CopyError::TooLarge {
            payload: payload.len(),
        });
    }
    Ok(match mux {
        Multiplexer::None => osc52(&payload, b"\x1b\\"),
        Multiplexer::Tmux => wrap_tmux(&osc52(&payload, b"\x1b\\")),
        Multiplexer::Screen => wrap_screen(&osc52(&payload, &[BEL])),
    })
}

/// `ESC P tmux ; <seq with every ESC doubled> ESC \`.
fn wrap_tmux(seq: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(seq.len() + 16);
    out.extend_from_slice(b"\x1bPtmux;");
    for &b in seq {
        if b == ESC {
            out.push(ESC);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// `ESC P <chunk> ESC \` for every [`SCREEN_CHUNK`]-byte piece of `seq`.
fn wrap_screen(seq: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(seq.len() + seq.len() / SCREEN_CHUNK * 4 + 4);
    for chunk in seq.chunks(SCREEN_CHUNK) {
        out.extend_from_slice(b"\x1bP");
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Writes a queued sequence and flushes. Call it after `terminal.draw`
/// returned, on the same stdout handle, never from inside a component.
pub fn write_sequence(out: &mut impl Write, seq: &[u8]) -> io::Result<()> {
    out.write_all(seq)?;
    out.flush()
}

/// The text `y` copies: the unescaped field bytes, decoded (Windows-1252 is
/// transcoded, invalid UTF-8 replaced). Control characters are kept.
pub fn cell_text(unescaped: &[u8], encoding: Encoding) -> String {
    decode_field(unescaped, encoding).into_owned()
}

/// The text `Y` copies: every field (unescaped bytes, in source order),
/// joined with the source `delimiter` by
/// [`tachy_core::export::format_record`] with minimal quoting (as an
/// export would write it: `"` quotes, doubled inside), no trailing newline.
/// A field is quoted when it contains the delimiter, `"`, `\r` or `\n`; a
/// lone empty field is written `""`, so the row is not mistaken for a blank
/// line.
pub fn row_text<F: AsRef<[u8]>>(fields: &[F], delimiter: u8, encoding: Encoding) -> String {
    let fields: Vec<&[u8]> = fields.iter().map(AsRef::as_ref).collect();
    let mut out = Vec::new();
    format_record(&fields, delimiter, Quoting::Minimal, &mut out);
    decode_field(&out, encoding).into_owned()
}

/// `copied cell (123 chars)`. OSC 52 has no acknowledgement: this means
/// "sent to the terminal".
pub fn copied_cell_toast(text: &str) -> Toast {
    let n = text.chars().count();
    let noun = if n == 1 { "char" } else { "chars" };
    Toast::new(ToastLevel::Info, format!("copied cell ({n} {noun})"))
}

/// `copied row (13 fields)`.
pub fn copied_row_toast(fields: usize) -> Toast {
    let noun = if fields == 1 { "field" } else { "fields" };
    Toast::new(ToastLevel::Info, format!("copied row ({fields} {noun})"))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// RFC 4648 §10 test vectors.
    #[test]
    fn base64_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
        assert_eq!(base64_encode(&[0xff, 0xfe, 0x00]), "//4A");
        assert_eq!(base64_encode("é\n".as_bytes()), "w6kK");
    }

    #[test]
    fn plain_sequence() {
        let seq = copy_sequence("foobar", Multiplexer::None).unwrap();
        assert_eq!(seq, b"\x1b]52;c;Zm9vYmFy\x1b\\");
    }

    #[test]
    fn tmux_sequence_doubles_every_esc() {
        let seq = copy_sequence("foobar", Multiplexer::Tmux).unwrap();
        assert_eq!(seq, b"\x1bPtmux;\x1b\x1b]52;c;Zm9vYmFy\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn screen_sequence_is_chunked() {
        let short = copy_sequence("foobar", Multiplexer::Screen).unwrap();
        assert_eq!(short, b"\x1bP\x1b]52;c;Zm9vYmFy\x07\x1b\\");

        // 1500 bytes → 2000 base64 bytes → 2008-byte inner sequence → 3 chunks.
        let text = "x".repeat(1500);
        let seq = copy_sequence(&text, Multiplexer::Screen).unwrap();
        let inner = osc52(&base64_encode(text.as_bytes()), &[BEL]);
        assert_eq!(inner.len(), 2008);
        // Unwrap the pieces: `ESC P <≤ 768 bytes> ESC \`, every piece but the
        // last exactly 768 bytes.
        let mut rebuilt = Vec::new();
        let mut rest = seq.as_slice();
        let mut chunks = 0;
        while !rest.is_empty() {
            assert!(rest.starts_with(b"\x1bP"));
            let body = &rest[2..];
            let len = (body.len() - 2).min(SCREEN_CHUNK);
            assert!(body[len..].starts_with(b"\x1b\\"));
            rebuilt.extend_from_slice(&body[..len]);
            rest = &body[len + 2..];
            chunks += 1;
        }
        assert_eq!(chunks, 3);
        assert_eq!(rebuilt, inner);
    }

    #[test]
    fn too_large_is_not_sent() {
        // 2 MB of text → 2.7 MB of base64.
        let text = "a".repeat(2 << 20);
        let err = copy_sequence(&text, Multiplexer::None).unwrap_err();
        assert_eq!(err.to_string(), "value too large to copy (2.7 MB)");
        assert_eq!(err.toast().level, ToastLevel::Error);
        // Just under the limit is fine: 786,432 bytes → exactly 1 MB.
        let text = "a".repeat(MAX_PAYLOAD / 4 * 3);
        assert!(copy_sequence(&text, Multiplexer::None).is_ok());
        let text = "a".repeat(MAX_PAYLOAD / 4 * 3 + 1);
        assert!(copy_sequence(&text, Multiplexer::None).is_err());
    }

    #[test]
    fn multiplexer_detection() {
        assert_eq!(Multiplexer::from_env(false, false), Multiplexer::None);
        assert_eq!(Multiplexer::from_env(true, false), Multiplexer::Tmux);
        assert_eq!(Multiplexer::from_env(false, true), Multiplexer::Screen);
        assert_eq!(Multiplexer::from_env(true, true), Multiplexer::Tmux);
    }

    #[test]
    fn cell_text_keeps_the_full_value_and_control_chars() {
        assert_eq!(cell_text(b"a\tb\nc", Encoding::Utf8), "a\tb\nc");
        assert_eq!(cell_text(b"caf\xe9", Encoding::Windows1252), "café");
        assert_eq!(cell_text(b"bad \xff", Encoding::Utf8), "bad \u{fffd}");
    }

    #[test]
    fn row_text_quotes_minimally() {
        let fields: [&[u8]; 6] = [b"plain", b"a,b", b"say \"hi\"", b"two\nlines", b"", b"cr\r"];
        assert_eq!(
            row_text(&fields, b',', Encoding::Utf8),
            "plain,\"a,b\",\"say \"\"hi\"\"\",\"two\nlines\",,\"cr\r\""
        );
        // The source delimiter is used; a comma needs no quotes in a TSV row.
        let tsv: [&[u8]; 3] = [b"a,b", b"c\td", b"e"];
        assert_eq!(row_text(&tsv, b'\t', Encoding::Utf8), "a,b\t\"c\td\"\te");
        // Another delimiter; quotes are always `"`; no trailing newline.
        let single: [&[u8]; 2] = [b"it's", b"x;y"];
        assert_eq!(row_text(&single, b';', Encoding::Utf8), "it's;\"x;y\"");
        let none: [&[u8]; 2] = [b"a|b", b"c"];
        assert_eq!(row_text(&none, b'|', Encoding::Utf8), "\"a|b\"|c");
        // A lone empty field is quoted; Windows-1252 is transcoded.
        let empty: [&[u8]; 1] = [b""];
        assert_eq!(row_text(&empty, b',', Encoding::Utf8), "\"\"");
        let latin: [&[u8]; 2] = [b"caf\xe9", b"x"];
        assert_eq!(row_text(&latin, b',', Encoding::Windows1252), "café,x");
    }

    #[test]
    fn toasts() {
        assert_eq!(copied_cell_toast("héllo").text, "copied cell (5 chars)");
        assert_eq!(copied_cell_toast("x").text, "copied cell (1 char)");
        assert_eq!(copied_row_toast(13).text, "copied row (13 fields)");
        assert_eq!(copied_row_toast(13).level, ToastLevel::Info);
    }

    #[test]
    fn write_sequence_writes_and_flushes() {
        let mut out = Vec::new();
        write_sequence(&mut out, b"\x1b]52;c;\x1b\\").unwrap();
        assert_eq!(out, b"\x1b]52;c;\x1b\\");
    }
}
