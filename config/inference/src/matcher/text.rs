//! Python-compatible text helpers: a port of the string half of
//! `train/learned-matcher/features.py`.
//!
//! Python's `re` and `str` differ from Rust's defaults, so the classes are
//! spelled out:
//! - Python `\w` (str patterns) is `str.isalnum() or "_"`, i.e. `[\p{L}\p{N}_]`
//!   (Rust's `\w` also counts combining marks and connector punctuation).
//! - Python `\s` / `str.isspace()` is Unicode White_Space plus U+001C–U+001F.
//! - Python `\b` is a boundary between those `\w` and non-`\w` characters,
//!   written here as a lookaround (fancy-regex).

use std::collections::HashSet;
use std::sync::OnceLock;

use fancy_regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// Python `\w` (str patterns).
const W: &str = r"\p{L}\p{N}_";
/// Python `\s` (str patterns).
const S: &str = r"\s\x1c-\x1f";

/// Marker names, in `features.MARKERS` order.
pub const MARKER_NAMES: [&str; 17] = [
    "live",
    "remix",
    "instrumental",
    "acoustic",
    "cover",
    "medley",
    "short",
    "mv",
    "arrange",
    "edit",
    "remaster",
    "ver",
    "acappella",
    "full",
    "demo",
    "stem",
    "performance",
];

const MARKER_PATTERNS: [&str; 17] = [
    r"\blive\b|ライブ|生歌",
    r"remix|リミックス|\bmix\)|bootleg",
    r"instrumental|\binst\b|inst\.|インスト|off ?vocal|karaoke|カラオケ|backing track",
    r"acoustic|アコースティック|unplugged",
    r"\bcover\b|カバー|歌ってみた|歌わせていただきました",
    r"medley|メドレー",
    r"tv ?size|short ?ver|short version|cut ?ver|one chorus|ショート|tv ?ver|\bshort\b|#shorts|試聴|preview|teaser|crossfade|クロスフェード",
    r"\bmv\b|music video|\bpv\b|ミュージックビデオ|official video",
    r"arrange|アレンジ|編曲",
    r"\bedit\b|radio edit",
    r"remaster|リマスター",
    r"\bver\b|ver\.|version|バージョン",
    r"a ?cappella|アカペラ",
    r"full ?ver|full size|フル",
    r"\bdemo\b",
    r"stem\b|multitrack|ステム|パラデータ",
    r"昼公演|夜公演|\bday ?\d|\bnight\b|公演|fes\b",
];

const ARTIST_STRIP_PATTERNS: [&str; 13] = [
    r"\s*-\s*topic$",
    r"\s*\((?:cv|c\.v|vo|voice)[.:：]?[^)]*\)",
    r"\s*（(?:cv|vo)[.:：]?[^）]*）",
    r"\s*\(\d+\)$",
    r"\s*\(all\)$",
    r"様$",
    r"\s*official$",
    r"\s*公式$",
    r"\s*vevo$",
    r"\s*\bch\..*$",
    r"\s*ch\.?$",
    r"\s*channel$",
    r"\s*チャンネル$",
];

/// Translate a Python pattern's `\b` and bare `\s` into the explicit forms.
/// Only for the patterns in this file: `\s` never appears inside a class there.
fn py(pattern: &str, ignore_case: bool) -> Regex {
    let b = format!(r"(?:(?<=[{W}])(?![{W}])|(?<![{W}])(?=[{W}]))");
    let p = pattern.replace(r"\b", &b).replace(r"\s", &format!("[{S}]"));
    let p = if ignore_case { format!("(?i){p}") } else { p };
    Regex::new(&p).unwrap_or_else(|e| panic!("bad pattern {pattern:?}: {e}"))
}

pub struct Patterns {
    pub markers: Vec<Regex>,
    holo: Regex,
    named_ver: Regex,
    digits: Regex,
    placeholder: Regex,
    brackets: Regex,
    slash_tail: Regex,
    artist_strip: Vec<Regex>,
    generic_artist: Regex,
    cjk: Regex,
    spaces: Regex,
    artist_core_junk: Regex,
}

pub fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        markers: MARKER_PATTERNS.iter().map(|p| py(p, true)).collect(),
        holo: py(r"hololive|ホロライブ", true),
        named_ver: Regex::new(&format!(r"(?i)([{W}぀-ヿ一-鿿]+)[{S}]*ver(?:sion|\.)?")).unwrap(),
        digits: Regex::new(r"\d+").unwrap(),
        placeholder: py(r"^\[?(private video|deleted video|untitled|unknown)\]?$", true),
        brackets: Regex::new(r"[\(（\[【<＜〈《][^\)）\]】>＞〉》]*[\)）\]】>＞〉》]").unwrap(),
        slash_tail: Regex::new(&format!(r"[{S}]*[/／|｜][{S}]*[^/／|｜]*$")).unwrap(),
        artist_strip: ARTIST_STRIP_PATTERNS.iter().map(|p| py(p, true)).collect(),
        generic_artist: py(
            r"^(release|various artists|ヴァリアス・アーティスト|v\.?a\.?|unknown artist|不明|anonymous|traditional)$",
            true,
        ),
        cjk: Regex::new(r"[぀-ヿ一-鿿ｦ-ﾟ]").unwrap(),
        spaces: Regex::new(&format!("[{S}]+")).unwrap(),
        artist_core_junk: Regex::new(&format!(r"[{S}・._-]+")).unwrap(),
    })
}

fn is_match(re: &Regex, s: &str) -> bool {
    re.is_match(s).unwrap_or(false)
}

fn sub(re: &Regex, rep: &str, s: &str) -> String {
    re.replace_all(s, rep).into_owned()
}

/// Python `str.isspace()`.
pub fn py_is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// Python `str.isalnum()` (the non-`_` part of `\w`).
fn py_is_alnum(c: char) -> bool {
    use unicode_general_category::{GeneralCategory as G, get_general_category};
    matches!(
        get_general_category(c),
        G::UppercaseLetter
            | G::LowercaseLetter
            | G::TitlecaseLetter
            | G::ModifierLetter
            | G::OtherLetter
            | G::DecimalNumber
            | G::LetterNumber
            | G::OtherNumber
    )
}

/// Python `str.strip()`.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_is_space)
}

/// `unicodedata.normalize("NFKC", s).casefold()`, whitespace runs collapsed, stripped.
pub fn norm(s: &str) -> String {
    let nfkc: String = s.nfkc().collect();
    let folded = caseless::default_case_fold_str(&nfkc);
    py_strip(&sub(&patterns().spaces, " ", &folded)).to_owned()
}

/// Python `len()`.
pub fn py_len(s: &str) -> usize {
    s.chars().count()
}

pub fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().filter(|&c| py_is_alnum(c)).collect();
    if chars.is_empty() {
        return HashSet::new();
    }
    let n = chars.len().saturating_sub(2).max(1);
    (0..n)
        .map(|i| chars[i..(i + 3).min(chars.len())].iter().collect())
        .collect()
}

pub fn tokens(s: &str) -> HashSet<String> {
    s.split(|c: char| !py_is_alnum(c))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn jacc<T: Eq + std::hash::Hash>(a: &HashSet<T>, b: &HashSet<T>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    inter as f64 / union as f64
}

/// Title with brackets, a trailing "/ artist" part and version markers removed.
pub fn core_title(name: &str) -> String {
    let p = patterns();
    let mut t = sub(&p.holo, "", &norm(name));
    let stripped = sub(&p.brackets, " ", &t);
    if !py_strip(&stripped).is_empty() {
        t = stripped;
    }
    let tail = sub(&p.slash_tail, "", &t);
    if !py_strip(&tail).is_empty() {
        t = tail;
    }
    for r in &p.markers {
        t = sub(r, " ", &t);
    }
    sub(&p.spaces, " ", &t)
        .trim_matches(|c| matches!(c, ' ' | '-' | '_' | '~' | '・'))
        .to_owned()
}

pub fn artist_core_raw(name: &str) -> String {
    let mut t = norm(name);
    for r in &patterns().artist_strip {
        let new = sub(r, "", &t);
        let new = py_strip(&new);
        if !new.is_empty() {
            t = new.to_owned();
        }
    }
    t
}

pub fn artist_core(name: &str) -> String {
    sub(&patterns().artist_core_junk, "", &artist_core_raw(name))
}

pub fn bracket_contents(name: &str) -> HashSet<String> {
    let p = patterns();
    let n = norm(name);
    let mut out = HashSet::new();
    for m in p.brackets.find_iter(&n).flatten() {
        let inner: Vec<char> = m.as_str().chars().collect();
        let inner: String = inner[1..inner.len() - 1].iter().collect();
        let v = py_strip(&sub(&p.spaces, " ", &inner)).to_owned();
        if !v.is_empty() {
            out.insert(v);
        }
    }
    out
}

/// (marker indices found, named "xxx ver" groups) over a list of names.
pub fn markers(names: &[String]) -> (HashSet<usize>, HashSet<String>) {
    let p = patterns();
    let (mut found, mut named) = (HashSet::new(), HashSet::new());
    for n in names {
        let t = sub(&p.holo, "", &norm(n));
        for (k, r) in p.markers.iter().enumerate() {
            if is_match(r, &t) {
                found.insert(k);
            }
        }
        for c in p.named_ver.captures_iter(&t).flatten() {
            if let Some(g) = c.get(1) {
                named.insert(g.as_str().to_owned());
            }
        }
    }
    (found, named)
}

pub fn digits(s: &str) -> HashSet<String> {
    patterns()
        .digits
        .find_iter(s)
        .flatten()
        .map(|m| m.as_str().to_owned())
        .collect()
}

pub fn has_cjk(s: &str) -> bool {
    is_match(&patterns().cjk, s)
}

pub fn is_placeholder(normed: &str) -> bool {
    is_match(&patterns().placeholder, normed)
}

pub fn is_generic_artist(normed: &str) -> bool {
    is_match(&patterns().generic_artist, normed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_semantics() {
        assert_eq!(norm("  Ｈｅｌｌｏ\u{3000}WORLD  "), "hello world");
        assert_eq!(norm("Straße"), "strasse");
        assert_eq!(
            core_title("【MV】Shiny Smily Story (Short Ver.) / hololive IDOL PROJECT"),
            "shiny smily story"
        );
        assert_eq!(artist_core("Spice Girls - Topic"), "spicegirls");
        let t = trigrams("ab");
        assert_eq!(t, HashSet::from(["ab".to_owned()]));
        let (m, v) = markers(&["Song (Acoustic ver.)".to_owned()]);
        assert!(m.contains(&3) && m.contains(&11));
        assert_eq!(v, HashSet::from(["acoustic".to_owned()]));
    }
}
