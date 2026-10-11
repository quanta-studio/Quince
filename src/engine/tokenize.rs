//! Splits the text left of the cursor into the words of the command being typed.
//!
//! This is a deliberately small model of shell syntax: quoting, backslash escapes, command
//! separators, redirections and `$( )` nesting. It does not expand anything.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    /// Completed words of the command the cursor is in, with quoting removed.
    pub words: Vec<String>,
    pub current: Current,
    /// The token under the cursor is the target of a redirection, so it names a file.
    pub redirect: bool,
}

/// The token under the cursor, up to the cursor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Current {
    /// As typed, including quotes and backslashes.
    pub raw: String,
    /// With quoting removed.
    pub value: String,
    /// The quote character still open at the cursor.
    pub quote: Option<char>,
}

#[derive(Default)]
struct Frame {
    words: Vec<String>,
    raw: String,
    value: String,
    in_word: bool,
    redirect_next: bool,
}

impl Frame {
    fn end_word(&mut self) {
        if self.in_word {
            let value = std::mem::take(&mut self.value);
            if self.redirect_next {
                self.redirect_next = false;
            } else {
                self.words.push(value);
            }
            self.raw.clear();
            self.in_word = false;
        }
    }

    fn push(&mut self, c: char) {
        self.raw.push(c);
        self.value.push(c);
        self.in_word = true;
    }
}

/// Returns `None` when the cursor is inside a comment.
pub fn tokenize(text: &str) -> Option<Line> {
    let mut frame = Frame::default();
    let mut outer: Vec<Frame> = Vec::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                frame.raw.push(c);
                if c == '\'' {
                    quote = None;
                } else {
                    frame.value.push(c);
                }
                continue;
            }
            Some(_) => {
                frame.raw.push(c);
                match c {
                    '"' => quote = None,
                    '\\' => match chars.peek() {
                        Some(&next) if matches!(next, '"' | '\\' | '$' | '`') => {
                            chars.next();
                            frame.raw.push(next);
                            frame.value.push(next);
                        }
                        _ => frame.value.push('\\'),
                    },
                    _ => frame.value.push(c),
                }
                continue;
            }
            None => {}
        }

        match c {
            '\\' => {
                frame.raw.push(c);
                frame.in_word = true;
                if let Some(next) = chars.next() {
                    frame.raw.push(next);
                    if next != '\n' {
                        frame.value.push(next);
                    }
                }
            }
            '\'' | '"' => {
                frame.raw.push(c);
                frame.in_word = true;
                quote = Some(c);
            }
            ' ' | '\t' => frame.end_word(),
            '(' => {
                // `$(` or a subshell: a new command starts, and the outer one resumes at `)`.
                outer.push(std::mem::take(&mut frame));
            }
            ')' => match outer.pop() {
                Some(resumed) => {
                    frame = resumed;
                    frame.in_word = true;
                }
                None => frame = Frame::default(),
            },
            '&' if frame.redirect_next && !frame.in_word => {} // `>&2`
            '&' if chars.peek() == Some(&'>') => frame.end_word(), // `&>file`
            '\n' | ';' | '|' | '&' | '`' => frame = Frame::default(),
            '<' | '>' => {
                if frame.in_word && frame.raw.bytes().all(|b| b.is_ascii_digit()) {
                    // `2>`: the descriptor number is not an argument.
                    frame.raw.clear();
                    frame.value.clear();
                    frame.in_word = false;
                } else {
                    frame.end_word();
                }
                frame.redirect_next = true;
            }
            '#' if !frame.in_word => return None,
            _ => frame.push(c),
        }
    }

    Some(Line {
        words: frame.words,
        current: Current {
            raw: frame.raw,
            value: frame.value,
            quote,
        },
        redirect: frame.redirect_next,
    })
}

/// Rewrites `value` as shell text that reads back as `value`, in the quoting style the user
/// started with. With an open quote the result begins with that quote and is left unclosed.
pub fn quote(value: &str, style: Option<char>) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    match style {
        Some('\'') => {
            out.push('\'');
            out.push_str(&value.replace('\'', r"'\''"));
        }
        Some(_) => {
            out.push('"');
            for c in value.chars() {
                if matches!(c, '"' | '\\' | '$' | '`') {
                    out.push('\\');
                }
                out.push(c);
            }
        }
        None => {
            for (i, c) in value.chars().enumerate() {
                let special = matches!(
                    c,
                    ' ' | '\t'
                        | '!'
                        | '"'
                        | '#'
                        | '$'
                        | '&'
                        | '\''
                        | '('
                        | ')'
                        | '*'
                        | ';'
                        | '<'
                        | '>'
                        | '?'
                        | '['
                        | '\\'
                        | ']'
                        | '^'
                        | '`'
                        | '{'
                        | '|'
                        | '}'
                ) || (c == '~' && i == 0 && !value.starts_with("~/") && value != "~");
                if special {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> Line {
        tokenize(text).expect("not a comment")
    }

    fn words(text: &str) -> Vec<String> {
        line(text).words
    }

    #[test]
    fn splits_words_and_tracks_the_current_token() {
        let l = line("git checkout ma");
        assert_eq!(l.words, ["git", "checkout"]);
        assert_eq!(l.current.raw, "ma");
        assert_eq!(l.current.value, "ma");
        assert!(!l.redirect);

        let l = line("git checkout ");
        assert_eq!(l.words, ["git", "checkout"]);
        assert_eq!(l.current, Current::default());

        assert_eq!(line(""), Line::default());
    }

    #[test]
    fn removes_quoting_but_keeps_the_raw_text() {
        let l = line(r#"cp 'my file' "a \"b\" $c" d\ e"#);
        assert_eq!(l.words, ["cp", "my file", r#"a "b" $c"#]);
        assert_eq!(l.current.raw, r"d\ e");
        assert_eq!(l.current.value, "d e");
        assert_eq!(l.current.quote, None);
    }

    #[test]
    fn reports_an_open_quote() {
        let l = line(r#"cat "my fi"#);
        assert_eq!(l.words, ["cat"]);
        assert_eq!(l.current.raw, r#""my fi"#);
        assert_eq!(l.current.value, "my fi");
        assert_eq!(l.current.quote, Some('"'));

        assert_eq!(line("cat 'it").current.quote, Some('\''));
    }

    #[test]
    fn only_the_last_command_counts() {
        assert_eq!(words("ls -la | grep foo && git st"), ["git"]);
        assert_eq!(words("echo a; npm run "), ["npm", "run"]);
        assert_eq!(words("sleep 1 & docker "), ["docker"]);
        assert_eq!(words("echo one\ngit "), ["git"]);
        // Separators inside quotes are text.
        assert_eq!(words("echo 'a | b' c "), ["echo", "a | b", "c"]);
    }

    #[test]
    fn command_substitution_nests_and_resumes() {
        assert_eq!(words("echo $(git br"), ["git"]);
        let l = line("echo $(git branch) fo");
        assert_eq!(l.words, ["echo", "$"]);
        assert_eq!(l.current.value, "fo");
    }

    #[test]
    fn redirections_are_not_arguments() {
        let l = line("git log > out.txt --one");
        assert_eq!(l.words, ["git", "log"]);
        assert_eq!(l.current.value, "--one");
        assert!(!l.redirect);

        let l = line("git log 2> er");
        assert_eq!(l.words, ["git", "log"]);
        assert_eq!(l.current.value, "er");
        assert!(l.redirect);

        assert!(line("sort < ").redirect);
        assert_eq!(words("make 2>&1 all "), ["make", "all"]);
        assert_eq!(words("make &> log all "), ["make", "all"]);
    }

    #[test]
    fn comments_have_no_line() {
        assert_eq!(tokenize("git status # show wh"), None);
        assert_eq!(tokenize("# note"), None);
        // A `#` inside a word is not a comment.
        assert_eq!(words("git show HEAD#1 "), ["git", "show", "HEAD#1"]);
    }

    #[test]
    fn quote_round_trips_through_tokenize() {
        for value in [
            "plain",
            "my file.txt",
            "it's",
            r#"say "hi""#,
            "a$b",
            "x&y",
            "dir/sub dir/",
            "~weird",
            "~/ok",
        ] {
            for style in [None, Some('"'), Some('\'')] {
                let mut text = format!("cmd {}", quote(value, style));
                if let Some(q) = style {
                    text.push(q);
                }
                text.push(' ');
                assert_eq!(words(&text), ["cmd", value], "{value:?} with {style:?}: {text}");
            }
        }
        assert_eq!(quote("my file", None), r"my\ file");
        assert_eq!(quote("~/a b", None), r"~/a\ b");
    }
}
