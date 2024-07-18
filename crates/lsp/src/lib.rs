mod utils;

use dashmap::DashMap;
use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use ast_grep_config::{CombinedScan, RuleCollection, RuleConfig};
use ast_grep_core::{language::Language, AstGrep, Doc, StrDoc};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use utils::{convert_match_to_diagnostic, diagnostic_to_code_action, RewriteData};

pub use tower_lsp::{LspService, Server};

pub trait LSPLang: Language + Eq + Send + Sync + 'static {}
impl<T> LSPLang for T where T: Language + Eq + Send + Sync + 'static {}

struct VersionedAst<D: Doc> {
  version: i32,
  root: AstGrep<D>,
}

pub struct Backend<L: LSPLang> {
  client: Client,
  map: DashMap<String, VersionedAst<StrDoc<L>>>,
  base: PathBuf,
  rules: std::result::Result<RuleCollection<L>, String>,
}

const FALLBACK_CODE_ACTION_PROVIDER: Option<CodeActionProviderCapability> =
  Some(CodeActionProviderCapability::Simple(true));

pub const APPLY_ALL_FIXES: &str = "ast-grep.applyAllFixes";

fn code_action_provider(
  client_capability: &ClientCapabilities,
) -> Option<CodeActionProviderCapability> {
  let is_literal_supported = client_capability
    .text_document
    .as_ref()?
    .code_action
    .as_ref()?
    .code_action_literal_support
    .is_some();
  if !is_literal_supported {
    return None;
  }
  Some(CodeActionProviderCapability::Options(CodeActionOptions {
    code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
    work_done_progress_options: Default::default(),
    resolve_provider: Some(true),
  }))
}

#[tower_lsp::async_trait]
impl<L: LSPLang> LanguageServer for Backend<L> {
  async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
    Ok(InitializeResult {
      server_info: Some(ServerInfo {
        name: "ast-grep language server".to_string(),
        version: None,
      }),
      capabilities: ServerCapabilities {
        // TODO: change this to incremental
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        code_action_provider: code_action_provider(&params.capabilities)
          .or(FALLBACK_CODE_ACTION_PROVIDER),
        execute_command_provider: Some(ExecuteCommandOptions {
          commands: vec![APPLY_ALL_FIXES.to_string()],
          work_done_progress_options: Default::default(),
        }),
        ..ServerCapabilities::default()
      },
    })
  }

  async fn initialized(&self, _: InitializedParams) {
    self
      .client
      .log_message(MessageType::INFO, "server initialized!")
      .await;

    // Report errors loading config once, upon initialization
    if let Err(error) = &self.rules {
      // popup message
      self
        .client
        .show_message(
          MessageType::ERROR,
          format!("Failed to load rules: {}", error),
        )
        .await;
      // log message
      self
        .client
        .log_message(
          MessageType::ERROR,
          format!("Failed to load rules: {}", error),
        )
        .await;
    }
  }

  async fn shutdown(&self) -> Result<()> {
    Ok(())
  }

  async fn did_change_workspace_folders(&self, _: DidChangeWorkspaceFoldersParams) {
    self
      .client
      .log_message(MessageType::INFO, "workspace folders changed!")
      .await;
  }

  async fn did_change_configuration(&self, _: DidChangeConfigurationParams) {
    self
      .client
      .log_message(MessageType::INFO, "configuration changed!")
      .await;
  }

  async fn did_change_watched_files(&self, _: DidChangeWatchedFilesParams) {
    self
      .client
      .log_message(MessageType::INFO, "watched files have changed!")
      .await;
  }
  async fn did_open(&self, params: DidOpenTextDocumentParams) {
    self
      .client
      .log_message(MessageType::INFO, "file opened!")
      .await;
    self.on_open(params).await;
  }

  async fn did_change(&self, params: DidChangeTextDocumentParams) {
    self.on_change(params).await;
  }

  async fn did_save(&self, _: DidSaveTextDocumentParams) {
    self
      .client
      .log_message(MessageType::INFO, "file saved!")
      .await;
  }

  async fn did_close(&self, params: DidCloseTextDocumentParams) {
    self.on_close(params).await;
    self
      .client
      .log_message(MessageType::INFO, "file closed!")
      .await;
  }

  async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
    Ok(self.on_code_action(params).await)
  }

  async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
    Ok(self.on_execute_command(params).await)
  }
}

impl<L: LSPLang> Backend<L> {
  pub fn new(
    client: Client,
    base: PathBuf,
    rules: std::result::Result<RuleCollection<L>, String>,
  ) -> Self {
    Self {
      client,
      rules,
      base,
      map: DashMap::new(),
    }
  }

  fn get_rules(&self, uri: &Url) -> Option<Vec<&RuleConfig<L>>> {
    let absolute_path = uri.to_file_path().ok()?;
    // for_path needs relative path, see https://github.com/ast-grep/ast-grep/issues/1272
    let base = Path::new("./");
    let path = if let Ok(p) = absolute_path.strip_prefix(&self.base) {
      base.join(p)
    } else {
      absolute_path
    };
    let rules = self.rules.as_ref().ok()?.for_path(&path);
    Some(rules)
  }

  fn get_diagnostics(
    &self,
    uri: &Url,
    versioned: &VersionedAst<StrDoc<L>>,
  ) -> Option<Vec<Diagnostic>> {
    let rules = self.get_rules(uri)?;
    let scan = CombinedScan::new(rules);
    let hit_set = scan.all_kinds();
    let matches = scan.scan(&versioned.root, hit_set, false).matches;
    let mut diagnostics = vec![];
    for (id, ms) in matches {
      let rule = scan.get_rule(id);
      let to_diagnostic = |m| convert_match_to_diagnostic(m, rule, uri);
      diagnostics.extend(ms.into_iter().map(to_diagnostic));
    }
    Some(diagnostics)
  }

  async fn publish_diagnostics(&self, uri: Url, versioned: &VersionedAst<StrDoc<L>>) -> Option<()> {
    let diagnostics = self.get_diagnostics(&uri, versioned).unwrap_or_default();
    self
      .client
      .publish_diagnostics(uri, diagnostics, Some(versioned.version))
      .await;
    Some(())
  }

  async fn on_open(&self, params: DidOpenTextDocumentParams) -> Option<()> {
    let text_doc = params.text_document;
    let uri = text_doc.uri.as_str().to_owned();
    let text = text_doc.text;
    self
      .client
      .log_message(MessageType::LOG, "Parsing doc.")
      .await;
    let lang = Self::infer_lang_from_uri(&text_doc.uri)?;
    let root = AstGrep::new(text, lang);
    let versioned = VersionedAst {
      version: text_doc.version,
      root,
    };
    self
      .client
      .log_message(MessageType::LOG, "Publishing init diagnostics.")
      .await;
    self.publish_diagnostics(text_doc.uri, &versioned).await;
    self.map.insert(uri.to_owned(), versioned); // don't lock dashmap
    Some(())
  }
  async fn on_change(&self, params: DidChangeTextDocumentParams) -> Option<()> {
    let text_doc = params.text_document;
    let uri = text_doc.uri.as_str();
    let text = &params.content_changes[0].text;
    self
      .client
      .log_message(MessageType::LOG, "Parsing changed doc.")
      .await;
    let lang = Self::infer_lang_from_uri(&text_doc.uri)?;
    let root = AstGrep::new(text, lang);
    let mut versioned = self.map.get_mut(uri)?;
    // skip old version update
    if versioned.version > text_doc.version {
      return None;
    }
    *versioned = VersionedAst {
      version: text_doc.version,
      root,
    };
    self
      .client
      .log_message(MessageType::LOG, "Publishing diagnostics.")
      .await;
    self.publish_diagnostics(text_doc.uri, &versioned).await;
    Some(())
  }
  async fn on_close(&self, params: DidCloseTextDocumentParams) {
    self.map.remove(params.text_document.uri.as_str());
  }

  fn compute_all_fixes(
    &self,
    text_document: TextDocumentIdentifier,
    mut diagnostics: Vec<Diagnostic>,
  ) -> Option<HashMap<Url, Vec<TextEdit>>>
  where
    L: ast_grep_core::Language + std::cmp::Eq,
  {
    diagnostics.sort_by_key(|d| (d.range.start, d.range.end));
    let mut last = Position {
      line: 0,
      character: 0,
    };
    let edits: Vec<_> = diagnostics
      .into_iter()
      .filter_map(|d| {
        if d.range.start < last {
          return None;
        }
        let rewrite_data = RewriteData::from_value(d.data?)?;
        let edit = TextEdit::new(d.range, rewrite_data.fixed);
        last = d.range.end;
        Some(edit)
      })
      .collect();
    if edits.is_empty() {
      return None;
    }
    let mut changes = HashMap::new();
    changes.insert(text_document.uri, edits);
    Some(changes)
  }

  async fn on_code_action(&self, params: CodeActionParams) -> Option<CodeActionResponse> {
    let text_doc = params.text_document;
    let response = params
      .context
      .diagnostics
      .into_iter()
      .filter_map(|d| diagnostic_to_code_action(&text_doc, d))
      .map(CodeActionOrCommand::from)
      .collect();
    Some(response)
  }

  // TODO: support other urls besides file_scheme
  fn infer_lang_from_uri(uri: &Url) -> Option<L> {
    let path = uri.to_file_path().ok()?;
    L::from_path(path)
  }

  async fn on_execute_command(&self, params: ExecuteCommandParams) -> Option<Value> {
    let ExecuteCommandParams {
      arguments,
      command,
      work_done_progress_params: _,
    } = params;

    match command.as_ref() {
      APPLY_ALL_FIXES => {
        self.on_apply_all_fix(command, arguments).await?;
        None
      }
      _ => {
        self
          .client
          .log_message(
            MessageType::LOG,
            format!("Unrecognized command: {}", command),
          )
          .await;
        None
      }
    }
  }

  async fn on_apply_all_fix_impl(
    &self,
    first: Value,
  ) -> std::result::Result<WorkspaceEdit, LspError> {
    let text_doc: TextDocumentItem =
      serde_json::from_value(first).map_err(LspError::JSONDecodeError)?;
    let uri = text_doc.uri;
    let Some(lang) = Self::infer_lang_from_uri(&uri) else {
      return Err(LspError::UnsupportedFileType);
    };

    let version = text_doc.version;
    let root = AstGrep::new(text_doc.text, lang);
    let versioned = VersionedAst { version, root };

    let Some(diagnostics) = self.get_diagnostics(&uri, &versioned) else {
      return Err(LspError::NoActionableFix);
    };
    let changes = self.compute_all_fixes(TextDocumentIdentifier::new(uri), diagnostics);
    let workspace_edit = WorkspaceEdit {
      changes,
      document_changes: None,
      change_annotations: None,
    };
    Ok(workspace_edit)
  }

  async fn on_apply_all_fix(&self, command: String, arguments: Vec<Value>) -> Option<()> {
    self
      .client
      .log_message(
        MessageType::INFO,
        format!("Running ExecuteCommand {}", command),
      )
      .await;
    let first = arguments.first()?.clone();
    let workspace_edit = match self.on_apply_all_fix_impl(first).await {
      Ok(workspace_edit) => workspace_edit,
      Err(error) => {
        self.report_error(error).await;
        return None;
      }
    };
    self.client.apply_edit(workspace_edit).await.ok()?;
    None
  }

  async fn report_error(&self, error: LspError) {
    match error {
      LspError::JSONDecodeError(e) => {
        self
          .client
          .log_message(
            MessageType::ERROR,
            format!("JSON deserialization error: {}", e),
          )
          .await;
      }
      LspError::UnsupportedFileType => {
        self
          .client
          .log_message(MessageType::ERROR, "Unsupported file type")
          .await;
      }
      LspError::NoActionableFix => {
        self
          .client
          .log_message(MessageType::LOG, "No actionable fix")
          .await;
      }
    }
  }
}

enum LspError {
  JSONDecodeError(serde_json::Error),
  UnsupportedFileType,
  NoActionableFix,
}
