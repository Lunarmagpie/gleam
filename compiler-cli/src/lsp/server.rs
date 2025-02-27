use super::feedback::{Feedback, FeedbackBookKeeper};
use super::progress::ProgressReporter;
use super::{src_span_to_lsp_range, uri_to_module_name, LspProjectCompiler};
use crate::fs::ProjectIO;
use gleam_core::Warning;
use gleam_core::{ast::Import, io::FileSystemReader, language_server::FileSystemProxy};
use gleam_core::{
    ast::Statement,
    build::{Located, Module},
    config::PackageConfig,
    line_numbers::LineNumbers,
    type_::pretty::Printer,
    Error, Result,
};
use lsp::DidOpenTextDocumentParams;
use lsp_types::{
    self as lsp, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidSaveTextDocumentParams, Hover, HoverContents, MarkedString, Position, Range, TextEdit, Url,
};
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub struct Response<T> {
    pub payload: Option<T>,
    pub feedback: Feedback,
}

pub struct LanguageServer<'a> {
    /// A cached copy of the absolute path of the project root
    project_root: PathBuf,

    /// A compiler for the project that supports repeat compilation of the root
    /// package.
    /// In the event the the project config changes this will need to be
    /// discarded and reloaded to handle any changes to dependencies.
    compiler: Option<LspProjectCompiler<FileSystemProxy<ProjectIO>>>,

    fs_proxy: FileSystemProxy<ProjectIO>,

    config: Option<PackageConfig>,

    feedback: FeedbackBookKeeper,
    modules_compiled_since_last_feedback: Vec<PathBuf>,

    // Used to publish progress notifications to the client without waiting for
    // the usual request-response loop.
    progress_reporter: ProgressReporter<'a>,
}

impl<'a> LanguageServer<'a> {
    pub fn new(
        config: Option<PackageConfig>,
        progress_reporter: ProgressReporter<'a>,
    ) -> Result<Self> {
        let project_root = std::env::current_dir().expect("Project root");
        let mut language_server = Self {
            modules_compiled_since_last_feedback: vec![],
            feedback: FeedbackBookKeeper::default(),
            fs_proxy: FileSystemProxy::new(ProjectIO::new()),
            compiler: None,
            progress_reporter,
            project_root,
            config,
        };
        language_server.create_new_compiler()?;
        Ok(language_server)
    }

    pub fn compile_please(&mut self) -> Feedback {
        self.notified(Self::compile)
    }

    /// Compile the project if we are in one. Otherwise do nothing.
    fn compile(&mut self) -> Result<(), Error> {
        self.progress_reporter.started();
        let result = match self.compiler.as_mut() {
            Some(compiler) => compiler.compile(),
            None => Ok(vec![]),
        };
        self.progress_reporter.finished();

        let modules = result?;
        self.modules_compiled_since_last_feedback
            .extend(modules.into_iter());

        Ok(())
    }

    fn take_warnings(&mut self) -> Vec<Warning> {
        if let Some(compiler) = self.compiler.as_mut() {
            compiler.warnings.take()
        } else {
            vec![]
        }
    }

    pub fn create_new_compiler(&mut self) -> Result<(), Error> {
        if let Some(config) = self.config.as_ref() {
            let compiler = LspProjectCompiler::new(config.clone(), self.fs_proxy.clone())?;
            self.compiler = Some(compiler);
        }
        Ok(())
    }

    pub fn text_document_did_open(&mut self, params: DidOpenTextDocumentParams) -> Feedback {
        self.notified(|this| {
            // A file opened in the editor which might be unsaved so store a copy of the new content in memory and compile
            let path = params.text_document.uri.path().to_string();
            this.fs_proxy
                .write_mem_cache(Path::new(path.as_str()), &params.text_document.text)?;
            this.compile()?;
            Ok(())
        })
    }

    pub fn text_document_did_save(&mut self, params: DidSaveTextDocumentParams) -> Feedback {
        self.notified(|this| {
            // The file is in sync with the file system, discard our cache of the changes
            this.fs_proxy
                .delete_mem_cache(Path::new(params.text_document.uri.path()))?;
            // The files on disc have changed, so compile the project with the new changes
            this.compile()?;
            Ok(())
        })
    }

    pub fn text_document_did_close(&mut self, params: DidCloseTextDocumentParams) -> Feedback {
        self.notified(|this| {
            // The file is in sync with the file system, discard our cache of the changes
            this.fs_proxy
                .delete_mem_cache(Path::new(params.text_document.uri.path()))?;
            Ok(())
        })
    }

    pub fn text_document_did_change(&mut self, params: DidChangeTextDocumentParams) -> Feedback {
        self.notified(|this| {
            // A file has changed in the editor so store a copy of the new content in memory and compile
            let path = params.text_document.uri.path().to_string();
            if let Some(changes) = params.content_changes.into_iter().next() {
                this.fs_proxy
                    .write_mem_cache(Path::new(path.as_str()), changes.text.as_str())?;
                this.compile()?;
            }
            Ok(())
        })
    }

    // TODO: test local variables
    // TODO: test same module constants
    // TODO: test imported module constants
    // TODO: test unqualified imported module constants
    // TODO: test same module records
    // TODO: test imported module records
    // TODO: test unqualified imported module records
    // TODO: test same module functions
    // TODO: test module function calls
    // TODO: test different package module function calls
    //
    //
    //
    // TODO: implement unqualified imported module functions
    // TODO: implement goto definition of modules that do not belong to the top
    // level package.
    //
    pub fn goto_definition(
        &mut self,
        params: lsp::GotoDefinitionParams,
    ) -> Response<Option<lsp::Location>> {
        self.respond(|this| {
            let params = params.text_document_position_params;
            let (line_numbers, node) = match this.node_at_position(&params) {
                Some(location) => location,
                None => return Ok(None),
            };

            let location = match node.definition_location() {
                Some(location) => location,
                None => return Ok(None),
            };

            let (uri, line_numbers) = match location.module {
                None => (params.text_document.uri, &line_numbers),
                Some(name) => {
                    let module = match this
                        .compiler
                        .as_ref()
                        .and_then(|compiler| compiler.sources.get(name))
                    {
                        Some(module) => module,
                        // TODO: support goto definition for functions defined in
                        // different packages. Currently it is not possible as the
                        // required LineNumbers and source file path information is
                        // not stored in the module metadata.
                        None => return Ok(None),
                    };
                    let url = Url::parse(&format!("file:///{}", &module.path))
                        .expect("goto definition URL parse");
                    (url, &module.line_numbers)
                }
            };
            let range = src_span_to_lsp_range(location.span, line_numbers);

            Ok(Some(lsp::Location { uri, range }))
        })
    }

    // TODO: function & constructor labels
    // TODO: module types (including private)
    // TODO: module values (including private)
    // TODO: locally defined variables
    // TODO: imported module values
    // TODO: imported module types
    // TODO: record accessors
    pub fn completion(
        &mut self,
        params: lsp::CompletionParams,
    ) -> Response<Option<Vec<lsp::CompletionItem>>> {
        self.respond(|this| {
            let found = this
                .node_at_position(&params.text_document_position)
                .map(|(_, found)| found);

            Ok(match found {
                // TODO: test
                None | Some(Located::Statement(Statement::Import(Import { .. }))) => {
                    this.completion_for_import()
                }

                // TODO: autocompletion for other statements
                Some(Located::Statement(_expression)) => None,

                // TODO: autocompletion for expressions
                Some(Located::Expression(_expression)) => None,
            })
        })
    }

    fn respond<T>(&mut self, handler: impl FnOnce(&Self) -> Result<T>) -> Response<T> {
        let result = handler(self);
        let warnings = self.take_warnings();
        let modules = self.modules_compiled_since_last_feedback.drain(..);
        match result {
            Ok(payload) => Response {
                payload: Some(payload),
                feedback: self.feedback.diagnostics(modules, warnings),
            },
            Err(e) => Response {
                payload: None,
                feedback: self.feedback.diagnostics_with_error(e, modules, warnings),
            },
        }
    }

    fn notified(&mut self, handler: impl FnOnce(&mut Self) -> Result<()>) -> Feedback {
        let result = handler(self);
        let warnings = self.take_warnings();
        let modules = self.modules_compiled_since_last_feedback.drain(..);
        match result {
            Ok(()) => self.feedback.diagnostics(modules, warnings),
            Err(e) => self.feedback.diagnostics_with_error(e, modules, warnings),
        }
    }

    pub fn format(&mut self, params: lsp::DocumentFormattingParams) -> Response<Vec<TextEdit>> {
        self.respond(|this| {
            let path = params.text_document.uri.path();
            let mut new_text = String::new();

            let src = this.fs_proxy.read(Path::new(path))?.into();
            gleam_core::format::pretty(&mut new_text, &src, Path::new(path))?;
            let line_count = src.lines().count() as u32;

            let edit = TextEdit {
                range: Range::new(Position::new(0, 0), Position::new(line_count, 0)),
                new_text,
            };
            Ok(vec![edit])
        })
    }

    fn completion_for_import(&self) -> Option<Vec<lsp::CompletionItem>> {
        let compiler = self.compiler.as_ref()?;
        // TODO: Test
        let dependencies_modules = compiler
            .project_compiler
            .get_importable_modules()
            .keys()
            .map(|name| name.to_string());
        // TODO: Test
        let project_modules = compiler
            .modules
            .iter()
            // TODO: We should autocomplete test modules if we are in the test dir
            // TODO: Test
            .filter(|(_name, module)| module.origin.is_src())
            .map(|(name, _module)| name)
            .cloned();
        let modules = dependencies_modules
            .chain(project_modules)
            .map(|label| lsp::CompletionItem {
                label,
                kind: None,
                documentation: None,
                ..Default::default()
            })
            .collect();
        Some(modules)
    }

    pub fn hover(&mut self, params: lsp::HoverParams) -> Response<Option<Hover>> {
        self.respond(|this| {
            let params = params.text_document_position_params;

            let (line_numbers, found) = match this.node_at_position(&params) {
                Some(value) => value,
                None => return Ok(None),
            };

            let expression = match found {
                Located::Expression(expression) => expression,
                Located::Statement(_) => return Ok(None),
            };

            // Show the type of the hovered node to the user
            let type_ = Printer::new().pretty_print(expression.type_().as_ref(), 0);
            let contents = format!(
                "```gleam
{type_}
```"
            );
            Ok(Some(Hover {
                contents: HoverContents::Scalar(MarkedString::String(contents)),
                range: Some(src_span_to_lsp_range(expression.location(), &line_numbers)),
            }))
        })
    }

    fn node_at_position(
        &self,
        params: &lsp::TextDocumentPositionParams,
    ) -> Option<(LineNumbers, Located<'_>)> {
        let module = self.module_for_uri(&params.text_document.uri);
        let module = module?;
        let line_numbers = LineNumbers::new(&module.code);
        let byte_index = line_numbers.byte_index(params.position.line, params.position.character);
        let node = module.find_node(byte_index);
        let node = node?;
        Some((line_numbers, node))
    }

    fn module_for_uri(&self, uri: &Url) -> Option<&Module> {
        self.compiler.as_ref().and_then(|compiler| {
            let module_name =
                uri_to_module_name(uri, &self.project_root).expect("uri to module name");
            compiler.modules.get(&module_name)
        })
    }
}
