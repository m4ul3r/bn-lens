//! A small pseudo-C tokenizer for decompiler output (replaces pygments).
//! Pure and testable: turns text into per-line runs of (text, kind). Colour
//! mapping lives in the viewer so this stays UI-free.
//!
//! One scanner serves both flavours of BN output. The differences — comments,
//! string literals, how a number is read, how a bare word is classified — are
//! data on a [`Dialect`], not a second copy of the loop, so a lexing fix lands
//! in decompile *and* MLIL/disasm/xrefs at once.
//!
//! Two invariants the consumers depend on:
//!
//! - **Round-trip.** Concatenating a line's segment texts reproduces the source
//!   line exactly. `build_spans` derives hotspot columns by accumulating segment
//!   widths, and rendering/mouse hit-testing follow from those columns.
//! - **Kinds are lexical, not visual.** A `0x…` literal is [`Tok::Hex`] whether
//!   or not it turns out to name a mapped address; deciding that is the section
//!   map's job downstream. Colour is `theme.rs`'s job.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Comment,
    Keyword,
    Type,
    Str,
    /// A bare numeric run: a decimal literal, or an unprefixed hex column in a
    /// disassembly dump.
    Num,
    /// A `0x…` literal. Whether it *is* an address is resolved against the
    /// section map by the hotspot pass — the lexer only sees the prefix.
    Hex,
    Name,
    /// An operator or separator, longest-match (`::`, `->`, `==`, …).
    Punct,
    /// Whitespace, and anything the dialect does not recognise.
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

/// Multi-character operators, longest first so the scan is greedy. Single
/// characters need no entry — they are the fallback.
const OPERATORS: &[&str] = &[
    "<<=", ">>=", "...", "::", "->", "==", "!=", "<=", ">=", "&&", "||", "<<", ">>", "++", "--",
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
];

/// How a dialect reads a numeric run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NumberStyle {
    /// Pseudo-C: a digit opens a literal that may carry a `0x` prefix.
    C,
    /// Dump output: `0x…` is explicit, and a bare run is hex (an address
    /// column or byte value), not decimal.
    HexDump,
}

/// How a dialect classifies a bare word.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdentStyle {
    /// C keywords, C types, and the `_t` suffix convention.
    C,
    /// Assembly: hex-looking runs dim as numbers, a few control words read as
    /// keywords, and everything else (mnemonics, registers) stays a plain name.
    Asm,
}

#[derive(Clone, Copy)]
struct Dialect {
    comments: bool,
    strings: bool,
    numbers: NumberStyle,
    idents: IdentStyle,
}

const C_DIALECT: Dialect = Dialect {
    comments: true,
    strings: true,
    numbers: NumberStyle::C,
    idents: IdentStyle::C,
};

const PLAIN_DIALECT: Dialect = Dialect {
    comments: false,
    strings: false,
    numbers: NumberStyle::HexDump,
    idents: IdentStyle::Asm,
};

fn classify_ident(id: &str) -> Tok {
    if KEYWORDS.contains(&id) {
        Tok::Keyword
    } else if TYPES.contains(&id) || id.ends_with("_t") {
        Tok::Type
    } else {
        Tok::Name
    }
}

/// A 2-char (byte) or >=5-char (address) all-hex run that lexes as an
/// identifier is really a hex value in a disasm dump — tag it `Num` so it dims
/// uniformly. 3-4 chars stay a `Name` so mnemonics like `add`/`adc` aren't
/// dimmed.
fn classify_asm_ident(id: &str) -> Tok {
    let len = id.chars().count();
    let hexish = (len == 2 || len >= 5) && id.chars().all(|c| c.is_ascii_hexdigit());
    if hexish {
        Tok::Num
    } else if PLAIN_KEYWORDS.contains(&id) {
        Tok::Keyword
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

fn consume_while(ch: &[char], i: &mut usize, pred: impl Fn(char) -> bool) {
    while *i < ch.len() && pred(ch[*i]) {
        *i += 1;
    }
}

/// Scan one identifier, joining a C++ qualified name (`mtd::run`,
/// `mtd::SessionBase::dispatch`) into a single segment.
///
/// This is what makes a demangled callee navigable: the hotspot pass matches a
/// segment against `ctx.func_names`, which holds BN's `display_name` verbatim,
/// so the name has to survive lexing in one piece. Only a **doubled** colon
/// followed by an identifier joins, which leaves `a ? b : c` and `case 1:`
/// alone.
///
/// Known limit: a qualification that runs into a non-identifier — `operator=`,
/// or a template argument list — joins only up to that point.
fn scan_ident(ch: &[char], i: &mut usize, dialect: Dialect) -> Seg {
    let start = *i;
    consume_while(ch, i, is_ident);

    while *i + 2 < ch.len() && ch[*i] == ':' && ch[*i + 1] == ':' && is_ident_start(ch[*i + 2]) {
        *i += 2;
        consume_while(ch, i, is_ident);
    }

    let text: String = ch[start..*i].iter().collect();
    let kind = match dialect.idents {
        IdentStyle::C => classify_ident(&text),
        IdentStyle::Asm => classify_asm_ident(&text),
    };
    Seg { text, kind }
}

fn scan_number(ch: &[char], i: &mut usize, dialect: Dialect) -> Seg {
    let start = *i;
    match dialect.numbers {
        NumberStyle::C => {
            // `0x9c4`, `16`, and BN's occasional `1000x` all read as one run.
            consume_while(ch, i, |c| is_ident(c) || c == 'x');
        }
        NumberStyle::HexDump => {
            if ch[*i] == '0' && *i + 1 < ch.len() && ch[*i + 1] == 'x' {
                *i += 2;
                consume_while(ch, i, |c| c.is_ascii_hexdigit());
            } else {
                // Addresses and byte columns are bare hex like `0043274c` —
                // don't split at a-f.
                consume_while(ch, i, |c| c.is_ascii_hexdigit());
            }
        }
    }
    let text: String = ch[start..*i].iter().collect();
    let kind = if text.starts_with("0x") {
        Tok::Hex
    } else {
        Tok::Num
    };
    Seg { text, kind }
}

fn scan_punct(ch: &[char], i: &mut usize) -> Seg {
    for op in OPERATORS {
        let len = op.chars().count();
        if *i + len <= ch.len() && ch[*i..*i + len].iter().copied().eq(op.chars()) {
            *i += len;
            return Seg {
                text: (*op).to_string(),
                kind: Tok::Punct,
            };
        }
    }
    let text = ch[*i].to_string();
    *i += 1;
    Seg {
        text,
        kind: Tok::Punct,
    }
}

/// Tokenize one line given the dialect and the incoming block-comment state;
/// returns the segments and whether we are still inside a block comment.
fn tokenize_line(line: &str, dialect: Dialect, mut in_block: bool) -> (Line, bool) {
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
            consume_while(&ch, &mut i, char::is_whitespace);
            segs.push(seg(&ch[start..i], Tok::Plain));
        } else if dialect.comments && c == '/' && i + 1 < n && ch[i + 1] == '/' {
            segs.push(seg(&ch[i..n], Tok::Comment));
            i = n;
        } else if dialect.comments && c == '/' && i + 1 < n && ch[i + 1] == '*' {
            if let Some(end) = find_pair(&ch, i + 2, '*', '/') {
                segs.push(seg(&ch[i..=end + 1], Tok::Comment));
                i = end + 2;
            } else {
                segs.push(seg(&ch[i..n], Tok::Comment));
                return (segs, true);
            }
        } else if dialect.strings && (c == '"' || c == '\'') {
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
            segs.push(scan_number(&ch, &mut i, dialect));
        } else if is_ident_start(c) {
            segs.push(scan_ident(&ch, &mut i, dialect));
        } else {
            segs.push(scan_punct(&ch, &mut i));
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
        let (segs, nb) = tokenize_line(line, C_DIALECT, in_block);
        in_block = nb;
        out.push(segs);
    }
    out
}

/// Tokenize plain output (xrefs, mlil, disasm): addresses / numbers /
/// identifiers. No comment or string state — that output has neither.
pub fn tokenize_plain(text: &str) -> Vec<Line> {
    text.split('\n')
        .map(|line| tokenize_line(line, PLAIN_DIALECT, false).0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &Line) -> Vec<(&str, Tok)> {
        line.iter().map(|s| (s.text.as_str(), s.kind)).collect()
    }

    fn joined(line: &Line) -> String {
        line.iter().map(|s| s.text.clone()).collect()
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
        assert!(segs.iter().any(|(t, k)| *t == "0x40336c" && *k == Tok::Hex));

        let ret = &tokenize_plain("00403404  noreturn")[0];
        assert!(kinds(ret)
            .iter()
            .any(|(t, k)| *t == "noreturn" && *k == Tok::Keyword));

        // `ret` is an aarch64 mnemonic, not the C `return` — must stay a Name.
        let asm = &tokenize_plain("00403404  ret")[0];
        assert!(kinds(asm)
            .iter()
            .any(|(t, k)| *t == "ret" && *k == Tok::Name));
    }

    #[test]
    fn tokenizes_a_line() {
        let lines = tokenize_c("int64_t x0 = msg_alloc();");
        let segs = kinds(&lines[0]);
        assert!(segs.iter().any(|(t, k)| *t == "int64_t" && *k == Tok::Type));
        assert!(segs
            .iter()
            .any(|(t, k)| *t == "msg_alloc" && *k == Tok::Name));
        assert_eq!(joined(&lines[0]), "int64_t x0 = msg_alloc();");
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
            .any(|s| s.text == "0x9c4" && s.kind == Tok::Hex));
        assert!(lines[0]
            .iter()
            .any(|s| s.text == "16" && s.kind == Tok::Num));
    }

    #[test]
    fn plain_addresses() {
        let lines = tokenize_plain("  0x402620  build_and_send  (1 site: 0x402658)");
        assert!(lines[0]
            .iter()
            .any(|s| s.text == "0x402620" && s.kind == Tok::Hex));
        assert!(lines[0]
            .iter()
            .any(|s| s.text == "build_and_send" && s.kind == Tok::Name));
    }

    // ---- qualified C++ names -------------------------------------------

    #[test]
    fn joins_a_qualified_call_into_one_name() {
        // The whole point: `func_names` holds BN's demangled `display_name`, so
        // the token has to arrive intact for the hotspot pass to match it.
        let lines = tokenize_c("    return mtd::run(argc, argv);");
        let segs = kinds(&lines[0]);
        assert!(segs
            .iter()
            .any(|(t, k)| *t == "mtd::run" && *k == Tok::Name));
        assert!(!segs.iter().any(|(t, _)| *t == "mtd"));
        assert_eq!(joined(&lines[0]), "    return mtd::run(argc, argv);");
    }

    #[test]
    fn joins_deeply_qualified_names_in_both_dialects() {
        let c = &tokenize_c("mtd::SessionBase::dispatch(x);")[0];
        assert!(kinds(c)
            .iter()
            .any(|(t, k)| *t == "mtd::SessionBase::dispatch" && *k == Tok::Name));

        // MLIL/disasm/xrefs get the same treatment from the same scanner.
        let plain = &tokenize_plain("  0x40761c  mtd::SessionBase::dispatch  (2 sites)")[0];
        assert!(kinds(plain)
            .iter()
            .any(|(t, k)| *t == "mtd::SessionBase::dispatch" && *k == Tok::Name));
    }

    #[test]
    fn a_single_colon_never_joins() {
        // Ternaries and case labels must keep their colon as punctuation, or
        // `cond ? a : b` would lex as one bogus identifier.
        let ternary = &tokenize_c("x = cond ? a : b;")[0];
        let segs = kinds(ternary);
        assert!(segs.iter().any(|(t, k)| *t == "a" && *k == Tok::Name));
        assert!(segs.iter().any(|(t, k)| *t == "b" && *k == Tok::Name));
        assert!(segs.iter().any(|(t, k)| *t == ":" && *k == Tok::Punct));
        assert_eq!(joined(ternary), "x = cond ? a : b;");

        let label = &tokenize_c("  case 1:")[0];
        assert!(kinds(label)
            .iter()
            .any(|(t, k)| *t == ":" && *k == Tok::Punct));
    }

    #[test]
    fn a_trailing_colon_pair_does_not_run_off_the_end() {
        // `::` with nothing after it must not be swallowed into the name.
        for line in ["foo::", "foo::;", "foo::1"] {
            let segs = &tokenize_c(line)[0];
            assert!(kinds(segs)
                .iter()
                .any(|(t, k)| *t == "foo" && *k == Tok::Name));
            assert_eq!(joined(segs), line);
        }
    }

    // ---- structural invariants ------------------------------------------

    #[test]
    fn multi_char_operators_are_single_punct_segments() {
        let segs = &tokenize_c("a->b == c && d << 2;")[0];
        let k = kinds(segs);
        for op in ["->", "==", "&&", "<<"] {
            assert!(
                k.iter().any(|(t, kind)| *t == op && *kind == Tok::Punct),
                "expected a single Punct segment for {op}"
            );
        }
    }

    #[test]
    fn every_line_round_trips_through_both_dialects() {
        // Hotspot columns are accumulated segment widths, so a lexer that drops
        // or duplicates a character silently misplaces every hotspot after it.
        let samples = [
            "int64_t x0 = msg_alloc();",
            "    return mtd::run(argc, argv);",
            "x = cond ? a : b;  // trailing",
            "if (a->len >= 0x40 && b != 0) { return -1; }",
            "  0x402620  build_and_send  (1 site: 0x402658)",
            "0040338c  goto 16 @ 0x40336c",
            "char* s = \"quoted :: not a name\";",
            "",
            "   ",
        ];
        for text in samples {
            assert_eq!(joined(&tokenize_c(text)[0]), text, "tokenize_c: {text:?}");
            assert_eq!(
                joined(&tokenize_plain(text)[0]),
                text,
                "tokenize_plain: {text:?}"
            );
        }
    }

    #[test]
    fn a_qualified_name_inside_a_string_stays_a_string() {
        let segs = &tokenize_c("log(\"mtd::run failed\");")[0];
        assert!(kinds(segs)
            .iter()
            .any(|(t, k)| t.contains("mtd::run") && *k == Tok::Str));
        assert!(!kinds(segs)
            .iter()
            .any(|(t, k)| *t == "mtd::run" && *k == Tok::Name));
    }
}
