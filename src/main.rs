use std::{env, fs, process};

use veryl_analyzer::{Analyzer, Context, ir::Ir};
use veryl_metadata::Metadata;
use veryl_parser::Parser;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: veryl-ir-sample <file.veryl>");
        process::exit(1);
    });

    let code = fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("failed to read {path}: {e}");
        process::exit(1);
    });

    let metadata = Metadata::create_default("prj").unwrap_or_else(|e| {
        eprintln!("failed to create metadata: {e:?}");
        process::exit(1);
    });

    let parser = Parser::parse(&code, &path).unwrap_or_else(|e| {
        eprintln!("parse error: {e:?}");
        process::exit(1);
    });

    let analyzer = Analyzer::new(&metadata);
    let mut context = Context::default();
    let mut ir = Ir::default();

    let mut errors = vec![];
    errors.append(&mut analyzer.analyze_pass1("prj", &parser.veryl));
    errors.append(&mut Analyzer::analyze_post_pass1());
    errors.append(&mut analyzer.analyze_pass2("prj", &parser.veryl, &mut context, Some(&mut ir)));
    errors.append(&mut Analyzer::analyze_post_pass2());

    if !errors.is_empty() {
        eprintln!("analyzer errors:");
        for e in &errors {
            eprintln!("{e:?}");
        }
        eprintln!();
    }

    println!("{}", ir);
}
