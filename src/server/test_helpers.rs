//! Test fixtures shared by more than one submodule's tests.

pub(super) fn vecs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}
