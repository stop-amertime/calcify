//! Measure pure tokenisation cost vs. full calcite parse.
//!
//! Splits parse time into:
//!   1. Reading the file into memory
//!   2. cssparser token stream, no tree-building (just consume every token)
//!   3. cssparser with nested_block walking (still no tree-building)
//!   4. Full calcite::parse_stylesheet (tree-building, the real parse)
//!
//! Tells us how much of the "parse" phase is actually cssparser tokenising
//! vs. our Expr-tree allocation.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe_parse_cost <css>");

    let t = Instant::now();
    let css = std::fs::read_to_string(&path).expect("read");
    println!("[1] read_to_string: {:.2}s  ({} bytes)", t.elapsed().as_secs_f64(), css.len());

    // Step 2: tokenise only
    let t = Instant::now();
    let mut input = cssparser::ParserInput::new(&css);
    let mut parser = cssparser::Parser::new(&mut input);
    let mut token_count = 0u64;
    loop {
        match parser.next_including_whitespace_and_comments() {
            Ok(_) => token_count += 1,
            Err(_) => break,
        }
    }
    println!("[2] tokenise only (flat): {:.2}s  ({} tokens)",
        t.elapsed().as_secs_f64(), token_count);

    // Step 3: tokenise with nesting (reset + walk into blocks)
    let t = Instant::now();
    let mut input = cssparser::ParserInput::new(&css);
    let mut parser = cssparser::Parser::new(&mut input);
    let mut nested_count = 0u64;
    walk_nested(&mut parser, &mut nested_count);
    println!("[3] tokenise with nesting: {:.2}s  ({} tokens)",
        t.elapsed().as_secs_f64(), nested_count);

    // Step 4: full calcite parse
    let t = Instant::now();
    let program = calcite_core::parser::parse_stylesheet(&css).expect("parse");
    println!("[4] full calcite parse: {:.2}s  ({} props, {} fns, {} assigns)",
        t.elapsed().as_secs_f64(),
        program.properties.len(),
        program.functions.len(),
        program.assignments.len());
}

fn walk_nested<'i, 't>(parser: &mut cssparser::Parser<'i, 't>, count: &mut u64) {
    loop {
        match parser.next() {
            Ok(token) => {
                *count += 1;
                let is_block = matches!(
                    token,
                    cssparser::Token::Function(_)
                        | cssparser::Token::ParenthesisBlock
                        | cssparser::Token::SquareBracketBlock
                        | cssparser::Token::CurlyBracketBlock
                );
                if is_block {
                    let _ = parser.parse_nested_block(
                        |inner: &mut cssparser::Parser<'i, '_>| -> Result<(), cssparser::ParseError<'i, ()>> {
                            walk_nested(inner, count);
                            Ok(())
                        },
                    );
                }
            }
            Err(_) => break,
        }
    }
}
