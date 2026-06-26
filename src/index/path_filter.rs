use std::borrow::Cow;

pub(super) fn normalize(path: &str) -> Cow<'_, str> {
    if path.contains('\\') {
        let normalized = path.replace('\\', "/");
        Cow::Owned(normalized.trim_start_matches("./").to_owned())
    } else {
        Cow::Borrowed(path.trim_start_matches("./"))
    }
}

pub(super) struct PathFilterSet<'a> {
    normalized: Vec<Cow<'a, str>>,
    lowercase: Vec<String>,
}

impl<'a> PathFilterSet<'a> {
    pub(super) fn new(filters: &'a [String]) -> Self {
        let normalized: Vec<_> = filters
            .iter()
            .map(|filter| normalize(filter))
            .filter(|filter| !filter.is_empty())
            .collect();
        let lowercase = normalized
            .iter()
            .map(|filter| filter.to_ascii_lowercase())
            .collect();
        Self {
            normalized,
            lowercase,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.normalized.len()
    }

    pub(super) fn allows(&self, path: &str) -> bool {
        self.normalized.is_empty()
            || self
                .normalized
                .iter()
                .any(|filter| path_matches_filter(path, filter.as_ref()))
    }

    pub(super) fn explicitly_requests(&self, path: &str, query_lower: &str) -> bool {
        let path_lower = path.to_ascii_lowercase();
        let name = path_lower.rsplit('/').next().unwrap_or(path_lower.as_str());
        query_lower.contains(&path_lower)
            || query_lower.contains(name)
            || self.lowercase.iter().any(|filter| {
                let filter_name = filter.rsplit('/').next().unwrap_or(filter.as_str());
                filter == &path_lower || filter_name == name
            })
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

    #[test]
    fn explicit_request_does_not_treat_ancestor_filter_as_specific_file() {
        let filters = vec!["src".to_owned()];
        let set = PathFilterSet::new(&filters);

        assert!(!set.explicitly_requests("src/package-lock.json", "dependency lock"));
        assert!(set.explicitly_requests("src/package-lock.json", "package-lock.json"));
    }
}
