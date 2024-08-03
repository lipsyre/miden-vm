mod context;
mod errors;
mod passes;

pub use self::context::AnalysisContext;
pub use self::errors::{SemanticAnalysisError, SyntaxError};

use self::passes::{ConstEvalVisitor, VerifyInvokeTargets};

use crate::{ast::*, diagnostics::SourceFile, LibraryPath, Spanned};
use alloc::collections::BTreeSet;
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};

/// Constructs and validates a [Module], given the forms constituting the module body.
///
/// As part of this process, the following is also done:
///
/// * Documentation comments are attached to items they decorate
/// * Import table is constructed
/// * Symbol resolution is performed:
///   * Constants referenced by name are replaced with the value of that constant.
///   * Calls to imported procedures are resolved concretely
/// * Semantic analysis is performed on the module to validate it
pub fn analyze(
    source: Arc<SourceFile>,
    kind: ModuleKind,
    path: LibraryPath,
    forms: Vec<Form>,
    warnings_as_errors: bool,
) -> Result<Box<Module>, SyntaxError> {
    let mut analyzer = AnalysisContext::new(source.clone());
    analyzer.set_warnings_as_errors(warnings_as_errors);

    let mut module = Box::new(Module::new(kind, path).with_source_file(Some(source)));

    let mut forms = VecDeque::from(forms);
    let mut docs = None;
    while let Some(form) = forms.pop_front() {
        match form {
            Form::ModuleDoc(docstring) => {
                assert!(docs.is_none());
                module.set_docs(Some(docstring));
            }
            Form::Doc(docstring) => {
                if let Some(unused) = docs.replace(docstring) {
                    analyzer.error(SemanticAnalysisError::UnusedDocstring {
                        span: unused.span(),
                    });
                }
            }
            Form::Constant(constant) => {
                analyzer.define_constant(constant.with_docs(docs.take()))?;
            }
            Form::Import(import) => {
                if let Some(docs) = docs.take() {
                    analyzer.error(SemanticAnalysisError::ImportDocstring { span: docs.span() });
                }
                define_import(import, &mut module, &mut analyzer)?;
            }
            Form::Procedure(export @ Export::Alias(_)) => match kind {
                ModuleKind::Kernel => {
                    docs.take();
                    analyzer.error(SemanticAnalysisError::ReexportFromKernel {
                        span: export.span(),
                    });
                }
                ModuleKind::Executable => {
                    docs.take();
                    analyzer.error(SemanticAnalysisError::UnexpectedExport {
                        span: export.span(),
                    });
                }
                ModuleKind::Library => {
                    define_procedure(export.with_docs(docs.take()), &mut module, &mut analyzer)?;
                }
            },
            Form::Procedure(export) => match kind {
                ModuleKind::Executable
                    if export.visibility().is_exported() && !export.is_main() =>
                {
                    docs.take();
                    analyzer.error(SemanticAnalysisError::UnexpectedExport {
                        span: export.span(),
                    });
                }
                _ => {
                    define_procedure(export.with_docs(docs.take()), &mut module, &mut analyzer)?;
                }
            },
            Form::Begin(body) if matches!(kind, ModuleKind::Executable) => {
                let docs = docs.take();
                let source_file = analyzer.source_file();
                let procedure =
                    Procedure::new(body.span(), Visibility::Public, ProcedureName::main(), 0, body)
                        .with_docs(docs)
                        .with_source_file(Some(source_file));
                define_procedure(Export::Procedure(procedure), &mut module, &mut analyzer)?;
            }
            Form::Begin(body) => {
                docs.take();
                analyzer.error(SemanticAnalysisError::UnexpectedEntrypoint { span: body.span() });
            }
        }
    }

    if let Some(unused) = docs.take() {
        analyzer.error(SemanticAnalysisError::UnusedDocstring {
            span: unused.span(),
        });
    }

    if matches!(kind, ModuleKind::Executable) && !module.has_entrypoint() {
        analyzer.error(SemanticAnalysisError::MissingEntrypoint);
    }

    analyzer.has_failed()?;

    // Run procedure checks
    visit_procedures(&mut module, &mut analyzer)?;

    // Check unused imports
    for import in module.imports() {
        if !import.is_used() {
            analyzer.error(SemanticAnalysisError::UnusedImport {
                span: import.span(),
            });
        }
    }

    analyzer.into_result().map(move |_| module)
}

/// Visit all of the procedures of the current analysis context,
/// and apply various transformation and analysis passes.
///
/// When this function returns, all local analysis is complete,
/// and all that remains is construction of a module graph and
/// global program analysis to perform any remaining transformations.
fn visit_procedures(
    module: &mut Module,
    analyzer: &mut AnalysisContext,
) -> Result<(), SyntaxError> {
    let is_kernel = module.is_kernel();
    let locals = BTreeSet::from_iter(module.procedures().map(|p| p.name().clone()));
    let mut procedures = VecDeque::from(core::mem::take(&mut module.procedures));
    while let Some(procedure) = procedures.pop_front() {
        match procedure {
            Export::Procedure(mut procedure) => {
                // Rewrite visibility for exported kernel procedures
                if is_kernel && matches!(procedure.visibility(), Visibility::Public) {
                    procedure.set_visibility(Visibility::Syscall);
                }

                // Evaluate all named immediates to their concrete values
                {
                    let mut visitor = ConstEvalVisitor::new(analyzer);
                    visitor.visit_mut_procedure(&mut procedure);
                }

                // Next, verify invoke targets:
                //
                // * Kernel procedures cannot use `syscall` or `call`
                // * Mark imports as used if they have at least one call to a procedure defined in
                //   that module
                // * Verify that all external callees have a matching import
                {
                    let mut visitor = VerifyInvokeTargets::new(
                        analyzer,
                        module,
                        &locals,
                        procedure.name().clone(),
                    );
                    visitor.visit_mut_procedure(&mut procedure);
                }
                module.procedures.push(Export::Procedure(procedure));
            }
            Export::Alias(mut alias) => {
                // Resolve the underlying import, and expand the `target`
                // to its fully-qualified path. This is needed because after
                // parsing, the path only contains the last component,
                // e.g. `u64` of `std::math::u64`.
                let is_absolute = alias.is_absolute();
                if !is_absolute {
                    if let AliasTarget::ProcedurePath(ref mut target) = alias.target_mut() {
                        let imported_module =
                            target.module.namespace().to_ident().with_span(target.span);
                        if let Some(import) = module.resolve_import_mut(&imported_module) {
                            target.module = import.path.clone();
                            // Mark the backing import as used
                            import.uses += 1;
                        } else {
                            // Missing import
                            analyzer
                                .error(SemanticAnalysisError::MissingImport { span: alias.span() });
                        }
                    }
                }
                module.procedures.push(Export::Alias(alias));
            }
        }
    }

    Ok(())
}

fn define_import(
    import: Import,
    module: &mut Module,
    context: &mut AnalysisContext,
) -> Result<(), SyntaxError> {
    if let Err(err) = module.define_import(import) {
        match err {
            SemanticAnalysisError::ImportConflict { .. } => {
                // Proceed anyway, to try and capture more errors
                context.error(err);
            }
            err => {
                // We can't proceed without producing a bunch of errors
                context.error(err);
                context.has_failed()?;
            }
        }
    }

    Ok(())
}

fn define_procedure(
    export: Export,
    module: &mut Module,
    context: &mut AnalysisContext,
) -> Result<(), SyntaxError> {
    let name = export.name().clone();
    if let Err(err) = module.define_procedure(export) {
        match err {
            SemanticAnalysisError::SymbolConflict { .. } => {
                // Proceed anyway, to try and capture more errors
                context.error(err);
            }
            err => {
                // We can't proceed without producing a bunch of errors
                context.error(err);
                context.has_failed()?;
            }
        }
    }

    context.register_procedure_name(name);

    Ok(())
}
