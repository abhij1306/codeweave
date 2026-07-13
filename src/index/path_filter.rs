use std::borrow::Cow;

pub(super) fn normalize(path: &str) -> Cow<'_, str> {
    if path.contains('\\') {
        let normalized = path.replace('\\', "/");
        Cow::Owned(normalized.trim_start_matches("./").to_owned())
    } else {
        Cow::Borrowed(path.trim_start_matches("./"))
    }
}

pub(crate) struct PathFilterSet<'a> {
    normalized: Vec<Cow<'a, str>>,
}

impl<'a> PathFilterSet<'a> {
    pub(crate) fn new(filters: &'a [String]) -> Self {
        let normalized: Vec<_> = filters
            .iter()
            .map(|filter| normalize(filter))
            .filter(|filter| !filter.is_empty())
            .collect();
        Self { normalized }
    }

    pub(super) fn len(&self) -> usize {
        self.normalized.len()
    }

    pub(crate) fn allows(&self, path: &str) -> bool {
        self.normalized.is_empty()
            || self
                .normalized
                .iter()
                .any(|filter| path_matches_filter(path, filter.as_ref()))
    }
}

fn path_matches_filter(path: &str, filter: &str) -> bool {
    if filter.ends_with('/') {
        return path.starts_with(filter);
    }
    path == filter
        || path
            .strip_prefix(filter)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_matches_only_prefix_segments() {
        let filters = vec!["src".to_owned()];
        let set = PathFilterSet::new(&filters);

        assert!(set.allows("src/main.rs"));
        assert!(set.allows("src"));
        assert!(!set.allows("tests/src/main.rs"));
        assert!(!set.allows("src-old/main.rs"));
    }
}
