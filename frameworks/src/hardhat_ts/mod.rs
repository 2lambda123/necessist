use super::{
    ts_utils, AbstractTypes, GenericVisitor, MaybeNamed, Named, ParseLow, RunHigh, Spanned,
    WalkDirResult,
};
use anyhow::{anyhow, Result};
use assert_cmd::output::OutputError;
use if_chain::if_chain;
use lazy_static::lazy_static;
use log::debug;
use necessist_core::{
    framework::Postprocess, source_warn, LightContext, LineColumn, SourceFile, Span, WarnFlags,
    Warning,
};
use regex::Regex;
use std::{
    cell::RefCell,
    collections::BTreeMap,
    convert::Infallible,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};
use subprocess::{Exec, NullFile};
use swc_core::{
    common::{BytePos, Loc, SourceMap, Span as SwcSpan, Spanned as SwcSpanned, SyntaxContext},
    ecma::{
        ast::{
            ArrowExpr, AwaitExpr, BlockStmtOrExpr, CallExpr, Callee, EsVersion, Expr, ExprStmt,
            Invalid, Lit, MemberExpr, MemberProp, Module, Stmt, Str,
        },
        atoms::JsWord,
        parser::{lexer::Lexer, Parser, StringInput, Syntax, TsConfig},
    },
};

mod storage;
use storage::Storage;

mod visitor;
use visitor::visit;

static INVALID: Expr = Expr::Invalid(Invalid {
    span: SwcSpan {
        lo: BytePos(0),
        hi: BytePos(0),
        ctxt: SyntaxContext::empty(),
    },
});

#[derive(Debug, Eq, PartialEq)]
enum ItMessageState {
    NotFound,
    Found,
    WarningEmitted,
}

impl Default for ItMessageState {
    fn default() -> Self {
        Self::NotFound
    }
}

pub struct HardhatTs {
    source_map: Rc<SourceMap>,
    span_it_message_map: BTreeMap<Span, String>,
    test_file_it_message_state_map: RefCell<BTreeMap<PathBuf, BTreeMap<String, ItMessageState>>>,
}

impl HardhatTs {
    pub fn applicable(context: &LightContext) -> Result<bool> {
        context
            .root
            .join("hardhat.config.ts")
            .try_exists()
            .map_err(Into::into)
    }

    pub fn new() -> Self {
        Self {
            source_map: Rc::default(),
            span_it_message_map: BTreeMap::new(),
            test_file_it_message_state_map: RefCell::new(BTreeMap::new()),
        }
    }
}

lazy_static! {
    static ref LINE_WITH_TIME_RE: Regex = {
        // smoelius: The initial `.` is the check mark.
        #[allow(clippy::unwrap_used)]
        Regex::new(r"^\s*. (.*) \(.*\)$").unwrap()
    };
    static ref LINE_WITHOUT_TIME_RE: Regex = {
        #[allow(clippy::unwrap_used)]
        Regex::new(r"^\s*. (.*)$").unwrap()
    };
}

#[derive(Clone, Copy)]
pub struct Test<'ast> {
    it_message: &'ast JsWord,
    stmts: &'ast Vec<Stmt>,
}

pub struct SourceMapped<'ast, T> {
    source_map: &'ast Rc<SourceMap>,
    node: &'ast T,
}

impl<'ast, T> Clone for SourceMapped<'ast, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'ast, T> Copy for SourceMapped<'ast, T> {}

impl<'ast, T: PartialEq> PartialEq for SourceMapped<'ast, T> {
    fn eq(&self, other: &Self) -> bool {
        self.node.eq(other.node)
    }
}

impl<'ast, T: Eq> Eq for SourceMapped<'ast, T> {}

pub struct Types;

impl AbstractTypes for Types {
    type Storage<'ast> = Storage<'ast>;
    type File = (Rc<SourceMap>, Module);
    type Test<'ast> = Test<'ast>;
    type Statement<'ast> = SourceMapped<'ast, Stmt>;
    type Expression<'ast> = SourceMapped<'ast, Expr>;
    type Await<'ast> = &'ast AwaitExpr;
    type Field<'ast> = SourceMapped<'ast, MemberExpr>;
    type Call<'ast> = SourceMapped<'ast, CallExpr>;
    type MacroCall<'ast> = Infallible;
}

impl<'ast> Named for Test<'ast> {
    fn name(&self) -> String {
        self.it_message.to_string()
    }
}

impl<'ast> MaybeNamed for <Types as AbstractTypes>::Expression<'ast> {
    fn name(&self) -> Option<String> {
        #[allow(clippy::match_same_arms)]
        match self.node {
            Expr::Await(_) => None,
            Expr::Member(member) => {
                <<Types as AbstractTypes>::Field<'ast> as MaybeNamed>::name(&SourceMapped {
                    source_map: self.source_map,
                    node: member,
                })
            }
            Expr::Call(call) => {
                <<Types as AbstractTypes>::Call<'ast> as MaybeNamed>::name(&SourceMapped {
                    source_map: self.source_map,
                    node: call,
                })
            }
            Expr::Ident(ident) => Some(ident.as_ref().to_owned()),
            _ => None,
        }
    }
}

impl<'ast> MaybeNamed for <Types as AbstractTypes>::Field<'ast> {
    fn name(&self) -> Option<String> {
        if let MemberProp::Ident(ident) = &self.node.prop {
            Some(ident.as_ref().to_owned())
        } else {
            None
        }
    }
}

impl<'ast> MaybeNamed for <Types as AbstractTypes>::Call<'ast> {
    fn name(&self) -> Option<String> {
        if_chain! {
            if let Callee::Expr(callee) = &self.node.callee;
            if let Expr::Ident(ident) = &**callee;
            then {
                Some(ident.as_ref().to_owned())
            } else {
                None
            }
        }
    }
}

impl<'ast> Spanned for <Types as AbstractTypes>::Statement<'ast> {
    fn span(&self, source_file: &SourceFile) -> Span {
        SwcSpanned::span(self.node).to_internal_span(self.source_map, source_file)
    }
}

impl<'ast> Spanned for <Types as AbstractTypes>::Field<'ast> {
    fn span(&self, source_file: &SourceFile) -> Span {
        let mut span = SwcSpanned::span(self.node).to_internal_span(self.source_map, source_file);
        let span_obj =
            SwcSpanned::span(&self.node.obj).to_internal_span(self.source_map, source_file);
        span.start = span_obj.end;
        span
    }
}

impl<'ast> Spanned for <Types as AbstractTypes>::Call<'ast> {
    fn span(&self, source_file: &SourceFile) -> Span {
        SwcSpanned::span(self.node).to_internal_span(self.source_map, source_file)
    }
}

impl ParseLow for HardhatTs {
    type Types = Types;

    const IGNORED_FUNCTIONS: Option<&'static [&'static str]> =
        Some(&["assert", "assert.*", "expect"]);

    const IGNORED_MACROS: Option<&'static [&'static str]> = None;

    const IGNORED_METHODS: Option<&'static [&'static str]> = Some(&["toNumber", "toString"]);

    fn walk_dir(root: &Path) -> Box<dyn Iterator<Item = WalkDirResult>> {
        Box::new(
            walkdir::WalkDir::new(root.join("test"))
                .into_iter()
                .filter_entry(|entry| {
                    let path = entry.path();
                    !path.is_file() || path.extension() == Some(OsStr::new("ts"))
                }),
        )
    }

    fn parse_file(&self, test_file: &Path) -> Result<<Self::Types as AbstractTypes>::File> {
        let source_file = self.source_map.load_file(test_file)?;
        let lexer = Lexer::new(
            Syntax::Typescript(TsConfig::default()),
            EsVersion::default(),
            StringInput::from(&*source_file),
            None,
        );
        let mut parser = Parser::new_from(lexer);
        parser
            .parse_typescript_module()
            .map(|module| (self.source_map.clone(), module))
            .map_err(|error| anyhow!(format!("{error:?}")))
    }

    fn storage_from_file<'ast>(
        &self,
        file: &'ast <Self::Types as AbstractTypes>::File,
    ) -> <Self::Types as AbstractTypes>::Storage<'ast> {
        Storage::new(file)
    }

    fn visit_file<'ast>(
        generic_visitor: GenericVisitor<'_, '_, '_, 'ast, Self>,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        file: &'ast <Self::Types as AbstractTypes>::File,
    ) -> Result<Vec<Span>> {
        Ok(visit(generic_visitor, storage, &file.1))
    }

    fn on_candidate_found(
        &mut self,
        _context: &LightContext,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'_>>,
        test_name: &str,
        span: &Span,
    ) {
        self.set_span_it_message(span, test_name.to_owned());
    }

    fn test_statements<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        test: <Self::Types as AbstractTypes>::Test<'ast>,
    ) -> Vec<<Self::Types as AbstractTypes>::Statement<'ast>> {
        test.stmts
            .iter()
            .map(|stmt| SourceMapped {
                source_map: storage.borrow().source_map,
                node: stmt,
            })
            .collect()
    }

    fn statement_is_expression<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        statement: <Self::Types as AbstractTypes>::Statement<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Expression<'ast>> {
        if let Stmt::Expr(ExprStmt { expr, .. }) = statement.node {
            Some(SourceMapped {
                source_map: storage.borrow().source_map,
                node: expr,
            })
        } else {
            None
        }
    }

    fn statement_is_control<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        statement: <Self::Types as AbstractTypes>::Statement<'ast>,
    ) -> bool {
        matches!(
            statement.node,
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Return(_)
        )
    }

    fn statement_is_declaration<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        statement: <Self::Types as AbstractTypes>::Statement<'ast>,
    ) -> bool {
        matches!(statement.node, Stmt::Decl(_))
    }

    fn expression_is_await<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Await<'ast>> {
        if let Expr::Await(await_) = expression.node {
            Some(await_)
        } else {
            None
        }
    }

    fn expression_is_field<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Field<'ast>> {
        if let Expr::Member(member) = expression.node {
            Some(SourceMapped {
                source_map: storage.borrow().source_map,
                node: member,
            })
        } else {
            None
        }
    }

    fn expression_is_call<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::Call<'ast>> {
        if let Expr::Call(call) = expression.node {
            Some(SourceMapped {
                source_map: storage.borrow().source_map,
                node: call,
            })
        } else {
            None
        }
    }

    fn expression_is_macro_call<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        _expression: <Self::Types as AbstractTypes>::Expression<'ast>,
    ) -> Option<<Self::Types as AbstractTypes>::MacroCall<'ast>> {
        None
    }

    fn await_arg<'ast>(
        &self,
        storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        await_: <Self::Types as AbstractTypes>::Await<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        SourceMapped {
            source_map: storage.borrow().source_map,
            node: &*await_.arg,
        }
    }

    fn field_base<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        field: <Self::Types as AbstractTypes>::Field<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        SourceMapped {
            source_map: field.source_map,
            node: &*field.node.obj,
        }
    }

    fn call_callee<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        call: <Self::Types as AbstractTypes>::Call<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        if let Callee::Expr(expr) = &call.node.callee {
            SourceMapped {
                source_map: call.source_map,
                node: expr,
            }
        } else {
            SourceMapped {
                source_map: call.source_map,
                node: &INVALID,
            }
        }
    }

    fn macro_call_callee<'ast>(
        &self,
        _storage: &RefCell<<Self::Types as AbstractTypes>::Storage<'ast>>,
        _macro_call: <Self::Types as AbstractTypes>::MacroCall<'ast>,
    ) -> <Self::Types as AbstractTypes>::Expression<'ast> {
        unreachable!()
    }
}

fn is_it_call_stmt(stmt: &Stmt) -> Option<Test<'_>> {
    if let Stmt::Expr(ExprStmt { expr, .. }) = stmt {
        is_it_call_expr(expr)
    } else {
        None
    }
}

fn is_it_call_expr(expr: &Expr) -> Option<Test<'_>> {
    if_chain! {
        if let Expr::Call(CallExpr {
            callee: Callee::Expr(callee),
            args,
            ..
        }) = expr;
        if let Expr::Ident(ident) = &**callee;
        if ident.as_ref() == "it";
        if let [arg0, arg1] = args.as_slice();
        if let Expr::Lit(Lit::Str(Str { value, .. })) = &*arg0.expr;
        if let Expr::Arrow(ArrowExpr { body, .. }) = &*arg1.expr;
        if let BlockStmtOrExpr::BlockStmt(block) = &**body;
        then {
            Some(Test {
                it_message: value,
                stmts: &block.stmts,
            })
        } else {
            None
        }
    }
}

impl RunHigh for HardhatTs {
    fn dry_run(&self, context: &LightContext, test_file: &Path) -> Result<()> {
        ts_utils::install_node_modules(context)?;

        compile(context)?;

        let mut command = Command::new("npx");
        command.current_dir(context.root.as_path());
        command.args(["hardhat", "test", &test_file.to_string_lossy()]);
        command.args(&context.opts.args);

        debug!("{:?}", command);

        let output = command.output()?;
        if !output.status.success() {
            return Err(OutputError::new(output).into());
        }

        let mut test_file_it_message_state_map = self.test_file_it_message_state_map.borrow_mut();
        let it_message_state_map = test_file_it_message_state_map
            .entry(test_file.to_path_buf())
            .or_insert_with(Default::default);

        let stdout = std::str::from_utf8(&output.stdout)?;
        for line in stdout.lines() {
            if let Some(captures) = LINE_WITH_TIME_RE
                .captures(line)
                .or_else(|| LINE_WITHOUT_TIME_RE.captures(line))
            {
                assert!(captures.len() == 2);
                it_message_state_map.insert(captures[1].to_string(), ItMessageState::Found);
            }
        }

        Ok(())
    }

    fn exec(
        &self,
        context: &LightContext,
        span: &Span,
    ) -> Result<Option<(Exec, Option<Box<Postprocess>>)>> {
        if let Err(error) = compile(context) {
            debug!("{}", error);
            return Ok(None);
        }

        #[allow(clippy::expect_used)]
        let it_message = self
            .span_it_message_map
            .get(span)
            .expect("`it` message is not set");

        let mut test_file_it_message_state_map = self.test_file_it_message_state_map.borrow_mut();
        #[allow(clippy::expect_used)]
        let it_message_state_map = test_file_it_message_state_map
            .get_mut(span.source_file.as_ref())
            .expect("Source file is not in map");

        let state = it_message_state_map
            .entry(it_message.clone())
            .or_insert_with(Default::default);
        if *state != ItMessageState::Found {
            if *state == ItMessageState::NotFound {
                source_warn(
                    context,
                    Warning::ItMessageNotFound,
                    span,
                    &format!("`it` message {it_message:?} was not found during dry run"),
                    WarnFlags::empty(),
                )?;
                *state = ItMessageState::WarningEmitted;
            }
            // smoelius: Returning `None` here causes Necessist to associate `Outcome::Nonbuildable`
            // with this span. This is not ideal, but there is no ideal choice for this situation
            // currently.
            return Ok(None);
        }

        let mut exec = Exec::cmd("npx");
        exec = exec.cwd(context.root.as_path());
        exec = exec.args(&["hardhat", "test", &span.source_file.to_string_lossy()]);
        exec = exec.args(&context.opts.args);
        exec = exec.stdout(NullFile);
        exec = exec.stderr(NullFile);

        debug!("{:?}", exec);

        Ok(Some((exec, None)))
    }
}

impl HardhatTs {
    fn set_span_it_message(&mut self, span: &Span, it_message: String) {
        self.span_it_message_map.insert(span.clone(), it_message);
    }
}

fn compile(context: &LightContext) -> Result<()> {
    let mut command = Command::new("npx");
    command.current_dir(context.root.as_path());
    command.args(["hardhat", "compile"]);
    command.args(&context.opts.args);

    debug!("{:?}", command);

    let output = command.output()?;
    if !output.status.success() {
        return Err(OutputError::new(output).into());
    };
    Ok(())
}

trait ToInternalSpan {
    fn to_internal_span(&self, source_map: &SourceMap, source_file: &SourceFile) -> Span;
}

impl ToInternalSpan for SwcSpan {
    fn to_internal_span(&self, source_map: &SourceMap, source_file: &SourceFile) -> Span {
        Span {
            source_file: source_file.clone(),
            start: self.lo.to_line_column(source_map),
            end: self.hi.to_line_column(source_map),
        }
    }
}

trait ToLineColumn {
    fn to_line_column(&self, source_map: &SourceMap) -> LineColumn;
}

impl ToLineColumn for BytePos {
    fn to_line_column(&self, source_map: &SourceMap) -> LineColumn {
        let Loc {
            line, col_display, ..
        } = source_map.lookup_char_pos(*self);
        LineColumn {
            line,
            column: col_display,
        }
    }
}
