use crate::FrontEndLanguage;

use ast_grep_config::{RuleCore, SerializableRuleCore};
use ast_grep_core::replacer::IndentSensitive;
use ast_grep_core::source::{Content, Doc, Edit, TSParseError};
use ast_grep_core::Language;
use napi::anyhow::Error;
use napi::bindgen_prelude::Result as NapiResult;
use napi_derive::napi;
use tree_sitter::{InputEdit, Node, Parser, ParserError, Point, Tree};

use std::borrow::Cow;
use std::ops::Range;

/// Rule configuration similar to YAML
/// See https://ast-grep.github.io/reference/yaml.html
#[napi(object)]
pub struct NapiConfig {
  /// The rule object, see https://ast-grep.github.io/reference/rule.html
  pub rule: serde_json::Value,
  /// See https://ast-grep.github.io/guide/rule-config.html#constraints
  pub constraints: Option<serde_json::Value>,
  /// Available languages: html, css, js, jsx, ts, tsx
  pub language: Option<FrontEndLanguage>,
  /// https://ast-grep.github.io/reference/yaml.html#transform
  pub transform: Option<serde_json::Value>,
  /// https://ast-grep.github.io/guide/rule-config/utility-rule.html
  pub utils: Option<serde_json::Value>,
}

impl NapiConfig {
  pub fn parse_with(self, language: FrontEndLanguage) -> NapiResult<RuleCore<FrontEndLanguage>> {
    let lang = self.language.unwrap_or(language);
    let rule = SerializableRuleCore {
      language: lang,
      rule: serde_json::from_value(self.rule)?,
      constraints: self.constraints.map(serde_json::from_value).transpose()?,
      transform: self.transform.map(serde_json::from_value).transpose()?,
      utils: self.utils.map(serde_json::from_value).transpose()?,
      fix: None,
    };
    rule.get_matcher(&Default::default()).map_err(|e| {
      let error = Error::from(e)
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
      napi::Error::new(napi::Status::InvalidArg, error.join("\n |->"))
    })
  }
}

#[derive(Clone)]
pub struct Wrapper {
  inner: Vec<u16>,
}

impl Content for Wrapper {
  type Underlying = u16;
  fn parse_tree_sitter(
    &self,
    parser: &mut Parser,
    tree: Option<&Tree>,
  ) -> std::result::Result<Option<Tree>, ParserError> {
    parser.parse_utf16(self.inner.as_slice(), tree)
  }
  fn get_range(&self, range: Range<usize>) -> &[Self::Underlying] {
    // the range is in byte offset, but our underlying is u16
    let start = range.start / 2;
    let end = range.end / 2;
    &self.inner.as_slice()[start..end]
  }
  fn accept_edit(&mut self, edit: &Edit<Self>) -> InputEdit {
    let start_byte = edit.position;
    let old_end_byte = edit.position + edit.deleted_length;
    let new_end_byte = edit.position + edit.inserted_text.len() * 2;
    let input = &mut self.inner;
    let start_position = pos_for_byte_offset(input, start_byte);
    let old_end_position = pos_for_byte_offset(input, old_end_byte);
    input.splice(start_byte / 2..old_end_byte / 2, edit.inserted_text.clone());
    let new_end_position = pos_for_byte_offset(input, new_end_byte);
    InputEdit::new(
      start_byte as u32,
      old_end_byte as u32,
      new_end_byte as u32,
      &start_position,
      &old_end_position,
      &new_end_position,
    )
  }
  fn get_text<'a>(&'a self, node: &Node) -> Cow<'a, str> {
    let slice = self.inner.as_slice();
    let start = node.start_byte() as usize / 2;
    let end = node.end_byte() as usize / 2;
    String::from_utf16_lossy(&slice[start..end]).into()
  }

  fn decode_str(src: &str) -> Cow<[Self::Underlying]> {
    let v: Vec<_> = src.encode_utf16().collect();
    Cow::Owned(v)
  }

  fn encode_bytes(bytes: &[Self::Underlying]) -> Cow<str> {
    let s = String::from_utf16_lossy(bytes);
    Cow::Owned(s)
  }
}

impl IndentSensitive for Wrapper {
  const NEW_LINE: u16 = b'\n' as u16;
  const SPACE: u16 = b' ' as u16;
  const TAB: u16 = b'\t' as u16;
}

fn pos_for_byte_offset(input: &[u16], byte_offset: usize) -> Point {
  let offset = byte_offset / 2;
  debug_assert!(offset <= input.len());
  let (mut row, mut col) = (0, 0);
  for c in char::decode_utf16(input.iter().copied()).take(offset) {
    if let Ok('\n') = c {
      row += 1;
      col = 0;
    } else {
      col += 1;
    }
  }
  Point::new(row, col)
}

#[derive(Clone)]
pub struct JsDoc {
  lang: FrontEndLanguage,
  source: Wrapper,
}

impl JsDoc {
  pub fn new(src: String, lang: FrontEndLanguage) -> Self {
    let source = Wrapper {
      inner: src.encode_utf16().collect(),
    };
    Self { source, lang }
  }
}

impl Doc for JsDoc {
  type Lang = FrontEndLanguage;
  type Source = Wrapper;
  fn parse(&self, old_tree: Option<&Tree>) -> std::result::Result<Tree, TSParseError> {
    let mut parser = Parser::new()?;
    let ts_lang = self.lang.get_ts_language();
    parser.set_language(&ts_lang)?;
    if let Some(tree) = self.source.parse_tree_sitter(&mut parser, old_tree)? {
      Ok(tree)
    } else {
      Err(TSParseError::TreeUnavailable)
    }
  }
  fn get_lang(&self) -> &Self::Lang {
    &self.lang
  }
  fn get_source(&self) -> &Self::Source {
    &self.source
  }
  fn get_source_mut(&mut self) -> &mut Self::Source {
    &mut self.source
  }
  fn from_str(src: &str, lang: Self::Lang) -> Self {
    JsDoc::new(src.into(), lang)
  }
}

#[cfg(test)]
mod test {
  use super::*;
  use ast_grep_core::AstGrep;
  #[test]
  fn test_js_doc() {
    let doc = JsDoc::new("console.log(123)".into(), FrontEndLanguage::JavaScript);
    let grep = AstGrep::doc(doc);
    assert_eq!(grep.root().text(), "console.log(123)");
    let node = grep.root().find("console");
    assert!(node.is_some());
  }

  #[test]
  fn test_js_doc_single_node_replace() {
    let doc = JsDoc::new(
      "console.log(1 + 2 + 3)".into(),
      FrontEndLanguage::JavaScript,
    );
    let mut grep = AstGrep::doc(doc);
    let edit = grep
      .root()
      .replace("console.log($SINGLE)", "log($SINGLE)")
      .expect("should exist");
    grep.edit(edit).expect("should work");
    assert_eq!(grep.root().text(), "log(1 + 2 + 3)");
  }

  #[test]
  fn test_js_doc_multiple_node_replace() {
    let doc = JsDoc::new(
      "console.log(1 + 2 + 3)".into(),
      FrontEndLanguage::JavaScript,
    );
    let mut grep = AstGrep::doc(doc);
    let edit = grep
      .root()
      .replace("console.log($$$MULTI)", "log($$$MULTI)")
      .expect("should exist");
    grep.edit(edit).expect("should work");
    assert_eq!(grep.root().text(), "log(1 + 2 + 3)");
  }
}
