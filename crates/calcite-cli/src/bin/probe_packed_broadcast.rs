//! Diagnostic probe: parse a packed CSS file and verify that the new
//! `recognise_packed_broadcast` pattern recognizer fires on its `--mc{N}`
//! cell assignments. Prints port summary and a few sample address mappings.

use std::path::PathBuf;

use calcite_core::pattern::packed_broadcast_write::recognise_packed_broadcast;

fn main() {
    let css_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("C:/Users/AdmT9N0CX01V65438A/Documents/src/CSS-DOS/tmp/vsync-packed.css")
        });
    eprintln!("Parsing {}...", css_path.display());
    let css = std::fs::read_to_string(&css_path).expect("read");
    let parsed = calcite_core::parser::parse_css(&css).expect("parse");
    eprintln!(
        "  {} assignments, {} properties, {} functions",
        parsed.assignments.len(),
        parsed.properties.len(),
        parsed.functions.len()
    );

    // Count --mc{N} assignments before recognition.
    let mc_count = parsed
        .assignments
        .iter()
        .filter(|a| a.property.starts_with("--mc") && a.property[4..].parse::<u32>().is_ok())
        .count();
    eprintln!("  --mc{{N}} cells in input: {}", mc_count);

    // Debug: sample first 5 properties starting with "--mc".
    let mc_samples: Vec<&str> = parsed.assignments.iter()
        .filter(|a| a.property.starts_with("--mc"))
        .take(5)
        .map(|a| a.property.as_str())
        .collect();
    eprintln!("  sample --mc* props: {:?}", mc_samples);

    // Sample any property to see naming.
    let any_samples: Vec<&str> = parsed.assignments.iter()
        .take(5)
        .map(|a| a.property.as_str())
        .collect();
    eprintln!("  sample any props: {:?}", any_samples);
    let last_samples: Vec<&str> = parsed.assignments.iter()
        .rev()
        .take(5)
        .map(|a| a.property.as_str())
        .collect();
    eprintln!("  sample last props: {:?}", last_samples);

    let n_buffer = parsed.assignments.iter().filter(|a| a.property.starts_with("--__1mc")).count();
    let n_write = parsed.assignments.iter().filter(|a| {
        let p = a.property.as_str();
        p.starts_with("--mc") && !p.starts_with("--mca") && !p.starts_with("--mcb")
    }).count();
    eprintln!("  __1mc count: {}, --mc count: {}", n_buffer, n_write);
    eprintln!("  prebuilt_packed_broadcast_ports: {}", parsed.prebuilt_packed_broadcast_ports.len());
    eprintln!("  prebuilt_broadcast_writes: {}", parsed.prebuilt_broadcast_writes.len());
    eprintln!("  fast_path_absorbed: {}", parsed.fast_path_absorbed.len());

    eprintln!();
    eprintln!("Running recognise_packed_broadcast...");
    let t = std::time::Instant::now();
    let result = recognise_packed_broadcast(&parsed.assignments);
    eprintln!("  done in {:.3}s", t.elapsed().as_secs_f64());
    eprintln!();
    eprintln!("== Recognition result ==");
    eprintln!("  ports: {}", result.ports.len());
    eprintln!("  absorbed_properties: {}", result.absorbed_properties.len());
    eprintln!("  pack: {}", result.pack);
    eprintln!();
    for (i, port) in result.ports.iter().enumerate() {
        eprintln!(
            "  port[{}]: gate={} addr={} val={} entries={}",
            i, port.gate_property, port.addr_property, port.val_property,
            port.address_map.len()
        );
        // Show three sample mappings.
        let mut samples: Vec<(&i64, &String)> = port.address_map.iter().take(3).collect();
        samples.sort_by_key(|(k, _)| *k);
        for (addr, name) in samples {
            eprintln!("    {} -> {}", addr, name);
        }
    }
    eprintln!();
    if result.ports.is_empty() {
        eprintln!("✗ Recognizer did NOT fire — investigate why.");
    } else {
        eprintln!("✓ Recognizer fired on {} cells across {} slot ports",
            result.absorbed_properties.len(), result.ports.len());
    }
}
