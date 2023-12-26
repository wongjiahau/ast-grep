use ast_grep_core::language::{Language, TSLanguage};
use ignore::types::{Types, TypesBuilder};
use ignore::{WalkBuilder, WalkParallel};
use napi::anyhow::anyhow;
use napi::anyhow::Error;
use napi::bindgen_prelude::Result;
use napi_derive::napi;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

#[napi]
#[derive(PartialEq, Eq, Hash)]
pub enum FrontEndLanguage {
  Html,
  JavaScript,
  Tsx,
  Css,
  TypeScript,
}

impl Language for FrontEndLanguage {
  fn get_ts_language(&self) -> TSLanguage {
    use FrontEndLanguage::*;
    match self {
      Html => tree_sitter_html::language(),
      JavaScript => tree_sitter_javascript::language(),
      TypeScript => tree_sitter_typescript::language_typescript(),
      Css => tree_sitter_css::language(),
      Tsx => tree_sitter_typescript::language_tsx(),
    }
    .into()
  }
  fn expando_char(&self) -> char {
    use FrontEndLanguage::*;
    match self {
      Css => '_',
      _ => '$',
    }
  }
  fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
    use FrontEndLanguage::*;
    match self {
      Css => (),
      _ => return Cow::Borrowed(query),
    }
    // use stack buffer to reduce allocation
    let mut buf = [0; 4];
    let expando = self.expando_char().encode_utf8(&mut buf);
    // TODO: use more precise replacement
    let replaced = query.replace(self.meta_var_char(), expando);
    Cow::Owned(replaced)
  }
}

pub type LanguageGlobs = HashMap<FrontEndLanguage, Vec<String>>;

impl FrontEndLanguage {
  pub const fn all_langs() -> &'static [FrontEndLanguage] {
    use FrontEndLanguage::*;
    &[Html, JavaScript, Tsx, Css, TypeScript]
  }
  pub fn lang_globs(map: HashMap<String, Vec<String>>) -> LanguageGlobs {
    let mut ret = HashMap::new();
    for (name, patterns) in map {
      if let Ok(lang) = FrontEndLanguage::from_str(&name) {
        ret.insert(lang, patterns);
      }
    }
    ret
  }
}

const fn alias(lang: &FrontEndLanguage) -> &[&str] {
  use FrontEndLanguage::*;
  match lang {
    Css => &["css"],
    Html => &["html"],
    JavaScript => &["javascript", "js", "jsx"],
    TypeScript => &["ts", "typescript"],
    Tsx => &["tsx"],
  }
}

impl FromStr for FrontEndLanguage {
  type Err = Error;
  fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
    for lang in Self::all_langs() {
      for moniker in alias(lang) {
        if s.eq_ignore_ascii_case(moniker) {
          return Ok(*lang);
        }
      }
    }
    Err(anyhow!(format!("{} is not supported in napi", s.to_string())).into())
  }
}

pub enum LangOption {
  /// Used when language is inferred from file path
  /// e.g. in parse_files
  Inferred(Vec<(FrontEndLanguage, Types)>),
  /// Used when language is specified
  /// e.g. in frontend_lang.find_in_files
  Specified(FrontEndLanguage),
}

impl LangOption {
  pub fn get_lang(&self, path: &Path) -> Option<FrontEndLanguage> {
    use LangOption::*;
    match self {
      Specified(lang) => Some(*lang),
      Inferred(pairs) => pairs
        .iter()
        .find_map(|(lang, types)| types.matched(path, false).is_whitelist().then(|| *lang)),
    }
  }
  pub fn infer(language_globs: &HashMap<FrontEndLanguage, Vec<String>>) -> Self {
    let mut types = vec![];
    let empty = vec![];
    for lang in FrontEndLanguage::all_langs() {
      let (tpe, list) = file_patterns(lang);
      let mut builder = TypesBuilder::new();
      for pattern in list {
        builder.add(tpe, pattern).expect("should build");
      }
      for pattern in language_globs.get(lang).unwrap_or(&empty) {
        builder.add(tpe, pattern).expect("should build");
      }
      builder.select(tpe);
      types.push((*lang, builder.build().unwrap()));
    }
    Self::Inferred(types)
  }
}

const fn file_patterns(lang: &FrontEndLanguage) -> (&str, &[&str]) {
  match lang {
    FrontEndLanguage::TypeScript => ("myts", &["*.ts", "*.mts", "*.cts"]),
    FrontEndLanguage::Tsx => ("mytsx", &["*.tsx", "*.mtsx", "*.ctsx"]),
    FrontEndLanguage::Css => ("mycss", &["*.css", "*.scss"]),
    FrontEndLanguage::Html => ("myhtml", &["*.html", "*.htm", "*.xhtml"]),
    FrontEndLanguage::JavaScript => ("myjs", &["*.cjs", "*.js", "*.mjs", "*.jsx"]),
  }
}

pub fn build_files(
  paths: Vec<String>,
  language_globs: &HashMap<FrontEndLanguage, Vec<String>>,
) -> Result<WalkParallel> {
  if paths.is_empty() {
    return Err(anyhow!("paths cannot be empty.").into());
  }
  let mut types = TypesBuilder::new();
  let empty = vec![];
  for lang in FrontEndLanguage::all_langs() {
    let (type_name, default_types) = file_patterns(lang);
    let custom = language_globs.get(lang).unwrap_or(&empty);
    select_custom(&mut types, type_name, default_types, custom);
  }
  let types = types.build().unwrap();
  let mut paths = paths.into_iter();
  let mut builder = WalkBuilder::new(paths.next().unwrap());
  for path in paths {
    builder.add(path);
  }
  let walk = builder.types(types).build_parallel();
  Ok(walk)
}

fn select_custom<'b>(
  builder: &'b mut TypesBuilder,
  file_type: &str,
  default_suffix_list: &[&str],
  custom_suffix_list: &[String],
) -> &'b mut TypesBuilder {
  for suffix in default_suffix_list {
    builder
      .add(file_type, suffix)
      .expect("file pattern must compile");
  }
  for suffix in custom_suffix_list {
    builder
      .add(file_type, suffix)
      .expect("file pattern must compile");
  }
  builder.select(file_type)
}

pub fn find_files_with_lang(
  paths: Vec<String>,
  lang: &FrontEndLanguage,
  language_globs: Option<Vec<String>>,
) -> Result<WalkParallel> {
  if paths.is_empty() {
    return Err(anyhow!("paths cannot be empty.").into());
  }

  let mut types = TypesBuilder::new();
  let custom_file_type = language_globs.unwrap_or_else(Vec::new);
  let (type_name, default_types) = file_patterns(lang);
  let types = select_custom(&mut types, type_name, default_types, &custom_file_type)
    .build()
    .unwrap();
  let mut paths = paths.into_iter();
  let mut builder = WalkBuilder::new(paths.next().unwrap());
  for path in paths {
    builder.add(path);
  }
  let walk = builder.types(types).build_parallel();
  Ok(walk)
}