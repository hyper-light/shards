//! go-yaml v2.4.2's parser (parserc.go): tokens to events, its state machine and its
//! errors as there.

use super::scanner::{ErrorKind, Mark, Result, ScalarStyle, Scanner, Token, TokenType, YamlError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    NoEvent,
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    Alias,
    Scalar,
    SequenceStart,
    SequenceEnd,
    MappingStart,
    MappingEnd,
}

impl EventType {
    pub fn name(self) -> &'static str {
        match self {
            EventType::NoEvent => "none",
            EventType::StreamStart => "stream start",
            EventType::StreamEnd => "stream end",
            EventType::DocumentStart => "document start",
            EventType::DocumentEnd => "document end",
            EventType::Alias => "alias",
            EventType::Scalar => "scalar",
            EventType::SequenceStart => "sequence start",
            EventType::SequenceEnd => "sequence end",
            EventType::MappingStart => "mapping start",
            EventType::MappingEnd => "mapping end",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub typ: EventType,
    pub anchor: Option<Vec<u8>>,
    pub tag: Vec<u8>,
    pub value: Vec<u8>,
    pub implicit: bool,
}

impl Event {
    fn new(typ: EventType, _start_mark: Mark) -> Event {
        Event {
            typ,
            anchor: None,
            tag: Vec::new(),
            value: Vec::new(),
            implicit: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    StreamStart,
    ImplicitDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
    End,
}

#[derive(Debug)]
pub struct Parser<'a> {
    scanner: Scanner<'a>,
    state: State,
    states: Vec<State>,
    marks: Vec<Mark>,
    tag_directives: Vec<(Vec<u8>, Vec<u8>)>,
    failed: bool,
}

fn parser_error(problem: &str, problem_mark: Mark) -> YamlError {
    YamlError {
        kind: ErrorKind::Parser,
        problem: problem.to_string(),
        problem_mark,
        context_mark: Mark::default(),
    }
}

fn parser_error_context(context_mark: Mark, problem: &str, problem_mark: Mark) -> YamlError {
    YamlError {
        kind: ErrorKind::Parser,
        problem: problem.to_string(),
        problem_mark,
        context_mark,
    }
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a [u8]) -> Parser<'a> {
        Parser {
            scanner: Scanner::new(input),
            state: State::StreamStart,
            states: Vec::new(),
            marks: Vec::new(),
            tag_directives: Vec::new(),
            failed: false,
        }
    }

    /// yaml_parser_parse: the next event, `NoEvent` once the stream has ended.
    pub fn parse(&mut self) -> Result<Event> {
        if self.scanner.stream_end_produced || self.state == State::End {
            return Ok(Event::new(EventType::NoEvent, Mark::default()));
        }
        if self.failed {
            return Err(parser_error("", Mark::default()));
        }
        let r = self.state_machine();
        if r.is_err() {
            self.failed = true;
        }
        r
    }

    fn peek(&mut self) -> Result<Token> {
        self.scanner.peek().cloned()
    }

    fn skip(&mut self) {
        self.scanner.skip_token();
    }

    fn pop_state(&mut self) {
        self.state = self.states.pop().unwrap_or(State::End);
    }

    fn state_machine(&mut self) -> Result<Event> {
        match self.state {
            State::StreamStart => self.parse_stream_start(),
            State::ImplicitDocumentStart => self.parse_document_start(true),
            State::DocumentStart => self.parse_document_start(false),
            State::DocumentContent => self.parse_document_content(),
            State::DocumentEnd => self.parse_document_end(),
            State::BlockNode => self.parse_node(true, false),
            State::BlockSequenceFirstEntry => self.parse_block_sequence_entry(true),
            State::BlockSequenceEntry => self.parse_block_sequence_entry(false),
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry(),
            State::BlockMappingFirstKey => self.parse_block_mapping_key(true),
            State::BlockMappingKey => self.parse_block_mapping_key(false),
            State::BlockMappingValue => self.parse_block_mapping_value(),
            State::FlowSequenceFirstEntry => self.parse_flow_sequence_entry(true),
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(false),
            State::FlowSequenceEntryMappingKey => self.parse_flow_sequence_entry_mapping_key(),
            State::FlowSequenceEntryMappingValue => self.parse_flow_sequence_entry_mapping_value(),
            State::FlowSequenceEntryMappingEnd => self.parse_flow_sequence_entry_mapping_end(),
            State::FlowMappingFirstKey => self.parse_flow_mapping_key(true),
            State::FlowMappingKey => self.parse_flow_mapping_key(false),
            State::FlowMappingValue => self.parse_flow_mapping_value(false),
            State::FlowMappingEmptyValue => self.parse_flow_mapping_value(true),
            State::End => Ok(Event::new(EventType::NoEvent, Mark::default())),
        }
    }

    fn parse_stream_start(&mut self) -> Result<Event> {
        let token = self.peek()?;
        if token.typ != TokenType::StreamStart {
            return Err(parser_error(
                "did not find expected <stream-start>",
                token.start_mark,
            ));
        }
        self.state = State::ImplicitDocumentStart;
        self.skip();
        Ok(Event::new(EventType::StreamStart, token.start_mark))
    }

    fn parse_document_start(&mut self, implicit: bool) -> Result<Event> {
        let mut token = self.peek()?;
        if !implicit {
            while token.typ == TokenType::DocumentEnd {
                self.skip();
                token = self.peek()?;
            }
        }
        if implicit
            && !matches!(
                token.typ,
                TokenType::VersionDirective
                    | TokenType::TagDirective
                    | TokenType::DocumentStart
                    | TokenType::StreamEnd
            )
        {
            self.process_directives()?;
            self.states.push(State::DocumentEnd);
            self.state = State::BlockNode;
            Ok(Event::new(EventType::DocumentStart, token.start_mark))
        } else if token.typ != TokenType::StreamEnd {
            let start_mark = token.start_mark;
            self.process_directives()?;
            let token = self.peek()?;
            if token.typ != TokenType::DocumentStart {
                return Err(parser_error(
                    "did not find expected <document start>",
                    token.start_mark,
                ));
            }
            self.states.push(State::DocumentEnd);
            self.state = State::DocumentContent;
            self.skip();
            Ok(Event::new(EventType::DocumentStart, start_mark))
        } else {
            self.state = State::End;
            self.skip();
            Ok(Event::new(EventType::StreamEnd, token.start_mark))
        }
    }

    fn parse_document_content(&mut self) -> Result<Event> {
        let token = self.peek()?;
        if matches!(
            token.typ,
            TokenType::VersionDirective
                | TokenType::TagDirective
                | TokenType::DocumentStart
                | TokenType::DocumentEnd
                | TokenType::StreamEnd
        ) {
            self.pop_state();
            return Ok(empty_scalar(token.start_mark));
        }
        self.parse_node(true, false)
    }

    fn parse_document_end(&mut self) -> Result<Event> {
        let token = self.peek()?;
        let start_mark = token.start_mark;
        if token.typ == TokenType::DocumentEnd {
            self.skip();
        }
        self.tag_directives.clear();
        self.state = State::DocumentStart;
        Ok(Event::new(EventType::DocumentEnd, start_mark))
    }

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> Result<Event> {
        let mut token = self.peek()?;
        if token.typ == TokenType::Alias {
            self.pop_state();
            let mut e = Event::new(EventType::Alias, token.start_mark);
            e.anchor = Some(token.value);
            self.skip();
            return Ok(e);
        }
        let mut start_mark = token.start_mark;
        let mut tag_token = false;
        let mut tag_handle = Vec::new();
        let mut tag_suffix = Vec::new();
        let mut anchor: Option<Vec<u8>> = None;
        let mut tag_mark = Mark::default();
        if token.typ == TokenType::Anchor {
            anchor = Some(token.value.clone());
            start_mark = token.start_mark;
            self.skip();
            token = self.peek()?;
            if token.typ == TokenType::Tag {
                tag_token = true;
                tag_handle = token.value.clone();
                tag_suffix = token.suffix.clone();
                tag_mark = token.start_mark;
                self.skip();
                token = self.peek()?;
            }
        } else if token.typ == TokenType::Tag {
            tag_token = true;
            tag_handle = token.value.clone();
            tag_suffix = token.suffix.clone();
            start_mark = token.start_mark;
            tag_mark = token.start_mark;
            self.skip();
            token = self.peek()?;
            if token.typ == TokenType::Anchor {
                anchor = Some(token.value.clone());
                self.skip();
                token = self.peek()?;
            }
        }
        let mut tag = Vec::new();
        if tag_token {
            if tag_handle.is_empty() {
                tag = tag_suffix;
            } else {
                for (handle, prefix) in &self.tag_directives {
                    if *handle == tag_handle {
                        tag = prefix.clone();
                        tag.extend_from_slice(&tag_suffix);
                        break;
                    }
                }
                if tag.is_empty() {
                    return Err(parser_error_context(
                        start_mark,
                        "found undefined tag handle",
                        tag_mark,
                    ));
                }
            }
        }
        let implicit = tag.is_empty();
        let node_event = |typ: EventType, tag: Vec<u8>, anchor: Option<Vec<u8>>| {
            let mut e = Event::new(typ, start_mark);
            e.anchor = anchor;
            e.tag = tag;
            e.implicit = implicit;
            e
        };
        if indentless_sequence && token.typ == TokenType::BlockEntry {
            self.state = State::IndentlessSequenceEntry;
            return Ok(node_event(EventType::SequenceStart, tag, anchor));
        }
        if token.typ == TokenType::Scalar {
            let plain_implicit = (tag.is_empty() && token.style == ScalarStyle::Plain)
                || (tag.len() == 1 && tag.first() == Some(&b'!'));
            self.pop_state();
            let mut e = node_event(EventType::Scalar, tag, anchor);
            e.value = token.value;
            e.implicit = plain_implicit;
            self.skip();
            return Ok(e);
        }
        if token.typ == TokenType::FlowSequenceStart {
            self.state = State::FlowSequenceFirstEntry;
            return Ok(node_event(EventType::SequenceStart, tag, anchor));
        }
        if token.typ == TokenType::FlowMappingStart {
            self.state = State::FlowMappingFirstKey;
            return Ok(node_event(EventType::MappingStart, tag, anchor));
        }
        if block && token.typ == TokenType::BlockSequenceStart {
            self.state = State::BlockSequenceFirstEntry;
            return Ok(node_event(EventType::SequenceStart, tag, anchor));
        }
        if block && token.typ == TokenType::BlockMappingStart {
            self.state = State::BlockMappingFirstKey;
            return Ok(node_event(EventType::MappingStart, tag, anchor));
        }
        if anchor.as_ref().is_some_and(|a| !a.is_empty()) || !tag.is_empty() {
            self.pop_state();
            return Ok(node_event(EventType::Scalar, tag, anchor));
        }
        Err(parser_error_context(
            start_mark,
            "did not find expected node content",
            token.start_mark,
        ))
    }

    fn parse_block_sequence_entry(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek()?;
            self.marks.push(token.start_mark);
            self.skip();
        }
        let mut token = self.peek()?;
        if token.typ == TokenType::BlockEntry {
            let mark = token.end_mark;
            self.skip();
            token = self.peek()?;
            if token.typ != TokenType::BlockEntry && token.typ != TokenType::BlockEnd {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = State::BlockSequenceEntry;
            return Ok(empty_scalar(mark));
        }
        if token.typ == TokenType::BlockEnd {
            self.pop_state();
            self.marks.pop();
            self.skip();
            return Ok(Event::new(EventType::SequenceEnd, token.start_mark));
        }
        let context_mark = self.marks.pop().unwrap_or_default();
        Err(parser_error_context(
            context_mark,
            "did not find expected '-' indicator",
            token.start_mark,
        ))
    }

    fn parse_indentless_sequence_entry(&mut self) -> Result<Event> {
        let mut token = self.peek()?;
        if token.typ == TokenType::BlockEntry {
            let mark = token.end_mark;
            self.skip();
            token = self.peek()?;
            if !matches!(
                token.typ,
                TokenType::BlockEntry | TokenType::Key | TokenType::Value | TokenType::BlockEnd
            ) {
                self.states.push(State::IndentlessSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = State::IndentlessSequenceEntry;
            return Ok(empty_scalar(mark));
        }
        self.pop_state();
        Ok(Event::new(EventType::SequenceEnd, token.start_mark))
    }

    fn parse_block_mapping_key(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek()?;
            self.marks.push(token.start_mark);
            self.skip();
        }
        let mut token = self.peek()?;
        if token.typ == TokenType::Key {
            let mark = token.end_mark;
            self.skip();
            token = self.peek()?;
            if !matches!(token.typ, TokenType::Key | TokenType::Value | TokenType::BlockEnd) {
                self.states.push(State::BlockMappingValue);
                return self.parse_node(true, true);
            }
            self.state = State::BlockMappingValue;
            return Ok(empty_scalar(mark));
        } else if token.typ == TokenType::BlockEnd {
            self.pop_state();
            self.marks.pop();
            self.skip();
            return Ok(Event::new(EventType::MappingEnd, token.start_mark));
        }
        let context_mark = self.marks.pop().unwrap_or_default();
        Err(parser_error_context(
            context_mark,
            "did not find expected key",
            token.start_mark,
        ))
    }

    fn parse_block_mapping_value(&mut self) -> Result<Event> {
        let mut token = self.peek()?;
        if token.typ == TokenType::Value {
            let mark = token.end_mark;
            self.skip();
            token = self.peek()?;
            if !matches!(token.typ, TokenType::Key | TokenType::Value | TokenType::BlockEnd) {
                self.states.push(State::BlockMappingKey);
                return self.parse_node(true, true);
            }
            self.state = State::BlockMappingKey;
            return Ok(empty_scalar(mark));
        }
        self.state = State::BlockMappingKey;
        Ok(empty_scalar(token.start_mark))
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek()?;
            self.marks.push(token.start_mark);
            self.skip();
        }
        let mut token = self.peek()?;
        if token.typ != TokenType::FlowSequenceEnd {
            if !first {
                if token.typ == TokenType::FlowEntry {
                    self.skip();
                    token = self.peek()?;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return Err(parser_error_context(
                        context_mark,
                        "did not find expected ',' or ']'",
                        token.start_mark,
                    ));
                }
            }
            if token.typ == TokenType::Key {
                self.state = State::FlowSequenceEntryMappingKey;
                let mut e = Event::new(EventType::MappingStart, token.start_mark);
                e.implicit = true;
                self.skip();
                return Ok(e);
            } else if token.typ != TokenType::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        self.pop_state();
        self.marks.pop();
        self.skip();
        Ok(Event::new(EventType::SequenceEnd, token.start_mark))
    }

    fn parse_flow_sequence_entry_mapping_key(&mut self) -> Result<Event> {
        let token = self.peek()?;
        if !matches!(
            token.typ,
            TokenType::Value | TokenType::FlowEntry | TokenType::FlowSequenceEnd
        ) {
            self.states.push(State::FlowSequenceEntryMappingValue);
            return self.parse_node(false, false);
        }
        let mark = token.end_mark;
        self.skip();
        self.state = State::FlowSequenceEntryMappingValue;
        Ok(empty_scalar(mark))
    }

    fn parse_flow_sequence_entry_mapping_value(&mut self) -> Result<Event> {
        let token = self.peek()?;
        if token.typ == TokenType::Value {
            self.skip();
            let token = self.peek()?;
            if token.typ != TokenType::FlowEntry && token.typ != TokenType::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(false, false);
            }
        }
        self.state = State::FlowSequenceEntryMappingEnd;
        Ok(empty_scalar(token.start_mark))
    }

    fn parse_flow_sequence_entry_mapping_end(&mut self) -> Result<Event> {
        let token = self.peek()?;
        self.state = State::FlowSequenceEntry;
        Ok(Event::new(EventType::MappingEnd, token.start_mark))
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek()?;
            self.marks.push(token.start_mark);
            self.skip();
        }
        let mut token = self.peek()?;
        if token.typ != TokenType::FlowMappingEnd {
            if !first {
                if token.typ == TokenType::FlowEntry {
                    self.skip();
                    token = self.peek()?;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return Err(parser_error_context(
                        context_mark,
                        "did not find expected ',' or '}'",
                        token.start_mark,
                    ));
                }
            }
            if token.typ == TokenType::Key {
                self.skip();
                token = self.peek()?;
                if !matches!(
                    token.typ,
                    TokenType::Value | TokenType::FlowEntry | TokenType::FlowMappingEnd
                ) {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = State::FlowMappingValue;
                return Ok(empty_scalar(token.start_mark));
            } else if token.typ != TokenType::FlowMappingEnd {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        self.pop_state();
        self.marks.pop();
        self.skip();
        Ok(Event::new(EventType::MappingEnd, token.start_mark))
    }

    fn parse_flow_mapping_value(&mut self, empty: bool) -> Result<Event> {
        let mut token = self.peek()?;
        if empty {
            self.state = State::FlowMappingKey;
            return Ok(empty_scalar(token.start_mark));
        }
        if token.typ == TokenType::Value {
            self.skip();
            token = self.peek()?;
            if token.typ != TokenType::FlowEntry && token.typ != TokenType::FlowMappingEnd {
                self.states.push(State::FlowMappingKey);
                return self.parse_node(false, false);
            }
        }
        self.state = State::FlowMappingKey;
        Ok(empty_scalar(token.start_mark))
    }

    fn process_directives(&mut self) -> Result<()> {
        let mut version_seen = false;
        let mut token = self.peek()?;
        while token.typ == TokenType::VersionDirective || token.typ == TokenType::TagDirective {
            if token.typ == TokenType::VersionDirective {
                if version_seen {
                    return Err(parser_error("found duplicate %YAML directive", token.start_mark));
                }
                if token.major != 1 || token.minor != 1 {
                    return Err(parser_error("found incompatible YAML document", token.start_mark));
                }
                version_seen = true;
            } else {
                self.append_tag_directive(
                    token.value.clone(),
                    token.prefix.clone(),
                    false,
                    token.start_mark,
                )?;
            }
            self.skip();
            token = self.peek()?;
        }
        self.append_tag_directive(b"!".to_vec(), b"!".to_vec(), true, token.start_mark)?;
        self.append_tag_directive(
            b"!!".to_vec(),
            b"tag:yaml.org,2002:".to_vec(),
            true,
            token.start_mark,
        )?;
        Ok(())
    }

    fn append_tag_directive(
        &mut self,
        handle: Vec<u8>,
        prefix: Vec<u8>,
        allow_duplicates: bool,
        mark: Mark,
    ) -> Result<()> {
        if self.tag_directives.iter().any(|(h, _)| *h == handle) {
            if allow_duplicates {
                return Ok(());
            }
            return Err(parser_error("found duplicate %TAG directive", mark));
        }
        self.tag_directives.push((handle, prefix));
        Ok(())
    }
}

fn empty_scalar(mark: Mark) -> Event {
    let mut e = Event::new(EventType::Scalar, mark);
    e.implicit = true;
    e
}
