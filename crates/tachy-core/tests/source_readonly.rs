//! §1 / M1-01: the source file is never opened for writing. `source.rs` must
//! not contain any write-mode `OpenOptions` call.

#[test]
fn source_rs_never_opens_for_writing() {
    let code = include_str!("../src/source.rs");
    for flag in ["write", "append", "create", "truncate"] {
        let needle = format!("{flag}(true)");
        assert!(!code.contains(&needle), "source.rs contains `{needle}`");
    }
    assert!(!code.contains(&["Open", "Options"].concat()));
}
