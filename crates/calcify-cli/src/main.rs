use clap::Parser;
use std::path::PathBuf;

/// calc(ify) — JIT compiler for computational CSS.
///
/// Parses CSS files, recognises computational patterns, and evaluates
/// them efficiently. Primary target: running x86CSS faster than Chrome.
#[derive(Parser, Debug)]
#[command(name = "calcify", version, about)]
struct Cli {
    /// Path to the CSS file to evaluate.
    #[arg(short, long)]
    input: PathBuf,

    /// Number of ticks to run.
    #[arg(short = 'n', long, default_value = "1")]
    ticks: u32,

    /// Print register state after each tick.
    #[arg(short, long)]
    verbose: bool,

    /// Only parse and analyse patterns (don't evaluate).
    #[arg(long)]
    parse_only: bool,
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let css = match std::fs::read_to_string(&cli.input) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Error reading {}: {e}", cli.input.display());
            std::process::exit(1);
        }
    };

    log::info!(
        "Read {} bytes of CSS from {}",
        css.len(),
        cli.input.display()
    );

    match calcify_core::parser::parse_css(&css) {
        Ok(parsed) => {
            log::info!(
                "Parsed: {} properties, {} functions, {} assignments",
                parsed.properties.len(),
                parsed.functions.len(),
                parsed.assignments.len(),
            );

            if cli.parse_only {
                println!(
                    "Parsed: {} @property, {} @function, {} assignments",
                    parsed.properties.len(),
                    parsed.functions.len(),
                    parsed.assignments.len(),
                );
                return;
            }

            match calcify_core::pattern::compile(&parsed) {
                Ok(program) => {
                    let evaluator = calcify_core::Evaluator::new(program);
                    let mut state = calcify_core::State::default();

                    for tick in 0..cli.ticks {
                        let result = evaluator.tick(&mut state);
                        if cli.verbose {
                            println!("Tick {tick}: {} changes", result.changes.len());
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Compilation error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("Parse error: {e}");
            std::process::exit(1);
        }
    }
}
