use std::fs;
use std::path::{Path, PathBuf};

const MAX_RUST_FILE_LINES: usize = 800;

fn logical_line_count(contents: &str) -> usize {
    contents.lines().count()
}

fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("reading {}: {error}", directory.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| panic!("reading an entry in {}: {error}", directory.display()));
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("inspecting {}: {error}", path.display()));
        if file_type.is_dir() {
            collect_rust_files(&path, files);
        } else if file_type.is_file() && path.extension().is_some_and(|value| value == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn rust_source_files_stay_within_the_maintainability_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rust_files(&root.join("src"), &mut files);
    collect_rust_files(&root.join("tests"), &mut files);

    let violations = files
        .into_iter()
        .filter_map(|path| {
            let contents = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
            let lines = logical_line_count(&contents);
            (lines > MAX_RUST_FILE_LINES).then(|| {
                let relative = path.strip_prefix(root).unwrap_or(&path);
                format!("{}: {lines} lines", relative.display())
            })
        })
        .collect::<Vec<_>>();

    assert!(
        violations.is_empty(),
        "Rust files must not exceed {MAX_RUST_FILE_LINES} logical lines:\n{}",
        violations.join("\n")
    );
}

#[test]
fn line_limit_includes_800_and_rejects_801() {
    assert_eq!(logical_line_count(&"line\n".repeat(800)), 800);
    assert_eq!(logical_line_count(&"line\n".repeat(801)), 801);
    assert!(logical_line_count(&"line\n".repeat(800)) <= MAX_RUST_FILE_LINES);
    assert!(logical_line_count(&"line\n".repeat(801)) > MAX_RUST_FILE_LINES);
}
