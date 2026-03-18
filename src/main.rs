use anyhow::{Context as _, Result, anyhow};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use veryl_analyzer::ir::Ir;
use veryl_analyzer::{Analyzer, Context};
use veryl_metadata::Metadata;
use veryl_parser::Parser;

mod codegen;

fn main() -> Result<()> {
    let input = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("example.veryl"));

    let code = fs::read_to_string(&input)
        .with_context(|| format!("failed to read {}", input.display()))?;

    let metadata = Metadata::create_default("prj")?;
    let parser = Parser::parse(&code, &input.display().to_string())
        .map_err(|e| anyhow!("parse error: {e:?}"))?;

    let analyzer = Analyzer::new(&metadata);
    let mut context = Context::default();
    let mut ir = Ir::default();

    let mut errors = vec![];
    errors.extend(analyzer.analyze_pass1("prj", &parser.veryl));
    errors.extend(Analyzer::analyze_post_pass1());
    errors.extend(analyzer.analyze_pass2("prj", &parser.veryl, &mut context, Some(&mut ir)));
    errors.extend(Analyzer::analyze_post_pass2());

    if !errors.is_empty() {
        eprintln!("analyzer errors:");
        for error in &errors {
            eprintln!("  {error:?}");
        }
    }

    let output = codegen::emit(&ir)?;
    let css_path = css_output_path(&input);
    fs::write(&css_path, &output.css)
        .with_context(|| format!("failed to write {}", css_path.display()))?;
    eprintln!("wrote {}", css_path.display());

    Ok(())
}

fn css_output_path(input: &Path) -> PathBuf {
    input.with_file_name("output.css")
}
