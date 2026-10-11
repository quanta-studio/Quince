//! Scanner for the byte stream coming out of the shell.
//!
//! It does two jobs on the way to the real terminal:
//!
//! 1. Pulls out the private OSC sequences the shell integration emits, so they never reach the
//!    terminal.
//! 2. Tracks whether the stream is between escape sequences and between UTF-8 characters. Only
//!    then is it safe to inject our own bytes (the popup) into the output.

/// OSC number reserved for shell-to-wrapper messages: `ESC ] 6973 ; <payload> BEL`.
pub const OSC_PREFIX: &[u8] = b"6973;";

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

/// Longest message payload accepted. Anything larger is swallowed and dropped.
const MAX_PAYLOAD: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// An ESC is held back until the next byte says what it introduces.
    Esc,
    EscIntermediate,
    Csi,
    /// `ESC ]` seen; `held` has the bytes after it while we decide whether the OSC is ours.
    OscProbe,
    OscOurs,
    OscOursEsc,
    OscOther,
    OscOtherEsc,
    /// DCS, SOS, PM or APC string.
    Str,
    StrEsc,
}

#[derive(Debug)]
pub struct Scanner {
    state: State,
    /// Continuation bytes still expected for the current UTF-8 character.
    utf8_left: u8,
    held: Vec<u8>,
    overflow: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            utf8_left: 0,
            held: Vec::new(),
            overflow: false,
        }
    }

    /// True when bytes of our own can be written to the terminal without landing inside an
    /// escape sequence or a multi-byte character.
    pub fn is_ground(&self) -> bool {
        self.state == State::Ground && self.utf8_left == 0
    }

    /// Consumes `input`. Bytes meant for the terminal are appended to `forward`; the payload of
    /// each complete private OSC is appended to `messages`.
    pub fn feed(&mut self, input: &[u8], forward: &mut Vec<u8>, messages: &mut Vec<Vec<u8>>) {
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            match self.state {
                State::Ground => {
                    // Plain text is the common case: copy the whole run up to the next ESC.
                    let run = input[i..].iter().position(|&c| c == ESC).unwrap_or(input.len() - i);
                    if run == 0 {
                        self.state = State::Esc;
                        i += 1;
                    } else {
                        for &c in &input[i..i + run] {
                            self.track_utf8(c);
                        }
                        forward.extend_from_slice(&input[i..i + run]);
                        i += run;
                    }
                    continue;
                }
                State::Esc => match b {
                    b']' => {
                        self.held.clear();
                        self.overflow = false;
                        self.state = State::OscProbe;
                    }
                    ESC => forward.push(ESC),
                    _ => {
                        forward.extend_from_slice(&[ESC, b]);
                        self.utf8_left = 0;
                        self.state = match b {
                            b'[' => State::Csi,
                            b'P' | b'X' | b'^' | b'_' => State::Str,
                            0x20..=0x2f => State::EscIntermediate,
                            _ => State::Ground,
                        };
                    }
                },
                State::EscIntermediate => match b {
                    ESC => self.state = State::Esc,
                    _ => {
                        forward.push(b);
                        if !(0x20..=0x2f).contains(&b) {
                            self.state = State::Ground;
                        }
                    }
                },
                State::Csi => match b {
                    ESC => self.state = State::Esc,
                    _ => {
                        forward.push(b);
                        if (0x40..=0x7e).contains(&b) || b == CAN || b == SUB {
                            self.state = State::Ground;
                        }
                    }
                },
                State::OscProbe => {
                    self.held.push(b);
                    if !OSC_PREFIX.starts_with(&self.held) {
                        // Someone else's OSC: release what was held and let the byte that
                        // settled it be handled as ordinary OSC content.
                        self.held.pop();
                        forward.extend_from_slice(&[ESC, b']']);
                        forward.append(&mut self.held);
                        self.state = State::OscOther;
                        continue;
                    }
                    if self.held.len() == OSC_PREFIX.len() {
                        self.held.clear();
                        self.state = State::OscOurs;
                    }
                }
                State::OscOurs => match b {
                    BEL => self.finish_message(messages),
                    ESC => self.state = State::OscOursEsc,
                    _ if self.held.len() >= MAX_PAYLOAD => self.overflow = true,
                    _ => self.held.push(b),
                },
                State::OscOursEsc => {
                    self.finish_message(messages);
                    if b != b'\\' {
                        // The ESC ended our message and starts something else.
                        self.state = State::Esc;
                        continue;
                    }
                }
                State::OscOther => match b {
                    ESC => self.state = State::OscOtherEsc,
                    _ => {
                        forward.push(b);
                        if b == BEL {
                            self.state = State::Ground;
                        }
                    }
                },
                State::Str => match b {
                    ESC => self.state = State::StrEsc,
                    _ => {
                        forward.push(b);
                        if b == CAN || b == SUB {
                            self.state = State::Ground;
                        }
                    }
                },
                State::OscOtherEsc | State::StrEsc => {
                    if b == b'\\' {
                        forward.extend_from_slice(&[ESC, b'\\']);
                        self.state = State::Ground;
                    } else {
                        self.state = State::Esc;
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    fn finish_message(&mut self, messages: &mut Vec<Vec<u8>>) {
        let payload = std::mem::take(&mut self.held);
        if !self.overflow {
            messages.push(payload);
        }
        self.overflow = false;
        self.state = State::Ground;
    }

    fn track_utf8(&mut self, b: u8) {
        if self.utf8_left > 0 && (0x80..=0xbf).contains(&b) {
            self.utf8_left -= 1;
        } else {
            self.utf8_left = match b {
                0xc2..=0xdf => 1,
                0xe0..=0xef => 2,
                0xf0..=0xf4 => 3,
                _ => 0,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>, Scanner) {
        let mut scanner = Scanner::new();
        let (mut forward, mut messages) = (Vec::new(), Vec::new());
        for chunk in chunks {
            scanner.feed(chunk, &mut forward, &mut messages);
        }
        (forward, messages, scanner)
    }

    #[test]
    fn plain_text_passes_through() {
        let (forward, messages, scanner) = run(&[b"hello\r\nworld"]);
        assert_eq!(forward, b"hello\r\nworld");
        assert!(messages.is_empty());
        assert!(scanner.is_ground());
    }

    #[test]
    fn private_osc_is_extracted_and_stripped() {
        let (forward, messages, scanner) = run(&[b"ab\x1b]6973;B;3;git\x07cd"]);
        assert_eq!(forward, b"abcd");
        assert_eq!(messages, vec![b"B;3;git".to_vec()]);
        assert!(scanner.is_ground());
    }

    #[test]
    fn private_osc_survives_any_split() {
        let stream = b"x\x1b]6973;P;/tmp\x07y\x1b]6973;X\x1b\\z";
        for cut in 0..=stream.len() {
            let (forward, messages, scanner) = run(&[&stream[..cut], &stream[cut..]]);
            assert_eq!(forward, b"xyz", "cut at {cut}");
            assert_eq!(messages, vec![b"P;/tmp".to_vec(), b"X".to_vec()], "cut at {cut}");
            assert!(scanner.is_ground(), "cut at {cut}");
        }
    }

    #[test]
    fn foreign_sequences_are_forwarded_byte_for_byte() {
        let stream: &[u8] =
            b"\x1b]0;title\x07\x1b]6;1;bg\x07\x1b]697\x07\x1b]8;;http://x\x1b\\\x1b[1;31mred\x1b[0m\x1bPq#0\x1b\\\x1b(B\x1b7\x1b8";
        for cut in 0..=stream.len() {
            let (forward, messages, scanner) = run(&[&stream[..cut], &stream[cut..]]);
            assert_eq!(forward, stream, "cut at {cut}");
            assert!(messages.is_empty(), "cut at {cut}");
            assert!(scanner.is_ground(), "cut at {cut}");
        }
    }

    #[test]
    fn not_ground_inside_sequences_or_characters() {
        for partial in [
            &b"\x1b"[..],
            b"\x1b[1;3",
            b"\x1b]0;tit",
            b"\x1b]69",
            b"\x1bPabc",
            b"\xe2\x94",
            b"\xf0\x9f",
        ] {
            let (_, _, scanner) = run(&[partial]);
            assert!(!scanner.is_ground(), "{partial:?}");
        }
        let (forward, _, scanner) = run(&["─é🙂".as_bytes()]);
        assert_eq!(forward, "─é🙂".as_bytes());
        assert!(scanner.is_ground());
    }

    #[test]
    fn escape_inside_a_string_starts_a_new_sequence() {
        let (forward, messages, scanner) = run(&[b"\x1b]0;abc\x1b[31mX\x1b]6973;F\x1b[0m"]);
        assert_eq!(forward, b"\x1b]0;abc\x1b[31mX\x1b[0m");
        assert_eq!(messages, vec![b"F".to_vec()]);
        assert!(scanner.is_ground());
    }

    #[test]
    fn oversized_message_is_dropped() {
        let mut stream = b"\x1b]6973;".to_vec();
        stream.extend(std::iter::repeat_n(b'a', MAX_PAYLOAD + 10));
        stream.extend_from_slice(b"\x07ok");
        let (forward, messages, scanner) = run(&[&stream]);
        assert_eq!(forward, b"ok");
        assert!(messages.is_empty());
        assert!(scanner.is_ground());
    }
}
