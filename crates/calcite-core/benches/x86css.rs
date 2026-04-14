use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};

use calcite_core::eval::Evaluator;
use calcite_core::parser::parse_css;
use calcite_core::State;

fn load_css() -> String {
    std::fs::read_to_string("../../tests/fixtures/x86css-main.css")
        .expect("x86css-main.css fixture required for benchmarks")
}

/// Try to load a v4 CSS file from output/ for realistic benchmarks.
/// Returns None if no v4 file is available (they're large and not in git).
fn load_v4_css() -> Option<String> {
    // Try smallest first
    for path in &[
        "../../output/fib.css",
        "../../output/bootle.css",
        "../../output/rogue.css",
    ] {
        if let Ok(css) = std::fs::read_to_string(path) {
            return Some(css);
        }
    }
    None
}

fn bench_parse(c: &mut Criterion) {
    let css = load_css();
    c.bench_function("parse_x86css", |b| {
        b.iter(|| parse_css(&css).unwrap());
    });
}

fn bench_setup(c: &mut Criterion) {
    let css = load_css();
    let parsed = parse_css(&css).unwrap();
    c.bench_function("evaluator_from_parsed", |b| {
        b.iter(|| Evaluator::from_parsed(&parsed));
    });
}

fn bench_tick(c: &mut Criterion) {
    let css = load_css();
    let parsed = parse_css(&css).unwrap();
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = Evaluator::from_parsed(&parsed);
    // Warm up past BIOS init
    for _ in 0..50 {
        evaluator.tick(&mut state);
    }

    c.bench_function("single_tick", |b| {
        b.iter(|| evaluator.tick(&mut state));
    });
}

fn bench_batch(c: &mut Criterion) {
    let css = load_css();
    let parsed = parse_css(&css).unwrap();
    // load_properties must be called to install the address map before from_parsed
    let mut init_state = State::default();
    init_state.load_properties(&parsed.properties);
    let mut evaluator = Evaluator::from_parsed(&parsed);

    let mut group = c.benchmark_group("batch_ticks");
    for &count in &[10, 100] {
        group.bench_function(format!("{count}_ticks"), |b| {
            b.iter_with_setup(
                || {
                    let mut state = State::default();
                    state.load_properties(&parsed.properties);
                    state
                },
                |mut state| evaluator.run_batch(&mut state, count),
            );
        });
    }
    group.finish();
}

/// Benchmark tick execution with v4 CSS (realistic DOS program).
/// Only runs if a v4 CSS file is available in output/.
fn bench_v4_tick(c: &mut Criterion) {
    let css = match load_v4_css() {
        Some(css) => css,
        None => {
            eprintln!("Skipping v4 benchmarks: no CSS files in output/");
            return;
        }
    };

    let parsed = parse_css(&css).unwrap();
    let mut state = State::default();
    state.load_properties(&parsed.properties);
    let mut evaluator = Evaluator::from_parsed(&parsed);

    // Warm up: 100 ticks to get past BIOS init
    for _ in 0..100 {
        evaluator.tick(&mut state);
    }

    let mut group = c.benchmark_group("v4");
    group.sample_size(20); // fewer samples since each tick is slower

    group.bench_function("single_tick", |b| {
        b.iter(|| evaluator.tick(&mut state));
    });

    for &count in &[10, 100] {
        group.bench_function(BenchmarkId::new("batch", count), |b| {
            b.iter(|| evaluator.run_batch(&mut state, count));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_parse, bench_setup, bench_tick, bench_batch, bench_v4_tick);
criterion_main!(benches);
