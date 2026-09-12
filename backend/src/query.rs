//! The topic query language: a real parser to an AST, and an evaluator over
//! tokenized text.
//!
//! A saved topic is the thing that decides what wakes you up. That makes it the one
//! piece of this app a user is entitled to reason about precisely, which is why it is
//! a grammar rather than a substring match:
//!
//! - **`AND` / `OR` / `NOT`, phrases, and field scoping** mean a topic can say what it
//!   actually means. `tariff NOT title:sport` is a thought; `tariff` is a wish.
//! - **Token matching, not substring matching.** `ai` must not fire on "said",
//!   "campaign" or "mainstream". A substring matcher makes short topics useless and
//!   users learn to pad them with spaces, which then breaks on punctuation.
//! - **A parse error is returned with its column and REFUSES THE SAVE.** A watch that
//!   quietly matches nothing is strictly worse than one that will not save: the user
//!   believes they are covered and finds out by missing the story.
//!
//! # Grammar
//!
//! ```text
//! expr   := or
//! or     := and ( "OR" and )*
//! and    := not ( "AND"? not )*      // adjacency means AND
//! not    := ( "NOT" | "-" )? atom
//! atom   := "(" expr ")" | field ":" ( phrase | term ) | phrase | term
//! phrase := '"' ... '"'
//! field  := title | body | source | author | url
//! ```
//!
//! Bare adjacency meaning `AND` is the choice users expect from every search box they
//! have ever used, and the alternative (adjacency meaning `OR`) makes adding a word to
//! a topic *widen* it, which nobody predicts.
//!
//! # Determinism
//!
//! Parsing and evaluation are pure. The stored AST is the authority — a topic saved
//! under one build evaluates identically under the next, and re-parsing the source
//! text is only ever a convenience.

use serde::{Deserialize, Serialize};

use crate::text::{normalize, query_tokens};

/// Longest query accepted, in bytes. A topic is a line, not a document, and the cap
/// bounds both the parser and the per-article evaluation cost.
pub const MAX_QUERY_LEN: usize = 2048;

/// Maximum parenthesis nesting. The parser is recursive-descent, so this is what
/// stands between a hostile `((((((…` and a stack overflow — which is not a catchable
/// panic, it aborts the process.
pub const MAX_DEPTH: usize = 32;

// ── Errors ─────────────────────────────────────────────────────────────────────

/// A parse failure, with the column the user should look at.
///
/// `column` is a 1-based CHARACTER offset, not a byte offset: the UI puts a caret
/// under it, and a byte offset points at the wrong place the moment the query
/// contains a non-ASCII character.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryError {
    pub message: String,
    pub column: usize,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (column {})", self.message, self.column)
    }
}

impl std::error::Error for QueryError {}

// ── Fields ─────────────────────────────────────────────────────────────────────

/// Which part of an article an atom is scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// The default: title and body together.
    Any,
    Title,
    Body,
    Source,
    Author,
    Url,
}

impl Field {
    fn parse(name: &str) -> Option<Field> {
        match name {
            "title" => Some(Field::Title),
            "body" => Some(Field::Body),
            "source" => Some(Field::Source),
            "author" => Some(Field::Author),
            "url" => Some(Field::Url),
            _ => None,
        }
    }

    /// The field names, for the error message that lists what IS valid. Sorted so the
    /// message is stable.
    pub const NAMES: &'static [&'static str] = &["author", "body", "source", "title", "url"];
}

// ── AST ────────────────────────────────────────────────────────────────────────

/// One node of a parsed topic query.
///
/// `#[serde(tag = "kind")]` so the stored JSON is readable and diffable — a topic's
/// AST is shown in the UI, and `{"kind":"and","nodes":[…]}` can be understood without
/// this file open next to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Or {
        nodes: Vec<Node>,
    },
    And {
        nodes: Vec<Node>,
    },
    Not {
        node: Box<Node>,
    },
    /// A single word.
    Term {
        field: Field,
        word: String,
    },
    /// A contiguous run of words.
    Phrase {
        field: Field,
        words: Vec<String>,
    },
}

// ── Lexer ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    And,
    Or,
    Not,
    LParen,
    RParen,
    Colon,
    Word(String),
    Quoted(String),
}

#[derive(Debug, Clone)]
struct Spanned {
    tok: Tok,
    /// 1-based character column where this token starts.
    column: usize,
}

fn lex(input: &str) -> Result<Vec<Spanned>, QueryError> {
    let mut out = Vec::new();
    // Character-indexed so every reported column is a character offset.
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        let column = i + 1;
        match ch {
            '(' => {
                out.push(Spanned {
                    tok: Tok::LParen,
                    column,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    tok: Tok::RParen,
                    column,
                });
                i += 1;
            }
            ':' => {
                out.push(Spanned {
                    tok: Tok::Colon,
                    column,
                });
                i += 1;
            }
            '-' => {
                // A leading `-` is negation; a `-` inside a word is part of it
                // ("covid-19"), which the word branch below handles.
                out.push(Spanned {
                    tok: Tok::Not,
                    column,
                });
                i += 1;
            }
            '"' => {
                let mut buf = String::new();
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    buf.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    return Err(QueryError {
                        message: "unterminated quoted phrase — add a closing \"".into(),
                        column,
                    });
                }
                out.push(Spanned {
                    tok: Tok::Quoted(buf),
                    column,
                });
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '(' | ')' | ':' | '"')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let tok = match word.as_str() {
                    "AND" => Tok::And,
                    "OR" => Tok::Or,
                    "NOT" => Tok::Not,
                    _ => Tok::Word(word),
                };
                out.push(Spanned { tok, column });
            }
        }
    }
    Ok(out)
}

// ── Parser ─────────────────────────────────────────────────────────────────────

struct Parser {
    toks: Vec<Spanned>,
    pos: usize,
    /// Character length of the source, for "unexpected end of query" columns.
    end_column: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|s| &s.tok)
    }

    fn column(&self) -> usize {
        self.toks
            .get(self.pos)
            .map_or(self.end_column, |s| s.column)
    }

    fn bump(&mut self) -> Option<Spanned> {
        let out = self.toks.get(self.pos).cloned();
        if out.is_some() {
            self.pos += 1;
        }
        out
    }

    fn err(&self, message: impl Into<String>) -> QueryError {
        QueryError {
            message: message.into(),
            column: self.column(),
        }
    }

    fn parse_or(&mut self, depth: usize) -> Result<Node, QueryError> {
        let mut nodes = vec![self.parse_and(depth)?];
        while matches!(self.peek(), Some(Tok::Or)) {
            self.bump();
            nodes.push(self.parse_and(depth)?);
        }
        Ok(if nodes.len() == 1 {
            nodes.pop().unwrap_or_else(|| unreachable!("len checked"))
        } else {
            Node::Or { nodes }
        })
    }

    fn parse_and(&mut self, depth: usize) -> Result<Node, QueryError> {
        let mut nodes = vec![self.parse_not(depth)?];
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.bump();
                    nodes.push(self.parse_not(depth)?);
                }
                // Adjacency is AND. Anything that can BEGIN an atom continues the
                // conjunction; `OR` and `)` end it.
                Some(Tok::Not | Tok::LParen | Tok::Word(_) | Tok::Quoted(_)) => {
                    nodes.push(self.parse_not(depth)?);
                }
                _ => break,
            }
        }
        Ok(if nodes.len() == 1 {
            nodes.pop().unwrap_or_else(|| unreachable!("len checked"))
        } else {
            Node::And { nodes }
        })
    }

    fn parse_not(&mut self, depth: usize) -> Result<Node, QueryError> {
        if matches!(self.peek(), Some(Tok::Not)) {
            self.bump();
            let node = self.parse_not(depth)?;
            return Ok(Node::Not {
                node: Box::new(node),
            });
        }
        self.parse_atom(depth)
    }

    fn parse_atom(&mut self, depth: usize) -> Result<Node, QueryError> {
        if depth >= MAX_DEPTH {
            return Err(self.err(format!("query nests deeper than {MAX_DEPTH} parentheses")));
        }
        let Some(spanned) = self.bump() else {
            return Err(QueryError {
                message: "query ended where a word was expected".into(),
                column: self.end_column,
            });
        };
        match spanned.tok {
            Tok::LParen => {
                let inner = self.parse_or(depth + 1)?;
                if !matches!(self.peek(), Some(Tok::RParen)) {
                    return Err(self.err("missing closing parenthesis"));
                }
                self.bump();
                Ok(inner)
            }
            Tok::Quoted(text) => Ok(phrase_node(Field::Any, &text, spanned.column)?),
            Tok::Word(word) => {
                // `field:` only when a colon actually follows.
                if matches!(self.peek(), Some(Tok::Colon)) {
                    let Some(field) = Field::parse(&word.to_lowercase()) else {
                        let names = Field::NAMES.join(", ");
                        return Err(QueryError {
                            message: format!("unknown field '{word}' — valid fields are {names}"),
                            column: spanned.column,
                        });
                    };
                    self.bump(); // the colon
                    let Some(next) = self.bump() else {
                        return Err(QueryError {
                            message: format!("'{word}:' has nothing after it"),
                            column: self.end_column,
                        });
                    };
                    return match next.tok {
                        Tok::Quoted(text) => phrase_node(field, &text, next.column),
                        Tok::Word(w) => term_node(field, &w, next.column),
                        _ => Err(QueryError {
                            message: format!("'{word}:' must be followed by a word or a phrase"),
                            column: next.column,
                        }),
                    };
                }
                term_node(Field::Any, &word, spanned.column)
            }
            Tok::RParen => Err(QueryError {
                message: "closing parenthesis with nothing to close".into(),
                column: spanned.column,
            }),
            Tok::Colon => Err(QueryError {
                message: "':' must follow a field name".into(),
                column: spanned.column,
            }),
            Tok::And | Tok::Or => Err(QueryError {
                message: "AND/OR must sit between two terms".into(),
                column: spanned.column,
            }),
            Tok::Not => unreachable!("NOT is consumed by parse_not"),
        }
    }
}

/// Build a `Term`, normalizing the word the same way documents are normalized.
///
/// A word that normalizes to nothing (`"!!!"`) is an error rather than a node that
/// silently never matches — the whole point of refusing the save.
fn term_node(field: Field, raw: &str, column: usize) -> Result<Node, QueryError> {
    let mut tokens = query_tokens(&normalize(raw));
    match tokens.len() {
        0 => Err(QueryError {
            message: format!("'{raw}' has no searchable characters in it"),
            column,
        }),
        // A "word" that normalizes into several (`covid-19` → `covid`, `19`) is a
        // phrase, not a term: matching them independently would fire on any article
        // containing the number 19.
        1 => Ok(Node::Term {
            field,
            word: tokens.remove(0),
        }),
        _ => Ok(Node::Phrase {
            field,
            words: tokens,
        }),
    }
}

fn phrase_node(field: Field, raw: &str, column: usize) -> Result<Node, QueryError> {
    let words = query_tokens(&normalize(raw));
    if words.is_empty() {
        return Err(QueryError {
            message: "empty quoted phrase".into(),
            column,
        });
    }
    if words.len() == 1 {
        let mut words = words;
        return Ok(Node::Term {
            field,
            word: words.remove(0),
        });
    }
    Ok(Node::Phrase { field, words })
}

/// Parse a topic query into its AST.
///
/// # Errors
///
/// Returns the first parse failure with a 1-based character column. Callers must
/// surface it and refuse the save.
pub fn parse(input: &str) -> Result<Node, QueryError> {
    if input.len() > MAX_QUERY_LEN {
        return Err(QueryError {
            message: format!("query is longer than {MAX_QUERY_LEN} bytes"),
            column: MAX_QUERY_LEN,
        });
    }
    let toks = lex(input)?;
    let end_column = input.chars().count() + 1;
    if toks.is_empty() {
        return Err(QueryError {
            message: "query is empty".into(),
            column: 1,
        });
    }
    let mut parser = Parser {
        toks,
        pos: 0,
        end_column,
    };
    let node = parser.parse_or(0)?;
    if parser.pos < parser.toks.len() {
        return Err(parser.err("unexpected trailing input"));
    }
    Ok(node)
}

// ── Evaluation ─────────────────────────────────────────────────────────────────

/// An article reduced to the token lists the evaluator needs.
///
/// Built ONCE per article and reused across every topic, because a node evaluates
/// against tokens and re-tokenizing per topic is the difference between one pass and
/// one pass per saved topic.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub title: Vec<String>,
    pub body: Vec<String>,
    pub source: Vec<String>,
    pub author: Vec<String>,
    pub url: Vec<String>,
}

impl Document {
    /// Tokenize the parts of an article a query can address.
    #[must_use]
    pub fn new(title: &str, body: &str, source: &str, author: &str, url: &str) -> Document {
        Document {
            title: query_tokens(&normalize(title)),
            body: query_tokens(&normalize(body)),
            source: query_tokens(&normalize(source)),
            author: query_tokens(&normalize(author)),
            // The URL is split on its punctuation by `normalize`, so `example.test/ai`
            // becomes the tokens `example`, `test`, `ai` and `url:ai` works.
            url: query_tokens(&normalize(url)),
        }
    }

    fn tokens_for(&self, field: Field) -> Vec<&[String]> {
        match field {
            Field::Any => vec![&self.title, &self.body],
            Field::Title => vec![&self.title],
            Field::Body => vec![&self.body],
            Field::Source => vec![&self.source],
            Field::Author => vec![&self.author],
            Field::Url => vec![&self.url],
        }
    }
}

impl Node {
    /// Whether this node matches `doc`.
    #[must_use]
    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Node::Or { nodes } => nodes.iter().any(|n| n.matches(doc)),
            Node::And { nodes } => nodes.iter().all(|n| n.matches(doc)),
            Node::Not { node } => !node.matches(doc),
            Node::Term { field, word } => doc
                .tokens_for(*field)
                .iter()
                .any(|tokens| tokens.iter().any(|t| t == word)),
            Node::Phrase { field, words } => doc
                .tokens_for(*field)
                .iter()
                .any(|tokens| contains_run(tokens, words)),
        }
    }
}

/// Whether `haystack` contains `needle` as a contiguous run.
fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(a, b)| a == b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::new(
            "Chip export controls tighten",
            "The trade ministry published a revised list covering lithography tools.",
            "Example Wire",
            "B. Writer",
            "https://example.test/chips/export-controls",
        )
    }

    fn matches(query: &str) -> bool {
        parse(query).expect("query must parse").matches(&doc())
    }

    #[test]
    fn adjacency_is_and_not_or() {
        // The single most important semantic choice here: adding a word must NARROW.
        assert!(matches("chip export"));
        assert!(!matches("chip rutabaga"));
    }

    #[test]
    fn boolean_operators_and_grouping() {
        assert!(matches("chip AND export"));
        assert!(matches("rutabaga OR chip"));
        assert!(matches("NOT rutabaga"));
        assert!(matches("-rutabaga"));
        assert!(matches("(chip OR rutabaga) AND controls"));
        assert!(!matches("(rutabaga OR turnip) AND controls"));
    }

    #[test]
    fn or_binds_looser_than_and() {
        // `a AND b OR c` must read as `(a AND b) OR c`. If precedence inverted, this
        // would be `chip AND (rutabaga OR lithography)` and still match — so the
        // discriminating case is one where only the correct grouping is true.
        assert!(matches("rutabaga AND turnip OR chip"));
    }

    #[test]
    fn field_scoping() {
        assert!(matches("title:chip"));
        assert!(!matches("title:lithography"));
        assert!(matches("body:lithography"));
        assert!(matches("source:wire"));
        assert!(matches("author:writer"));
        assert!(matches("url:chips"));
        // The default field is title + body, and nothing else — a topic must not fire
        // because the WORD appears in the source's name.
        assert!(!matches("example"));
    }

    #[test]
    fn phrases_must_be_contiguous() {
        assert!(matches("\"export controls\""));
        assert!(!matches("\"controls export\""));
        assert!(matches("title:\"chip export\""));
    }

    #[test]
    fn matching_is_by_token_never_by_substring() {
        // THE case that makes short topics usable. "chip" must not fire on "chips" in
        // the URL when scoped to the title, and a fragment must never match.
        assert!(!matches("hip"));
        assert!(!matches("ontrol"));
    }

    #[test]
    fn a_hyphenated_word_becomes_a_phrase_not_two_terms() {
        // `covid-19` normalizes to two tokens. Matching them independently would fire
        // on any article containing the number 19.
        let node = parse("covid-19").expect("must parse");
        assert!(matches!(node, Node::Phrase { ref words, .. } if words == &["covid", "19"]));
    }

    #[test]
    fn parse_errors_carry_a_useful_column() {
        let cases: &[(&str, usize, &str)] = &[
            ("chip AND", 9, "ends after an operator"),
            ("(chip", 6, "unclosed paren"),
            ("chip)", 5, "unopened paren"),
            ("\"chip", 1, "unterminated phrase"),
            ("headline:chip", 1, "unknown field"),
            ("", 1, "empty"),
            // Whitespace before the colon does not un-scope it — `chip :` is the same
            // as `chip:`, the way every search box treats it — so this reports the
            // unknown FIELD at the word, not a stray colon at the colon.
            ("chip :", 1, "space before the colon still scopes"),
            (":chip", 1, "colon with no field at all"),
        ];
        for (query, column, why) in cases {
            let err = parse(query).expect_err(why);
            assert_eq!(err.column, *column, "{why}: {} ", err.message);
        }
    }

    #[test]
    fn an_unknown_field_names_the_valid_ones() {
        // The error a user actually has to act on, so it must say what IS allowed.
        let err = parse("headline:chip").expect_err("unknown field must fail");
        for name in Field::NAMES {
            assert!(err.message.contains(name), "message omits '{name}'");
        }
    }

    #[test]
    fn a_term_with_no_searchable_characters_is_refused_not_silently_dead() {
        // A node that can never match is the exact failure this design exists to
        // prevent: the user believes the watch covers them.
        assert!(parse("\"\"").is_err());
        assert!(parse("!!!").is_err());
    }

    #[test]
    fn deep_nesting_is_refused_rather_than_overflowing_the_stack() {
        // A stack overflow aborts the process and cannot be caught, so "returns an
        // error" is the only testable behaviour.
        let hostile = format!("{}chip{}", "(".repeat(500), ")".repeat(500));
        let err = parse(&hostile).expect_err("must refuse");
        assert!(err.message.contains("nests deeper"));
    }

    #[test]
    fn the_ast_round_trips_through_json() {
        // The AST is STORED. If it stops round-tripping, every saved topic silently
        // stops evaluating the way it did when it was written.
        let node = parse("(chip OR wafer) AND NOT title:\"export controls\"").expect("parses");
        let json = serde_json::to_value(&node).expect("serializes");
        let back: Node = serde_json::from_value(json).expect("deserializes");
        assert_eq!(node, back);
        assert_eq!(node.matches(&doc()), back.matches(&doc()));
    }

    #[test]
    fn case_is_irrelevant_to_matching_but_operators_are_uppercase_only() {
        assert!(matches("CHIP"));
        assert!(matches("Chip Export"));
        // Lowercase `and` is a WORD, not an operator — otherwise a topic could never
        // search for the word "and", and more importantly the user's intent is
        // ambiguous. `chip and export` therefore means all three words.
        let node = parse("chip and export").expect("parses");
        assert!(matches!(node, Node::And { ref nodes } if nodes.len() == 3));
    }
}
