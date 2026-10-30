use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::knihovny::SITE;
use crate::marc::Marc;

/// The spec's schema. Fields after the blank line are proposed extensions that a client which
/// does not know them ignores.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookMetadata {
    pub media_type: String,
    pub provider_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub narrators: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub published_year: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cover: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn13: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isbn10: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Series>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language_code: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pages: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration: i64,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// The code the site's cover is found by: the edition's ISBN, else its barcode (EAN).
    #[serde(skip)]
    pub cover_code: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub series: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sequence: String,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// A record shaped as an answer, with what ranking needs alongside: every ISBN the edition
/// lists, the library it comes from, and the title without its subtitle.
#[derive(Debug)]
pub struct Candidate {
    pub book: BookMetadata,
    pub isbns: Vec<String>,
}

const MAX_GENRES: usize = 10;
const MIN_PUBLISHED_YEAR: i64 = 1000;
const MAX_PUBLISHED_YEAR: i64 = 2200;
/// Genre headings that name a format rather than a genre.
const FORMAT_HEADINGS: &[&str] = &[
    "audioknihy",
    "kompaktní disky",
    "zvukové knihy",
    "e-knihy",
    "elektronické knihy",
    "mluvené slovo",
    "zvukové záznamy",
    "audiokazety",
];
/// MARC language codes, as BookOrbit's codes.
const LANGUAGES: &[(&str, &str)] = &[
    ("cze", "cs"),
    ("slo", "sk"),
    ("eng", "en"),
    ("ger", "de"),
    ("pol", "pl"),
    ("fre", "fr"),
    ("spa", "es"),
    ("ita", "it"),
    ("rus", "ru"),
    ("hun", "hu"),
];
/// Relator codes for the people who read an audiobook.
const NARRATOR_ROLES: &[&str] = &["nrt", "stl"];

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid pattern")
}

static LIFE_DATES: LazyLock<Regex> = LazyLock::new(|| regex(r",?\s*\d{3,4}-(\d{3,4})?\.?\s*$"));
static TRAILING_PUNCTUATION: LazyLock<Regex> = LazyLock::new(|| regex(r"[\s/:;=,.]+$"));
static PAGES: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(\d+)\s*(?:stran|stránek|strana|stránky|s\.|listů)"));
static YEAR: LazyLock<Regex> = LazyLock::new(|| regex(r"\d{4}"));
static HOURS_MINUTES: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(\d+)\s*hod\.?(?:\s*,?\s*(\d+)\s*min)?"));
static CLOCK: LazyLock<Regex> = LazyLock::new(|| regex(r"^(\d{1,3}):(\d{2}):(\d{2})$"));
static SERIES_NAME_NOISE: LazyLock<Regex> = LazyLock::new(|| regex(r"\s*\([^)]*\)\s*$"));
static READ_BY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)^\s*(?:čte|čtou|načetl[ai]?|interpret(?:i)?:?|účinkuj[eí])\s+"));

/// Every record of an answer as a candidate, in the catalogue's order. A record without a title
/// or an id is left out.
pub fn candidates(records: &[Value], searched_isbn: Option<&str>) -> Vec<Candidate> {
    records
        .iter()
        .filter_map(|record| map_record(record, searched_isbn))
        .collect()
}

pub fn map_record(record: &Value, searched_isbn: Option<&str>) -> Option<Candidate> {
    let id = record["id"].as_str().unwrap_or_default().to_string();
    let marc = Marc::parse(record["fullRecord"].as_str().unwrap_or_default());
    let (title, subtitle, mut series) =
        title_parts(&marc, record["title"].as_str().unwrap_or_default());
    if id.is_empty() || title.is_empty() {
        return None;
    }
    if series.is_none() {
        series = series_from_490(&marc, &title);
    }
    let isbns = isbns(record, &marc);
    let cover_codes = codes(record, &marc);
    let (isbn10, isbn13) = choose_isbn(&isbns, searched_isbn);
    let formats = strings(&record["formats"]).join(" ");
    let genres = genres(&marc);
    let audiobook = formats.contains("AUDIO");
    let comic = genres.iter().any(|genre| fold(genre).starts_with("komiks"));
    let book = BookMetadata {
        media_type: if audiobook {
            "audiobook"
        } else if comic {
            "comic"
        } else {
            "book"
        }
        .into(),
        provider_id: id.clone(),
        title,
        subtitle,
        authors: authors(record, &marc),
        narrators: if audiobook { narrators(&marc) } else { vec![] },
        publisher: publisher(record, &marc),
        published_year: year(record, &marc)
            .map(|year| year.to_string())
            .unwrap_or_default(),
        description: description(record, &marc),
        isbn13,
        isbn10,
        genres,
        series: series
            .map(|(series, sequence)| vec![Series { series, sequence }])
            .unwrap_or_default(),
        language_code: language(&marc),
        pages: if audiobook { 0 } else { pages(&marc) },
        duration: if audiobook {
            duration(record, &marc)
        } else {
            0
        },
        url: record_url(&id),
        ..BookMetadata::default()
    };
    let mut book = book;
    book.cover_code = [&book.isbn13, &book.isbn10]
        .into_iter()
        .find(|isbn| !isbn.is_empty())
        .cloned()
        .or_else(|| cover_codes.first().cloned())
        .unwrap_or_default();
    Some(Candidate { book, isbns })
}

/// The public address of a record.
pub fn record_url(id: &str) -> String {
    format!("{SITE}/Record/{}", id.replace('*', "%2A"))
}

/// The title, its subtitle, and a series it names. A title in parts ("Zaklínač. I., Poslední
/// přání": `$a` the series, `$n` its number, `$p` the part) is the part, in a series. Otherwise
/// `$a` is the title and `$b` the subtitle.
fn title_parts(marc: &Marc, display: &str) -> (String, String, Option<(String, String)>) {
    let Some(field) = marc.fields("245").next() else {
        return (clean(display), String::new(), None);
    };
    let main = clean(field.first('a').unwrap_or_default());
    let subtitle = clean(field.first('b').unwrap_or_default());
    match (field.first('n'), field.first('p')) {
        (number, Some(part)) => (
            clean(part),
            subtitle,
            Some((main, number.map(sequence).unwrap_or_default())),
        ),
        (Some(number), None) => (main.clone(), subtitle, Some((main, sequence(number)))),
        (None, None) if !main.is_empty() => (main, subtitle, None),
        (None, None) => (clean(display), String::new(), None),
    }
}

/// A series from 490 ("Zaklínač (SK) ; 1") when its name begins the title. Publishers' editions
/// ("Leonardo fantasy ; 8") are also written there and are not a story's series.
fn series_from_490(marc: &Marc, title: &str) -> Option<(String, String)> {
    let folded_title = fold(title);
    marc.fields("490").find_map(|field| {
        let name = clean(&SERIES_NAME_NOISE.replace(&clean(field.first('a')?), ""));
        let number = field.first('v').map(sequence).unwrap_or_default();
        (!name.is_empty() && !number.is_empty() && folded_title.starts_with(&fold(&name)))
            .then_some((name, number))
    })
}

/// A part number as digits: "I." is "1", "12," is "12".
fn sequence(raw: &str) -> String {
    let raw = clean(raw)
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_uppercase();
    if raw.chars().all(|c| c.is_ascii_digit()) {
        return raw;
    }
    let value = |c: char| match c {
        'I' => 1,
        'V' => 5,
        'X' => 10,
        'L' => 50,
        'C' => 100,
        'D' => 500,
        'M' => 1000,
        _ => 0,
    };
    if !raw.chars().all(|c| value(c) > 0) {
        return raw;
    }
    let (mut total, mut highest) = (0, 0);
    for c in raw.chars().rev() {
        let v = value(c);
        if v < highest {
            total -= v;
        } else {
            total += v;
            highest = v;
        }
    }
    total.to_string()
}

fn authors(record: &Value, marc: &Marc) -> Vec<String> {
    let listed: Vec<String> = record["authors"]["primary"]
        .as_object()
        .map(|names| names.keys().map(|name| clean_name(name)).collect())
        .unwrap_or_default();
    if !listed.is_empty() {
        return unique(listed);
    }
    marc.fields("100")
        .filter_map(|field| field.first('a'))
        .map(inverted_name)
        .collect()
}

/// The readers of an audiobook: people with a narrator's role, else the performer note
/// ("Čte Vladimír Kudla ; hrají ...").
fn narrators(marc: &Marc) -> Vec<String> {
    let by_role: Vec<String> = marc
        .fields("700")
        .filter(|field| field.all('4').any(|role| NARRATOR_ROLES.contains(&role)))
        .filter_map(|field| field.first('a'))
        .map(inverted_name)
        .collect();
    if !by_role.is_empty() {
        return unique(by_role);
    }
    let note = marc.first("511", 'a').unwrap_or_default();
    let first = note.split(';').next().unwrap_or_default();
    if !READ_BY.is_match(first) {
        return vec![];
    }
    unique(
        READ_BY
            .replace(first, "")
            .split([',', '&'])
            .flat_map(|part| part.split(" a "))
            .map(clean)
            .filter(|name| !name.is_empty())
            .collect(),
    )
}

fn publisher(record: &Value, marc: &Marc) -> String {
    strings(&record["publishers"])
        .into_iter()
        .map(|name| clean(&name))
        .find(|name| !name.is_empty())
        .or_else(|| {
            marc.first("264", 'b')
                .or_else(|| marc.first("260", 'b'))
                .map(clean)
        })
        .unwrap_or_default()
}

fn year(record: &Value, marc: &Marc) -> Option<i64> {
    strings(&record["publicationDates"])
        .iter()
        .find_map(|date| YEAR.find(date).and_then(|y| y.as_str().parse().ok()))
        .or_else(|| marc.control("008").and_then(|f| f.get(7..11)?.parse().ok()))
        .filter(|year| (MIN_PUBLISHED_YEAR..=MAX_PUBLISHED_YEAR).contains(year))
}

fn description(record: &Value, marc: &Marc) -> String {
    let summary = strings(&record["summary"]).join("\n\n");
    let summary = if summary.trim().is_empty() {
        marc.fields("520")
            .filter_map(|field| field.first('a'))
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        summary
    };
    summary
        .split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The record's language from its fixed-length data (008/35-37), else its language codes (041).
fn language(marc: &Marc) -> String {
    let code = marc
        .control("008")
        .and_then(|field| field.get(35..38))
        .filter(|code| code.chars().all(|c| c.is_ascii_lowercase()))
        .or_else(|| marc.first("041", 'a'))
        .unwrap_or_default();
    LANGUAGES
        .iter()
        .find(|(marc_code, _)| *marc_code == code)
        .map(|(_, ours)| ours.to_string())
        .unwrap_or_default()
}

fn pages(marc: &Marc) -> i64 {
    marc.fields("300")
        .filter_map(|field| field.first('a'))
        .find_map(|extent| PAGES.captures(extent).and_then(|c| c[1].parse().ok()))
        .filter(|pages| *pages > 0)
        .unwrap_or(0)
}

/// Running time in seconds: the catalogue's playing time (306, "123100" is 12:31:00), else the
/// extent's "12 hod., 31 min.".
fn duration(record: &Value, marc: &Marc) -> i64 {
    marc.first("306", 'a')
        .and_then(clock_seconds)
        .or_else(|| {
            strings(&record["playingTimes"])
                .iter()
                .find_map(|t| clock_seconds(t))
        })
        .or_else(|| {
            marc.fields("300")
                .filter_map(|field| field.first('a'))
                .find_map(|extent| {
                    let c = HOURS_MINUTES.captures(extent)?;
                    let minutes = c
                        .get(2)
                        .and_then(|m| m.as_str().parse::<i64>().ok())
                        .unwrap_or(0);
                    Some(c[1].parse::<i64>().ok()? * 3600 + minutes * 60)
                })
        })
        .filter(|seconds| *seconds > 0)
        .unwrap_or(0)
}

/// "12:31:00", or "123100" as the catalogue's playing time writes it, in seconds.
fn clock_seconds(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (h, m, s) = match CLOCK.captures(raw) {
        Some(c) => (c[1].to_string(), c[2].to_string(), c[3].to_string()),
        None if raw.len() == 6 && raw.chars().all(|c| c.is_ascii_digit()) => (
            raw[0..2].to_string(),
            raw[2..4].to_string(),
            raw[4..6].to_string(),
        ),
        None => return None,
    };
    Some(h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60 + s.parse::<i64>().ok()?)
}

/// The Czech genre headings (655 from the national authority file), else its topical headings
/// (650), without headings that name a format.
fn genres(marc: &Marc) -> Vec<String> {
    let czech = |tag: &str| -> Vec<String> {
        marc.fields(tag)
            .filter(|field| field.first('2') == Some("czenas"))
            .filter_map(|field| field.first('a'))
            .map(clean)
            .filter(|heading| {
                !FORMAT_HEADINGS
                    .iter()
                    .any(|format| fold(format) == fold(heading))
            })
            .collect()
    };
    let genres = czech("655");
    let genres = if genres.is_empty() {
        czech("650")
    } else {
        genres
    };
    unique(genres).into_iter().take(MAX_GENRES).collect()
}

/// Every ISBN the edition lists, without hyphens, in order.
fn isbns(record: &Value, marc: &Marc) -> Vec<String> {
    codes(record, marc)
        .into_iter()
        .filter(|code| is_isbn(code))
        .collect()
}

/// Every ISBN and barcode the record lists (020, 024), without hyphens, in order. An audiobook
/// often has only a barcode, which the site's covers are found by too.
fn codes(record: &Value, marc: &Marc) -> Vec<String> {
    let listed = strings(&record["isbns"]);
    let from_marc = ["020", "024"]
        .into_iter()
        .flat_map(|tag| marc.fields(tag).filter_map(|field| field.first('a')))
        .map(str::to_string);
    unique(
        listed
            .into_iter()
            .chain(from_marc)
            .map(|raw| isbn_digits(&raw))
            .filter(|code| matches!(code.len(), 10 | 13))
            .collect(),
    )
}

/// An ISBN-10, or an ISBN-13, which always begins 978 or 979; other 13-digit codes are barcodes.
fn is_isbn(code: &str) -> bool {
    match code.len() {
        13 => {
            (code.starts_with("978") || code.starts_with("979"))
                && code.bytes().all(|b| b.is_ascii_digit())
        }
        10 => code[..9].bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

/// The searched ISBN when the edition lists it, else the edition's first; as (`isbn10`, `isbn13`).
fn choose_isbn(isbns: &[String], searched: Option<&str>) -> (String, String) {
    let chosen = searched
        .and_then(|wanted| isbns.iter().find(|isbn| same_isbn(isbn, wanted)))
        .or(isbns.first());
    match chosen {
        Some(isbn) if isbn.len() == 13 => (String::new(), isbn.clone()),
        Some(isbn) => (isbn.clone(), String::new()),
        None => (String::new(), String::new()),
    }
}

/// The digits of an ISBN as a catalogue writes it ("978-80-7477-079-1 (váz.)").
pub fn isbn_digits(raw: &str) -> String {
    let head = raw
        .split(['(', ' ', ':'])
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    head.chars()
        .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
        .collect::<String>()
        .to_uppercase()
}

/// Whether two ISBNs name the same book: an ISBN-10 and the ISBN-13 made from it are the same.
pub fn same_isbn(a: &str, b: &str) -> bool {
    match (as_isbn13(&isbn_digits(a)), as_isbn13(&isbn_digits(b))) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn as_isbn13(isbn: &str) -> Option<String> {
    match isbn.len() {
        13 if isbn.bytes().all(|b| b.is_ascii_digit()) => Some(isbn.to_string()),
        10 if isbn[..9].bytes().all(|b| b.is_ascii_digit()) => {
            let body = format!("978{}", &isbn[..9]);
            let sum: u32 = body
                .bytes()
                .enumerate()
                .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 0 { 1 } else { 3 })
                .sum();
            Some(format!("{body}{}", (10 - sum % 10) % 10))
        }
        _ => None,
    }
}

/// An ISBN of zeros, which badly tagged files carry for "no ISBN".
pub fn is_placeholder_isbn(isbn: &str) -> bool {
    let body = match isbn.len() {
        13 => &isbn[3..12],
        _ => &isbn[..isbn.len().min(9)],
    };
    !body.is_empty() && body.bytes().all(|b| b == b'0')
}

/// How closely the best of a search's results matches what was asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Closeness {
    None,
    /// A book by the author asked for, under another title.
    Author,
    /// A book with the title asked for.
    Title,
}

/// A candidate's rank, compared highest first: author and title, medium, a Czech or Slovak
/// edition, a description, the National Library's own record, an ISBN.
type Score = (u8, bool, bool, bool, bool, bool);

struct Wanted {
    title: String,
    author: HashSet<String>,
    audiobook: bool,
}

impl Wanted {
    fn new(title: &str, author: &str, audiobook: bool) -> Self {
        Self {
            title: main_title(title),
            author: fold(author)
                .split(|c: char| !c.is_alphanumeric())
                .filter(|word| word.chars().count() > 1)
                .map(str::to_string)
                .collect(),
            audiobook,
        }
    }

    fn by_author(&self, book: &BookMetadata) -> bool {
        !self.author.is_empty()
            && book.authors.iter().any(|name| {
                fold(name)
                    .split(|c: char| !c.is_alphanumeric())
                    .rfind(|word| !word.is_empty())
                    .is_some_and(|surname| self.author.contains(surname))
            })
    }

    fn same_title(&self, book: &BookMetadata) -> bool {
        !self.title.is_empty() && main_title(&book.title) == self.title
    }

    fn closeness(&self, book: &BookMetadata) -> Closeness {
        if self.same_title(book) {
            Closeness::Title
        } else if self.by_author(book) {
            Closeness::Author
        } else {
            Closeness::None
        }
    }

    /// Higher is better: the author and the title asked for, the medium asked for, the
    /// National Library's own record, an edition with an ISBN.
    fn score(&self, candidate: &Candidate) -> Score {
        let book = &candidate.book;
        (
            u8::from(self.by_author(book)) * 2 + u8::from(self.same_title(book)),
            (book.media_type == "audiobook") == self.audiobook,
            // The catalogue holds translations too: "Krakatit" brings the German editions,
            // which the National Library catalogues as well.
            matches!(book.language_code.as_str(), "cs" | "sk" | ""),
            !book.description.is_empty(),
            book.provider_id.starts_with("nkp."),
            !candidate.isbns.is_empty(),
        )
    }
}

pub fn closeness(candidates: &[Candidate], title: &str, author: &str) -> Closeness {
    let wanted = Wanted::new(title, author, false);
    candidates
        .iter()
        .map(|candidate| wanted.closeness(&candidate.book))
        .max()
        .unwrap_or(Closeness::None)
}

/// The candidates best first, one per edition. The catalogue unites many libraries, so one
/// edition arrives once per library that holds it; the best-placed copy of it stays.
pub fn rank(
    candidates: Vec<Candidate>,
    title: &str,
    author: &str,
    audiobook: bool,
) -> Vec<Candidate> {
    let wanted = Wanted::new(title, author, audiobook);
    let mut scored: Vec<(Score, usize, Candidate)> = candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| (wanted.score(&candidate), index, candidate))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut seen = HashSet::new();
    scored
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .filter(|candidate| seen.insert(edition_key(candidate)))
        .collect()
}

fn edition_key(candidate: &Candidate) -> String {
    match candidate.isbns.first().and_then(|isbn| as_isbn13(isbn)) {
        Some(isbn) => isbn,
        // Without an ISBN, an edition is its title, authors, year, medium and language: a
        // translation of the same title and year is another edition.
        None => format!(
            "{}|{}|{}|{}|{}",
            fold(&candidate.book.title),
            fold(&candidate.book.authors.join(" ")),
            candidate.book.published_year,
            candidate.book.media_type,
            candidate.book.language_code
        ),
    }
}

/// A title without its subtitle or alternative title: "Hobit, aneb, Cesta tam a zase zpátky"
/// and "Sapiens : úchvatný i úděsný příběh lidstva" compare as "hobit" and "sapiens".
fn main_title(title: &str) -> String {
    let head = title.split(" : ").next().unwrap_or(title);
    let folded = fold(head);
    match folded.find(" aneb ") {
        Some(at) => folded[..at].to_string(),
        None => folded,
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn unique(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        if !value.is_empty() && !out.contains(&value) {
            out.push(value);
        }
    }
    out
}

fn clean(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    TRAILING_PUNCTUATION
        .replace(&collapsed, "")
        .trim()
        .to_string()
}

/// "Andrzej Sapkowski, 1948-" as "Andrzej Sapkowski".
fn clean_name(name: &str) -> String {
    clean(&LIFE_DATES.replace(name.trim(), ""))
}

/// "Kudla, Vladimír," as "Vladimír Kudla".
fn inverted_name(name: &str) -> String {
    let name = clean_name(name);
    match name.split_once(", ") {
        Some((last, first)) if !first.is_empty() => format!("{} {}", clean(first), clean(last)),
        _ => name,
    }
}

/// Lowercase without diacritics or punctuation, for comparing names and titles.
pub fn fold(value: &str) -> String {
    let lowered: String = value
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ą' | 'ã' | 'å' => 'a',
            'č' | 'ć' | 'ç' => 'c',
            'ď' => 'd',
            'é' | 'ě' | 'è' | 'ê' | 'ë' | 'ę' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ľ' | 'ĺ' | 'ł' => 'l',
            'ň' | 'ń' | 'ñ' => 'n',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ő' => 'o',
            'ř' | 'ŕ' => 'r',
            'š' | 'ś' => 's',
            'ť' => 't',
            'ú' | 'ů' | 'ù' | 'û' | 'ü' | 'ű' => 'u',
            'ý' | 'ÿ' => 'y',
            'ž' | 'ź' | 'ż' => 'z',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    lowered.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first record of a synthetic API answer in `testdata/`; the books in them are invented.
    fn fixture(json: &str) -> Value {
        let answer: Value = serde_json::from_str(json).unwrap();
        answer["records"][0].clone()
    }

    fn book(json: &str) -> BookMetadata {
        map_record(&fixture(json), None).unwrap().book
    }

    #[test]
    fn a_print_edition() {
        let b = book(include_str!("../testdata/record-print.json"));
        assert_eq!(b.provider_id, "nkp.NKC01-900000001");
        assert_eq!(b.media_type, "book");
        // The title's parts: the series, its number, then the part.
        assert_eq!(b.title, "Tichý přístav");
        assert_eq!(
            b.series,
            [Series {
                series: "Kronika mlhy".into(),
                sequence: "1".into()
            }]
        );
        assert_eq!(b.authors, ["Jana Testová"]);
        assert_eq!(b.publisher, "Vzorové nakladatelství");
        assert_eq!(b.published_year, "2001");
        assert_eq!(b.language_code, "cs");
        assert_eq!(b.pages, 312);
        assert_eq!(
            (b.isbn10.as_str(), b.isbn13.as_str()),
            ("", "9788090000025")
        );
        assert_eq!(b.cover_code, "9788090000025");
        // Format headings and English headings are left out.
        assert_eq!(b.genres, ["české povídky", "fantasy povídky"]);
        assert!(b
            .description
            .starts_with("Sbírka povídek z přístavního města"));
        assert_eq!(b.url, "https://www.knihovny.cz/Record/nkp.NKC01-900000001");
        assert!(b.narrators.is_empty());
        assert_eq!(b.duration, 0);
    }

    #[test]
    fn a_publishers_edition_is_not_a_story_series() {
        // 490 "Vzorová edice ; 3" names the publisher's edition, not the book's series.
        let b = book(include_str!("../testdata/record-print.json"));
        assert!(b.series.iter().all(|s| s.series != "Vzorová edice"));
    }

    #[test]
    fn an_audiobook_with_narrators_by_role() {
        let b = book(include_str!("../testdata/record-audio.json"));
        assert_eq!(b.media_type, "audiobook");
        assert_eq!(b.narrators, ["Bohdan Vzorový"]);
        assert_eq!(b.duration, 9 * 3600 + 41 * 60);
        assert_eq!(b.pages, 0);
        // A barcode is not an ISBN, but the cover is found by it.
        assert_eq!((b.isbn10.as_str(), b.isbn13.as_str()), ("", ""));
        assert_eq!(b.cover_code, "8590000000005");
        assert_eq!(b.genres, ["české povídky"]);
        assert_eq!(b.url, "https://www.knihovny.cz/Record/kjm.KjmUsCat%2A900");
    }

    #[test]
    fn an_audiobook_with_narrators_in_the_performer_note() {
        let b = book(include_str!("../testdata/record-audio-note.json"));
        assert_eq!(b.narrators, ["Cyril Ukázkový", "Dana Pokusná"]);
        assert_eq!(b.duration, 3 * 3600 + 5 * 60);
    }

    #[test]
    fn a_slovak_edition_with_its_series_in_490() {
        let b = book(include_str!("../testdata/record-slovak.json"));
        assert_eq!(b.language_code, "sk");
        assert_eq!(
            b.series,
            [Series {
                series: "Kronika hmly".into(),
                sequence: "1".into()
            }]
        );
    }

    #[test]
    fn an_edition_with_three_isbns_reports_the_searched_one() {
        let record = fixture(include_str!("../testdata/record-three-isbns.json"));
        let first = map_record(&record, None).unwrap();
        assert_eq!(first.isbns.len(), 3);
        assert_eq!(first.book.isbn13, "9788090000025");
        for searched in ["9788090000032", "978-80-900010-0-8"] {
            let b = map_record(&record, Some(searched)).unwrap().book;
            assert_eq!(b.isbn13, isbn_digits(searched), "{searched}");
        }
    }

    #[test]
    fn a_subtitle_is_split_off() {
        let b = book(include_str!("../testdata/record-subtitle.json"));
        assert_eq!(
            (b.title.as_str(), b.subtitle.as_str()),
            ("Vzorník", "úvod do všeho")
        );
    }

    #[test]
    fn comics_are_told_by_their_genre() {
        assert_eq!(
            book(include_str!("../testdata/record-comic.json")).media_type,
            "comic"
        );
    }

    #[test]
    fn a_record_without_a_title_is_no_book() {
        assert!(map_record(
            &fixture(include_str!("../testdata/record-no-title.json")),
            None
        )
        .is_none());
    }

    #[test]
    fn ranking_folds_copies_and_puts_the_author_first() {
        let answer: Value =
            serde_json::from_str(include_str!("../testdata/search-tichy-pristav.json")).unwrap();
        let all = candidates(answer["records"].as_array().unwrap(), None);
        assert_eq!(all.len(), 5);
        let ids: Vec<String> = rank(all, "Tichý přístav", "Jana Testová", false)
            .into_iter()
            .map(|c| c.book.provider_id)
            .collect();
        // The National Library's copy of the edition stays, the municipal copy is folded into it;
        // the print edition before the audiobook; another author's book of the same name last.
        assert_eq!(ids[0], "nkp.NKC01-900000001");
        assert!(!ids.contains(&"mkvzor.12345".to_string()), "{ids:?}");
        assert_eq!(ids.last().unwrap(), "mkjiny.777");
        let audio_at = ids.iter().position(|id| id == "kjm.KjmUsCat*900").unwrap();
        let ids_audio: Vec<String> = rank(
            candidates(answer["records"].as_array().unwrap(), None),
            "Tichý přístav",
            "Jana Testová",
            true,
        )
        .into_iter()
        .map(|c| c.book.provider_id)
        .collect();
        assert!(audio_at > 0);
        assert_eq!(
            ids_audio[0], "kjm.KjmUsCat*900",
            "an audiobook search: {ids_audio:?}"
        );
    }

    #[test]
    fn a_czech_edition_with_a_description_beats_a_national_library_translation() {
        // Seen live: "Krakatit" by Čapek brought the National Library's German editions first.
        let record = |id: &str, year: u16, lang: &str, summary: &str| {
            serde_json::json!({
                "id": id, "title": "Vzorník", "authors": {"primary": {"Jana Testová": []}},
                "summary": if summary.is_empty() { vec![] } else { vec![summary] },
                "fullRecord": format!(
                    r#"<record><controlfield tag="008">220523s{year}    xr a   g      000 j {lang}  </controlfield><datafield tag="245" ind1="1" ind2="0"><subfield code="a">Vzorník</subfield></datafield></record>"#
                ),
            })
        };
        let records = [
            record("nkp.NKC01-1", 2001, "ger", ""),
            record("mkvzor.2", 2005, "cze", ""),
            record("mkvzor.3", 2001, "cze", "Popis knihy."),
        ];
        let ids: Vec<String> = rank(candidates(&records, None), "Vzorník", "Jana Testová", false)
            .into_iter()
            .map(|c| c.book.provider_id)
            .collect();
        assert_eq!(ids, ["mkvzor.3", "mkvzor.2", "nkp.NKC01-1"]);
    }

    #[test]
    fn closeness_of_a_search() {
        let answer: Value =
            serde_json::from_str(include_str!("../testdata/search-tichy-pristav.json")).unwrap();
        let all = candidates(answer["records"].as_array().unwrap(), None);
        assert_eq!(closeness(&all, "Tichý přístav", ""), Closeness::Title);
        assert_eq!(
            closeness(&all, "Druhý břeh", "Jana Testová"),
            Closeness::Author
        );
        assert_eq!(
            closeness(&all, "Druhý břeh", "Nikdo Další"),
            Closeness::None
        );
    }

    #[test]
    fn main_titles_drop_subtitles_and_alternative_titles() {
        for (title, want) in [
            ("Hobit, aneb, Cesta tam a zase zpátky", "hobit"),
            ("Sapiens : úchvatný i úděsný příběh lidstva", "sapiens"),
            ("Tichý přístav", "tichy pristav"),
        ] {
            assert_eq!(main_title(title), want, "{title}");
        }
    }

    #[test]
    fn part_numbers() {
        for (raw, want) in [
            ("I.,", "1"),
            ("IV", "4"),
            ("XII.", "12"),
            ("3", "3"),
            ("12,", "12"),
        ] {
            assert_eq!(sequence(raw), want, "{raw}");
        }
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("Andrzej Sapkowski, 1948-"), "Andrzej Sapkowski");
        assert_eq!(
            clean_name("J. R. R. Tolkien, 1892-1973"),
            "J. R. R. Tolkien"
        );
        assert_eq!(
            inverted_name("Kudla, Vladimír, 1947-2021"),
            "Vladimír Kudla"
        );
        assert_eq!(inverted_name("Madonna"), "Madonna");
    }

    #[test]
    fn isbns_and_barcodes() {
        assert!(is_isbn("9788090000025") && is_isbn("8085951142") && is_isbn("080442957X"));
        assert!(!is_isbn("8590000000005"), "a barcode");
        assert_eq!(isbn_digits("978-80-7477-079-1 (váz.)"), "9788074770791");
        assert!(same_isbn("0261102214", "978-0-261-10221-7"));
        assert!(!same_isbn("8085951142", "9788074770791"));
        assert!(is_placeholder_isbn("0000000000") && is_placeholder_isbn("9780000000002"));
        assert!(!is_placeholder_isbn("9788090000025"));
    }

    #[test]
    fn durations() {
        for (raw, want) in [
            ("094100", Some(34860)),
            ("12:31:00", Some(45060)),
            ("x", None),
        ] {
            assert_eq!(clock_seconds(raw), want, "{raw}");
        }
    }

    #[test]
    fn folding() {
        assert_eq!(fold("  Žluťoučký   KŮŇ, úpěl! "), "zlutoucky kun upel");
    }
}
