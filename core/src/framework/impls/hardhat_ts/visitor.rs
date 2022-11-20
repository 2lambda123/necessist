use crate::{Config, LineColumn, SourceFile, Span};
use if_chain::if_chain;
use std::{
    path::{Path, PathBuf},
    rc::Rc,
};
use swc_core::{
    common::{BytePos, Loc, SourceMap, Span as SwcSpan, Spanned},
    ecma::{
        ast::{
            CallExpr, Callee, Expr, ExprOrSpread, ExprStmt, Ident, MemberExpr, MemberProp, Module,
            Stmt, TsTypeParamInstantiation,
        },
        visit::{visit_expr, visit_stmt, Visit},
    },
};

pub(super) fn visit(
    config: &Config,
    source_map: Rc<SourceMap>,
    root: Rc<PathBuf>,
    test_file: &Path,
    module: &Module,
) -> Vec<Span> {
    let mut visitor = Visitor::new(config, source_map, root, test_file);
    visitor.visit_module(module);
    visitor.spans
}

pub struct Visitor<'config> {
    config: &'config Config,
    source_map: Rc<SourceMap>,
    source_file: SourceFile,
    in_it_call_expr: bool,
    n_stmt_leaves_visited: usize,
    spans: Vec<Span>,
}

#[allow(dead_code)]
struct MethodCall<'a> {
    pub span: &'a SwcSpan,
    pub obj: &'a Expr,
    pub path: Vec<&'a Ident>,
    pub args: &'a Vec<ExprOrSpread>,
    pub type_args: &'a Option<Box<TsTypeParamInstantiation>>,
}

#[allow(dead_code)]
struct FunctionCall<'a> {
    pub span: &'a SwcSpan,
    pub path: Vec<&'a Ident>,
    pub args: &'a Vec<ExprOrSpread>,
    pub type_args: &'a Option<Box<TsTypeParamInstantiation>>,
}

impl<'config> Visit for Visitor<'config> {
    fn visit_expr(&mut self, expr: &Expr) {
        if is_it_call_expr(expr) {
            assert!(!self.in_it_call_expr);
            self.in_it_call_expr = true;

            visit_expr(self, expr);

            assert!(self.in_it_call_expr);
            self.in_it_call_expr = false;

            return;
        }

        visit_expr(self, expr);

        if_chain! {
            if self.in_it_call_expr;
            if let Some(MethodCall {
                span,
                obj,
                path,
                args,
                ..
            }) = is_method_call(expr);
            if !is_ignored_method(&path, args);
            then {
                let mut span = *span;
                span.lo = obj.span().hi;
                assert!(span.lo <= span.hi);
                self.elevate_span(span.to_internal_span(&self.source_map, &self.source_file));
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        // smoelius: If the statement is an ignored function call, do not visit its `Member`
        // subexpressions. E.g., in a call of the form `assert.equal(...)`, do not remove
        // `.equal(...)`.
        if let Some(FunctionCall {
            args, type_args, ..
        }) = is_ignored_function_call(self.config, stmt)
        {
            for arg in args {
                self.visit_expr_or_spread(arg);
            }
            for type_arg in type_args {
                self.visit_ts_type_param_instantiation(type_arg);
            }
        } else {
            let n_before = self.n_stmt_leaves_visited;
            visit_stmt(self, stmt);
            let n_after = self.n_stmt_leaves_visited;

            // smoelius: Consider this a "leaf" if-and-only-if no "leaves" were added during the
            // recursive call.
            if n_before != n_after {
                return;
            }
            self.n_stmt_leaves_visited += 1;

            if self.in_it_call_expr
                && !matches!(
                    stmt,
                    Stmt::Break(_) | Stmt::Continue(_) | Stmt::Decl(_) | Stmt::Return(_)
                )
            {
                let span = stmt
                    .span()
                    .to_internal_span(&self.source_map, &self.source_file);
                self.elevate_span(span);
            }
        }
    }
}

impl<'config> Visitor<'config> {
    fn new(
        config: &'config Config,
        source_map: Rc<SourceMap>,
        root: Rc<PathBuf>,
        test_file: &Path,
    ) -> Self {
        Self {
            config,
            source_map,
            source_file: SourceFile::new(root, Rc::new(test_file.to_path_buf())),
            in_it_call_expr: false,
            n_stmt_leaves_visited: 0,
            spans: Vec::new(),
        }
    }

    fn elevate_span(&mut self, span: Span) {
        self.spans.push(span);
    }
}

fn is_it_call_expr(expr: &Expr) -> bool {
    if_chain! {
        if let Expr::Call(CallExpr {
            callee: Callee::Expr(callee),
            ..
        }) = expr;
        if let Expr::Ident(ident) = &**callee;
        if ident.as_ref() == "it";
        then {
            true
        } else {
            false
        }
    }
}

fn is_method_call(mut expr: &Expr) -> Option<MethodCall> {
    if let Expr::Call(CallExpr {
        span,
        callee: Callee::Expr(ref callee),
        args,
        type_args,
    }) = expr
    {
        expr = callee;
        let mut path_reversed = Vec::new();
        while let Expr::Member(MemberExpr {
            span: _,
            obj,
            prop: MemberProp::Ident(ident),
        }) = expr
        {
            expr = obj;
            path_reversed.push(ident);
        }
        if path_reversed.is_empty() {
            None
        } else {
            Some(MethodCall {
                span,
                obj: expr,
                path: {
                    path_reversed.reverse();
                    path_reversed
                },
                args,
                type_args,
            })
        }
    } else {
        None
    }
}

fn is_ignored_method(path: &[&Ident], args: &[ExprOrSpread]) -> bool {
    if let &[ident, ..] = path {
        ident.as_ref() == "should"
            || ident.as_ref() == "to"
            || ((ident.as_ref() == "toNumber" || ident.as_ref() == "toString")
                && path.len() == 1
                && args.is_empty())
    } else {
        false
    }
}

fn is_ignored_function_call<'a>(config: &Config, stmt: &'a Stmt) -> Option<FunctionCall<'a>> {
    if let Stmt::Expr(ExprStmt { ref expr, .. }) = stmt {
        let mut expr = trim_expr(expr);
        loop {
            if let Some(function_call) = is_function_call(expr) {
                return if is_ignored_function(config, &function_call.path) {
                    Some(function_call)
                } else {
                    None
                };
            } else if let Some(method_call) = is_method_call(expr) {
                expr = method_call.obj;
                continue;
            }
            break;
        }
    }
    None
}

fn trim_expr(mut expr: &Expr) -> &Expr {
    loop {
        match expr {
            Expr::Await(await_expr) => {
                expr = &await_expr.arg;
            }
            Expr::Member(member_expr) => {
                expr = &member_expr.obj;
            }
            _ => {
                break;
            }
        }
    }
    expr
}

fn is_function_call(mut expr: &Expr) -> Option<FunctionCall> {
    if let Expr::Call(CallExpr {
        span,
        callee: Callee::Expr(ref callee),
        args,
        type_args,
    }) = expr
    {
        expr = callee;
        let mut path_reversed = Vec::new();
        while let Expr::Member(MemberExpr {
            span: _,
            obj,
            prop: MemberProp::Ident(ident),
        }) = expr
        {
            expr = obj;
            path_reversed.push(ident);
        }
        if let Expr::Ident(ident) = expr {
            path_reversed.push(ident);
            Some(FunctionCall {
                span,
                path: {
                    path_reversed.reverse();
                    path_reversed
                },
                args,
                type_args,
            })
        } else {
            None
        }
    } else {
        None
    }
}

fn is_ignored_function(config: &Config, path: &[&Ident]) -> bool {
    if let &[ident, ..] = path {
        if ident.as_ref() == "assert" || (ident.as_ref() == "expect" && path.len() == 1) {
            return true;
        }
    }

    let path = path.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(".");
    config.ignored_functions.iter().any(|s| s == &path)
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
