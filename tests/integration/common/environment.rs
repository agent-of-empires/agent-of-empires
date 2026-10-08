#[must_use]
pub struct EnvGuard {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub fn new(keys: &[&'static str]) -> Self {
        Self {
            vars: keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        }
    }
    pub fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        if !self.vars.iter().any(|(saved, _)| *saved == key) {
            self.vars.push((key, std::env::var_os(key)));
        }
        std::env::set_var(key, value);
    }
    pub fn and_set(mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.set(key, value);
        self
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, old) in self.vars.drain(..).rev() {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
