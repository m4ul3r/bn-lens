//! A small pseudo-C tokenizer for decompiler output (replaces pygments).
//! Pure and testable: turns text into per-line runs of (text, kind). Colour
//! mapping lives in the viewer so this stays UI-free.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Comment,
    Keyword,
    Type,
    Str,
    Num,
    Name,
    Plain,
}

#[derive(Clone, Debug)]
pub struct Seg {
    pub text: String,
    pub kind: Tok,
}

pub type Line = Vec<Seg>;

const KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "do", "return", "goto", "switch", "case", "break", "continue",
    "default", "sizeof", "struct", "union", "enum", "typedef", "static", "const", "volatile",
    "unsigned", "signed", "extern", "register", "inline",
];

const TYPES: &[&str] = &[
    "void", "char", "short", "int", "long", "float", "double", "bool", "size_t", "ssize_t",
    "wchar_t", "FILE",
];

/// Control-flow words highlighted in the plain (MLIL/disasm) tokenizer, which
/// otherwise leaves identifiers unstyled. `return`/`goto` already read as
/// keywords in decompile; MLIL adds `noreturn`. Kept narrow so mnemonics and
/// registers stay unstyled.
const PLAIN_KEYWORDS: &[&str] = &["return", "noreturn", "goto"];

fn classify_ident(id: &str) -> Tok {
    if KEYWORDS.contains(&id) {
        Tok::Keyword
    } else if TYPES.contains(&id) || id.ends_with("_t") {
        Tok::Type
    } else {
        Tok::Name
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Tokenize one C line given the incoming block-comment state; returns the
/// segments and whether we are still inside a block comment.
fn tokenize_line(line: &str, mut in_block: bool) -> (Line, bool) {
    let mut segs: Line = Vec::new();
    let ch: Vec<char> = line.chars().collect();
    let n = ch.len();
    let mut i = 0;

    if in_block {
        // consume until "*/"
        if let Some(end) = find_pair(&ch, 0, '*', '/') {
            segs.push(seg(&ch[0..=end + 1], Tok::Comment));
            i = end + 2;
            in_block = false;
        } else {
            segs.push(seg(&ch[0..n], Tok::Comment));
            return (segs, true);
        }
    }

    while i < n {
        let c = ch[i];
        if c.is_whitespace() {
            let start = i;
            while i < n && ch[i].is_whitespace() {
                i += 1;
            }
            segs.push(seg(&ch[start..i], Tok::Plain));
        } else if c == '/' && i + 1 < n && ch[i + 1] == '/' {
            segs.push(seg(&ch[i..n], Tok::Comment));
            i = n;
        } else if c == '/' && i + 1 < n && ch[i + 1] == '*' {
            if let Some(end) = find_pair(&ch, i + 2, '*', '/') {
                segs.push(seg(&ch[i..=end + 1], Tok::Comment));
                i = end + 2;
            } else {
                segs.push(seg(&ch[i..n], Tok::Comment));
                return (segs, true);
            }
        } else if c == '"' || c == '\'' {
            let start = i;
            let q = c;
            i += 1;
            while i < n {
                if ch[i] == '\\' {
                    i += 2;
                    continue;
                }
                if ch[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            segs.push(seg(&ch[start..i.min(n)], Tok::Str));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < n && (is_ident(ch[i]) || ch[i] == 'x') {
                i += 1;
            }
            segs.push(seg(&ch[start..i], Tok::Num));
        } else if is_ident_start(c) {
            let start = i;
            while i < n && is_ident(ch[i]) {
                i += 1;
            }
            let id: String = ch[start..i].iter().collect();
            let kind = classify_ident(&id);
            segs.push(Seg { text: id, kind });
        } else {
            segs.push(seg(&ch[i..i + 1], Tok::Plain));
            i += 1;
        }
    }
    (segs, in_block)
}

fn find_pair(ch: &[char], from: usize, a: char, b: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < ch.len() {
        if ch[i] == a && ch[i + 1] == b {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn seg(chars: &[char], kind: Tok) -> Seg {
    Seg {
        text: chars.iter().collect(),
        kind,
    }
}

/// Tokenize decompiled C into per-line segment runs.
pub fn tokenize_c(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.split('\n') {
        let (segs, nb) = tokenize_line(line, in_block);
        in_block = nb;
        out.push(segs);
    }
    out
}

/// Tokenize plain output (xrefs, hex): addresses / numbers / identifiers.
pub fn tokenize_plain(text: &str) -> Vec<Line> {
    text.split('\n')
        .map(|line| {
            let ch: Vec<char> = line.chars().collect();
            let n = ch.len();
            let mut segs: Line = Vec::new();
            let mut i = 0;
            while i < n {
                let c = ch[i];
                if c == '0' && i + 1 < n && ch[i + 1] == 'x' {
                    let start = i;
                    i += 2;
                    while i < n && ch[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                    segs.push(seg(&ch[start..i], Tok::Type)); // address -> cyan
                } else if c.is_ascii_digit() {
                    // consume the whole hex run (disasm/mlil addresses & byte
                    // columns are bare hex like `0043274c`) — don't split at a-f
                    let start = i;
                    while i < n && ch[i].is_ascii_hexdigit() {
                        i += 1;
                    }
                    segs.push(seg(&ch[start..i], Tok::Num));
                } else if is_ident_start(c) {
                    let start = i;
                    while i < n && is_ident(ch[i]) {
                        i += 1;
                    }
                    // a 2-char (byte) or >=5-char (address) all-hex run that lexes
                    // as an identifier is really a hex value in a disasm dump — tag
                    // it Num so it dims uniformly. 3-4 char stays a Name so
                    // mnemonics like `add`/`adc` aren't dimmed.
                    let t: &[char] = &ch[start..i];
                    let hexish =
                        (t.len() == 2 || t.len() >= 5) && t.iter().all(|c| c.is_ascii_hexdigit());
                    let word: String = t.iter().collect();
                    let kind = if hexish {
                        Tok::Num
                    } else if PLAIN_KEYWORDS.contains(&word.as_str()) {
                        Tok::Keyword
                    } else {
                        Tok::Name
                    };
                    segs.push(seg(t, kind));
                } else {
                    segs.push(seg(&ch[i..i + 1], Tok::Plain));
                    i += 1;
                }
            }
            segs
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &Line) -> Vec<(&str, Tok)> {
        line.iter().map(|s| (s.text.as_str(), s.kind)).collect()
    }

    #[test]
    fn classifies_keywords_types_names() {
        assert_eq!(classify_ident("return"), Tok::Keyword);
        assert_eq!(classify_ident("uint64_t"), Tok::Type);
        assert_eq!(classify_ident("int"), Tok::Type);
        assert_eq!(classify_ident("srv_state"), Tok::Name);
    }

    #[test]
    fn plain_tokenizer_highlights_control_keywords_not_mnemonics() {
        // MLIL/disasm control words read as keywords; mnemonics, registers, and
        // hex columns keep their existing kinds.
        let line = &tokenize_plain("0040338c  goto 16 @ 0x40336c")[0];
        let segs = kinds(line);
        assert!(segs.iter().any(|(t, k)| *t == "goto" && *k == Tok::Keyword));
        assert!(segs.iter().any(|(t, k)| *t == "0040338c" && *k == Tok::Num));
        assert!(segs.iter().any(|(t, k)| *t == "0x40336c" && *k == Tok::Type));

        let ret = &tokenize_plain("00403404  noreturn")[0];
        assert!(kinds(ret)
            .iter()
            .any(|(t, k)| *t == "noreturn" && *k == Tok::Keyword));

        // `ret` is an aarch64 mnemonic, not the C `return` — must stay a Name.
        let asm = &tokenize_plain("00403404  ret")[0];
        assert!(kinds(asm).iter().any(|(t, k)| *t == "ret" && *k == Tok::Name));
    }

    #[test]
    fn tokenizes_a_line() {
        let lines = tokenize_c("int64_t x0 = msg_alloc();");
        let segs = kinds(&lines[0]);
        assert!(segs.iter().any(|(t, k)| *t == "int64_t" && *k == Tok::Type));
        assert!(segs
            .iter()
            .any(|(t, k)| *t == "msg_alloc" && *k == Tok::Name));
        // reassembling the segments round-trips the source text
        let joined: String = lines[0].iter().map(|s| s.text.clone()).collect();
        assert_eq!(joined, "int64_t x0 = msg_alloc();");
    }

    #[test]
    fn line_comment() {
        let lines = tokenize_c("x = 1;  // note here");
        assert!(lines[0]
            .iter()
            .any(|s| s.kind == Tok::Comment && s.text.contains("note here")));
    }

    #[test]
    fn block_comment_spans_lines() {
        let lines = tokenize_c("a /* start\nmiddle\nend */ b");
        assert_eq!(lines[1][0].kind, Tok::Comment); // whole middle line
        assert!(lines[2]
            .iter()
            .any(|s| s.text == "b" && s.kind != Tok::Comment));
    }

    #[test]
    fn hex_and_numbers() {
        let lines = tokenize_c("y = 0x9c4 + 16;");
        assert!(lines[0]
            .iter()
            .any(|s| s.text == "0x9c4" && s.kind == Tok::Num));
    }

    #[test]
    fn plain_addresses() {
        let lines = tokenize_plain("  0x402620  build_and_send  (1 site: 0x402658)");
        assert!(lines[0]
            .iter()
            .any(|s| s.text == "0x402620" && s.kind == Tok::Type));
        assert!(lines[0]
            .iter()
            .any(|s| s.text == "build_and_send" && s.kind == Tok::Name));
    }
}
