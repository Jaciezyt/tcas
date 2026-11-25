//! Static parsing of a Bash command line (harness.md section 5.1). Nothing is executed
//! and no variable is expanded: a word that depends on the environment is
//! [`Word::Dynamic`], and callers decide what to do with it.

/// One shell word after quote removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Word {
    /// The exact text the shell would pass on.
    Lit(String),
    /// Depends on expansion (`$x`, `${x}`, `$(…)`, backticks, unquoted globs `*?[`).
    /// Holds the raw source text.
    Dynamic(String),
}

impl Word {
    /// The literal text, or `None` for a dynamic word.
    pub fn lit(&self) -> Option<&str> {
        match self {
            Word::Lit(s) => Some(s),
            Word::Dynamic(_) => None,
        }
    }

    /// The literal text or the raw source of a dynamic word.
    pub fn text(&self) -> &str {
        match self {
            Word::Lit(s) | Word::Dynamic(s) => s,
        }
    }

    pub fn is_dynamic(&self) -> bool {
        matches!(self, Word::Dynamic(_))
    }
}

/// One simple command with the context it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simple {
    /// Directory it runs in: the start cwd adjusted by `cd`/`pushd`/`popd` seen earlier.
    pub dir: String,
    /// False after `cd $X`, `cd -`, or a `cd` to something computed.
    pub dir_known: bool,
    /// `VAR=x` prefixes, `env VAR=x`, and `export VAR=x` seen earlier in the same shell.
    pub env: Vec<(String, Word)>,
    /// Program and arguments after stripping wrappers.
    pub words: Vec<Word>,
    /// Targets of `>`, `>>`, `2>`, `&>`, `>|`, `<`, … (not heredoc delimiters, here-strings
    /// or fd duplications such as `2>&1`).
    pub redirects: Vec<Word>,
    /// Wrappers stripped, outermost first, e.g. `["env", "sudo"]`, `["bash -c"]`, `["xargs"]`.
    pub wrapped_by: Vec<String>,
}

/// Result of [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Every simple command found, in execution order (commands inside `$( )`, `bash -c`
    /// and `eval` text are included, before or in place of the command that holds them).
    pub commands: Vec<Simple>,
    /// The whole input is exactly one simple command: no `;` `&&` `||` `|` `&` newline,
    /// no redirects, no `$( )` or backticks, no subshell or group.
    pub single: bool,
}

/// The marker word appended to commands whose arguments come from `xargs` or `find -exec`.
pub const FROM_XARGS: &str = "<from xargs>";

const MAX_DEPTH: usize = 24;

/// Parse `command` as run from `cwd`. `home` expands a leading `~`.
pub fn parse(command: &str, cwd: &str, home: Option<&str>) -> Parsed {
    let lexed = tokenize(command, home);
    let mut run = Run {
        home,
        out: Vec::new(),
        case_depth: 0,
    };
    let mut ctx = Ctx {
        dir: cwd.to_string(),
        known: true,
        stack: Vec::new(),
        exports: Vec::new(),
    };
    let mut pos = 0;
    run.list(&lexed.toks, &mut pos, &mut ctx, Term::Top, 0);

    let mut toks: &[Tok] = &lexed.toks;
    while let Some((Tok::Op(Op::Nl), rest)) = toks.split_last() {
        toks = rest;
    }
    while let Some((Tok::Op(Op::Nl), rest)) = toks.split_first() {
        toks = rest;
    }
    let first_reserved = match toks.first() {
        Some(Tok::Word(w)) => w.word.lit().is_some_and(|s| !w.quoted && is_reserved(s)),
        _ => false,
    };
    let single = !lexed.subst
        && !first_reserved
        && !toks.is_empty()
        && toks.iter().all(|t| matches!(t, Tok::Word(_)));
    Parsed {
        commands: run.out,
        single,
    }
}

/// Last path component of `s`.
pub fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Lexically normalize a path: drop `.` and empty parts, resolve `..`. Absolute stays
/// absolute (`/..` is `/`); an empty relative result is `.`.
pub fn normalize(path: &str) -> String {
    let abs = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            x => parts.push(x),
        }
    }
    if abs {
        format!("/{}", parts.join("/"))
    } else if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// `rel` taken from `dir`, normalized. An absolute `rel` replaces `dir`.
pub fn join_dir(dir: &str, rel: &str) -> String {
    if rel.starts_with('/') {
        normalize(rel)
    } else {
        normalize(&format!("{dir}/{rel}"))
    }
}

// ---------------------------------------------------------------------------
// Lexer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Semi,
    And,
    Or,
    Pipe,
    Amp,
    Nl,
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct RawWord {
    word: Word,
    lit: String,
    raw: String,
    quoted: bool,
    tilde: bool,
    /// Texts of `$( )`, backtick and `<( )` substitutions inside the word.
    subs: Vec<String>,
    /// Length of the variable name when the word is `NAME=…`.
    assign: Option<usize>,
}

#[derive(Debug, Clone)]
enum Tok {
    Word(RawWord),
    Op(Op),
    Redir(String),
}

struct Lexed {
    toks: Vec<Tok>,
    /// Any `$( )`, backtick or process substitution.
    subst: bool,
}

fn tokenize(src: &str, home: Option<&str>) -> Lexed {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let mut i = 0;
    let mut toks = Vec::new();
    let mut subst = false;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut expect_heredoc: Option<bool> = None;
    while i < n {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        match ch {
            ' ' | '\t' | '\r' => i += 1,
            '\n' => {
                toks.push(Tok::Op(Op::Nl));
                i += 1;
                for (delim, strip) in heredocs.drain(..) {
                    i = skip_heredoc(&c, i, &delim, strip);
                }
            }
            '#' => {
                while i < n && c[i] != '\n' {
                    i += 1;
                }
            }
            ';' => {
                i += 1;
                if i < n && c[i] == ';' {
                    i += 1;
                    if i < n && c[i] == '&' {
                        i += 1;
                    }
                } else if i < n && c[i] == '&' {
                    i += 1;
                }
                toks.push(Tok::Op(Op::Semi));
            }
            '&' => match next {
                Some('&') => {
                    toks.push(Tok::Op(Op::And));
                    i += 2;
                }
                Some('>') => {
                    i += 2;
                    let mut op = String::from("&>");
                    if i < n && c[i] == '>' {
                        op.push('>');
                        i += 1;
                    }
                    toks.push(Tok::Redir(op));
                }
                _ => {
                    toks.push(Tok::Op(Op::Amp));
                    i += 1;
                }
            },
            '|' => match next {
                Some('|') => {
                    toks.push(Tok::Op(Op::Or));
                    i += 2;
                }
                Some('&') => {
                    toks.push(Tok::Op(Op::Pipe));
                    i += 2;
                }
                _ => {
                    toks.push(Tok::Op(Op::Pipe));
                    i += 1;
                }
            },
            '(' => {
                toks.push(Tok::Op(Op::LParen));
                i += 1;
            }
            ')' => {
                toks.push(Tok::Op(Op::RParen));
                i += 1;
            }
            '>' if next != Some('(') => {
                i += 1;
                let mut op = String::from(">");
                if let Some(x) = c.get(i).copied()
                    && matches!(x, '>' | '|' | '&')
                {
                    op.push(x);
                    i += 1;
                }
                toks.push(Tok::Redir(op));
            }
            '<' if next != Some('(') => {
                i += 1;
                let mut op = String::from("<");
                match c.get(i).copied() {
                    Some('<') => {
                        i += 1;
                        op.push('<');
                        match c.get(i).copied() {
                            Some('<') => {
                                i += 1;
                                op.push('<');
                            }
                            Some('-') => {
                                i += 1;
                                op.push('-');
                                expect_heredoc = Some(true);
                            }
                            _ => expect_heredoc = Some(false),
                        }
                    }
                    Some(x @ ('&' | '>')) => {
                        i += 1;
                        op.push(x);
                    }
                    _ => {}
                }
                toks.push(Tok::Redir(op));
            }
            _ => {
                let start = i;
                let w = scan_word(&c, &mut i, home, &mut subst);
                if i == start {
                    i += 1;
                }
                // `2>file`: a bare number before a redirect is a file descriptor.
                if matches!(c.get(i), Some('<' | '>'))
                    && c.get(i + 1) != Some(&'(')
                    && !w.quoted
                    && !w.word.is_dynamic()
                    && !w.lit.is_empty()
                    && w.lit.chars().all(|x| x.is_ascii_digit())
                {
                    continue;
                }
                if let Some(strip) = expect_heredoc.take() {
                    heredocs.push((w.lit.clone(), strip));
                }
                toks.push(Tok::Word(w));
            }
        }
    }
    Lexed { toks, subst }
}

/// Consume heredoc body lines from `i` up to and including the delimiter line.
fn skip_heredoc(c: &[char], mut i: usize, delim: &str, strip: bool) -> usize {
    let n = c.len();
    while i < n {
        let mut e = i;
        while e < n && c[e] != '\n' {
            e += 1;
        }
        let line: String = c[i..e].iter().collect();
        let line = line.trim_end_matches('\r');
        let line = if strip {
            line.trim_start_matches('\t')
        } else {
            line
        };
        i = (e + 1).min(n);
        if line == delim {
            break;
        }
    }
    i
}

/// Index of the `)` matching the `(` at `open`, or `c.len()` when unterminated.
fn skip_paren(c: &[char], open: usize) -> usize {
    let n = c.len();
    let mut depth = 0usize;
    let mut j = open;
    while j < n {
        match c[j] {
            '\\' => j += 1,
            '\'' => {
                j += 1;
                while j < n && c[j] != '\'' {
                    j += 1;
                }
            }
            '"' => {
                j += 1;
                while j < n && c[j] != '"' {
                    if c[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j += 1;
    }
    n
}

fn slice(c: &[char], a: usize, b: usize) -> String {
    let b = b.min(c.len());
    if a >= b {
        String::new()
    } else {
        c[a..b].iter().collect()
    }
}

fn assign_prefix_len(raw: &[char]) -> Option<usize> {
    let first = *raw.first()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut k = 1;
    while k < raw.len() && (raw[k].is_ascii_alphanumeric() || raw[k] == '_') {
        k += 1;
    }
    (raw.get(k) == Some(&'=')).then_some(k)
}

#[derive(Default)]
struct WordBuf {
    lit: String,
    dynamic: bool,
    quoted: bool,
    subs: Vec<String>,
}

fn scan_word(c: &[char], i: &mut usize, home: Option<&str>, subst: &mut bool) -> RawWord {
    let n = c.len();
    let start = *i;
    let mut b = WordBuf::default();
    let mut tilde = false;
    let mut in_brace = false;
    while *i < n {
        let ch = c[*i];
        match ch {
            ' ' | '\t' | '\n' | '\r' | ';' | '&' | '|' | ')' => break,
            '(' => {
                if assign_prefix_len(&c[start..*i]).is_some_and(|k| k + 1 == *i - start) {
                    // `A=(1 2 3)`: an array assignment is one word.
                    let close = skip_paren(c, *i);
                    b.dynamic = true;
                    *i = (close + 1).min(n);
                } else {
                    break;
                }
            }
            '<' | '>' => {
                if c.get(*i + 1) == Some(&'(') {
                    let close = skip_paren(c, *i + 1);
                    b.subs.push(slice(c, *i + 2, close));
                    b.dynamic = true;
                    *subst = true;
                    *i = (close + 1).min(n);
                } else {
                    break;
                }
            }
            '\\' => {
                b.quoted = true;
                match c.get(*i + 1) {
                    Some('\n') => *i += 2,
                    Some(&x) => {
                        b.lit.push(x);
                        *i += 2;
                    }
                    None => {
                        b.lit.push('\\');
                        *i += 1;
                    }
                }
            }
            '\'' => {
                b.quoted = true;
                *i += 1;
                while *i < n && c[*i] != '\'' {
                    b.lit.push(c[*i]);
                    *i += 1;
                }
                *i = (*i + 1).min(n);
            }
            '"' => {
                b.quoted = true;
                *i += 1;
                scan_double(c, i, &mut b, subst);
            }
            '`' => backtick(c, i, &mut b, subst),
            '$' => dollar(c, i, &mut b, subst, false),
            '*' | '?' | '[' => {
                b.dynamic = true;
                b.lit.push(ch);
                *i += 1;
            }
            '{' => {
                let next_ws = c.get(*i + 1).is_none_or(|x| x.is_whitespace());
                if !(b.lit.is_empty() && !b.quoted && next_ws) {
                    in_brace = true;
                }
                b.lit.push(ch);
                *i += 1;
            }
            '}' => {
                in_brace = false;
                b.lit.push(ch);
                *i += 1;
            }
            ',' => {
                if in_brace {
                    b.dynamic = true;
                }
                b.lit.push(ch);
                *i += 1;
            }
            '.' => {
                if in_brace && c.get(*i + 1) == Some(&'.') {
                    b.dynamic = true;
                }
                b.lit.push(ch);
                *i += 1;
            }
            '~' => {
                if *i == start {
                    tilde = true;
                }
                b.lit.push(ch);
                *i += 1;
            }
            _ => {
                b.lit.push(ch);
                *i += 1;
            }
        }
    }
    let raw_chars = &c[start..(*i).min(n)];
    let raw: String = raw_chars.iter().collect();
    let assign = assign_prefix_len(raw_chars);
    let mut lit = b.lit;
    if tilde
        && !b.dynamic
        && let Some(h) = home
        && (lit == "~" || lit.starts_with("~/"))
    {
        lit = format!("{}{}", h.trim_end_matches('/'), &lit[1..]);
        if lit.is_empty() {
            lit.push('/');
        }
    }
    let word = if b.dynamic {
        Word::Dynamic(raw.clone())
    } else {
        Word::Lit(lit.clone())
    };
    RawWord {
        word,
        lit,
        raw,
        quoted: b.quoted,
        tilde,
        subs: b.subs,
        assign,
    }
}

/// Inside `"…"`; `*i` is just past the opening quote.
fn scan_double(c: &[char], i: &mut usize, b: &mut WordBuf, subst: &mut bool) {
    let n = c.len();
    while *i < n {
        match c[*i] {
            '"' => {
                *i += 1;
                return;
            }
            '\\' => match c.get(*i + 1) {
                Some('\n') => *i += 2,
                Some(&x) if matches!(x, '$' | '`' | '"' | '\\') => {
                    b.lit.push(x);
                    *i += 2;
                }
                _ => {
                    b.lit.push('\\');
                    *i += 1;
                }
            },
            '$' => dollar(c, i, b, subst, true),
            '`' => backtick(c, i, b, subst),
            x => {
                b.lit.push(x);
                *i += 1;
            }
        }
    }
}

fn backtick(c: &[char], i: &mut usize, b: &mut WordBuf, subst: &mut bool) {
    let n = c.len();
    let mut j = *i + 1;
    let mut inner = String::new();
    while j < n && c[j] != '`' {
        if c[j] == '\\'
            && let Some(&x) = c.get(j + 1)
            && matches!(x, '`' | '\\' | '$')
        {
            inner.push(x);
            j += 2;
            continue;
        }
        inner.push(c[j]);
        j += 1;
    }
    *i = (j + 1).min(n);
    b.subs.push(inner);
    b.dynamic = true;
    *subst = true;
}

/// At a `$`.
fn dollar(c: &[char], i: &mut usize, b: &mut WordBuf, subst: &mut bool, in_dq: bool) {
    let n = c.len();
    match c.get(*i + 1).copied() {
        Some('(') => {
            let close = skip_paren(c, *i + 1);
            if c.get(*i + 2) != Some(&'(') {
                b.subs.push(slice(c, *i + 2, close));
                *subst = true;
            }
            b.dynamic = true;
            *i = (close + 1).min(n);
        }
        Some('{') => {
            let mut depth = 0usize;
            let mut j = *i + 1;
            while j < n {
                match c[j] {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            b.dynamic = true;
            *i = (j + 1).min(n);
        }
        Some('\'') if !in_dq => {
            *i += 2;
            ansi_c(c, i, &mut b.lit);
            b.quoted = true;
        }
        Some('"') if !in_dq => {
            // `$"…"` is a double-quoted string; the main loop sees the quote next.
            *i += 1;
        }
        Some(x) if x.is_alphabetic() || x == '_' => {
            *i += 1;
            while *i < n && (c[*i].is_alphanumeric() || c[*i] == '_') {
                *i += 1;
            }
            b.dynamic = true;
        }
        Some(x) if x.is_ascii_digit() || "@*#?!$-".contains(x) => {
            *i += 2;
            b.dynamic = true;
        }
        _ => {
            b.lit.push('$');
            *i += 1;
        }
    }
}

/// Body of `$'…'`; `*i` is just past the opening quote.
fn ansi_c(c: &[char], i: &mut usize, out: &mut String) {
    let n = c.len();
    while *i < n {
        let ch = c[*i];
        *i += 1;
        match ch {
            '\'' => return,
            '\\' if *i < n => {
                let e = c[*i];
                *i += 1;
                match e {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'a' => out.push('\u{7}'),
                    'b' => out.push('\u{8}'),
                    'e' | 'E' => out.push('\u{1b}'),
                    'f' => out.push('\u{c}'),
                    'v' => out.push('\u{b}'),
                    '\\' | '\'' | '"' | '?' => out.push(e),
                    'x' | 'u' | 'U' | '0'..='7' => {
                        let (radix, max, mut v, mut taken) = match e {
                            'x' => (16, 2, 0u32, 0),
                            'u' => (16, 4, 0, 0),
                            'U' => (16, 8, 0, 0),
                            d => (8, 2, d.to_digit(8).unwrap_or(0), 1),
                        };
                        let max = if radix == 8 { 3 } else { max };
                        while taken < max
                            && *i < n
                            && let Some(d) = c[*i].to_digit(radix)
                        {
                            v = v.saturating_mul(radix).saturating_add(d);
                            *i += 1;
                            taken += 1;
                        }
                        out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                    }
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            x => out.push(x),
        }
    }
}

// ---------------------------------------------------------------------------
// Parser

#[derive(Clone)]
struct Ctx {
    dir: String,
    known: bool,
    stack: Vec<(String, bool)>,
    exports: Vec<(String, Word)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Term {
    Top,
    Paren,
    Brace,
}

struct Run<'a> {
    home: Option<&'a str>,
    out: Vec<Simple>,
    case_depth: usize,
}

const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "while", "until", "do", "done", "!", "{", "}", "for",
    "select", "case", "esac", "function", "coproc",
];

fn is_reserved(s: &str) -> bool {
    RESERVED.contains(&s)
}

fn set_env(env: &mut Vec<(String, Word)>, k: String, v: Word) {
    env.retain(|(n, _)| *n != k);
    env.push((k, v));
}

fn assign_of(w: &RawWord, n: usize) -> (String, Word) {
    let name = w.raw.chars().take(n).collect::<String>();
    let value = match &w.word {
        Word::Dynamic(_) => Word::Dynamic(w.raw.chars().skip(n + 1).collect()),
        Word::Lit(s) => Word::Lit(s.chars().skip(n + 1).collect()),
    };
    (name, value)
}

/// Does a `case` pattern (`a|b)`) start at `pos`? Returns the token count.
fn case_pattern_len(toks: &[Tok], pos: usize) -> Option<usize> {
    let mut j = pos;
    loop {
        if !matches!(toks.get(j), Some(Tok::Word(_))) {
            return None;
        }
        j += 1;
        match toks.get(j) {
            Some(Tok::Op(Op::Pipe)) => j += 1,
            Some(Tok::Op(Op::RParen)) => return Some(j + 1 - pos),
            _ => return None,
        }
    }
}

/// Skip option words starting at `i`: short clusters whose letters are in `short_arg`
/// take a value (attached or the next word), `--long` takes one when listed in `long_arg`.
/// Returns the index of the first word that is not an option.
fn skip_options(words: &[RawWord], mut i: usize, short_arg: &str, long_arg: &[&str]) -> usize {
    while i < words.len() {
        if words[i].assign.is_some() {
            break;
        }
        let Some(s) = words[i].word.lit() else { break };
        if s == "--" {
            return i + 1;
        }
        if let Some(long) = s.strip_prefix("--") {
            i += if !long.contains('=') && long_arg.contains(&long) {
                2
            } else {
                1
            };
        } else if s.len() > 1 && s.starts_with('-') {
            let cl: Vec<char> = s[1..].chars().collect();
            let mut skip = 1;
            for (k, ch) in cl.iter().enumerate() {
                if short_arg.contains(*ch) {
                    if k + 1 == cl.len() {
                        skip = 2;
                    }
                    break;
                }
            }
            i += skip;
        } else {
            break;
        }
    }
    i.min(words.len())
}

/// Index of the `-c` option of a shell invocation.
fn find_dash_c(words: &[RawWord]) -> Option<usize> {
    let mut i = 1;
    while i < words.len() {
        let s = words[i].word.lit()?;
        if s == "--" || !(s.starts_with('-') || s.starts_with('+')) {
            return None;
        }
        if s.starts_with('-')
            && !s.starts_with("--")
            && s.len() > 1
            && s[1..].chars().all(|c| c.is_ascii_alphabetic())
            && s[1..].contains('c')
        {
            return Some(i);
        }
        if matches!(s, "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file") {
            i += 1;
        }
        i += 1;
    }
    None
}

/// A literal word, as if written unquoted.
fn lit_word(s: &str) -> RawWord {
    RawWord {
        word: Word::Lit(s.to_string()),
        lit: s.to_string(),
        raw: s.to_string(),
        quoted: false,
        tilde: false,
        subs: Vec::new(),
        assign: None,
    }
}

/// The value of a wrapper's option among its option words `opts`: `-X v`,
/// `-Xv`, a cluster ending in `X`, `--long v` or `--long=v`. The last one wins.
/// `short_arg` lists the short letters that take a value.
fn option_value(opts: &[RawWord], short_arg: &str, short: char, long: &str) -> Option<RawWord> {
    let mut found = None;
    let mut k = 0;
    while k < opts.len() {
        let Some(s) = opts[k].word.lit() else {
            k += 1;
            continue;
        };
        if let Some(l) = s.strip_prefix("--") {
            if l == long {
                found = opts.get(k + 1).cloned();
                k += 1;
            } else if let Some(v) = l.strip_prefix(long).and_then(|r| r.strip_prefix('=')) {
                found = Some(lit_word(v));
            }
        } else if s.len() > 1 && s.starts_with('-') {
            for (n, ch) in s[1..].char_indices() {
                if ch == short {
                    let rest = &s[1 + n + ch.len_utf8()..];
                    if rest.is_empty() {
                        found = opts.get(k + 1).cloned();
                        k += 1;
                    } else {
                        found = Some(lit_word(rest));
                    }
                    break;
                }
                if short_arg.contains(ch) {
                    if 1 + n + ch.len_utf8() == s.len() {
                        k += 1;
                    }
                    break;
                }
            }
        }
        k += 1;
    }
    found
}

/// The text inside a word written as one double-quoted string, with the
/// backslash escapes of double quotes removed: what the word stands for
/// before its `$…` are expanded. `None` for any other form.
fn double_quoted_text(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next()? {
                n @ ('"' | '\\' | '$' | '`') => out.push(n),
                '\n' => {}
                n => {
                    out.push('\\');
                    out.push(n);
                }
            },
            '"' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

impl Run<'_> {
    fn simple(
        &self,
        ctx: &Ctx,
        words: Vec<Word>,
        env: &[(String, Word)],
        redirects: Vec<Word>,
        wrapped_by: Vec<String>,
    ) -> Simple {
        let mut e = ctx.exports.clone();
        for (k, v) in env {
            set_env(&mut e, k.clone(), v.clone());
        }
        Simple {
            dir: ctx.dir.clone(),
            dir_known: ctx.known,
            env: e,
            words,
            redirects,
            wrapped_by,
        }
    }

    fn run_text(&mut self, text: &str, ctx: &mut Ctx, depth: usize) {
        if depth >= MAX_DEPTH {
            let s = self.simple(
                ctx,
                vec![Word::Dynamic(text.to_string())],
                &[],
                vec![],
                vec![],
            );
            self.out.push(s);
            return;
        }
        let lexed = tokenize(text, self.home);
        let mut pos = 0;
        self.list(&lexed.toks, &mut pos, ctx, Term::Top, depth + 1);
    }

    /// Runs words as the text of a script (`eval …`, `sh -c …`, `env -S …`),
    /// with `env` exported into it. A computed word stays computed in the
    /// script: `eval "$x"` is one command whose program is not known, and
    /// `sh -c "cd api && git $SUB"` is `cd api` then `git` with a computed
    /// subcommand.
    fn run_words_as_text<'w>(
        &mut self,
        words: impl Iterator<Item = &'w RawWord>,
        env: &[(String, Word)],
        ctx: &mut Ctx,
        depth: usize,
    ) {
        for (k, v) in env {
            set_env(&mut ctx.exports, k.clone(), v.clone());
        }
        let text = words
            .map(|w| match &w.word {
                Word::Lit(s) => s.clone(),
                Word::Dynamic(raw) => double_quoted_text(raw).unwrap_or_else(|| raw.clone()),
            })
            .collect::<Vec<_>>()
            .join(" ");
        self.run_text(&text, ctx, depth);
    }

    fn emit_subs(&mut self, subs: &[String], ctx: &Ctx, depth: usize) {
        for s in subs {
            let mut c2 = ctx.clone();
            c2.stack.clear();
            self.run_text(s, &mut c2, depth);
        }
    }

    fn list(&mut self, toks: &[Tok], pos: &mut usize, ctx: &mut Ctx, term: Term, depth: usize) {
        let mut prev_pipe = false;
        while *pos < toks.len() {
            let kw: Option<String> = match &toks[*pos] {
                Tok::Op(op) => {
                    let op = *op;
                    *pos += 1;
                    match op {
                        Op::Pipe => prev_pipe = true,
                        Op::RParen => {
                            if term == Term::Paren {
                                return;
                            }
                            prev_pipe = false;
                        }
                        Op::LParen => {
                            if depth < MAX_DEPTH {
                                let mut c2 = ctx.clone();
                                self.list(toks, pos, &mut c2, Term::Paren, depth + 1);
                            }
                            prev_pipe = false;
                        }
                        _ => prev_pipe = false,
                    }
                    continue;
                }
                Tok::Word(w) => match &w.word {
                    Word::Lit(s) if !w.quoted => Some(s.clone()),
                    _ => None,
                },
                Tok::Redir(_) => None,
            };
            if let Some(k) = kw {
                match k.as_str() {
                    "}" => {
                        *pos += 1;
                        if term == Term::Brace {
                            return;
                        }
                        continue;
                    }
                    "{" => {
                        *pos += 1;
                        if depth < MAX_DEPTH {
                            self.list(toks, pos, ctx, Term::Brace, depth + 1);
                        }
                        prev_pipe = false;
                        continue;
                    }
                    "if" | "then" | "else" | "elif" | "fi" | "while" | "until" | "do" | "done"
                    | "!" | "coproc" => {
                        *pos += 1;
                        continue;
                    }
                    "esac" => {
                        self.case_depth = self.case_depth.saturating_sub(1);
                        *pos += 1;
                        continue;
                    }
                    "for" | "select" | "case" => {
                        let is_case = k == "case";
                        *pos += 1;
                        while let Some(t) = toks.get(*pos) {
                            match t {
                                Tok::Op(Op::Semi | Op::Nl) => break,
                                Tok::Word(w) => {
                                    if !is_case && w.word.lit() == Some("do") && !w.quoted {
                                        break;
                                    }
                                    let subs = w.subs.clone();
                                    *pos += 1;
                                    self.emit_subs(&subs, ctx, depth);
                                    if is_case && w.word.lit() == Some("in") && !w.quoted {
                                        break;
                                    }
                                }
                                _ => *pos += 1,
                            }
                        }
                        if is_case {
                            self.case_depth += 1;
                        }
                        continue;
                    }
                    "function" => {
                        *pos += 1;
                        if matches!(toks.get(*pos), Some(Tok::Word(_))) {
                            *pos += 1;
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            if self.case_depth > 0
                && let Some(len) = case_pattern_len(toks, *pos)
            {
                *pos += len;
                continue;
            }

            // A simple command: words and redirects up to the next operator.
            let mut words: Vec<RawWord> = Vec::new();
            let mut redirs: Vec<Word> = Vec::new();
            let mut subs: Vec<String> = Vec::new();
            while let Some(t) = toks.get(*pos) {
                match t {
                    Tok::Word(w) => {
                        subs.extend(w.subs.iter().cloned());
                        words.push(w.clone());
                        *pos += 1;
                    }
                    Tok::Redir(op) => {
                        *pos += 1;
                        if let Some(Tok::Word(w)) = toks.get(*pos) {
                            *pos += 1;
                            subs.extend(w.subs.iter().cloned());
                            let fd_dup = op.ends_with('&')
                                && w.word.lit().is_some_and(|s| {
                                    s == "-" || s.chars().all(|c| c.is_ascii_digit())
                                });
                            if !matches!(op.as_str(), "<<" | "<<-" | "<<<") && !fd_dup {
                                redirs.push(w.word.clone());
                            }
                        }
                    }
                    Tok::Op(_) => break,
                }
            }
            self.emit_subs(&subs, ctx, depth);
            let iso = prev_pipe || matches!(toks.get(*pos), Some(Tok::Op(Op::Pipe | Op::Amp)));
            prev_pipe = false;
            if iso {
                let mut c2 = ctx.clone();
                self.interpret(words, redirs, Vec::new(), &mut c2, depth);
            } else {
                self.interpret(words, redirs, Vec::new(), ctx, depth);
            }
        }
    }

    fn interpret(
        &mut self,
        mut words: Vec<RawWord>,
        redirs: Vec<Word>,
        mut env: Vec<(String, Word)>,
        ctx: &mut Ctx,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            let ws = words.iter().map(|w| w.word.clone()).collect();
            let s = self.simple(ctx, ws, &env, redirs, Vec::new());
            self.out.push(s);
            return;
        }
        // Strip wrappers and leading assignments. `env -C` and `sudo -D` run the
        // wrapped command elsewhere: it gets its own context.
        let mut wrapped: Vec<String> = Vec::new();
        let mut moved: Option<Ctx> = None;
        let mut i = 0;
        while let Some(w) = words.get(i) {
            if let Some(n) = w.assign {
                let (k, v) = assign_of(w, n);
                set_env(&mut env, k, v);
                i += 1;
                continue;
            }
            let Some(head) = w.word.lit() else { break };
            let base = basename(head);
            let (short_arg, long_arg, extra): (&str, &[&str], usize) = match base {
                "env" => (
                    "uCSP",
                    &[
                        "unset",
                        "chdir",
                        "split-string",
                        "argv0",
                        "block-signal",
                        "default-signal",
                        "ignore-signal",
                    ],
                    0,
                ),
                "command" => {
                    let query = words
                        .get(i + 1)
                        .and_then(|x| x.word.lit())
                        .is_some_and(|s| {
                            s.starts_with('-') && !s.starts_with("--") && s.contains(['v', 'V'])
                        });
                    if query {
                        break;
                    }
                    ("", &[], 0)
                }
                "builtin" | "nohup" | "setsid" => ("", &[], 0),
                "exec" => ("a", &[], 0),
                "time" => ("fo", &[], 0),
                "nice" => ("n", &["adjustment"], 0),
                "timeout" => ("sk", &["signal", "kill-after"], 1),
                "sudo" => (
                    "ughpCDRrtTU",
                    &[
                        "user",
                        "group",
                        "host",
                        "prompt",
                        "close-from",
                        "chdir",
                        "chroot",
                        "role",
                        "type",
                        "command-timeout",
                        "other-user",
                    ],
                    0,
                ),
                _ => break,
            };
            let j = skip_options(&words, i + 1, short_arg, long_arg) + extra;
            let opts = &words[(i + 1).min(words.len())..j.min(words.len())];
            if let Some(chdir) = match base {
                "env" => option_value(opts, short_arg, 'C', "chdir"),
                "sudo" => option_value(opts, short_arg, 'D', "chdir"),
                _ => None,
            } {
                let mut c = moved.take().unwrap_or_else(|| ctx.clone());
                self.cd(&mut c, std::slice::from_ref(&chdir));
                moved = Some(c);
            }
            if base == "env"
                && let Some(script) = option_value(opts, short_arg, 'S', "split-string")
            {
                // `env -S "git stash"`: the string is split into the command.
                let mut c2 = moved.take().unwrap_or_else(|| ctx.clone());
                let mut prefix = wrapped.clone();
                prefix.push("env".to_string());
                let start = self.out.len();
                let rest: Vec<RawWord> = words[j.min(words.len())..].to_vec();
                self.run_words_as_text(std::iter::once(&script).chain(rest.iter()), &env, &mut c2, depth);
                self.prefix_wrappers(start, &prefix, None);
                let plain: Vec<Word> = words.iter().map(|w| w.word.clone()).collect();
                self.outer_redirects(ctx, plain, &env, redirs, wrapped);
                return;
            }
            wrapped.push(base.to_string());
            i = j.min(words.len());
        }
        let ctx: &mut Ctx = match moved.as_mut() {
            Some(c) => c,
            None => ctx,
        };
        let words: Vec<RawWord> = words.split_off(i.min(words.len()));
        let plain: Vec<Word> = words.iter().map(|w| w.word.clone()).collect();
        let Some(head) = words.first().and_then(|w| w.word.lit()).map(basename) else {
            if !plain.is_empty() || !env.is_empty() || !redirs.is_empty() {
                let s = self.simple(ctx, plain, &env, redirs, wrapped);
                self.out.push(s);
            }
            return;
        };

        // Commands whose argument is more commands.
        if matches!(head, "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash")
            && let Some(k) = find_dash_c(&words)
            // `bash -c -- 'script'`: `--` ends the options before the script.
            && let Some(arg) = words.get(if words.get(k + 1).and_then(|w| w.word.lit()) == Some("--") { k + 2 } else { k + 1 })
        {
            let mut prefix = wrapped.clone();
            prefix.push(format!("{head} -c"));
            let start = self.out.len();
            let mut c2 = ctx.clone();
            c2.stack.clear();
            self.run_words_as_text(std::iter::once(arg), &env, &mut c2, depth);
            self.prefix_wrappers(start, &prefix, None);
            self.outer_redirects(ctx, plain, &env, redirs, wrapped);
            return;
        }
        if head == "eval" && words.len() > 1 {
            let mut prefix = wrapped.clone();
            prefix.push("eval".to_string());
            let start = self.out.len();
            if env.is_empty() {
                self.run_words_as_text(words[1..].iter(), &env, ctx, depth);
            } else {
                let mut c2 = ctx.clone();
                c2.stack.clear();
                self.run_words_as_text(words[1..].iter(), &env, &mut c2, depth);
            }
            self.prefix_wrappers(start, &prefix, None);
            self.outer_redirects(ctx, plain, &env, redirs, wrapped);
            return;
        }
        if head == "xargs" {
            let j = skip_options(
                &words,
                1,
                "IndPLsEa",
                &[
                    "max-args",
                    "max-procs",
                    "max-lines",
                    "max-chars",
                    "delimiter",
                    "arg-file",
                    "eof",
                ],
            );
            let mut prefix = wrapped.clone();
            prefix.push("xargs".to_string());
            let start = self.out.len();
            let inner: Vec<RawWord> = words[j.min(words.len())..].to_vec();
            if inner.is_empty() {
                let s = self.simple(
                    ctx,
                    vec![Word::Lit("echo".to_string())],
                    &env,
                    Vec::new(),
                    Vec::new(),
                );
                self.out.push(s);
            } else {
                let mut c2 = ctx.clone();
                self.interpret(inner, Vec::new(), env.clone(), &mut c2, depth + 1);
            }
            self.prefix_wrappers(start, &prefix, Some(Word::Dynamic(FROM_XARGS.to_string())));
            self.outer_redirects(ctx, plain, &env, redirs, wrapped);
            return;
        }

        // An ordinary command.
        let s = self.simple(ctx, plain, &env, redirs, wrapped.clone());
        self.out.push(s);

        if head == "find" {
            let mut prefix = wrapped.clone();
            prefix.push("find".to_string());
            let mut k = 1;
            while k < words.len() {
                if matches!(
                    words[k].word.lit(),
                    Some("-exec" | "-execdir" | "-ok" | "-okdir")
                ) {
                    let mut e = k + 1;
                    while e < words.len() && !matches!(words[e].word.lit(), Some(";" | "+")) {
                        e += 1;
                    }
                    let inner: Vec<RawWord> = words[k + 1..e].to_vec();
                    if !inner.is_empty() {
                        let start = self.out.len();
                        let mut c2 = ctx.clone();
                        self.interpret(inner, Vec::new(), env.clone(), &mut c2, depth + 1);
                        self.prefix_wrappers(
                            start,
                            &prefix,
                            Some(Word::Dynamic(FROM_XARGS.to_string())),
                        );
                    }
                    k = e + 1;
                } else {
                    k += 1;
                }
            }
        }

        // Effects on the shell itself.
        if wrapped.iter().all(|w| w == "command" || w == "builtin") {
            let args = &words[1..];
            match head {
                "cd" => self.cd(ctx, args),
                "pushd" => {
                    let first = args.first().and_then(|w| w.word.lit());
                    match first {
                        Some("-n") => {}
                        None => ctx.known = false,
                        Some(s) if s.starts_with('+') || (s.starts_with('-') && s.len() > 1) => {
                            ctx.known = false
                        }
                        _ => {
                            ctx.stack.push((ctx.dir.clone(), ctx.known));
                            self.cd(ctx, args);
                        }
                    }
                }
                "popd" => {
                    let first = args.first().and_then(|w| w.word.lit());
                    match first {
                        Some(s) if s.starts_with('-') || s.starts_with('+') => {
                            if s != "-n" {
                                ctx.known = false;
                            }
                        }
                        _ => {
                            if let Some((d, k)) = ctx.stack.pop() {
                                ctx.dir = d;
                                ctx.known = k;
                            }
                        }
                    }
                }
                "export" => {
                    for w in args {
                        if let Some(n) = w.assign {
                            let (k, v) = assign_of(w, n);
                            set_env(&mut ctx.exports, k, v);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Redirects of a command that was replaced by the commands it runs.
    fn outer_redirects(
        &mut self,
        ctx: &Ctx,
        words: Vec<Word>,
        env: &[(String, Word)],
        redirs: Vec<Word>,
        wrapped: Vec<String>,
    ) {
        if !redirs.is_empty() {
            let s = self.simple(ctx, words, env, redirs, wrapped);
            self.out.push(s);
        }
    }

    /// Put `prefix` in front of `wrapped_by`, and optionally append `marker` to the words,
    /// of every command emitted since `start`.
    fn prefix_wrappers(&mut self, start: usize, prefix: &[String], marker: Option<Word>) {
        for s in &mut self.out[start..] {
            let mut w = prefix.to_vec();
            w.append(&mut s.wrapped_by);
            s.wrapped_by = w;
            if let Some(m) = &marker {
                s.words.push(m.clone());
            }
        }
    }

    fn cd(&self, ctx: &mut Ctx, args: &[RawWord]) {
        let mut i = 0;
        while let Some(w) = args.get(i) {
            match w.word.lit() {
                Some("--") => {
                    i += 1;
                    break;
                }
                Some(s)
                    if s.len() > 1
                        && s.starts_with('-')
                        && s[1..].chars().all(|c| "LPe@".contains(c)) =>
                {
                    i += 1;
                }
                _ => break,
            }
        }
        match args.get(i) {
            None => match self.home {
                Some(h) => {
                    ctx.dir = normalize(h);
                    ctx.known = true;
                }
                None => ctx.known = false,
            },
            Some(w) => match &w.word {
                Word::Dynamic(_) => ctx.known = false,
                Word::Lit(s) => {
                    if s == "-" || (w.tilde && s.starts_with('~')) {
                        ctx.known = false;
                    } else if !s.is_empty() {
                        ctx.dir = join_dir(&ctx.dir, s);
                        if s.starts_with('/') {
                            ctx.known = true;
                        }
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    fn p(cmd: &str) -> Parsed {
        parse(cmd, "/w", Some(HOME))
    }

    fn lits(s: &Simple) -> Vec<String> {
        s.words.iter().map(|w| w.text().to_string()).collect()
    }

    fn progs(cmd: &str) -> Vec<Vec<String>> {
        p(cmd).commands.iter().map(lits).collect()
    }

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_on_operators() {
        assert_eq!(
            progs("a; b && c || d | e & f\ng"),
            vec![
                v(&["a"]),
                v(&["b"]),
                v(&["c"]),
                v(&["d"]),
                v(&["e"]),
                v(&["f"]),
                v(&["g"])
            ]
        );
    }

    #[test]
    fn quotes_protect_operators() {
        assert_eq!(
            progs(r#"git commit -m "fix; cleanup""#),
            vec![v(&["git", "commit", "-m", "fix; cleanup"])]
        );
        assert_eq!(progs("echo 'a && b'"), vec![v(&["echo", "a && b"])]);
        assert_eq!(progs(r"echo a\;b"), vec![v(&["echo", "a;b"])]);
        assert_eq!(
            progs(r#"echo "a\"b" 'c'd"#),
            vec![v(&["echo", "a\"b", "cd"])]
        );
        assert_eq!(progs(r"echo $'a\tb;c'"), vec![v(&["echo", "a\tb;c"])]);
        assert_eq!(progs(r"echo $'\x41\101'"), vec![v(&["echo", "AA"])]);
        assert_eq!(progs("echo \"\" x"), vec![v(&["echo", "", "x"])]);
    }

    #[test]
    fn single_flag() {
        assert!(p("git status").single);
        assert!(p("tcas lease wait a.txt  # note").single);
        assert!(p("git status\n").single);
        assert!(p("git commit -m 'a; b'").single);
        assert!(!p("a; b").single);
        assert!(!p("a && b").single);
        assert!(!p("a | b").single);
        assert!(!p("a &").single);
        assert!(!p("a\nb").single);
        assert!(!p("a > f").single);
        assert!(!p("a 2>&1").single);
        assert!(!p("echo $(a)").single);
        assert!(!p("echo `a`").single);
        assert!(!p("echo \"$(a)\"").single);
        assert!(!p("(a)").single);
        assert!(!p("{ a; }").single);
        assert!(!p("! a").single);
        assert!(!p("").single);
        assert!(p("echo $X").single);
    }

    #[test]
    fn cd_tracks_directory() {
        let r = p("cd api && git stash");
        assert_eq!(r.commands[1].dir, "/w/api");
        assert!(r.commands[1].dir_known);
        assert_eq!(r.commands[0].dir, "/w");
        let r = p("cd api; cd ../web; git status");
        assert_eq!(r.commands[2].dir, "/w/web");
        let r = p("cd ../app-backend && git stash");
        assert_eq!(r.commands[1].dir, "/app-backend");
        let r = p("cd /tmp/x/../y && ls");
        assert_eq!(r.commands[1].dir, "/tmp/y");
        let r = p("cd ~ && ls; cd ~/code && ls; cd /");
        assert_eq!(r.commands[1].dir, "/home/u");
        assert_eq!(r.commands[3].dir, "/home/u/code");
        let r = p("cd && ls");
        assert_eq!(r.commands[1].dir, "/home/u");
        let r = p("cd -- api && ls");
        assert_eq!(r.commands[1].dir, "/w/api");
    }

    #[test]
    fn cd_unknown() {
        let r = p("cd $X && git stash");
        assert!(!r.commands[1].dir_known);
        let r = p("cd - && git stash");
        assert!(!r.commands[1].dir_known);
        let r = p("cd $(mktemp -d) && git stash");
        assert!(!r.commands.last().unwrap().dir_known);
        let r = p("cd $X; cd api; git stash; cd /abs; git stash");
        assert!(!r.commands[2].dir_known);
        assert!(r.commands[4].dir_known);
        assert_eq!(r.commands[4].dir, "/abs");
        let r = parse("cd ~/x && ls", "/w", None);
        assert!(!r.commands[1].dir_known);
    }

    #[test]
    fn pushd_popd() {
        let r = p("pushd api >/dev/null; git status; popd; git status");
        assert_eq!(r.commands[1].dir, "/w/api");
        assert_eq!(r.commands[3].dir, "/w");
        let r = p("popd; ls");
        assert_eq!(r.commands[1].dir, "/w");
    }

    #[test]
    fn subshell_and_group() {
        let r = p("(cd web && git status); git status");
        assert_eq!(r.commands[1].dir, "/w/web");
        assert_eq!(r.commands[2].dir, "/w");
        let r = p("{ cd web; git status; }; git log");
        assert_eq!(r.commands[1].dir, "/w/web");
        assert_eq!(r.commands[2].dir, "/w/web");
        let r = p("cd a | cat; ls");
        assert_eq!(r.commands.last().unwrap().dir, "/w");
        let r = p("cd a & ls");
        assert_eq!(r.commands.last().unwrap().dir, "/w");
    }

    #[test]
    fn wrappers() {
        let r = p("FOO=1 git status");
        assert_eq!(lits(&r.commands[0]), v(&["git", "status"]));
        assert_eq!(
            r.commands[0].env,
            vec![("FOO".to_string(), Word::Lit("1".into()))]
        );
        let r = p("env -u X FOO=1 git status");
        assert_eq!(lits(&r.commands[0]), v(&["git", "status"]));
        assert_eq!(r.commands[0].wrapped_by, v(&["env"]));
        assert_eq!(r.commands[0].env[0].0, "FOO");
        let r = p("sudo -u root nice -n 5 nohup command exec git stash");
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]));
        assert_eq!(
            r.commands[0].wrapped_by,
            v(&["sudo", "nice", "nohup", "command", "exec"])
        );
        let r = p("time git stash");
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]));
        let r = p("command -v git");
        assert_eq!(lits(&r.commands[0]), v(&["command", "-v", "git"]));
        let r = p("/usr/bin/env git stash");
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]));
        let r = p("timeout 5 git fetch");
        assert_eq!(lits(&r.commands[0]), v(&["git", "fetch"]));
    }

    #[test]
    fn bare_assignment_and_export() {
        let r = p("TCAS_ALLOW=1");
        assert_eq!(r.commands.len(), 1);
        assert!(r.commands[0].words.is_empty());
        assert_eq!(r.commands[0].env[0].0, "TCAS_ALLOW");
        let r = p("export TCAS_ALLOW=1; git commit");
        assert_eq!(r.commands[1].env[0].0, "TCAS_ALLOW");
        let r = p("(export A=1); git commit");
        assert!(r.commands[1].env.is_empty());
        let r = p("export A=1 && bash -c 'git stash'");
        assert_eq!(r.commands[1].env[0].0, "A");
    }

    #[test]
    fn shell_c_recursion() {
        let r = p(r#"bash -c "cd api && git commit -am x""#);
        assert_eq!(r.commands.len(), 2);
        assert_eq!(r.commands[1].dir, "/w/api");
        assert_eq!(r.commands[1].wrapped_by, v(&["bash -c"]));
        assert_eq!(lits(&r.commands[1]), v(&["git", "commit", "-am", "x"]));
        assert!(r.single);
        let r = p("sh -c 'git stash'");
        assert_eq!(r.commands[0].wrapped_by, v(&["sh -c"]));
        let r = p("zsh -lc 'git stash'");
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]));
        // cd inside does not leak out.
        let r = p("bash -c 'cd api'; git stash");
        assert_eq!(r.commands.last().unwrap().dir, "/w");
        // dir is inherited
        let r = p("cd api; bash -c 'git stash'");
        assert_eq!(r.commands[1].dir, "/w/api");
        // not a -c invocation
        let r = p("bash script.sh");
        assert_eq!(lits(&r.commands[0]), v(&["bash", "script.sh"]));
        assert!(r.commands[0].wrapped_by.is_empty());
        let r = p("FOO=1 bash -c 'git stash'");
        assert_eq!(r.commands[0].env[0].0, "FOO");
    }

    #[test]
    fn shell_c_dynamic() {
        let r = p(r#"bash -c "$CMD""#);
        assert_eq!(r.commands.len(), 1);
        assert_eq!(r.commands[0].words, vec![Word::Dynamic("$CMD".into())]);
        assert_eq!(r.commands[0].wrapped_by, v(&["bash -c"]));
        let r = p("eval \"$x\"");
        assert_eq!(r.commands.len(), 1);
        assert!(r.commands[0].words[0].is_dynamic());
        assert_eq!(r.commands[0].wrapped_by, v(&["eval"]));
    }

    #[test]
    fn eval_literal() {
        let r = p(r#"eval "cd api; git stash""#);
        assert_eq!(r.commands[1].dir, "/w/api");
        assert_eq!(r.commands[1].wrapped_by, v(&["eval"]));
        // eval runs in the current shell, so cd persists
        let r = p("eval 'cd api'; git stash");
        assert_eq!(r.commands.last().unwrap().dir, "/w/api");
    }

    #[test]
    fn xargs_and_find() {
        let r = p("ls | xargs git -C api add");
        assert_eq!(r.commands.len(), 2);
        let g = &r.commands[1];
        assert_eq!(g.wrapped_by, v(&["xargs"]));
        assert_eq!(
            g.words,
            vec![
                Word::Lit("git".into()),
                Word::Lit("-C".into()),
                Word::Lit("api".into()),
                Word::Lit("add".into()),
                Word::Dynamic(FROM_XARGS.into())
            ]
        );
        let r = p("echo x | xargs -n 1 -I{} git add");
        assert_eq!(lits(&r.commands[1]), v(&["git", "add", FROM_XARGS]));
        let r = p("find . -name '*.rs' -exec git add {} \\;");
        assert_eq!(r.commands.len(), 2);
        assert_eq!(r.commands[0].words[0], Word::Lit("find".into()));
        assert_eq!(r.commands[1].wrapped_by, v(&["find"]));
        assert_eq!(
            r.commands[1].words.last().unwrap(),
            &Word::Dynamic(FROM_XARGS.into())
        );
        let r = p("find . -exec git add {} + -print");
        assert_eq!(lits(&r.commands[1]), v(&["git", "add", "{}", FROM_XARGS]));
        let r = p("sudo xargs git add");
        assert_eq!(r.commands[0].wrapped_by, v(&["sudo", "xargs"]));
    }

    #[test]
    fn dynamic_words() {
        let r = p("git $SUB ${X} \"$Y\" `z` $(w) *.c a?b [ab] '$no' \"\\$no\" a\\*b");
        let w = &r.commands.last().unwrap().words;
        assert_eq!(w[0], Word::Lit("git".into()));
        for (i, x) in w.iter().enumerate().take(9).skip(1) {
            assert!(x.is_dynamic(), "word {i}: {x:?}");
        }
        assert_eq!(w[1], Word::Dynamic("$SUB".into()));
        assert_eq!(w[9], Word::Lit("$no".into()));
        assert_eq!(w[10], Word::Lit("$no".into()));
        assert_eq!(w[11], Word::Lit("a*b".into()));
        assert_eq!(progs("echo $ x$"), vec![v(&["echo", "$", "x$"])]);
        let r = p("echo {a,b} {1..3}");
        assert!(r.commands[0].words[1].is_dynamic());
        assert!(r.commands[0].words[2].is_dynamic());
        assert_eq!(progs("echo \"*\" '?'"), vec![v(&["echo", "*", "?"])]);
    }

    #[test]
    fn substitution_commands_are_found() {
        let r = p("echo $(git stash) done");
        assert_eq!(r.commands.len(), 2);
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]));
        let r = p("echo \"`git stash`\"");
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]));
        let r = p("diff <(git stash list) b");
        assert_eq!(lits(&r.commands[0]), v(&["git", "stash", "list"]));
        let r = p("cd api; echo $(git stash)");
        assert_eq!(r.commands[1].dir, "/w/api");
    }

    #[test]
    fn redirects() {
        let r = p("echo x > a.txt 2> b.txt >> c < d &> e 2>&1 >| f");
        let c = &r.commands[0];
        assert_eq!(lits(c), v(&["echo", "x"]));
        let t: Vec<&str> = c.redirects.iter().map(|w| w.text()).collect();
        assert_eq!(t, vec!["a.txt", "b.txt", "c", "d", "e", "f"]);
        let r = p("> ~/.tcas/x");
        assert_eq!(
            r.commands[0].redirects,
            vec![Word::Lit("/home/u/.tcas/x".into())]
        );
        assert!(r.commands[0].words.is_empty());
        let r = p("cat a>b");
        assert_eq!(r.commands[0].redirects, vec![Word::Lit("b".into())]);
        let r = p("echo 2 > f");
        assert_eq!(lits(&r.commands[0]), v(&["echo", "2"]));
    }

    #[test]
    fn tilde() {
        let r = p("cat ~/.tcas/x ~ ~root '~/q'");
        assert_eq!(
            lits(&r.commands[0]),
            v(&["cat", "/home/u/.tcas/x", "/home/u", "~root", "~/q"])
        );
    }

    #[test]
    fn heredoc_bodies_are_not_commands() {
        let r = p("cat <<EOF\ngit stash\nrm -rf x\nEOF\ngit status");
        assert_eq!(r.commands.len(), 2);
        assert_eq!(lits(&r.commands[0]), v(&["cat"]));
        assert_eq!(lits(&r.commands[1]), v(&["git", "status"]));
        let r = p("cat <<'EOF' > f\n$(git stash)\nEOF\n");
        assert_eq!(r.commands.len(), 1);
        let r = p("cat <<-EOF\n\tgit stash\n\tEOF\nls");
        assert_eq!(r.commands.len(), 2);
        let r = p("cat <<EOF | git apply\ndiff\nEOF");
        assert_eq!(r.commands.len(), 2);
        assert_eq!(lits(&r.commands[1]), v(&["git", "apply"]));
        // unterminated: the rest is body
        let r = p("cat <<EOF\ngit stash");
        assert_eq!(r.commands.len(), 1);
        let r = p("cat <<< 'git stash'");
        assert_eq!(r.commands.len(), 1);
        assert!(r.commands[0].redirects.is_empty());
        let r = p("a <<A <<B\n1\nA\n2\nB\nc");
        assert_eq!(r.commands.len(), 2);
    }

    #[test]
    fn comments() {
        assert_eq!(progs("git status # git stash"), vec![v(&["git", "status"])]);
        assert_eq!(
            progs("# git stash\ngit status"),
            vec![v(&["git", "status"])]
        );
        assert_eq!(progs("echo a#b"), vec![v(&["echo", "a#b"])]);
        assert_eq!(progs("echo '#x' \"#y\""), vec![v(&["echo", "#x", "#y"])]);
        assert_eq!(progs("ls;# c\nls"), vec![v(&["ls"]), v(&["ls"])]);
    }

    #[test]
    fn control_flow() {
        let r = p("if git diff --quiet; then git stash; fi");
        assert_eq!(r.commands.len(), 2);
        assert_eq!(lits(&r.commands[1]), v(&["git", "stash"]));
        let r = p("for f in a b; do git add $f; done");
        assert_eq!(lits(&r.commands[0]), v(&["git", "add", "$f"]));
        let r = p("while true; do git stash; done");
        assert_eq!(r.commands.len(), 2);
        let r = p("case $x in a) git stash;; b|c) ls;; esac");
        assert_eq!(
            r.commands.iter().map(lits).collect::<Vec<_>>(),
            vec![v(&["git", "stash"]), v(&["ls"])]
        );
        let r = p("f() { git stash; }; f");
        assert!(r.commands.iter().any(|c| lits(c) == v(&["git", "stash"])));
    }

    #[test]
    fn assignment_arrays_and_misc() {
        let r = p("A=(1 2) ls");
        assert_eq!(lits(&r.commands[0]), v(&["ls"]));
        let r = p("A=$(git stash) ls");
        assert_eq!(r.commands.len(), 2);
        let r = p("git commit -m \"a\"=b");
        assert_eq!(lits(&r.commands[0]), v(&["git", "commit", "-m", "a=b"]));
    }

    #[test]
    fn malformed_input_never_panics() {
        for s in [
            "",
            " ",
            "\n\n",
            ";",
            ";;",
            "&&",
            "|",
            "(",
            ")",
            "((",
            "))",
            "{",
            "}",
            "'",
            "\"",
            "`",
            "$(",
            "$((",
            "${",
            "$'",
            "$'\\",
            "\\",
            "a \\",
            "echo \"unterminated",
            "echo 'unterminated; git stash",
            "git commit -m \"fix; cleanup",
            "cat <<",
            "cat <<EOF",
            ">",
            "2>",
            "<(",
            "a=(",
            "bash -c",
            "eval",
            "xargs",
            "find -exec",
            "env",
            "sudo -u",
            "case",
            "for",
            "function",
            "cd",
            "pushd",
            "popd; popd",
            "$'\\x",
            "echo $'\\u12",
            "if",
            "!",
            "$(( 1 ",
            "\u{0}",
            "é$é",
            "echo \"a$(b \"c",
        ] {
            let _ = p(s);
        }
        let r = p("echo \"unterminated; git stash");
        assert_eq!(r.commands.len(), 1);
        assert_eq!(
            lits(&r.commands[0]),
            v(&["echo", "unterminated; git stash"])
        );
        let deep = "(".repeat(5000) + "git stash" + &")".repeat(5000);
        let _ = p(&deep);
        let deep = "$(".repeat(3000);
        let _ = p(&deep);
        let deep = "eval ".repeat(3000);
        let _ = p(&deep);
        let deep = "xargs ".repeat(3000);
        let _ = p(&deep);
        let deep = "bash -c '".repeat(200);
        let _ = p(&deep);
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a/../.."), "..");
        assert_eq!(normalize(""), ".");
        assert_eq!(join_dir("/w", "api/"), "/w/api");
        assert_eq!(join_dir("/w", "/x"), "/x");
    }

    #[test]
    fn computed_scripts_are_parsed_as_far_as_they_are_literal() {
        // The literal parts of a double-quoted script are read; a computed
        // word stays computed.
        let r = p(r#"bash -c "cd api && echo $X && git stash""#);
        let git = r.commands.iter().find(|s| s.words.first() == Some(&Word::Lit("git".into()))).unwrap();
        assert_eq!(git.dir, "/w/api");
        assert_eq!(git.wrapped_by, v(&["bash -c"]));
        let r = p(r#"eval "$x""#);
        assert_eq!(r.commands.len(), 1);
        assert_eq!(r.commands[0].words, vec![Word::Dynamic("$x".into())]);
        assert_eq!(r.commands[0].wrapped_by, v(&["eval"]));
        // `\$` inside the double quotes is expanded by the inner shell.
        let r = p(r#"sh -c "git \$SUB""#);
        assert_eq!(r.commands[0].words, vec![Word::Lit("git".into()), Word::Dynamic("$SUB".into())]);
    }

    #[test]
    fn dash_c_after_double_dash_and_options_with_values() {
        assert_eq!(progs("bash -c -- 'git stash'"), vec![v(&["git", "stash"])]);
        assert_eq!(progs("bash --rcfile /dev/null -c 'git stash'"), vec![v(&["git", "stash"])]);
    }

    #[test]
    fn env_and_sudo_change_directory_for_the_command_only() {
        for cmd in ["env -C api git stash", "env -Capi git stash", "env --chdir=api git stash", "env --chdir api git stash", "sudo -D api git stash", "sudo --chdir=api git stash"] {
            let r = p(&format!("{cmd}; git status"));
            assert_eq!(r.commands[0].dir, "/w/api", "{cmd}");
            assert_eq!(lits(&r.commands[0]), v(&["git", "stash"]), "{cmd}");
            assert_eq!(r.commands[1].dir, "/w", "{cmd}: the directory does not leak");
        }
        let r = p("env -i -C /x git stash");
        assert_eq!(r.commands[0].dir, "/x");
        assert!(r.commands[0].dir_known);
    }

    #[test]
    fn env_split_string_is_a_command() {
        let r = p(r#"env -S "git stash""#);
        assert_eq!(progs(r#"env -S "git stash""#), vec![v(&["git", "stash"])]);
        assert_eq!(r.commands[0].wrapped_by, v(&["env"]));
        assert_eq!(progs(r#"env --split-string="git add" a.txt"#), vec![v(&["git", "add", "a.txt"])]);
    }
}
