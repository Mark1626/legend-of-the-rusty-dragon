//! What a turn produced.
//!
//! The reference pushes strings straight at IRC through `network.broadcast()`
//! and `network.reply()`, splicing in raw control codes. Neither survives the
//! move to HTTP: there is no socket to push down, and a web client wants to
//! style its own output rather than parse `\x0304`.
//!
//! So the `Network` trait is gone. A turn instead *returns* structured lines:
//! `feed` is the append-only channel log every client polls, and `reply` is the
//! private answer that becomes the HTTP response body.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Game lifecycle: version banners, ascensions, players entering or leaving.
    System,
    /// The Bulletin Board: quests appearing, changing and being removed.
    BBoard,
    /// Absalom's store.
    Store,
    /// Random world events.
    Event,
    /// The outcome of somebody accepting a quest.
    Quest,
}

impl Kind {
    /// The bracketed prefix the IRC build printed in front of each category.
    pub fn header(self) -> Option<&'static str> {
        match self {
            Kind::System => None,
            Kind::BBoard => Some("BBOARD"),
            Kind::Store => Some("STORE"),
            Kind::Event => Some("EVENT"),
            Kind::Quest => Some("QUEST"),
        }
    }

    /// The accent colour the IRC build used for this category's header.
    pub fn color(self) -> Color {
        match self {
            Kind::System => Color::Default,
            Kind::BBoard => Color::Magenta,
            Kind::Store => Color::Green,
            Kind::Event => Color::Yellow,
            Kind::Quest => Color::Red,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    #[default]
    Default,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
}

/// A run of text sharing one style.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_default_color")]
    pub color: Color,
}

fn is_default_color(c: &Color) -> bool {
    *c == Color::Default
}

impl Span {
    fn same_style_as(&self, other: &Span) -> bool {
        self.bold == other.bold && self.color == other.color
    }
}

/// One rendered message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub kind: Kind,
    /// Every player named in this line, so a client can filter its own history
    /// and the feed table can index on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<String>,
    pub spans: Vec<Span>,
    /// When this happened, in unix seconds.
    ///
    /// Left unset by the builders and filled in by [`Out::stamp`], so that a
    /// catch-up replay can date each tick's output to the moment it would have
    /// occurred rather than to when someone finally asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

impl Line {
    pub fn new(kind: Kind) -> Self {
        Self { kind, actors: Vec::new(), spans: Vec::new(), at: None }
    }

    pub fn system() -> Self {
        Self::new(Kind::System)
    }
    pub fn bboard() -> Self {
        Self::new(Kind::BBoard)
    }
    pub fn store() -> Self {
        Self::new(Kind::Store)
    }
    pub fn event() -> Self {
        Self::new(Kind::Event)
    }
    pub fn quest() -> Self {
        Self::new(Kind::Quest)
    }

    fn push(mut self, span: Span) -> Self {
        if span.text.is_empty() {
            return self;
        }
        match self.spans.last_mut() {
            Some(last) if last.same_style_as(&span) => last.text.push_str(&span.text),
            _ => self.spans.push(span),
        }
        self
    }

    pub fn text(self, text: impl std::fmt::Display) -> Self {
        self.push(Span { text: text.to_string(), bold: false, color: Color::Default })
    }

    pub fn bold(self, text: impl std::fmt::Display) -> Self {
        self.push(Span { text: text.to_string(), bold: true, color: Color::Default })
    }

    pub fn colored(self, color: Color, text: impl std::fmt::Display) -> Self {
        self.push(Span { text: text.to_string(), bold: false, color })
    }

    pub fn accent(self, color: Color, text: impl std::fmt::Display) -> Self {
        self.push(Span { text: text.to_string(), bold: true, color })
    }

    /// A player's name: rendered bold, and recorded so the line can be filtered
    /// by who it concerns.
    pub fn nick(mut self, nick: &str) -> Self {
        if !self.actors.iter().any(|a| a == nick) {
            self.actors.push(nick.to_string());
        }
        self.bold(nick)
    }

    /// Everything the line says, with styling dropped.
    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// The result of one turn: what the world saw, and what the caller is told.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Out {
    /// Broadcast to everyone; appended to the feed log.
    pub feed: Vec<Line>,
    /// Private to whoever issued the command; becomes the HTTP response.
    pub reply: Vec<Line>,
    /// Rare out-of-band notices (new game, ascension) for external channels.
    pub announce: Vec<String>,
}

impl Out {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn broadcast(&mut self, line: Line) {
        if !line.is_empty() {
            self.feed.push(line);
        }
    }

    pub fn reply(&mut self, line: Line) {
        if !line.is_empty() {
            self.reply.push(line);
        }
    }

    pub fn announce(&mut self, msg: impl Into<String>) {
        self.announce.push(msg.into());
    }

    /// Date everything added since the last call. Lines already carrying a
    /// time keep it, so replayed ticks are not overwritten by the wall clock.
    pub fn stamp(&mut self, at: i64) {
        for line in self.feed.iter_mut().chain(self.reply.iter_mut()) {
            line.at.get_or_insert(at);
        }
    }

    pub fn absorb(&mut self, other: Out) {
        self.feed.extend(other.feed);
        self.reply.extend(other.reply);
        self.announce.extend(other.announce);
    }

    /// Plain-text transcript of the broadcast channel, for tests and logs.
    pub fn transcript(&self) -> Vec<String> {
        self.feed.iter().map(|l| l.plain()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_line_the_way_the_reference_phrased_it() {
        let line = Line::event()
            .text(" ")
            .nick("Absalom")
            .text(" has gained ")
            .text(42)
            .text(" XP!");
        assert_eq!(line.plain(), " Absalom has gained 42 XP!");
        assert_eq!(line.actors, vec!["Absalom"]);
        assert_eq!(line.kind, Kind::Event);
    }

    #[test]
    fn adjacent_spans_of_one_style_are_merged() {
        let line = Line::system().text("a").text("b").bold("C").bold("D").text("e");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].text, "ab");
        assert_eq!(line.spans[1].text, "CD");
        assert!(line.spans[1].bold);
        assert_eq!(line.spans[2].text, "e");
    }

    #[test]
    fn empty_fragments_never_create_spans() {
        let line = Line::system().text("").text("x").text("");
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.plain(), "x");
    }

    #[test]
    fn every_named_player_is_recorded_once() {
        let line = Line::event()
            .nick("Ann")
            .text(" beat ")
            .nick("Bob")
            .text(" and ")
            .nick("Ann");
        assert_eq!(line.actors, vec!["Ann", "Bob"]);
        assert_eq!(line.plain(), "Ann beat Bob and Ann");
    }

    #[test]
    fn categories_keep_their_irc_header_and_colour() {
        assert_eq!(Kind::BBoard.header(), Some("BBOARD"));
        assert_eq!(Kind::BBoard.color(), Color::Magenta);
        assert_eq!(Kind::Store.color(), Color::Green);
        assert_eq!(Kind::Event.color(), Color::Yellow);
        assert_eq!(Kind::Quest.color(), Color::Red);
        assert_eq!(Kind::System.header(), None);
    }

    #[test]
    fn blank_lines_are_never_emitted() {
        let mut out = Out::new();
        out.broadcast(Line::system());
        out.reply(Line::system());
        assert!(out.feed.is_empty());
        assert!(out.reply.is_empty());
    }

    #[test]
    fn feed_and_reply_stay_separate_channels() {
        let mut out = Out::new();
        out.broadcast(Line::quest().text("public"));
        out.reply(Line::system().text("private"));
        out.announce("ascension");
        assert_eq!(out.transcript(), vec!["public"]);
        assert_eq!(out.reply.len(), 1);
        assert_eq!(out.announce, vec!["ascension"]);
    }

    #[test]
    fn absorb_concatenates_all_three_channels() {
        let mut a = Out::new();
        a.broadcast(Line::system().text("one"));
        let mut b = Out::new();
        b.broadcast(Line::system().text("two"));
        b.reply(Line::system().text("hi"));
        a.absorb(b);
        assert_eq!(a.transcript(), vec!["one", "two"]);
        assert_eq!(a.reply.len(), 1);
    }

    #[test]
    fn stamping_dates_only_the_undated() {
        let mut out = Out::new();
        out.broadcast(Line::system().text("earlier"));
        out.stamp(100);
        out.broadcast(Line::system().text("later"));
        out.reply(Line::system().text("private"));
        out.stamp(200);

        assert_eq!(out.feed[0].at, Some(100), "an earlier tick keeps its time");
        assert_eq!(out.feed[1].at, Some(200));
        assert_eq!(out.reply[0].at, Some(200), "replies are dated too");
    }

    #[test]
    fn styling_defaults_are_omitted_from_the_wire_format() {
        let json = serde_json::to_string(&Line::system().text("plain")).unwrap();
        assert!(!json.contains("bold"), "{json}");
        assert!(!json.contains("color"), "{json}");
        assert!(!json.contains("actors"), "{json}");
    }
}
