//! The deterministic rule engine that runs in front of the SmolLM judge.
//!
//! Ported from `src/rules.ts` in the smol-model-test repo, where it was
//! developed against all 15,374 real (query, candidate) pairs and audited by
//! hand twice. Two things justify it sitting in front of the model:
//!
//!   * **Cost.** It settles ~83% of real candidates with no inference at all,
//!     turning a 200-candidate query from ~40s of model time into a few
//!     seconds.
//!   * **Accuracy.** On 826 held-out rows the hybrid beat the model alone
//!     (96.0% vs 93.6% accuracy, 26 vs 32 wrong files accepted). The rules also
//!     catch a failure the model reliably gets wrong: a *different* track
//!     sitting in a folder named after the requested one, e.g. asking for
//!     "Unreasonable Behaviour" and being handed
//!     `…/Unreasonable Behaviour/The Sound Of The Big Babou.mp3`.
//!
//! Only `Gold` verdicts short-circuit the model. `Weak` ones exist precisely
//! because an audit showed them unreliable — `wrong_artist` was demoted after
//! being wrong on both sampled rows — so those still go to the model.
//!
//! Deliberately no `regex` dependency: every pattern here is a prefix scan or a
//! word-boundary substring test, and hand-rolling them keeps the crate's build
//! unchanged.

use super::smol::Label;

/// How much the engine trusts its own answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Final. The model is not consulted.
    Gold,
    /// Recorded but not trusted; the model decides.
    Weak,
    /// No opinion.
    Abstain,
}

#[derive(Debug, Clone)]
pub struct RuleVerdict {
    pub label: Option<Label>,
    pub tier: Tier,
    /// Which rule fired, for the judgement log.
    pub rule: &'static str,
}

impl RuleVerdict {
    fn gold(label: Label, rule: &'static str) -> Self {
        Self {
            label: Some(label),
            tier: Tier::Gold,
            rule,
        }
    }
    fn weak(label: Label, rule: &'static str) -> Self {
        Self {
            label: Some(label),
            tier: Tier::Weak,
            rule,
        }
    }
    fn abstain(rule: &'static str) -> Self {
        Self {
            label: None,
            tier: Tier::Abstain,
            rule,
        }
    }
}

// ---------------------------------------------------------------- normalising

/// Lowercase and strip Latin diacritics: "Jürgen" -> "jurgen".
///
/// A lookup rather than full Unicode NFD, because the corpus is Latin-script
/// with the occasional CJK title, and CJK needs no folding — only preserving.
fn fold(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        let lower = ch.to_lowercase().next().unwrap_or(ch);
        out.push(match lower {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' => 'a',
            'é' | 'è' | 'ê' | 'ë' | 'ē' => 'e',
            'í' | 'ì' | 'î' | 'ï' | 'ī' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' | 'ō' => 'o',
            'ú' | 'ù' | 'û' | 'ü' | 'ū' => 'u',
            'ñ' => 'n',
            'ç' => 'c',
            'ý' | 'ÿ' => 'y',
            'š' => 's',
            'ž' => 'z',
            'ð' => 'd',
            other => other,
        });
    }
    out
}

/// Words that carry no identifying weight. `mix`/`version`/`original` are here
/// because version information is extracted separately; leaving them in would
/// make "(Original Mix)" look like part of the title.
const STOPWORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "of",
    "and",
    "feat",
    "ft",
    "featuring",
    "original",
    "mix",
    "version",
    "vs",
    "pt",
    "part",
];

/// Fold, split on anything that is not a letter or digit, drop stopwords and
/// single characters.
///
/// Keeps all Unicode letters, not just ASCII: an ASCII-only class erased whole
/// filenames (`川辺のバザール` reduced to nothing) and they were then labelled
/// `uncertain` instead of compared.
fn tokens(input: &str) -> Vec<String> {
    fold(input)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() > 1 && !STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// Fold to bare alphanumerics: "K.T.B." -> "ktb".
///
/// Initialisms tokenise to nothing because every piece is one character. Three
/// real queries are in that state, covering 42 pairs, and without this they
/// matched nothing and were all called `different`. Only ever compared for
/// equality — a substring test on "w" would match almost any filename.
fn condense(input: &str) -> String {
    fold(input)
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Tokens carrying no title information: bare numbers, scene content hashes,
/// rip tags. They appear inside filename leaves constantly and would otherwise
/// read as surplus title words.
fn is_noise_token(token: &str) -> bool {
    if token.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if token.len() >= 6 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    matches!(
        token,
        "web"
            | "flac"
            | "mp3"
            | "wav"
            | "opus"
            | "aiff"
            | "m4a"
            | "cdm"
            | "cds"
            | "vinyl"
            | "vls"
            | "bit"
            | "kbps"
            | "320"
            | "promo"
            | "retail"
            | "rip"
            | "scene"
            | "va"
    )
}

pub struct PathParts {
    /// Filename with extension and track number removed.
    pub leaf: String,
    /// The raw filename, extension stripped.
    pub raw_leaf: String,
    pub parent: String,
    pub ancestors: Vec<String>,
}

/// Split a Windows-style Soulseek path. Every filename in the corpus is
/// backslash-delimited.
pub fn split_path(filename: &str) -> PathParts {
    let segments: Vec<&str> = filename.split('\\').filter(|s| !s.is_empty()).collect();
    let last = segments.last().copied().unwrap_or("");
    let raw_leaf = strip_extension(last);
    PathParts {
        leaf: strip_track_number(&raw_leaf),
        raw_leaf,
        parent: segments
            .get(segments.len().wrapping_sub(2))
            .copied()
            .unwrap_or("")
            .to_string(),
        ancestors: segments[..segments.len().saturating_sub(1)]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

fn strip_extension(leaf: &str) -> String {
    match leaf.rfind('.') {
        Some(dot) if leaf.len() - dot <= 6 && dot > 0 => {
            let ext = &leaf[dot + 1..];
            if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
                return leaf[..dot].to_string();
            }
            leaf.to_string()
        }
        _ => leaf.to_string(),
    }
}

fn is_separator(c: char) -> bool {
    matches!(c, '-' | '.' | '_' | ' ' | ')')
}

/// Remove leading track/disc/vinyl-side numbering: "01. ", "075.", "19 ",
/// "1-04 ", "01-02. ", "b1-", "62-", and doubled prefixes like "02 - 12 - ".
pub fn strip_track_number(leaf: &str) -> String {
    let mut out = leaf.replace('_', " ").trim().to_string();

    // Vinyl side, e.g. "b1-laurent garnier - sore fingers".
    let chars: Vec<char> = out.chars().collect();
    if chars.len() > 2
        && chars[0].is_ascii_alphabetic()
        && matches!(chars[0].to_ascii_lowercase(), 'a'..='d')
        && chars[1].is_ascii_digit()
    {
        let rest: String = chars[2..].iter().collect();
        let trimmed = rest.trim_start_matches(is_separator);
        if trimmed.len() != rest.len() && trimmed.chars().count() >= 1 {
            out = trimmed.to_string();
        }
    }

    // Up to two leading numeric groups: "02 - 12 - Antena - …".
    for _ in 0..2 {
        let digits: String = out.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() || digits.len() > 3 {
            break;
        }
        let rest = &out[digits.len()..];
        let trimmed = rest.trim_start_matches(is_separator);
        // Never strip so far that nothing is left. One character, not three:
        // single-letter and initialism titles are real ("04 W.flac").
        if trimmed.len() == rest.len() || trimmed.trim().is_empty() {
            break;
        }
        out = trimmed.trim().to_string();
    }

    out.trim().to_string()
}

/// Drop a trailing featured credit: "Help Me Lose My Mind feat. London
/// Grammar" must still match the query title.
fn strip_featured(input: &str) -> String {
    let lower = fold(input);
    for marker in [
        " feat.",
        " feat ",
        " ft.",
        " ft ",
        " featuring ",
        "(feat",
        "[feat",
    ] {
        if let Some(at) = lower.find(marker) {
            return input[..at].trim().to_string();
        }
    }
    input.trim().to_string()
}

/// True when `needle` appears in `haystack` as a contiguous run of tokens.
///
/// Contiguity is what stops "Yes" matching "Well Yes But Actually No" — the
/// bug that made a substring-based prototype unusable.
fn contains_token_run(haystack: &[String], needle: &[String]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Every token of `needle` present somewhere in `haystack`, order-independent.
/// Used for artists, since releases credit collaborators in any order.
fn contains_all_tokens(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty() && needle.iter().all(|t| haystack.contains(t))
}

fn token_overlap(haystack: &[String], needle: &[String]) -> f32 {
    if needle.is_empty() {
        return 0.0;
    }
    let hit = needle.iter().filter(|t| haystack.contains(t)).count();
    hit as f32 / needle.len() as f32
}

/// True when a title is not distinctive enough to identify a track alone.
/// "Yes", "MZ", "Bby", "Beton" collide with unrelated filenames constantly.
fn is_short_title(title_tokens: &[String]) -> bool {
    match title_tokens.len() {
        0 => true,
        1 => title_tokens[0].chars().count() < 10,
        _ => {
            title_tokens
                .iter()
                .map(|t| t.chars().count())
                .sum::<usize>()
                < 5
        }
    }
}

// ------------------------------------------------------------------- versions

/// A version family. Two recordings are the same only if these agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Family {
    Remix,
    Live,
    Edit,
    Extended,
    Instrumental,
    Dub,
    DjMix,
    Demo,
    Acoustic,
}

/// Annotations describing the requested recording rather than a different one.
/// "(Original Mix)" is how releases label the plain track — 888 occurrences.
const NEUTRAL: &[&str] = &[
    "original",
    "main",
    "album version",
    "single version",
    "remaster",
    "remastered",
    "full length",
    "master",
];

/// Not version information at all: audio format, rip provenance, censorship,
/// year, region. "(Clean)" appears 222 times and is a radio service marker.
const NON_VERSION: &[&str] = &[
    "clean",
    "dirty",
    "explicit",
    "intro clean",
    "intro dirty",
    "intro",
    "flac",
    "mp3",
    "wav",
    "aiff",
    "opus",
    "m4a",
    "320",
    "320kbps",
    "v0",
    "vinyl",
    "web",
    "cd",
    "cdm",
    "cds",
    "promo",
];

fn family_of(text: &str) -> Vec<Family> {
    let mut out = Vec::new();
    let has = |words: &[&str]| words.iter().any(|w| contains_word(text, w));

    if has(&[
        "remix", "rmx", "remixed", "rework", "reshape", "retouch", "bootleg", "refix", "flip",
        "mashup", "vip",
    ]) {
        out.push(Family::Remix);
    }
    if has(&["live", "boiler room", "unplugged", "en vivo", "cercle"]) {
        out.push(Family::Live);
    }
    if has(&["edit", "radio"]) {
        out.push(Family::Edit);
    }
    if has(&["extended", "long version", "club mix"]) {
        out.push(Family::Extended);
    }
    if has(&["instrumental", "acapella", "beats"]) {
        out.push(Family::Instrumental);
    }
    if has(&["dub", "dubstrumental"]) {
        out.push(Family::Dub);
    }
    if has(&[
        "dj mix",
        "dj kicks",
        "mixed",
        "continuous",
        "fabriclive",
        "minimix",
    ]) {
        out.push(Family::DjMix);
    }
    if has(&["demo", "rough", "sketch"]) {
        out.push(Family::Demo);
    }
    if has(&["acoustic", "piano version", "stripped"]) {
        out.push(Family::Acoustic);
    }
    out
}

/// Word-boundary substring test, so "dub" does not match inside "dublin".
fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        let start = from + at;
        let end = start + needle.len();
        let before_ok = start == 0
            || !haystack[..start]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        let after_ok = end >= haystack.len()
            || !haystack[end..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= haystack.len() {
            break;
        }
    }
    false
}

#[derive(Debug, Clone)]
struct VersionTag {
    text: String,
    families: Vec<Family>,
    /// Tokens naming the remixer, e.g. ["mano","le","tough"].
    attribution: Vec<String>,
}

fn classify_tag(raw: &str) -> Option<VersionTag> {
    let text = fold(raw).replace(['-', '_'], " ").trim().to_string();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || NON_VERSION.contains(&text.as_str()) {
        return None;
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let mut families = family_of(&text);

    // A named mix is a remix: "(Kirlian Mix)". Fallback only, so "(Extended
    // Mix)" stays `Extended` and "(Original Mix)" stays neutral.
    if families.is_empty()
        && text.ends_with("mix")
        && !NEUTRAL.iter().any(|n| contains_word(&text, n))
    {
        families.push(Family::Remix);
    }
    if families.is_empty() {
        return None;
    }

    let version_words = [
        "remix",
        "rmx",
        "remixed",
        "rework",
        "reshape",
        "retouch",
        "bootleg",
        "refix",
        "flip",
        "mashup",
        "vip",
        "live",
        "edit",
        "radio",
        "extended",
        "instrumental",
        "acapella",
        "dub",
        "mix",
        "mixed",
        "continuous",
        "demo",
        "acoustic",
        "version",
        "by",
    ];
    let attribution = tokens(&text)
        .into_iter()
        .filter(|t| !version_words.contains(&t.as_str()) && !NEUTRAL.contains(&t.as_str()))
        .collect();

    Some(VersionTag {
        text,
        families,
        attribution,
    })
}

#[derive(Debug, Clone, Default)]
struct VersionProfile {
    tags: Vec<VersionTag>,
    /// More than one distinct family: never gold-label a mismatch on these.
    stacked: bool,
}

/// Pull version tags out of a title or filename leaf.
///
/// Bracketed groups are the reliable signal. A trailing " - suffix" is only a
/// tag when it is short and bracket-free, otherwise "Antena - Camino Del Sol
/// (Todd Terje remix)" would yield the whole song title as its own tag.
fn extract_versions(input: &str) -> VersionProfile {
    let folded = fold(input);
    let mut tags = Vec::new();

    let chars: Vec<char> = folded.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        if open == '(' || open == '[' {
            let close = if open == '(' { ')' } else { ']' };
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == close) {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                if inner.chars().count() <= 80 {
                    if let Some(tag) = classify_tag(&inner) {
                        tags.push(tag);
                    }
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }

    // Spotify stacks annotations on the dash tail: "A Bit More - Valentín
    // Huedo Remix - Edit". The length cap keeps a song title from being read
    // as a tag; it is generous enough for "Allein - Augen Zu Und Durch...
    // Remix By Sascha Funke", which is eight tokens and genuinely a tag.
    let segments: Vec<&str> = folded.split(" - ").collect();
    for segment in segments.iter().skip(1) {
        if segment.contains('(') || segment.contains('[') {
            continue;
        }
        if tokens(segment).len() > 10 {
            continue;
        }
        if let Some(tag) = classify_tag(segment) {
            tags.push(tag);
        }
    }

    let mut families: Vec<Family> = tags.iter().flat_map(|t| t.families.clone()).collect();
    families.sort();
    families.dedup();
    VersionProfile {
        tags,
        stacked: families.len() > 1,
    }
}

fn signature(tags: &[VersionTag]) -> Vec<Family> {
    let mut families: Vec<Family> = tags.iter().flat_map(|t| t.families.clone()).collect();
    families.sort();
    families.dedup();
    families
}

fn attributions(tags: &[VersionTag]) -> Vec<String> {
    tags.iter().flat_map(|t| t.attribution.clone()).collect()
}

/// Two attributions agree when either is empty (unnamed remix — do not guess)
/// or one is a subset of the other.
fn attributions_agree(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    a.iter().all(|t| b.contains(t)) || b.iter().all(|t| a.contains(t))
}

pub struct VersionComparison {
    pub compatible: bool,
    /// False when the verdict is too shaky to gold-label.
    pub confident: bool,
}

fn evaluate_versions(query: &[VersionTag], file: &[VersionTag]) -> bool {
    let q = signature(query);
    let f = signature(file);
    if q != f {
        return false;
    }
    if q.contains(&Family::Remix) {
        return attributions_agree(&attributions(query), &attributions(file));
    }
    true
}

/// Is the candidate the same recording that was asked for?
///
/// Three passes. The plain comparison first; the two rescues only ever fire on
/// an apparent mismatch, so they cannot turn a genuine match into one.
///
///  1. plain — "(Mano Le Tough Remix)" vs "- Mano Le Tough Remix" agrees here.
///  2. drop query-side "- mixed" — Spotify lists tracks from continuous-mix
///     albums as "Klinsmann - mixed", which does not mean the user wants a
///     beatmatched file.
///  3. veto file tags the query names — "Observer Effect - DJ-Kicks" (album
///     "Observer Effect (DJ-Kicks)") asks for exactly the file tagged
///     "(DJ-Kicks)", so that tag is requested, not a mismatch.
fn compare_versions(
    query: &VersionProfile,
    file: &VersionProfile,
    query_text: &str,
) -> VersionComparison {
    if evaluate_versions(&query.tags, &file.tags) {
        return VersionComparison {
            compatible: true,
            confident: true,
        };
    }

    let query_has_djmix = query
        .tags
        .iter()
        .any(|t| t.families.contains(&Family::DjMix));
    let file_has_djmix = file
        .tags
        .iter()
        .any(|t| t.families.contains(&Family::DjMix));

    if query_has_djmix && !file_has_djmix {
        let without: Vec<VersionTag> = query
            .tags
            .iter()
            .filter(|t| !(t.families.len() == 1 && t.families[0] == Family::DjMix))
            .cloned()
            .collect();
        if evaluate_versions(&without, &file.tags) {
            return VersionComparison {
                compatible: true,
                confident: true,
            };
        }
    }

    let query_tokens = tokens(query_text);
    let kept: Vec<VersionTag> = file
        .tags
        .iter()
        .filter(|t| {
            let tag_tokens = tokens(&t.text);
            tag_tokens.is_empty() || !contains_token_run(&query_tokens, &tag_tokens)
        })
        .cloned()
        .collect();
    if kept.len() != file.tags.len() && evaluate_versions(&query.tags, &kept) {
        return VersionComparison {
            compatible: true,
            confident: true,
        };
    }

    // A query asking for a mixed pressing when the file is the plain track is
    // a judgement call, not a clear mismatch.
    let asked_mixed_got_plain =
        query_has_djmix && !file_has_djmix && signature(&file.tags).is_empty();

    VersionComparison {
        compatible: false,
        confident: !query.stacked && !asked_mixed_got_plain,
    }
}

/// Strip the version annotation off a Spotify track name: "MZ - Mano Le Tough
/// Remix" -> "MZ".
fn base_title(track: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = track.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        if open == '(' || open == '[' {
            let close = if open == '(' { ')' } else { ']' };
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == close) {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                if classify_tag(&inner).is_some() {
                    i += end + 2;
                    continue;
                }
            }
        }
        out.push(open);
        i += 1;
    }

    let mut segments: Vec<&str> = out.split(" - ").collect();
    while segments.len() > 1 {
        let tail = segments[segments.len() - 1];
        if tokens(tail).len() <= 10 && classify_tag(tail).is_some() {
            segments.pop();
        } else {
            break;
        }
    }
    segments
        .join(" - ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// -------------------------------------------------------------------- artists

/// Folder names that never identify an artist. Peer share roots and genre
/// directories dominate these paths.
const GENERIC_FOLDER: &[&str] = &[
    "music",
    "musica",
    "musique",
    "share",
    "shared",
    "soulseek",
    "slsk",
    "nicotine",
    "complete",
    "completed",
    "download",
    "downloads",
    "audio",
    "va",
    "various artists",
    "unorganized tracks",
    "full albums",
    "unsorted",
    "temporal",
    "mp3",
    "mp3s",
    "flac",
    "wav",
    "opus",
    "aiff",
    "web",
    "rips",
    "media",
    "library",
    "beatport",
    "itunes music",
    "data",
    "contents",
    "files",
    "singles",
    "album",
    "albums",
    "electronic",
    "house",
    "techno",
    "hip hop",
    "jazz",
    "blues",
    "punk",
    "edm",
    "dance",
    "incoming",
    "tunes",
    "clean",
];

/// Aliases seen in the corpus. `KH` is Four Tet; `Polygon Window` is Aphex
/// Twin. This list can never be complete, which is why an artist appearing
/// here downgrades any `wrong_artist` verdict rather than risking a confident
/// error.
const ALIAS_GROUPS: &[&[&str]] = &[
    &["four tet", "kh", "kieran hebden", "percussions"],
    &["aphex twin", "polygon window", "afx", "richard d james"],
    &["joy orbison", "joy o"],
    &["mano le tough"],
    &["robag wruhme", "wighnomy brothers"],
];

fn is_alias_risky(artist: &str) -> bool {
    let folded = fold(artist);
    if folded.contains(',')
        || folded.contains('&')
        || contains_word(&folded, "feat")
        || contains_word(&folded, "ft")
        || contains_word(&folded, "vs")
        || contains_word(&folded, "and")
    {
        return true;
    }
    ALIAS_GROUPS
        .iter()
        .any(|group| group.contains(&folded.trim()))
}

fn aliases_of(artist: &str) -> Vec<String> {
    let folded = fold(artist).trim().to_string();
    for group in ALIAS_GROUPS {
        if group.contains(&folded.as_str()) {
            return group.iter().map(|s| s.to_string()).collect();
        }
    }
    vec![folded]
}

/// Split a collaboration credit: "Sascha Funke & Mano Le Tough" -> two names.
fn split_credits(artist: &str) -> Vec<String> {
    artist
        .split(|c| c == ',' || c == '&')
        .flat_map(|part| part.split(" feat ").collect::<Vec<_>>())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn usable_artist_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.starts_with("@@") || trimmed.len() < 2 {
        return false;
    }
    if trimmed.chars().filter(|c| c.is_alphabetic()).count() < 2 {
        return false;
    }
    let folded = fold(trimmed);
    if GENERIC_FOLDER.contains(&folded.as_str()) {
        return false;
    }
    // Bare years and disc markers.
    if folded.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtistMatch {
    /// Named in the filename itself.
    Leaf,
    /// Named only in a containing folder.
    Ancestor,
    /// Matched through an alias.
    Alias,
    None,
}

struct ArtistEvidence {
    matched: ArtistMatch,
    /// A path artist that is clearly not the requested one.
    conflicting: Option<String>,
}

/// Plausible artist names from the path structure, generic folders discarded.
fn path_artist_candidates(parts: &PathParts) -> Vec<String> {
    let mut out = Vec::new();

    // "Artist - Title" in the leaf.
    if let Some((left, _)) = parts.leaf.split_once(" - ") {
        out.push(left.to_string());
    }
    // "Artist - Album (Year)" parent folder.
    if let Some((left, _)) = parts.parent.split_once(" - ") {
        out.push(left.to_string());
    } else if parts.ancestors.len() >= 2 {
        // "Artist\Album\NN Title" — grandparent when the parent is a bare album.
        out.push(parts.ancestors[parts.ancestors.len() - 2].clone());
    }

    out.into_iter()
        .map(|s| {
            s.trim_start_matches(|c| c == '[' || c == '(')
                .trim()
                .to_string()
        })
        .filter(|s| usable_artist_name(s))
        .collect()
}

fn artist_names_match(query_artist: &str, other: &str) -> bool {
    let other_tokens = tokens(other);
    if other_tokens.is_empty() {
        return false;
    }
    for credit in split_credits(query_artist) {
        for alias in aliases_of(&credit) {
            let alias_tokens = tokens(&alias);
            if !alias_tokens.is_empty() && contains_all_tokens(&other_tokens, &alias_tokens) {
                return true;
            }
        }
    }
    false
}

fn analyze_artist(query_artist: &str, parts: &PathParts) -> ArtistEvidence {
    let leaf_tokens = tokens(&parts.leaf);
    let path_tokens = tokens(&parts.ancestors.join(" "));
    let credits = split_credits(query_artist);
    let alias_forms: Vec<String> = credits.iter().flat_map(|c| aliases_of(c)).collect();

    for alias in &alias_forms {
        let alias_tokens = tokens(alias);
        if !alias_tokens.is_empty() && contains_all_tokens(&leaf_tokens, &alias_tokens) {
            let direct = credits.iter().any(|c| fold(c).trim() == alias.trim());
            return ArtistEvidence {
                matched: if direct {
                    ArtistMatch::Leaf
                } else {
                    ArtistMatch::Alias
                },
                conflicting: None,
            };
        }
    }
    for alias in &alias_forms {
        let alias_tokens = tokens(alias);
        if !alias_tokens.is_empty() && contains_all_tokens(&path_tokens, &alias_tokens) {
            return ArtistEvidence {
                matched: ArtistMatch::Ancestor,
                conflicting: None,
            };
        }
    }

    let conflicting = path_artist_candidates(parts)
        .into_iter()
        .find(|cand| !artist_names_match(query_artist, cand));
    ArtistEvidence {
        matched: ArtistMatch::None,
        conflicting,
    }
}

// ---------------------------------------------------------------------- rules

const PLACEHOLDERS: &[&str] = &["untitled", "unknown", "audio track", "track", "pista"];

/// Candidate titles inside a filename leaf.
///
/// Filenames are inconsistent about ordering ("Artist - Title", "Title -
/// Artist", "Label - NN - Artist - Title"), so every dash-delimited segment is
/// a hypothesis and the rules ask whether *any* of them matches.
fn leaf_title_hypotheses(leaf: &str) -> Vec<String> {
    let cleaned = strip_featured(&base_title(leaf));
    let mut out: Vec<String> = cleaned
        .split(" - ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    out.push(cleaned);
    out.dedup();
    out
}

/// Run the rule engine over one (query, candidate) pair.
pub fn classify(artist: &str, track: &str, album: &str, filename: &str) -> RuleVerdict {
    let parts = split_path(filename);

    let query_title = strip_featured(&base_title(track));
    let query_title_tokens = tokens(&query_title);
    let title_is_short = is_short_title(&query_title_tokens);

    // Surplus words are counted only after removing everything that
    // legitimately appears beside a title — the artist, the album, scene noise.
    // Without this, "Laurent Garnier-The Sound Of The Big Babou.flac" scores
    // two surplus words (the artist's own name, because the dash has no
    // surrounding spaces to split on) and an obvious `exact` abstains.
    let mut ignorable: Vec<String> = query_title_tokens.clone();
    ignorable.extend(tokens(artist));
    ignorable.extend(tokens(album));

    let mut title_match = false;
    let mut file_extra: Vec<String> = Vec::new();

    if query_title_tokens.is_empty() {
        let target = condense(&query_title);
        title_match = !target.is_empty()
            && leaf_title_hypotheses(&parts.leaf)
                .iter()
                .any(|h| condense(h) == target);
    } else {
        for hypothesis in leaf_title_hypotheses(&parts.leaf) {
            let hypothesis_tokens = tokens(&hypothesis);
            if !contains_token_run(&hypothesis_tokens, &query_title_tokens) {
                continue;
            }
            let extra: Vec<String> = hypothesis_tokens
                .into_iter()
                .filter(|t| !ignorable.contains(t) && !is_noise_token(t))
                .collect();
            if !title_match || extra.len() < file_extra.len() {
                title_match = true;
                file_extra = extra;
            }
        }
    }

    let leaf_tokens = tokens(&parts.leaf);
    let overlap = token_overlap(&leaf_tokens, &query_title_tokens);
    let missing_rare = query_title_tokens
        .iter()
        .any(|t| !leaf_tokens.contains(t) && t.chars().count() >= 4);

    let artist_evidence = analyze_artist(artist, &parts);
    let artist_known = artist_evidence.matched != ArtistMatch::None;

    let query_text = format!("{track} {album}");
    let version = compare_versions(
        &extract_versions(track),
        &extract_versions(&parts.leaf),
        &query_text,
    );

    // Does the candidate sit in a folder named after the requested release? On
    // compilations the folder is credited to the compiler while the tracks are
    // by other artists, so a differing folder artist is not evidence of a
    // wrong artist.
    let album_tokens = tokens(&base_title(album));
    let album_distinctive = album_tokens.len() >= 2 && album_tokens != query_title_tokens;
    let album_in_path =
        album_distinctive && contains_token_run(&tokens(&parts.ancestors.join(" ")), &album_tokens);

    // R2 — the filename carries no identifying information.
    let leaf_lower = fold(parts.leaf.trim());
    if parts.leaf.trim().is_empty() || PLACEHOLDERS.iter().any(|p| leaf_lower.starts_with(p)) {
        return RuleVerdict::gold(Label::Different, "R2_placeholder_leaf");
    }

    // R3 — right song, wrong recording. The unrequested-remix failure.
    if title_match && artist_known && !version.compatible && version.confident {
        return RuleVerdict::gold(Label::WrongVersion, "R3_version_mismatch");
    }

    // R4 — the requested recording. The surplus budget depends on where the
    // artist was found: named in the filename, surplus words are almost always
    // collaborator credits; implied by a folder, any surplus could mean a
    // different song.
    let extra_budget = if artist_evidence.matched == ArtistMatch::Ancestor {
        1
    } else {
        3
    };
    if title_match && artist_known && version.compatible && file_extra.len() <= extra_budget {
        if title_is_short && artist_evidence.matched == ArtistMatch::Ancestor {
            return RuleVerdict::weak(Label::Exact, "W1_exact_short_title_folder_artist");
        }
        return RuleVerdict::gold(Label::Exact, "R4_exact");
    }

    // R4b — right title, on the requested release, no artist evidence.
    if title_match && !artist_known && album_in_path && version.compatible && file_extra.len() <= 1
    {
        return RuleVerdict::gold(Label::Exact, "R4b_exact_via_album");
    }

    // R5 — the requested title shares no word with the filename. Gold even
    // when the artist matches: a different track by the right artist is still
    // `different`. This is the album/title collision the model gets wrong.
    if overlap == 0.0 && !query_title_tokens.is_empty() {
        return RuleVerdict::gold(Label::Different, "R5_no_title_overlap");
    }

    // R6 — a meaningful word of the requested title is missing and there is no
    // artist evidence. "Dangerous Drive" vs Morgan Wallen's "Dangerous".
    if !title_match && !artist_known && missing_rare {
        return RuleVerdict::gold(Label::Different, "R6_missing_title_token");
    }

    // R7 — the title is present but the file names a longer, different song.
    if title_match && !artist_known && file_extra.len() >= 2 {
        return RuleVerdict::gold(Label::Different, "R7_title_superset");
    }

    // R7b — one surplus title word is enough to change the song: "Yes" vs
    // "Say Yes", "Without Pause" vs "Rebel Without A Pause". Settled by audit:
    // of the rows with exactly one surplus word, the reviewer called 21 of 21
    // `different`.
    if title_match && !artist_known && !file_extra.is_empty() {
        return RuleVerdict::gold(Label::Different, "R7b_title_plus_extra_word");
    }

    // Demoted from gold after an audit found it wrong on both sampled rows:
    // a DJ-mix folder credited to the mixer reads as a competing artist.
    if title_match
        && !artist_known
        && file_extra.is_empty()
        && artist_evidence.conflicting.is_some()
        && !title_is_short
        && !album_in_path
        && !is_alias_risky(artist)
    {
        return RuleVerdict::weak(Label::WrongArtist, "W3_wrong_artist_unverified");
    }

    if !title_match && !artist_known && overlap < 0.5 {
        return RuleVerdict::weak(Label::Different, "W2_low_title_overlap");
    }

    RuleVerdict::abstain("A_defer_to_model")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(artist: &str, track: &str, album: &str, filename: &str) -> (Option<Label>, Tier) {
        let v = classify(artist, track, album, filename);
        (v.label, v.tier)
    }

    #[test]
    fn unrequested_remix_is_the_wrong_version() {
        // The old judge scored this 1.0.
        let (label, tier) = label(
            "Armand Van Helden",
            "I Want Your Soul",
            "I Want Your Soul",
            "iTunes Music\\Armand Van Helden\\___\\I Want Your Soul (Crookers Remix).mp3",
        );
        assert_eq!(label, Some(Label::WrongVersion));
        assert_eq!(tier, Tier::Gold);
    }

    #[test]
    fn plain_file_is_exact() {
        let (label, tier) = label(
            "Jürgen Paape",
            "Whisper Echo",
            "Speicher 100",
            "nicotine\\Unorganized Tracks\\Jürgen Paape - Whisper Echo.flac",
        );
        assert_eq!(label, Some(Label::Exact));
        assert_eq!(tier, Tier::Gold);
    }

    #[test]
    fn original_mix_is_still_the_original() {
        let (label, _) = label(
            "Jürgen Paape",
            "Whisper Echo",
            "Speicher 100",
            "Deep House\\075.Jürgen Paape - Whisper Echo (Original Mix).mp3",
        );
        assert_eq!(label, Some(Label::Exact));
    }

    #[test]
    fn requested_remix_with_matching_remixer_is_exact() {
        let (label, _) = label(
            "Sascha Funke",
            "MZ - Mano Le Tough Remix",
            "MZ (Remixes)",
            "complete1\\Sascha Funke, Mano Le Tough - MZ (Mano Le Tough Remix).flac",
        );
        assert_eq!(label, Some(Label::Exact));
    }

    #[test]
    fn a_different_track_in_a_folder_named_after_the_request() {
        // The model alone gets this wrong: it sees the requested title in the
        // path and calls it exact.
        let (label, tier) = label(
            "Laurent Garnier",
            "The Sound of the Big Babou",
            "Unreasonable Behaviour",
            "@@x\\Laurent_Garnier_-_The_Sound_Of_The_Big_Babou_(F_111CD)\\02-laurent_garnier--sore_fingers-cmc_int.mp3",
        );
        assert_eq!(label, Some(Label::Different));
        assert_eq!(tier, Tier::Gold);
    }

    #[test]
    fn album_titled_like_the_track_does_not_make_everything_exact() {
        // Asking for "Unreasonable Behaviour" must not accept a different
        // track from that album.
        let (label, _) = label(
            "Laurent Garnier",
            "Unreasonable Behaviour",
            "Unreasonable Behaviour",
            "Electronic\\Various_unreasonable behaviour\\VA - 07 - Laurent Garnier - The Sound Of The Big Babou.mp3",
        );
        assert_ne!(label, Some(Label::Exact));
    }

    #[test]
    fn one_surplus_word_means_a_different_song() {
        for (artist, track, filename) in [
            (
                "Midland",
                "Without Pause",
                "0760 - Public Enemy - Rebel Without A Pause.mp3",
            ),
            (
                "Robag Wruhme",
                "Yes",
                "nicotine\\Kromestar - 01 - Say Yes.mp3",
            ),
        ] {
            let (label, tier) = label(artist, track, track, filename);
            assert_eq!(label, Some(Label::Different), "{filename}");
            assert_eq!(tier, Tier::Gold);
        }
    }

    #[test]
    fn initialism_titles_still_match() {
        // "K.T.B." tokenises to nothing; condensed equality rescues it.
        let (label, _) = label(
            "Robag Wruhme",
            "K.T.B.",
            "Wuzzelbud KK",
            "Music Library\\Robag Wruhme\\Wuzzelbud KK\\07 K.T.B..flac",
        );
        assert_eq!(label, Some(Label::Exact));
    }

    #[test]
    fn requested_dj_kicks_is_exact_not_a_wrong_version() {
        let (label, _) = label(
            "Disclosure",
            "Observer Effect - DJ-Kicks",
            "Observer Effect (DJ-Kicks)",
            "shared\\Disclosure\\0109 - Disclosure - Observer Effect (DJ-Kicks).flac",
        );
        assert_eq!(label, Some(Label::Exact));
    }

    #[test]
    fn wrong_artist_is_never_gold() {
        // Demoted after an audit found it wrong on both sampled rows.
        let v = classify(
            "Juan Campodónico",
            "Presentimiento",
            "Presentimiento",
            "c:\\0\\severino\\elizeth cardoso - se balancean en sucanta\\03 - Presentimiento.mp3",
        );
        assert_ne!(v.tier, Tier::Gold);
    }

    #[test]
    fn a_subtitle_is_not_a_version_tag() {
        assert!(
            extract_versions("Yes (Love Theme From Lost River)")
                .tags
                .is_empty()
        );
        assert!(extract_versions("Greed (Pt. 1+2)").tags.is_empty());
        assert!(extract_versions("Too Slow (All Night)").tags.is_empty());
    }

    #[test]
    fn word_boundaries_are_respected() {
        assert!(contains_word("acid dub", "dub"));
        assert!(!contains_word("dublin nights", "dub"));
    }
}
