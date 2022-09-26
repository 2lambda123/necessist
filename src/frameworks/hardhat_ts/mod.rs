use super::{Interface, Postprocess};
use crate::{util, LightContext, Span};
use anyhow::{anyhow, ensure, Context, Result};
use log::debug;
use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Stdio},
    rc::Rc,
};
use subprocess::{Exec, NullFile};
use swc_common::SourceMap;
use swc_ecma_ast::EsVersion;
use swc_ecma_parser::{lexer::Lexer, Parser, StringInput, Syntax, TsConfig};
use walkdir::WalkDir;

mod visitor;
use visitor::visit;

#[derive(Debug, Default)]
pub(super) struct HardhatTs;

impl Interface for HardhatTs {
    fn applicable(&self, context: &LightContext) -> Result<bool> {
        Ok(context.root.join("hardhat.config.ts").exists())
    }

    fn parse(&mut self, context: &LightContext, test_files: &[&Path]) -> Result<Vec<Span>> {
        let mut spans = Vec::new();

        let mut visit_test_file = |test_file: &Path| -> Result<()> {
            assert!(test_file.is_absolute());
            assert!(test_file.starts_with(context.root));
            let source_map: Rc<SourceMap> = Rc::default();
            let source_file = source_map.load_file(test_file)?;
            let lexer = Lexer::new(
                Syntax::Typescript(TsConfig::default()),
                EsVersion::default(),
                StringInput::from(&*source_file),
                None,
            );
            let mut parser = Parser::new_from(lexer);
            #[allow(clippy::unwrap_used)]
            let module = parser
                .parse_typescript_module()
                .map_err(|error| anyhow!(format!("{:?}", error)))
                .with_context(|| {
                    format!(
                        "Could not parse {:?}",
                        util::strip_prefix(test_file, context.root).unwrap()
                    )
                })?;
            let visited_spans = visit(source_map, test_file, &module);
            spans.extend(visited_spans);
            Ok(())
        };

        if test_files.is_empty() {
            for entry in WalkDir::new(context.root.join("test")) {
                let entry = entry?;
                let path = entry.path();

                if path.extension() != Some(OsStr::new("ts")) {
                    continue;
                }

                visit_test_file(path)?;
            }
        } else {
            for path in test_files {
                visit_test_file(path)?;
            }
        }

        Ok(spans)
    }

    fn dry_run(&self, _context: &LightContext, test_file: &Path) -> Result<()> {
        let mut command = Command::new("npx");
        command.args(["hardhat", "test", &test_file.to_string_lossy()]);

        debug!("{:?}", command);

        let output = command.output()?;
        ensure!(output.status.success(), "{:#?}", output);
        Ok(())
    }

    fn exec(
        &self,
        _context: &LightContext,
        span: &Span,
    ) -> Result<Option<(Exec, Option<Box<Postprocess>>)>> {
        {
            let mut command = Command::new("npx");
            command.args(["hardhat", "compile"]);
            command.stdout(Stdio::null());
            command.stderr(Stdio::null());

            debug!("{:?}", command);

            let status = command.status()?;
            if !status.success() {
                return Ok(None);
            }
        }

        let mut exec = Exec::cmd("npx");
        exec = exec.args(&["hardhat", "test", &span.source_file.to_string_lossy()]);
        exec = exec.stdout(NullFile);
        exec = exec.stderr(NullFile);

        debug!("{:?}", exec);

        Ok(Some((exec, None)))
    }
}