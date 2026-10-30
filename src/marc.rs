//! The MARC 21 record the API returns as `fullRecord`, in MARCXML. Only what the provider reads:
//! control fields by tag, and data fields with their subfields in order.

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Default)]
pub struct Marc {
    control: Vec<(String, String)>,
    data: Vec<Field>,
}

#[derive(Debug)]
pub struct Field {
    pub tag: String,
    pub subfields: Vec<(char, String)>,
}

static CONTROL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<(?:\w+:)?controlfield[^>]*\btag="(\d{3})"[^>]*>(.*?)</(?:\w+:)?controlfield>"#,
    )
    .expect("valid pattern")
});
static DATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<(?:\w+:)?datafield[^>]*\btag="(\d{3})"[^>]*>(.*?)</(?:\w+:)?datafield>"#)
        .expect("valid pattern")
});
static SUBFIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<(?:\w+:)?subfield[^>]*\bcode="(.)"[^>]*>(.*?)</(?:\w+:)?subfield>"#)
        .expect("valid pattern")
});

impl Marc {
    pub fn parse(xml: &str) -> Self {
        Self {
            control: CONTROL
                .captures_iter(xml)
                .map(|c| (c[1].to_string(), unescape(&c[2])))
                .collect(),
            data: DATA
                .captures_iter(xml)
                .map(|c| Field {
                    tag: c[1].to_string(),
                    subfields: SUBFIELD
                        .captures_iter(&c[2])
                        .filter_map(|s| Some((s[1].chars().next()?, unescape(&s[2]))))
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn control(&self, tag: &str) -> Option<&str> {
        self.control
            .iter()
            .find(|(t, _)| t == tag)
            .map(|(_, value)| value.as_str())
    }

    pub fn fields<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a Field> + 'a {
        self.data.iter().filter(move |field| field.tag == tag)
    }

    pub fn first(&self, tag: &str, code: char) -> Option<&str> {
        self.data
            .iter()
            .filter(|field| field.tag == tag)
            .find_map(|field| field.first(code))
    }
}

impl Field {
    pub fn first(&self, code: char) -> Option<&str> {
        self.subfields
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, value)| value.as_str())
    }

    pub fn all(&self, code: char) -> impl Iterator<Item = &str> {
        self.subfields
            .iter()
            .filter(move |(c, _)| *c == code)
            .map(|(_, value)| value.as_str())
    }
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<collection xmlns="http://www.loc.gov/MARC21/slim"><record>
  <leader>02441nam a2200493 i 4500</leader>
  <controlfield tag="008">220523s2022    xr a   g      000 j cze  </controlfield>
  <datafield tag="245" ind1="1" ind2="0">
    <subfield code="a">Vzor.</subfield><subfield code="n">II.,</subfield>
    <subfield code="p">Druhý díl &amp; dodatky /</subfield>
  </datafield>
  <datafield tag="700" ind1="1" ind2=" "><subfield code="a">Nový, Jan,</subfield><subfield code="4">trl</subfield></datafield>
  <datafield tag="700" ind1="1" ind2=" "><subfield code="a">Malá, Eva</subfield><subfield code="4">nrt</subfield></datafield>
</record></collection>"#;

    #[test]
    fn reads_control_and_data_fields() {
        let marc = Marc::parse(XML);
        assert_eq!(&marc.control("008").unwrap()[35..38], "cze");
        assert_eq!(marc.first("245", 'a'), Some("Vzor."));
        assert_eq!(marc.first("245", 'p'), Some("Druhý díl & dodatky /"));
        let roles: Vec<(&str, &str)> = marc
            .fields("700")
            .map(|f| (f.first('a').unwrap(), f.first('4').unwrap()))
            .collect();
        assert_eq!(roles, [("Nový, Jan,", "trl"), ("Malá, Eva", "nrt")]);
        assert_eq!(marc.control("001"), None);
    }

    #[test]
    fn reads_prefixed_marcxml() {
        let xml = r#"<marc:record xmlns:marc="http://www.loc.gov/MARC21/slim"><marc:datafield tag="020" ind1=" " ind2=" "><marc:subfield code="a">978-80-900000-2-5</marc:subfield></marc:datafield></marc:record>"#;
        assert_eq!(
            Marc::parse(xml).first("020", 'a'),
            Some("978-80-900000-2-5")
        );
    }

    #[test]
    fn nothing_from_nothing() {
        let marc = Marc::parse("");
        assert!(marc.control("008").is_none());
        assert_eq!(marc.fields("245").count(), 0);
    }
}
