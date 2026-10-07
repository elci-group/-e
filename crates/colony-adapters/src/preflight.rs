// SPDX-License-Identifier: MIT
//! Lucid preflight: pin a source snapshot and account tokens before planning.
use std::fs;
use std::path::{Component, Path};

use colony_core::{valid_path, ContextManifest, Error, FailureKind, Preflight, Result};

use crate::hash::sha256_hex;

const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const SAMPLE_BYTES: usize = 4096;
const MAX_DEPTH: usize = 8;

/// Reads a workspace and returns the `ContextManifest` Colony will pin.
#[derive(Debug, Clone)]
pub struct LucidPreflight {
    root: std::path::PathBuf,
    token_budget: u64,
}

impl LucidPreflight {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            token_budget: 2048,
        }
    }

    pub fn with_token_budget(mut self, tokens: u64) -> Self {
        self.token_budget = tokens;
        self
    }

    /// Select source that fits `max_input_tokens`, estimated as bytes / 4.
    pub fn context_within(
        &self,
        objective: &str,
        max_input_tokens: u64,
    ) -> Result<ContextManifest> {
        if max_input_tokens == 0 {
            return Err(Error::new(
                FailureKind::ContextOverflow,
                "preflight token budget is zero",
            ));
        }
        let meta = fs::metadata(&self.root).map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("preflight root {}: {error}", self.root.display()),
            )
        })?;
        if !meta.is_dir() {
            return Err(Error::new(
                FailureKind::ContractViolation,
                "preflight root is not a directory",
            ));
        }
        let tokens = objective_tokens(objective);
        let mut files = Vec::new();
        let mut untrusted = Vec::new();
        walk(
            &self.root,
            &self.root,
            0,
            &tokens,
            &mut files,
            &mut untrusted,
        )?;
        files.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.cmp(&b.path)));
        let mut selected = Vec::new();
        let mut used = 0u64;
        let mut saw_positive = false;
        for file in &files {
            if file.score == 0 {
                continue;
            }
            saw_positive = true;
            if selected.len() >= MAX_FILES {
                break;
            }
            let cost = token_cost(file.bytes);
            if used.saturating_add(cost) > max_input_tokens {
                continue;
            }
            selected.push(file.clone());
            used += cost;
        }
        if selected.is_empty() {
            let mut rest = files.clone();
            rest.sort_by(|a, b| a.path.cmp(&b.path));
            for file in rest {
                if file.score > 0 && saw_positive {
                    continue;
                }
                if selected.len() >= MAX_FILES {
                    break;
                }
                let cost = token_cost(file.bytes);
                if used.saturating_add(cost) > max_input_tokens {
                    continue;
                }
                selected.push(file);
                used += cost;
            }
        }
        if selected.is_empty() {
            return Err(Error::new(
                FailureKind::ContextOverflow,
                "no source file fits the preflight token budget",
            ));
        }
        let mut hasher_parts: Vec<Vec<u8>> = Vec::new();
        let mut paths = Vec::new();
        let mut symbols = std::collections::BTreeSet::new();
        let mut constraints = std::collections::BTreeSet::new();
        let mut trusted = Vec::new();
        for file in &selected {
            let bytes = fs::read(self.root.join(&file.path)).map_err(|error| {
                Error::new(
                    FailureKind::InfrastructureFailure,
                    format!("read {}: {error}", file.path),
                )
            })?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            hasher_parts.push(file.path.as_bytes().to_vec());
            hasher_parts.push(vec![0]);
            hasher_parts.push(bytes);
            for symbol in extract_symbols(&text) {
                symbols.insert(symbol);
            }
            for line in extract_constraints(&text) {
                constraints.insert(line);
            }
            trusted.push(format!("{} ({} tok)", file.path, token_cost(file.bytes)));
            paths.push(file.path.clone());
        }
        let mut retrieved = Vec::new();
        for file in untrusted {
            if retrieved.len() == 16 {
                break;
            }
            let cost = token_cost(file.bytes);
            if used.saturating_add(cost) > max_input_tokens {
                continue;
            }
            used += cost;
            retrieved.push(file.path);
        }
        let chunks: Vec<&[u8]> = hasher_parts.iter().map(Vec::as_slice).collect();
        Ok(ContextManifest {
            source_sha: sha256_hex(&chunks),
            relevant_paths: paths,
            symbols: symbols.into_iter().take(64).collect(),
            ontology_entities: Vec::new(),
            architectural_constraints: constraints.into_iter().take(12).collect(),
            trusted_context: trusted,
            retrieved_untrusted: retrieved,
            input_tokens: used,
        })
    }
}

impl Preflight for LucidPreflight {
    fn context(&self, objective: &str) -> Result<ContextManifest> {
        self.context_within(objective, self.token_budget)
    }
}

#[derive(Clone)]
struct FileHit {
    path: String,
    score: usize,
    bytes: u64,
}

fn token_cost(bytes: u64) -> u64 {
    bytes.saturating_add(3) / 4
}

fn objective_tokens(objective: &str) -> Vec<String> {
    let mut tokens = std::collections::BTreeSet::new();
    let mut current = String::new();
    for ch in objective.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if current.len() >= 3 {
            tokens.insert(std::mem::take(&mut current));
        } else {
            current.clear();
        }
    }
    if current.len() >= 3 {
        tokens.insert(current);
    }
    tokens.into_iter().collect()
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    tokens: &[String],
    files: &mut Vec<FileHit>,
    untrusted: &mut Vec<FileHit>,
) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = fs::read_dir(dir).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("read dir {}: {error}", dir.display()),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("read dir entry: {error}"),
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || skip_dir(name) {
            continue;
        }
        let path = entry.path();
        let kind = entry.file_type().map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("stat {}: {error}", path.display()),
            )
        })?;
        if kind.is_dir() {
            walk(root, &path, depth + 1, tokens, files, untrusted)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(rel) = relative_path(root, &path) else {
            continue;
        };
        let meta = entry.metadata().map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("stat {rel}: {error}"),
            )
        })?;
        if meta.len() == 0 || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let sample = read_sample(&path)?;
        if sample.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&sample).to_ascii_lowercase();
        let haystack = format!("{rel} {text}").to_ascii_lowercase();
        let score = tokens
            .iter()
            .filter(|token| haystack.contains(token.as_str()))
            .count();
        let hit = FileHit {
            path: rel,
            score,
            bytes: meta.len(),
        };
        if hit
            .path
            .split('/')
            .any(|part| part == "retrieved" || part == "untrusted")
        {
            untrusted.push(hit);
        } else {
            files.push(hit);
        }
    }
    Ok(())
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "target" | "node_modules" | "dist" | ".git" | ".kaptaind" | ".uni" | ".lwoodz" | ".cargo"
    )
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            _ => return None,
        }
    }
    let joined = parts.join("/");
    valid_path(&joined).then_some(joined)
}

fn read_sample(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("open {}: {error}", path.display()),
        )
    })?;
    let mut take = std::io::Read::take(file, SAMPLE_BYTES as u64);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut take, &mut buf).map_err(|error| {
        Error::new(
            FailureKind::InfrastructureFailure,
            format!("read {}: {error}", path.display()),
        )
    })?;
    Ok(buf)
}

fn extract_symbols(text: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        for key in [
            "fn ", "struct ", "enum ", "trait ", "class ", "def ", "type ",
        ] {
            let rest = line
                .strip_prefix(key)
                .or_else(|| {
                    line.strip_prefix("pub ")
                        .and_then(|rest| rest.strip_prefix(key))
                })
                .or_else(|| {
                    line.strip_prefix("pub(crate) ")
                        .and_then(|rest| rest.strip_prefix(key))
                });
            let Some(rest) = rest else { continue };
            let name: String = rest
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if !name.is_empty() {
                found.insert(name);
            }
        }
    }
    found.into_iter().collect()
}

fn extract_constraints(text: &str) -> Vec<String> {
    let mut found = std::collections::BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.contains("INVARIANT") || line.starts_with("MUST ") {
            found.insert(line.chars().take(200).collect());
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn pins_matching_source_and_counts_tokens() {
        let root = std::env::temp_dir().join(format!("colony-preflight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "name = \"portability\"\n").unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn dependency_audit() {}\n// INVARIANT: keep the sha pinned\n",
        )
        .unwrap();
        fs::write(root.join("src/other.rs"), "fn unrelated() {}\n").unwrap();
        let manifest = LucidPreflight::new(&root)
            .context("dependency audit portability")
            .unwrap();
        assert_eq!(manifest.source_sha.len(), 64);
        assert!(manifest.relevant_paths.iter().any(|p| p == "src/lib.rs"));
        assert!(manifest.input_tokens > 0);
        assert!(manifest.symbols.iter().any(|s| s == "dependency_audit"));
        assert!(manifest
            .architectural_constraints
            .iter()
            .any(|line| line.contains("INVARIANT")));
        let _ = fs::remove_dir_all(&root);
    }
}
