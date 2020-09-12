//! Tokenization.

use std::iter::Peekable;
use std::str::Chars;
use unicode_xid::UnicodeXID;

use crate::length::Length;
use crate::syntax::{Pos, Span, Spanned, Token};

use Token::*;
use TokenMode::*;

/// An iterator over the tokens of a string of source code.
#[derive(Debug)]
pub struct Tokens<'s> {
    src: &'s str,
    iter: Peekable<Chars<'s>>,
    mode: TokenMode,
    stack: Vec<TokenMode>,
    pos: Pos,
    index: usize,
}

/// Whether to tokenize in header mode which yields expression, comma and
/// similar tokens or in body mode which yields text and star, underscore,
/// backtick tokens.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum TokenMode {
    Header,
    Body,
}

impl<'s> Tokens<'s> {
    /// Create a new token iterator with the given mode.
    pub fn new(src: &'s str, mode: TokenMode) -> Self {
        Self {
            src,
            iter: src.chars().peekable(),
            mode,
            stack: vec![],
            pos: Pos::ZERO,
            index: 0,
        }
    }

    /// Change the token mode and push the old one on a stack.
    pub fn push_mode(&mut self, mode: TokenMode) {
        self.stack.push(self.mode);
        self.mode = mode;
    }

    /// Pop the old token mode from the stack. This panics if there is no mode
    /// on the stack.
    pub fn pop_mode(&mut self) {
        self.mode = self.stack.pop().expect("no pushed mode");
    }

    /// The index in the string at which the last token ends and next token will
    /// start.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The line-colunn position in the source at which the last token ends and
    /// next token will start.
    pub fn pos(&self) -> Pos {
        self.pos
    }
}

impl<'s> Iterator for Tokens<'s> {
    type Item = Spanned<Token<'s>>;

    /// Parse the next token in the source code.
    fn next(&mut self) -> Option<Self::Item> {
        let start = self.pos();
        let first = self.eat()?;

        let token = match first {
            // Comments.
            '/' if self.peek() == Some('/') => self.read_line_comment(),
            '/' if self.peek() == Some('*') => self.read_block_comment(),
            '*' if self.peek() == Some('/') => {
                self.eat();
                Invalid("*/")
            }

            // Whitespace.
            c if c.is_whitespace() => self.read_whitespace(start),

            // Functions and blocks.
            '[' => LeftBracket,
            ']' => RightBracket,
            '{' => LeftBrace,
            '}' => RightBrace,

            // Syntactic elements in function headers.
            '(' if self.mode == Header => LeftParen,
            ')' if self.mode == Header => RightParen,
            ':' if self.mode == Header => Colon,
            ',' if self.mode == Header => Comma,
            '=' if self.mode == Header => Equals,
            '>' if self.mode == Header && self.peek() == Some('>') => self.read_chain(),

            // Expression operators.
            '+' if self.mode == Header => Plus,
            '-' if self.mode == Header => Hyphen,
            '/' if self.mode == Header => Slash,

            // Star serves a double purpose as a style modifier
            // and a expression operator in the header.
            '*' => Star,

            // A hex expression.
            '#' if self.mode == Header => self.read_hex(),

            // String values.
            '"' if self.mode == Header => self.read_string(),

            // Style toggles.
            '_' if self.mode == Body => Underscore,
            '`' if self.mode == Body => self.read_raw(),

            // Sections.
            '#' if self.mode == Body => Hashtag,

            // Non-breaking spaces.
            '~' if self.mode == Body => Text("\u{00A0}"),

            // An escaped thing.
            '\\' if self.mode == Body => self.read_escaped(),

            // Expressions or just strings.
            c => {
                let body = self.mode == Body;

                let start_offset = -(c.len_utf8() as isize);
                let mut last_was_e = false;

                let (text, _) = self.read_string_until(false, start_offset, 0, |n| {
                    let val = match n {
                        c if c.is_whitespace() => true,
                        '[' | ']' | '{' | '}' | '/' | '*' => true,
                        '\\' | '_' | '`' | '#' | '~' if body => true,
                        ':' | '=' | ',' | '"' | '(' | ')' if !body => true,
                        '+' | '-' if !body && !last_was_e => true,
                        _ => false,
                    };

                    last_was_e = n == 'e' || n == 'E';
                    val
                });

                if self.mode == Header {
                    self.read_expr(text)
                } else {
                    Text(text)
                }
            }
        };

        let end = self.pos();
        let span = Span { start, end };

        Some(Spanned { v: token, span })
    }
}

impl<'s> Tokens<'s> {
    fn read_line_comment(&mut self) -> Token<'s> {
        self.eat();
        LineComment(self.read_string_until(false, 0, 0, is_newline_char).0)
    }

    fn read_block_comment(&mut self) -> Token<'s> {
        enum Last {
            Slash,
            Star,
            Other,
        }

        let mut depth = 0;
        let mut last = Last::Other;

        // Find the first `*/` that does not correspond to a nested `/*`.
        // Remove the last two bytes to obtain the raw inner text without `*/`.
        self.eat();
        let (content, _) = self.read_string_until(true, 0, -2, |c| {
            match c {
                '/' => match last {
                    Last::Star if depth == 0 => return true,
                    Last::Star => depth -= 1,
                    _ => last = Last::Slash,
                },
                '*' => match last {
                    Last::Slash => depth += 1,
                    _ => last = Last::Star,
                },
                _ => last = Last::Other,
            }

            false
        });

        BlockComment(content)
    }

    fn read_chain(&mut self) -> Token<'s> {
        assert!(self.eat() == Some('>'));
        Chain
    }

    fn read_whitespace(&mut self, start: Pos) -> Token<'s> {
        self.read_string_until(false, 0, 0, |n| !n.is_whitespace());
        let end = self.pos();

        Space(end.line - start.line)
    }

    fn read_string(&mut self) -> Token<'s> {
        let (string, terminated) = self.read_until_unescaped('"');
        Str { string, terminated }
    }

    fn read_raw(&mut self) -> Token<'s> {
        let mut backticks = 1;
        while self.peek() == Some('`') {
            self.eat();
            backticks += 1;
        }

        let mut lang = None;
        if backticks > 1 {
            // Read the lang tag (until newline or whitespace).
            let start = self.pos();
            let (tag, _) = self.read_string_until(false, 0, 0, |c| {
                c == '`' || c.is_whitespace() || is_newline_char(c)
            });
            let end = self.pos();

            if !tag.is_empty() {
                lang = Some(Spanned::new(tag, Span::new(start, end)));
            }
        }

        let start = self.index();
        let mut found = 0;

        while found < backticks {
            match self.eat() {
                Some('`') => found += 1,
                Some(_) => found = 0,
                None => break,
            }
        }

        let terminated = found == backticks;
        let end = self.index() - if terminated { found } else { 0 };

        Raw {
            backticks,
            lang,
            raw: &self.src[start .. end],
            terminated,
        }
    }

    fn read_until_unescaped(&mut self, end: char) -> (&'s str, bool) {
        let mut escaped = false;
        self.read_string_until(true, 0, -1, |c| {
            match c {
                c if c == end && !escaped => return true,
                '\\' => escaped = !escaped,
                _ => escaped = false,
            }

            false
        })
    }

    fn read_escaped(&mut self) -> Token<'s> {
        fn is_escapable(c: char) -> bool {
            match c {
                '[' | ']' | '\\' | '/' | '*' | '_' | '`' | '"' | '#' | '~' => true,
                _ => false,
            }
        }

        match self.peek() {
            Some('u') => {
                self.eat();
                if self.peek() == Some('{') {
                    self.eat();
                    let (sequence, _) =
                        self.read_string_until(false, 0, 0, |c| !c.is_ascii_hexdigit());

                    let terminated = self.peek() == Some('}');
                    if terminated {
                        self.eat();
                    }

                    UnicodeEscape { sequence, terminated }
                } else {
                    Text("\\u")
                }
            }
            Some(c) if is_escapable(c) => {
                let index = self.index();
                self.eat();
                Text(&self.src[index .. index + c.len_utf8()])
            }
            Some(c) if c.is_whitespace() => Backslash,
            Some(_) => Text("\\"),
            None => Backslash,
        }
    }

    fn read_hex(&mut self) -> Token<'s> {
        // This will parse more than the permissable 0-9, a-f, A-F character
        // ranges to provide nicer error messages later.
        Hex(self.read_string_until(false, 0, 0, |n| !n.is_ascii_alphanumeric()).0)
    }

    fn read_expr(&mut self, text: &'s str) -> Token<'s> {
        if let Ok(b) = text.parse::<bool>() {
            Bool(b)
        } else if let Ok(num) = text.parse::<f64>() {
            Number(num)
        } else if let Some(num) = parse_percentage(text) {
            Number(num / 100.0)
        } else if let Ok(length) = text.parse::<Length>() {
            Length(length)
        } else if is_ident(text) {
            Ident(text)
        } else {
            Invalid(text)
        }
    }

    /// Will read the input stream until `f` evaluates to `true`. When
    /// `eat_match` is true, the token for which `f` was true is consumed.
    /// Returns the string from the index where this was called offset by
    /// `offset_start` to the end offset by `offset_end`. The end is before or
    /// after the match depending on `eat_match`.
    fn read_string_until(
        &mut self,
        eat_match: bool,
        offset_start: isize,
        offset_end: isize,
        mut f: impl FnMut(char) -> bool,
    ) -> (&'s str, bool) {
        let start = ((self.index() as isize) + offset_start) as usize;
        let mut matched = false;

        while let Some(c) = self.peek() {
            if f(c) {
                matched = true;
                if eat_match {
                    self.eat();
                }
                break;
            }

            self.eat();
        }

        let mut end = self.index();
        if matched {
            end = ((end as isize) + offset_end) as usize;
        }

        (&self.src[start .. end], matched)
    }

    fn eat(&mut self) -> Option<char> {
        let c = self.iter.next()?;
        self.index += c.len_utf8();

        if is_newline_char(c) && !(c == '\r' && self.peek() == Some('\n')) {
            self.pos.line += 1;
            self.pos.column = 0;
        } else {
            self.pos.column += 1;
        }

        Some(c)
    }

    fn peek(&mut self) -> Option<char> {
        self.iter.peek().copied()
    }
}

fn parse_percentage(text: &str) -> Option<f64> {
    if text.ends_with('%') {
        text[.. text.len() - 1].parse::<f64>().ok()
    } else {
        None
    }
}

/// Whether this character denotes a newline.
pub fn is_newline_char(character: char) -> bool {
    match character {
        // Line Feed, Vertical Tab, Form Feed, Carriage Return.
        '\x0A' ..= '\x0D' => true,
        // Next Line, Line Separator, Paragraph Separator.
        '\u{0085}' | '\u{2028}' | '\u{2029}' => true,
        _ => false,
    }
}

/// Whether this word is a valid identifier.
pub fn is_ident(string: &str) -> bool {
    fn is_also_allowed(c: char) -> bool {
        c == '.' || c == '-' || c == '_'
    }

    let mut chars = string.chars();
    match chars.next() {
        Some(c) if c.is_xid_start() || is_also_allowed(c) => {}
        _ => return false,
    }

    for c in chars {
        if !c.is_xid_continue() && !is_also_allowed(c) {
            return false;
        }
    }

    true
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::length::Length;
    use crate::parse::check::*;
    use crate::syntax::Spanned;

    use Token::{
        BlockComment as BC, Bool, Chain, Hex, Hyphen as Min, Ident as Id,
        LeftBrace as LB, LeftBracket as L, LeftParen as LP, Length as Len,
        LineComment as LC, Number as Num, Plus, RightBrace as RB, RightBracket as R,
        RightParen as RP, Slash, Space as S, Star, Text as T,
    };

    fn Str(string: &str, terminated: bool) -> Token {
        Token::Str { string, terminated }
    }
    fn Raw<'a>(
        backticks: usize,
        lang: Option<Spanned<&'a str>>,
        raw: &'a str,
        terminated: bool,
    ) -> Token<'a> {
        Token::Raw { backticks, lang, raw, terminated }
    }
    fn Lang<'a, T: Into<Spanned<&'a str>>>(lang: T) -> Option<Spanned<&'a str>> {
        Some(Into::<Spanned<&str>>::into(lang))
    }
    fn UE(sequence: &str, terminated: bool) -> Token {
        Token::UnicodeEscape { sequence, terminated }
    }

    macro_rules! t { ($($tts:tt)*) => {test!(@spans=false, $($tts)*)} }
    macro_rules! ts { ($($tts:tt)*) => {test!(@spans=true, $($tts)*)} }
    macro_rules! test {
        (@spans=$spans:expr, $mode:expr, $src:expr => $($token:expr),*) => {
            let exp = vec![$(Into::<Spanned<Token>>::into($token)),*];
            let found = Tokens::new($src, $mode).collect::<Vec<_>>();
            check($src, exp, found, $spans);
        }
    }

    #[test]
    fn test_tokenize_whitespace() {
        t!(Body, ""             => );
        t!(Body, " "            => S(0));
        t!(Body, "    "         => S(0));
        t!(Body, "\t"           => S(0));
        t!(Body, "  \t"         => S(0));
        t!(Body, "\n"           => S(1));
        t!(Body, "\n "          => S(1));
        t!(Body, "  \n"         => S(1));
        t!(Body, "  \n   "      => S(1));
        t!(Body, "\r\n"         => S(1));
        t!(Body, "  \n\t \n  "  => S(2));
        t!(Body, "\n\r"         => S(2));
        t!(Body, " \r\r\n \x0D" => S(3));
        t!(Body, "a~b"          => T("a"), T("\u{00A0}"), T("b"));
    }

    #[test]
    fn test_tokenize_comments() {
        t!(Body, "a // bc\n "        => T("a"), S(0), LC(" bc"),  S(1));
        t!(Body, "a //a//b\n "       => T("a"), S(0), LC("a//b"), S(1));
        t!(Body, "a //a//b\r\n"      => T("a"), S(0), LC("a//b"), S(1));
        t!(Body, "a //a//b\n\nhello" => T("a"), S(0), LC("a//b"), S(2), T("hello"));
        t!(Body, "/**/"              => BC(""));
        t!(Body, "_/*_/*a*/*/"       => Underscore, BC("_/*a*/"));
        t!(Body, "/*/*/"             => BC("/*/"));
        t!(Body, "abc*/"             => T("abc"), Invalid("*/"));
        t!(Body, "/***/"             => BC("*"));
        t!(Body, "/**\\****/*/*/"    => BC("*\\***"), Invalid("*/"), Invalid("*/"));
        t!(Body, "/*abc"             => BC("abc"));
    }

    #[test]
    fn test_tokenize_body_only_tokens() {
        t!(Body, "_*"            => Underscore, Star);
        t!(Body, "***"           => Star, Star, Star);
        t!(Body, "[func]*bold*"  => L, T("func"), R, Star, T("bold"), Star);
        t!(Body, "hi_you_ there" => T("hi"), Underscore, T("you"), Underscore, S(0), T("there"));
        t!(Body, "# hi"          => Hashtag, S(0), T("hi"));
        t!(Body, "#()"           => Hashtag, T("()"));
        t!(Body, "\\ "           => Backslash, S(0));
        t!(Header, "_`"          => Invalid("_`"));
    }

    #[test]
    fn test_tokenize_raw() {
        // Basics.
        t!(Body, "`raw`"    => Raw(1, None, "raw", true));
        t!(Body, "`[func]`" => Raw(1, None, "[func]", true));
        t!(Body, "`]"       => Raw(1, None, "]", false));
        t!(Body, r"`\`` "   => Raw(1, None, r"\", true), Raw(1, None, " ", false));

        // Language tag.
        t!(Body, "``` hi```"     => Raw(3, None, " hi", true));
        t!(Body, "```rust hi```" => Raw(3, Lang("rust"), " hi", true));
        t!(Body, r"``` hi\````"  => Raw(3, None, r" hi\", true), Raw(1, None, "", false));
        t!(Body, "``` not `y`e`t finished```" => Raw(3, None, " not `y`e`t finished", true));
        t!(Body, "```js   \r\n  document.write(\"go\")`"
            => Raw(3, Lang("js"), "   \r\n  document.write(\"go\")`", false));

        // More backticks.
        t!(Body, "`````` ``````hi"  => Raw(6, None, " ", true), T("hi"));
        t!(Body, "````\n```js\nalert()\n```\n````" => Raw(4, None, "\n```js\nalert()\n```\n", true));
    }

    #[test]
    fn test_tokenize_header_only_tokens() {
        t!(Body, "a: b"                => T("a:"), S(0), T("b"));
        t!(Body, "c=d, "               => T("c=d,"), S(0));
        t!(Header, "(){}:=,"           => LP, RP, LB, RB, Colon, Equals, Comma);
        t!(Header, "a:b"               => Id("a"), Colon, Id("b"));
        t!(Header, "#6ae6dd"           => Hex("6ae6dd"));
        t!(Header, "#8A083c"           => Hex("8A083c"));
        t!(Header, "a: true, x=1"      => Id("a"), Colon, S(0), Bool(true), Comma, S(0),
                                          Id("x"), Equals, Num(1.0));
        t!(Header, "=3.14"             => Equals, Num(3.14));
        t!(Header, "12.3e5"            => Num(12.3e5));
        t!(Header, "120%"              => Num(1.2));
        t!(Header, "12e4%"             => Num(1200.0));
        t!(Header, "__main__"          => Id("__main__"));
        t!(Header, ">main"             => Invalid(">main"));
        t!(Header, ".func.box"         => Id(".func.box"));
        t!(Header, "arg, _b, _1"       => Id("arg"), Comma, S(0), Id("_b"), Comma, S(0), Id("_1"));
        t!(Header, "f: arg >> g"       => Id("f"), Colon, S(0), Id("arg"), S(0), Chain, S(0), Id("g"));
        t!(Header, "12_pt, 12pt"       => Invalid("12_pt"), Comma, S(0), Len(Length::pt(12.0)));
        t!(Header, "1e5in"             => Len(Length::inches(100000.0)));
        t!(Header, "2.3cm"             => Len(Length::cm(2.3)));
        t!(Header, "12e-3in"           => Len(Length::inches(12e-3)));
        t!(Header, "6.1cm + 4pt,a=1*2" => Len(Length::cm(6.1)), S(0), Plus, S(0), Len(Length::pt(4.0)),
                                          Comma, Id("a"), Equals, Num(1.0), Star, Num(2.0));
        t!(Header, "(5 - 1) / 2.1"     => LP, Num(5.0), S(0), Min, S(0), Num(1.0), RP,
                                          S(0), Slash, S(0), Num(2.1));
        t!(Header, "-1"                => Min, Num(1.0));
        t!(Header, "--1"               => Min, Min, Num(1.0));
        t!(Header, "- 1"               => Min, S(0), Num(1.0));
        t!(Header, "02.4mm"            => Len(Length::mm(2.4)));
        t!(Header, "2.4.cm"            => Invalid("2.4.cm"));
        t!(Header, "(1,2)"             => LP, Num(1.0), Comma, Num(2.0), RP);
        t!(Header, "{abc}"             => LB, Id("abc"), RB);
        t!(Header, "🌓, 🌍,"          => Invalid("🌓"), Comma, S(0), Invalid("🌍"), Comma);
    }

    #[test]
    fn test_tokenize_strings() {
        t!(Body, "a \"hi\" string"           => T("a"), S(0), T("\"hi\""), S(0), T("string"));
        t!(Header, "\"hello"                 => Str("hello", false));
        t!(Header, "\"hello world\""         => Str("hello world", true));
        t!(Header, "\"hello\nworld\""        => Str("hello\nworld", true));
        t!(Header, r#"1"hello\nworld"false"# => Num(1.0), Str("hello\\nworld", true), Bool(false));
        t!(Header, r#""a\"bc""#              => Str(r#"a\"bc"#, true));
        t!(Header, r#""a\\"bc""#             => Str(r#"a\\"#, true), Id("bc"), Str("", false));
        t!(Header, r#""a\tbc"#               => Str("a\\tbc", false));
        t!(Header, "\"🌎\""                  => Str("🌎", true));
    }

    #[test]
    fn test_tokenize_escaped_symbols() {
        t!(Body, r"\\"       => T(r"\"));
        t!(Body, r"\["       => T("["));
        t!(Body, r"\]"       => T("]"));
        t!(Body, r"\*"       => T("*"));
        t!(Body, r"\_"       => T("_"));
        t!(Body, r"\`"       => T("`"));
        t!(Body, r"\/"       => T("/"));
        t!(Body, r"\u{2603}" => UE("2603", true));
        t!(Body, r"\u{26A4"  => UE("26A4", false));
        t!(Body, r#"\""#     => T("\""));
    }

    #[test]
    fn test_tokenize_unescapable_symbols() {
        t!(Body, r"\a"     => T("\\"), T("a"));
        t!(Body, r"\:"     => T(r"\"), T(":"));
        t!(Body, r"\="     => T(r"\"), T("="));
        t!(Body, r"\u{2GA4"=> UE("2", false), T("GA4"));
        t!(Body, r"\u{ "   => UE("", false), Space(0));
        t!(Body, r"\u"     => T(r"\u"));
        t!(Header, r"\\\\" => Invalid(r"\\\\"));
        t!(Header, r"\a"   => Invalid(r"\a"));
        t!(Header, r"\:"   => Invalid(r"\"), Colon);
        t!(Header, r"\="   => Invalid(r"\"), Equals);
        t!(Header, r"\,"   => Invalid(r"\"), Comma);
    }

    #[test]
    fn test_tokenize_with_spans() {
        ts!(Body, "hello"          => s(0,0, 0,5, T("hello")));
        ts!(Body, "ab\r\nc"        => s(0,0, 0,2, T("ab")), s(0,2, 1,0, S(1)), s(1,0, 1,1, T("c")));
        ts!(Body, "// ab\r\n\nf"   => s(0,0, 0,5, LC(" ab")), s(0,5, 2,0, S(2)), s(2,0, 2,1, T("f")));
        ts!(Body, "/*b*/_"         => s(0,0, 0,5, BC("b")), s(0,5, 0,6, Underscore));
        ts!(Header, "a=10"         => s(0,0, 0,1, Id("a")), s(0,1, 0,2, Equals), s(0,2, 0,4, Num(10.0)));
    }
}
