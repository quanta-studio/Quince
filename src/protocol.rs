//! The conversation between the wrapper and the shell integration.
//!
//! # Shell to wrapper
//!
//! The shell writes private escape sequences to the terminal, `ESC ] 6973 ; <payload> BEL`,
//! which [`crate::scan`] takes out of the stream. A payload is the session's token, a
//! semicolon, and one of these messages:
//!
//! | message                          | meaning                                         |
//! |----------------------------------|-------------------------------------------------|
//! | `B;<cursor>;<keymap>;<buffer>`   | the line editor is active with this edit buffer |
//! | `P;<cwd>`                        | a prompt is about to be shown in this directory |
//! | `E;<name>=<value>\0...`          | the shell's exported variables, when they change|
//! | `X`                              | a command is about to run                       |
//! | `F`                              | the line editor finished reading a line         |
//!
//! Free text is escaped by the shell, so that neither the sequence's own terminator nor the
//! terminal driver's output processing can touch it: `\\`, `\n`, `\r`, `\t`, `\e` (ESC), `\a`
//! (BEL) and `\0` (NUL).
//!
//! The token is what makes a message the shell's. Anything shown in a terminal can contain
//! escape sequences: a file being printed, the output of a remote host. Such text must not be
//! able to tell the wrapper which directory or environment to run commands in. The wrapper
//! makes up the token for each session and hands it to the shell in its environment, where
//! text on the screen cannot learn it; a message without it is dropped.
//!
//! # Wrapper to shell
//!
//! The wrapper never edits the command line by typing. To apply a suggestion it sends
//! [`ACCEPT_KEY`], which the integration binds to a widget, followed by the length of a payload
//! as eight decimal digits and then the payload: the hex encoding of four NUL-separated fields.
//!
//! | field | meaning                                                    |
//! |-------|------------------------------------------------------------|
//! | 1     | cursor position the suggestion was computed for            |
//! | 2     | how far to move the cursor back afterwards                 |
//! | 3     | edit buffer the suggestion was computed for                |
//! | 4     | new text for everything left of the cursor                 |
//!
//! The widget applies the edit only if the buffer and cursor still match fields 3 and 1, so a
//! suggestion can never land on a line that has moved on. Hex keeps the payload clear of
//! control characters the terminal driver would act on, and makes its length the same number of
//! characters in every locale.

/// Key sequence bound to the integration's accept widget. No keyboard sends it.
pub const ACCEPT_KEY: &[u8] = b"\x1b[6973~";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// `cursor` counts characters, as zsh's `$CURSOR` does. `keymap` is zsh's `$KEYMAP`.
    Buffer {
        text: String,
        cursor: usize,
        keymap: String,
    },
    Cwd(String),
    /// Every exported variable, as name and value.
    Environment(Vec<(String, String)>),
    Exec,
    Finish,
}

/// The bytes that ask the shell to replace everything left of the cursor with `left`, provided
/// its buffer is still `buffer` with the cursor `cursor` characters in.
pub fn accept_request(buffer: &str, cursor: usize, left: &str, cursor_back: usize) -> Vec<u8> {
    let fields = format!("{cursor}\0{cursor_back}\0{buffer}\0{left}");
    let payload: String = fields.bytes().map(|byte| format!("{byte:02x}")).collect();
    let mut request = ACCEPT_KEY.to_vec();
    request.extend_from_slice(format!("{:08}", payload.len()).as_bytes());
    request.extend_from_slice(payload.as_bytes());
    request
}

/// Reads a payload. `None` unless it carries `token` and a well-formed message.
pub fn parse(payload: &[u8], token: &str) -> Option<Message> {
    let message = payload.strip_prefix(token.as_bytes())?.strip_prefix(b";")?;
    match message {
        b"X" => Some(Message::Exec),
        b"F" => Some(Message::Finish),
        [b'P', b';', rest @ ..] => Some(Message::Cwd(unescape(rest))),
        [b'E', b';', rest @ ..] => {
            let packed = unescape(rest);
            let pairs = packed.split('\0').filter_map(|pair| pair.split_once('='));
            Some(Message::Environment(
                pairs
                    .filter(|(name, _)| !name.is_empty())
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect(),
            ))
        }
        [b'B', b';', rest @ ..] => {
            let mut fields = rest.splitn(3, |&b| b == b';');
            let cursor = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
            let keymap = std::str::from_utf8(fields.next()?).ok()?.to_owned();
            Some(Message::Buffer {
                text: unescape(fields.next()?),
                cursor,
                keymap,
            })
        }
        _ => None,
    }
}

fn unescape(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.iter().copied();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b't') => out.push(b'\t'),
            Some(b'e') => out.push(0x1b),
            Some(b'a') => out.push(0x07),
            Some(b'0') => out.push(0),
            Some(other) => out.push(other),
            None => out.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "5ecret";

    /// Parses `message` as sent by a shell that knows the token.
    fn sent(message: &[u8]) -> Option<Message> {
        parse(&[TOKEN.as_bytes(), b";", message].concat(), TOKEN)
    }

    fn buffer(text: &str, cursor: usize, keymap: &str) -> Option<Message> {
        Some(Message::Buffer {
            text: text.into(),
            cursor,
            keymap: keymap.into(),
        })
    }

    #[test]
    fn parses_each_message() {
        assert_eq!(sent(b"X"), Some(Message::Exec));
        assert_eq!(sent(b"F"), Some(Message::Finish));
        assert_eq!(
            sent(b"P;/Users/me/my dir"),
            Some(Message::Cwd("/Users/me/my dir".into()))
        );
        assert_eq!(sent(b"B;6;main;git ch"), buffer("git ch", 6, "main"));
        assert_eq!(sent(b"B;0;vicmd;"), buffer("", 0, "vicmd"));
    }

    #[test]
    fn buffer_may_contain_separators_and_escapes() {
        assert_eq!(
            sent(br"B;3;main;a;b\nc\\d\e\a\r\tz"),
            buffer("a;b\nc\\d\x1b\x07\r\tz", 3, "main")
        );
    }

    #[test]
    fn environment_is_names_and_values() {
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        assert_eq!(
            sent(br"E;PATH=/bin:/usr/bin\0EMPTY=\0MULTI=a\nb=c\0BACK=x\\0y\0"),
            Some(Message::Environment(vec![
                pair("PATH", "/bin:/usr/bin"),
                pair("EMPTY", ""),
                pair("MULTI", "a\nb=c"),
                // A backslash followed by a zero in a value is not the separator.
                pair("BACK", "x\\0y"),
            ]))
        );
        assert_eq!(sent(b"E;"), Some(Message::Environment(Vec::new())));
        // Entries that are not a name and a value are left out.
        assert_eq!(
            sent(br"E;novalue\0=noname\0OK=1"),
            Some(Message::Environment(vec![pair("OK", "1")]))
        );
    }

    #[test]
    fn rejects_malformed_messages() {
        for bad in [
            &b""[..],
            b"B",
            b"B;",
            b"B;x;main;abc",
            b"B;12",
            b"B;12;main",
            b"Q;1",
            b"XX",
        ] {
            assert_eq!(sent(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_message_without_the_token_is_not_the_shells() {
        assert_eq!(parse(b"5ecret;X", TOKEN), Some(Message::Exec));
        for forged in [
            &b"X"[..],
            b"P;/tmp/evil",
            b";P;/tmp/evil",
            b"wrong;P;/tmp/evil",
            b"5ecre;P;/tmp/evil",
            b"5ecretP;/tmp/evil",
            b"5ecret",
        ] {
            assert_eq!(parse(forged, TOKEN), None, "{forged:?}");
        }
    }

    #[test]
    fn accept_request_is_a_key_a_length_and_hex_fields() {
        let request = accept_request("git ch", 6, "git checkout ", 0);
        let (key, rest) = request.split_at(ACCEPT_KEY.len());
        assert_eq!(key, ACCEPT_KEY);
        let (length, payload) = rest.split_at(8);
        assert_eq!(length, b"00000048");
        assert_eq!(payload.len(), 48);
        // "6", "0", "git ch" and "git checkout ", NUL separated.
        assert_eq!(payload, b"360030006769742063680067697420636865636b6f757420");
    }

    #[test]
    fn accept_request_stays_printable_ascii() {
        let request = accept_request("echo '日本\n\x03\r'", 9, "echo '日本語\x1b", 2);
        assert!(request[ACCEPT_KEY.len()..].iter().all(u8::is_ascii_hexdigit));
        let digits = std::str::from_utf8(&request[ACCEPT_KEY.len()..ACCEPT_KEY.len() + 8]).unwrap();
        assert_eq!(digits.parse::<usize>().unwrap(), request.len() - ACCEPT_KEY.len() - 8);
    }
}
