use crate::{
    ast::TodoKind,
    diagnostic::{self, Diagnostic, Location},
    type_,
};
use debug_ignore::DebugIgnore;
use smol_str::SmolStr;
use std::{
    io::Write,
    sync::{atomic::Ordering, Arc},
};
use std::{path::PathBuf, sync::atomic::AtomicUsize};
use termcolor::Buffer;

pub trait WarningEmitterIO {
    fn emit_warning(&self, warning: Warning);
}

#[derive(Debug, Clone, Copy)]
pub struct NullWarningEmitterIO;

impl WarningEmitterIO for NullWarningEmitterIO {
    fn emit_warning(&self, _warning: Warning) {}
}

#[derive(Debug, Clone, Default)]
pub struct VectorWarningEmitterIO {
    pub warnings: Arc<std::sync::RwLock<Vec<Warning>>>,
}

impl VectorWarningEmitterIO {
    pub fn take(&self) -> Vec<Warning> {
        let mut warnings = self.write_lock();
        std::mem::take(&mut *warnings)
    }

    pub fn drain(&mut self) -> Vec<Warning> {
        let mut warnings = self.write_lock();
        std::mem::take(&mut *warnings)
    }

    fn write_lock(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Warning>> {
        self.warnings.write().expect("Vector lock poisoned")
    }
}

impl WarningEmitterIO for VectorWarningEmitterIO {
    fn emit_warning(&self, warning: Warning) {
        let mut warnings = self.write_lock();
        warnings.push(warning);
    }
}

#[derive(Debug, Clone)]
pub struct WarningEmitter {
    /// The number of warnings emitted.
    /// In the context of the project compiler this is the count for the root
    /// package only, the count is reset back to zero after the dependencies are
    /// compiled.
    count: Arc<AtomicUsize>,
    emitter: DebugIgnore<Arc<dyn WarningEmitterIO>>,
}

impl WarningEmitter {
    pub fn new(emitter: Arc<dyn WarningEmitterIO>) -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
            emitter: DebugIgnore(emitter),
        }
    }

    pub fn reset_count(&self) {
        self.count.store(0, Ordering::Relaxed);
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    pub fn emit(&self, warning: Warning) {
        _ = self.count.fetch_add(1, Ordering::Relaxed);
        self.emitter.emit_warning(warning);
    }
}

#[derive(Debug, Clone)]
pub struct TypeWarningEmitter {
    module_path: PathBuf,
    module_src: SmolStr,
    emitter: WarningEmitter,
}

impl TypeWarningEmitter {
    pub fn new(module_path: PathBuf, module_src: SmolStr, emitter: WarningEmitter) -> Self {
        Self {
            module_path,
            module_src,
            emitter,
        }
    }

    pub fn null() -> Self {
        Self {
            module_path: PathBuf::new(),
            module_src: SmolStr::new(""),
            emitter: WarningEmitter::new(Arc::new(NullWarningEmitterIO)),
        }
    }

    pub fn emit(&self, warning: crate::type_::Warning) {
        self.emitter.emit(Warning::Type {
            path: self.module_path.clone(),
            src: self.module_src.clone(),
            warning,
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Warning {
    Type {
        path: PathBuf,
        src: SmolStr,
        warning: crate::type_::Warning,
    },
}

impl Warning {
    pub fn to_diagnostic(&self) -> Diagnostic {
        match self {
            Self::Type { path, warning, src } => match warning {
                type_::Warning::Todo {
                    kind,
                    location,
                    typ,
                } => {
                    let mut text = String::new();
                    text.push_str(
                        "\
This code will crash if it is run. Be sure to finish it before
running your program.",
                    );
                    let title = match kind {
                        TodoKind::Keyword => "Todo found",
                        TodoKind::EmptyFunction => "Unimplemented function",
                        TodoKind::IncompleteUse => {
                            text.push_str(
                                "
A use expression must always be followed by at least one more
expression.",
                            );
                            "Incomplete use expression"
                        }
                    }
                    .into();
                    if !typ.is_variable() {
                        text.push_str(&format!(
                            "\n\nHint: I think its type is `{}`.\n",
                            type_::pretty::Printer::new().pretty_print(typ, 0)
                        ));
                    }

                    Diagnostic {
                        title,
                        text,
                        level: diagnostic::Level::Warning,
                        location: Some(Location {
                            path: path.to_path_buf(),
                            src: src.clone(),
                            label: diagnostic::Label {
                                text: Some("This code is incomplete".into()),
                                span: *location,
                            },
                            extra_labels: Vec::new(),
                        }),
                        hint: None,
                    }
                }

                type_::Warning::ImplicitlyDiscardedResult { location } => Diagnostic {
                    title: "Unused result value".into(),
                    text: "".into(),
                    hint: Some("If you are sure you don't need it you can assign it to `_`".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        path: path.to_path_buf(),
                        src: src.clone(),
                        label: diagnostic::Label {
                            text: Some("The Result value created here is unused".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::UnusedLiteral { location } => Diagnostic {
                    title: "Unused literal".into(),
                    text: "".into(),
                    hint: Some("You can safely remove it.".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        path: path.to_path_buf(),
                        src: src.clone(),
                        label: diagnostic::Label {
                            text: Some("This value is never used".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::NoFieldsRecordUpdate { location } => Diagnostic {
                    title: "Fieldless record update".into(),
                    text: "".into(),
                    hint: Some(
                        "Add some fields to change or replace it with the record itself.".into(),
                    ),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        path: path.to_path_buf(),
                        src: src.clone(),
                        label: diagnostic::Label {
                            text: Some("This record update doesn't change any fields.".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::AllFieldsRecordUpdate { location } => Diagnostic {
                    title: "Redundant record update".into(),
                    text: "".into(),
                    hint: Some("It is better style to use the record creation syntax.".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        src: src.clone(),
                        path: path.to_path_buf(),
                        label: diagnostic::Label {
                            text: Some("This record update specifies all fields".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::UnusedType {
                    location, imported, ..
                } => {
                    let title = if *imported {
                        "Unused imported type".into()
                    } else {
                        "Unused private type".into()
                    };
                    let label = if *imported {
                        "This imported type is never used.".into()
                    } else {
                        "This private type is never used.".into()
                    };
                    Diagnostic {
                        title,
                        text: "".into(),
                        hint: Some("You can safely remove it.".into()),
                        level: diagnostic::Level::Warning,
                        location: Some(Location {
                            src: src.clone(),
                            path: path.to_path_buf(),
                            label: diagnostic::Label {
                                text: Some(label),
                                span: *location,
                            },
                            extra_labels: Vec::new(),
                        }),
                    }
                }

                type_::Warning::UnusedConstructor {
                    location, imported, ..
                } => {
                    let title = if *imported {
                        "Unused imported item".into()
                    } else {
                        "Unused private constructor".into()
                    };
                    let label = if *imported {
                        "This imported constructor is never used.".into()
                    } else {
                        "This private constructor is never used.".into()
                    };
                    Diagnostic {
                        title,
                        text: "".into(),
                        hint: Some("You can safely remove it.".into()),
                        level: diagnostic::Level::Warning,
                        location: Some(Location {
                            src: src.clone(),
                            path: path.to_path_buf(),
                            label: diagnostic::Label {
                                text: Some(label),
                                span: *location,
                            },
                            extra_labels: Vec::new(),
                        }),
                    }
                }

                type_::Warning::UnusedImportedModule { location, .. } => Diagnostic {
                    title: "Unused imported module".into(),
                    text: "".into(),
                    hint: Some("You can safely remove it.".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        src: src.clone(),
                        path: path.to_path_buf(),
                        label: diagnostic::Label {
                            text: Some("This imported module is never used.".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::UnusedImportedValue { location, .. } => Diagnostic {
                    title: "Unused imported value".into(),
                    text: "".into(),
                    hint: Some("You can safely remove it.".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        src: src.clone(),
                        path: path.to_path_buf(),
                        label: diagnostic::Label {
                            text: Some("This imported value is never used.".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::UnusedPrivateModuleConstant { location, .. } => Diagnostic {
                    title: "Unused private constant".into(),
                    text: "".into(),
                    hint: Some("You can safely remove it.".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        src: src.clone(),
                        path: path.to_path_buf(),
                        label: diagnostic::Label {
                            text: Some("This private constant is never used.".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::UnusedPrivateFunction { location, .. } => Diagnostic {
                    title: "Unused private function".into(),
                    text: "".into(),
                    hint: Some("You can safely remove it.".into()),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        src: src.clone(),
                        path: path.to_path_buf(),
                        label: diagnostic::Label {
                            text: Some("This private function is never used.".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },

                type_::Warning::UnusedVariable { location, name, .. } => Diagnostic {
                    title: "Unused variable".into(),
                    text: "".into(),
                    hint: Some(format!("You can ignore it with an underscore: `_{name}`.")),
                    level: diagnostic::Level::Warning,
                    location: Some(Location {
                        src: src.clone(),
                        path: path.to_path_buf(),
                        label: diagnostic::Label {
                            text: Some("This variable is never used.".into()),
                            span: *location,
                        },
                        extra_labels: Vec::new(),
                    }),
                },
            },
        }
    }

    pub fn pretty(&self, buffer: &mut Buffer) {
        buffer
            .write_all(b"\n")
            .expect("error pretty buffer write space before");
        self.to_diagnostic().write(buffer);
    }

    pub fn to_pretty_string(&self) -> String {
        let mut nocolor = Buffer::no_color();
        self.pretty(&mut nocolor);
        String::from_utf8(nocolor.into_inner()).expect("Warning printing produced invalid utf8")
    }
}
