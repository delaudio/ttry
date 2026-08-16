use regex::Regex;
use unicode_width::UnicodeWidthStr;

use crate::{Error, Result, Screen};

#[derive(Clone, Debug)]
pub enum TextMatcher {
    Literal { text: String, exact: bool },
    Regex(Regex),
}

impl TextMatcher {
    pub fn literal(text: impl Into<String>) -> Self {
        Self::Literal {
            text: text.into(),
            exact: false,
        }
    }
    pub fn exact(text: impl Into<String>) -> Self {
        Self::Literal {
            text: text.into(),
            exact: true,
        }
    }
    pub fn regex(pattern: &str) -> std::result::Result<Self, regex::Error> {
        Regex::new(pattern).map(Self::Regex)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundingBox {
    pub col: u16,
    pub row: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug)]
pub struct Locator {
    screen: Screen,
    matcher: TextMatcher,
}

impl Screen {
    pub fn get_by_text(&self, text: impl Into<String>) -> Locator {
        Locator::new(self.clone(), TextMatcher::literal(text))
    }
    pub fn get_by_exact_text(&self, text: impl Into<String>) -> Locator {
        Locator::new(self.clone(), TextMatcher::exact(text))
    }
    pub fn get_by_regex(&self, pattern: &str) -> std::result::Result<Locator, regex::Error> {
        Ok(Locator::new(self.clone(), TextMatcher::regex(pattern)?))
    }
}

impl Locator {
    pub fn new(screen: Screen, matcher: TextMatcher) -> Self {
        Self { screen, matcher }
    }
    pub fn screen(&self) -> &Screen {
        &self.screen
    }
    pub fn count(&self) -> usize {
        self.matches().len()
    }
    pub fn is_visible(&self) -> bool {
        self.count() > 0
    }
    pub fn text(&self) -> Result<String> {
        let matches = self.matches();
        if matches.len() != 1 {
            return Err(strict_error(&matches));
        }
        Ok(matches[0].0.clone())
    }
    pub fn bounding_box(&self) -> Result<BoundingBox> {
        let matches = self.matches();
        if matches.len() != 1 {
            return Err(strict_error(&matches));
        }
        Ok(matches[0].1)
    }
    pub(crate) fn describe(&self) -> String {
        format!("{:?}", self.matcher)
    }

    fn matches(&self) -> Vec<(String, BoundingBox)> {
        if matches!(
            &self.matcher,
            TextMatcher::Literal { text, .. } if text.is_empty()
        ) {
            return Vec::new();
        }
        let lines = self.screen.lines(false);
        let mut found = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            match &self.matcher {
                TextMatcher::Literal { text, exact: true } => {
                    if line == text {
                        found.push((
                            line.clone(),
                            BoundingBox {
                                col: 0,
                                row: row as u16,
                                width: UnicodeWidthStr::width(line.as_str()) as u16,
                                height: 1,
                            },
                        ));
                    }
                }
                TextMatcher::Literal { text, exact: false } => {
                    for (byte, matched) in line.match_indices(text) {
                        let col = UnicodeWidthStr::width(&line[..byte]) as u16;
                        found.push((
                            matched.into(),
                            BoundingBox {
                                col,
                                row: row as u16,
                                width: UnicodeWidthStr::width(matched) as u16,
                                height: 1,
                            },
                        ));
                    }
                }
                TextMatcher::Regex(regex) => {
                    for matched in regex.find_iter(line) {
                        if matched.is_empty() {
                            continue;
                        }
                        let col = UnicodeWidthStr::width(&line[..matched.start()]) as u16;
                        found.push((
                            matched.as_str().into(),
                            BoundingBox {
                                col,
                                row: row as u16,
                                width: UnicodeWidthStr::width(matched.as_str()) as u16,
                                height: 1,
                            },
                        ));
                    }
                }
            }
        }
        found
    }
}

fn strict_error(matches: &[(String, BoundingBox)]) -> Error {
    let locations = matches
        .iter()
        .map(|(_, bounds)| format!("({}, {})", bounds.col, bounds.row))
        .collect::<Vec<_>>()
        .join(", ");
    Error::StrictLocator {
        count: matches.len(),
        locations,
    }
}

#[cfg(test)]
mod tests {
    use crate::Terminal;
    #[test]
    fn locator_is_dynamic_and_strict() {
        let mut terminal = Terminal::new(20, 2).unwrap();
        let locator = terminal.screen().get_by_text("ready");
        assert!(!locator.is_visible());
        terminal.advance(b"ready ready");
        assert_eq!(locator.count(), 2);
        assert!(locator.text().is_err());
    }

    #[test]
    fn empty_literal_does_not_match_every_cell_boundary() {
        let mut terminal = crate::Terminal::new(5, 1).unwrap();
        terminal.advance(b"text");
        assert_eq!(terminal.screen().get_by_text("").count(), 0);
        assert_eq!(terminal.screen().get_by_exact_text("").count(), 0);
    }

    #[test]
    fn zero_width_regex_matches_are_not_visible() {
        let mut terminal = crate::Terminal::new(5, 1).unwrap();
        terminal.advance(b"text");
        assert_eq!(terminal.screen().get_by_regex("^").unwrap().count(), 0);
        assert_eq!(terminal.screen().get_by_regex(r"\b").unwrap().count(), 0);
    }
    #[test]
    fn regex_bounds_use_display_columns() {
        let mut terminal = Terminal::new(20, 1).unwrap();
        terminal.advance("界test".as_bytes());
        let bounds = terminal
            .screen()
            .get_by_regex("test")
            .unwrap()
            .bounding_box()
            .unwrap();
        assert_eq!((bounds.col, bounds.width), (2, 4));
    }
}
