//! Drawing code must use `Theme` style helpers, never raw colours (spec §14).
//!
//! Scans `src/**/*.rs` except `theme.rs` (where the palette lives) and
//! `config.rs` (the template's `styles` parser, kept unused per M6-03, maps
//! user style strings to `Color::Indexed`).

use std::{fs, path::Path};

const EXEMPT: &[&str] = &["theme.rs", "config.rs"];

/// ratatui's named colours.
const NAMED: &[&str] = &[
    "Reset",
    "Black",
    "Red",
    "Green",
    "Yellow",
    "Blue",
    "Magenta",
    "Cyan",
    "Gray",
    "DarkGray",
    "LightRed",
    "LightGreen",
    "LightYellow",
    "LightBlue",
    "LightMagenta",
    "LightCyan",
    "White",
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `Color::<Ident>` in `line` that is a raw colour.
fn raw_colors(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (i, _) in line.match_indices("Color::") {
        let rest = &line[i + "Color::".len()..];
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if ident == "Rgb" || ident == "Indexed" || NAMED.contains(&ident.as_str()) {
            found.push(format!("Color::{ident}"));
        }
    }
    found
}

#[test]
fn no_raw_colors_outside_theme() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.len() > 5, "no sources found in {}", src.display());

    let mut offences = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_str().unwrap();
        if file.parent() == Some(src.as_path()) && EXEMPT.contains(&name) {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            for color in raw_colors(line) {
                offences.push(format!("{}:{}: {color}", file.display(), n + 1));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "use Theme helpers instead of raw colours:\n{}",
        offences.join("\n")
    );
}

#[test]
fn detects_raw_colors() {
    assert_eq!(
        raw_colors("Style::new().fg(Color::Rgb(1, 2, 3))"),
        ["Color::Rgb"]
    );
    assert_eq!(raw_colors("bg(Color::Indexed(4))"), ["Color::Indexed"]);
    assert_eq!(
        raw_colors("fg(Color::Red).bg(Color::DarkGray)"),
        ["Color::Red", "Color::DarkGray"]
    );
    assert!(raw_colors("theme.dialog_border(theme.teal)").is_empty());
    assert!(raw_colors("fn gauge(fill: Color) -> Style").is_empty());
}
