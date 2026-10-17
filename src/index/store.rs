use super::entry::FileEntry;
use super::handle::hex;
use super::ids::{ContentHash, SnapshotId};
use crate::security::normalize_path_text;
use crate::symbols::Symbol;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Domain separator of snapshot ids. Changing it changes every snapshot id.
const SNAPSHOT_DOMAIN: &str = "codeweave-index";

/// One declaration found by `CodeIndex::find_symbols`.
#[derive(Debug, Clone)]
pub struct SymbolDefinition {
    /// Workspace-relative path of the declaring file.
    pub path: String,
    pub symbol: Symbol,
    /// Content hash of the declaring file.
    pub hash: ContentHash,
}

/// The in-memory workspace index. Mutation goes through `insert` and
/// `remove`, which keep the symbol postings and the snapshot digest current,
/// so every query, including `snapshot_id`, only needs `&self`.
#[derive(Debug, Default)]
pub struct CodeIndex {
    files: HashMap<String, FileEntry>,
    /// Symbol name to `(path, index into that file's symbols)`.
    symbol_index: HashMap<String, BTreeSet<(String, usize)>>,
    /// Order-independent digest of the file set. Each file contributes
    /// `sha256(path ‖ 0 ‖ hash ‖ 0)` XORed into this accumulator, so inserts
    /// and removals update it in O(1).
    snapshot_acc: [u8; 32],
}

impl CodeIndex {
    pub(super) fn insert(&mut self, entry: FileEntry) {
        self.remove(entry.path());
        xor_into(&mut self.snapshot_acc, &file_digest(&entry));
        for (position, symbol) in entry.symbols().iter().enumerate() {
            self.symbol_index
                .entry(symbol.name.clone())
                .or_default()
                .insert((entry.path().to_owned(), position));
        }
        self.files.insert(entry.path().to_owned(), entry);
    }

    pub(super) fn remove(&mut self, path: &str) -> Option<FileEntry> {
        let removed = self.files.remove(path)?;
        xor_into(&mut self.snapshot_acc, &file_digest(&removed));
        let names: HashSet<&str> = removed
            .symbols()
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        for name in names {
            if let Some(entries) = self.symbol_index.get_mut(name) {
                entries.retain(|(entry_path, _)| entry_path != removed.path());
                if entries.is_empty() {
                    self.symbol_index.remove(name);
                }
            }
        }
        Some(removed)
    }

    /// Remove `path` and every indexed file below it, returning the removed paths.
    pub(super) fn remove_tree(&mut self, path: &str) -> Vec<String> {
        let prefix = format!("{path}/");
        let removed: Vec<String> = self
            .files
            .keys()
            .filter(|candidate| *candidate == path || candidate.starts_with(&prefix))
            .cloned()
            .collect();
        for path in &removed {
            self.remove(path);
        }
        removed
    }

    /// Identity of the indexed file set at a repository `head`.
    pub fn snapshot_id(&self, head: &str) -> SnapshotId {
        let mut digest = Sha256::new();
        digest.update(SNAPSHOT_DOMAIN.as_bytes());
        digest.update([0]);
        digest.update(head.as_bytes());
        digest.update([0]);
        digest.update((self.files.len() as u64).to_le_bytes());
        digest.update(self.snapshot_acc);
        SnapshotId::from(format!("snap_{}", hex(&digest.finalize())))
    }

    pub fn get(&self, path: &str) -> Option<&FileEntry> {
        self.files.get(normalize_path_text(path).as_ref())
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub(crate) fn files(&self) -> impl Iterator<Item = &FileEntry> {
        self.files.values()
    }

    pub fn languages(&self) -> Vec<String> {
        let languages: BTreeSet<&str> = self
            .files
            .values()
            .map(FileEntry::language)
            .filter(|language| *language != "text")
            .collect();
        languages.into_iter().map(str::to_owned).collect()
    }

    /// Every distinct declared symbol name.
    pub(super) fn symbol_names(&self) -> impl Iterator<Item = &str> {
        self.symbol_index.keys().map(String::as_str)
    }

    /// Declarations per symbol name, for the metrics view.
    pub(super) fn symbol_postings(
        &self,
    ) -> impl Iterator<Item = (&String, &BTreeSet<(String, usize)>)> {
        self.symbol_index.iter()
    }

    /// Return every declaration matching `name`, optionally constrained to one
    /// workspace-relative path. Besides leaf names, this accepts qualified method
    /// names such as `BrowserAttemptRunner.run` and `BrowserAttemptRunner::run`.
    pub fn find_symbols(&self, path: Option<&str>, name: &str) -> Vec<SymbolDefinition> {
        let leaf_name = qualified_symbol_parts(name)
            .map(|(_, leaf)| leaf)
            .unwrap_or(name);
        let found = |file: &FileEntry, symbol: &Symbol| SymbolDefinition {
            path: file.path().to_owned(),
            symbol: symbol.clone(),
            hash: file.hash().to_owned(),
        };
        if let Some(path) = path {
            let Some(file) = self.get(path) else {
                return Vec::new();
            };
            return file
                .symbols()
                .iter()
                .filter(|symbol| symbol.name == leaf_name)
                .filter(|symbol| symbol_matches_qualified_name(file, symbol, name))
                .map(|symbol| found(file, symbol))
                .collect();
        }
        self.indexed_symbols(leaf_name)
            .filter(|(file, symbol)| symbol_matches_qualified_name(file, symbol, name))
            .map(|(file, symbol)| found(file, symbol))
            .collect()
    }

    pub(super) fn indexed_symbols<'a>(
        &'a self,
        name: &str,
    ) -> impl Iterator<Item = (&'a FileEntry, &'a Symbol)> + 'a {
        self.symbol_index
            .get(name)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .filter_map(|(path, position)| {
                let file = self.files.get(path)?;
                let symbol = file.symbols().get(*position)?;
                Some((file, symbol))
            })
    }
}

fn file_digest(file: &FileEntry) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(file.path().as_bytes());
    digest.update([0]);
    digest.update(file.hash().as_bytes());
    digest.update([0]);
    digest.finalize().into()
}

fn xor_into(acc: &mut [u8; 32], contribution: &[u8; 32]) {
    for (slot, byte) in acc.iter_mut().zip(contribution.iter()) {
        *slot ^= *byte;
    }
}

fn qualified_symbol_parts(name: &str) -> Option<(&str, &str)> {
    let dot = name.rfind('.').map(|index| (index, 1));
    let colon = name.rfind("::").map(|index| (index, 2));
    let (index, width) = match (dot, colon) {
        (Some(dot), Some(colon)) => {
            if dot.0 > colon.0 {
                dot
            } else {
                colon
            }
        }
        (Some(dot), None) => dot,
        (None, Some(colon)) => colon,
        (None, None) => return None,
    };
    let qualifier = &name[..index];
    let leaf = &name[index + width..];
    (!qualifier.is_empty() && !leaf.is_empty()).then_some((qualifier, leaf))
}

fn symbol_matches_qualified_name(file: &FileEntry, symbol: &Symbol, requested: &str) -> bool {
    if symbol.name == requested {
        return true;
    }
    let Some((qualifier, leaf)) = qualified_symbol_parts(requested) else {
        return false;
    };
    if symbol.name != leaf {
        return false;
    }
    let owner = qualified_symbol_parts(qualifier)
        .map(|(_, leaf)| leaf)
        .unwrap_or(qualifier);
    file.symbols().iter().any(|candidate| {
        candidate.name == owner
            && candidate.start_line <= symbol.start_line
            && candidate.end_line >= symbol.end_line
            && (candidate.start_line < symbol.start_line || candidate.end_line > symbol.end_line)
    })
}
