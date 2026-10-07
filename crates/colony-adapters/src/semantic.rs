// SPDX-License-Identifier: MIT
//! Padagonia-backed semantic source. Colony does not keep its own ontology store.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use colony_core::{
    score, Error, FailureKind, OntologySlice, Relation, RelationKind as ColonyKind, Result,
    SemanticSource,
};
use padagonia::storage::StoreError;
use padagonia::{Scalar, Store, StringTableExt};

use crate::hash::sha256_hex;

/// A Padagonia graph file projected into Colony's `OntologySlice`.
pub struct PadagoniaSource {
    store: Store,
    revision: String,
}

impl PadagoniaSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|error| {
            Error::new(
                FailureKind::InfrastructureFailure,
                format!("padagonia {}: {error}", path.display()),
            )
        })?;
        let store = Store::load(path).map_err(|error: StoreError| {
            Error::new(
                FailureKind::ContractViolation,
                format!("padagonia {}: {error}", path.display()),
            )
        })?;
        Ok(Self {
            store,
            revision: sha256_hex(&[&bytes]),
        })
    }
}

impl SemanticSource for PadagoniaSource {
    fn slice(&self, objective: &str) -> Result<OntologySlice> {
        let table = self.store.string_table();
        let objective = objective.to_ascii_lowercase();
        let mut best: BTreeMap<(String, String, u8), Relation> = BTreeMap::new();
        let mut locked = std::collections::BTreeSet::new();
        for node in self.store.nodes().values() {
            if !matches!(
                property(table, &node.properties, "locked"),
                Some(Scalar::Bool(true))
            ) {
                continue;
            }
            if objective_mentions(&objective, &node.external_id) {
                locked.insert(node.external_id.clone());
            }
        }
        for edge in self.store.edges().values() {
            let from = self
                .store
                .nodes()
                .get(&edge.src)
                .map(|node| node.external_id.as_str())
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        FailureKind::ContractViolation,
                        "padagonia edge is missing its source",
                    )
                })?;
            let to = self
                .store
                .nodes()
                .get(&edge.dst)
                .map(|node| node.external_id.as_str())
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    Error::new(
                        FailureKind::ContractViolation,
                        "padagonia edge is missing its target",
                    )
                })?;
            if !objective_mentions(&objective, from) || !objective_mentions(&objective, to) {
                continue;
            }
            let label = table.resolve_relation(edge.label).unwrap_or("");
            let kind = colony_kind(label)?;
            let strength = edge_strength(table, edge)?;
            if !score(strength) {
                return Err(Error::new(
                    FailureKind::ContractViolation,
                    format!("padagonia relation strength {strength} is outside [0,1]"),
                ));
            }
            let rank = kind_rank(&kind);
            let relation = Relation {
                from: from.to_string(),
                to: to.to_string(),
                kind,
                strength,
            };
            let key = (relation.from.clone(), relation.to.clone(), rank);
            match best.get(&key) {
                Some(existing) if existing.strength >= strength => {}
                _ => {
                    best.insert(key, relation);
                }
            }
        }
        Ok(OntologySlice {
            revision: self.revision.clone(),
            relations: best.into_values().collect(),
            locked_entities: locked,
        })
    }
}

fn objective_mentions(objective: &str, entity: &str) -> bool {
    let entity = entity.trim().to_ascii_lowercase();
    !entity.is_empty() && objective.contains(&entity)
}

fn property<'a>(
    table: &'a padagonia::StringTable,
    props: &'a [(padagonia::id::KeyId, Scalar)],
    key: &str,
) -> Option<&'a Scalar> {
    let id = table.key_id(key)?;
    props
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .map(|(_, value)| value)
}

fn edge_strength(table: &padagonia::StringTable, edge: &padagonia::Edge) -> Result<f64> {
    let value = match property(table, &edge.properties, "strength")
        .or_else(|| property(table, &edge.properties, "confidence"))
    {
        Some(Scalar::F64(value)) => *value,
        Some(Scalar::I64(value)) => *value as f64,
        Some(_) => {
            return Err(Error::new(
                FailureKind::ContractViolation,
                "padagonia strength must be numeric",
            ))
        }
        None => f64::from(edge.provenance.confidence),
    };
    if !value.is_finite() {
        return Err(Error::new(
            FailureKind::ContractViolation,
            "padagonia strength is not finite",
        ));
    }
    Ok(value)
}

fn colony_kind(label: &str) -> Result<ColonyKind> {
    let name = label.trim().to_ascii_lowercase().replace('-', "_");
    Ok(match name.as_str() {
        "depends_on" | "depends" | "supersedes" | "calls" | "derives_from" => ColonyKind::DependsOn,
        "implements" | "extends" | "adapts" | "wraps" => ColonyKind::Implements,
        "tests" | "validated_by" => ColonyKind::Tests,
        "independent" => ColonyKind::Independent,
        "shares_state" | "contains" | "member_of" | "paired_to" | "emitted" | "observed_in"
        | "belongs_to" | "repeats" | "supports" | "suggests" | "released_as" | "charged_to"
        | "generates" | "embeds" | "duplicates" | "interfaces_with" => ColonyKind::SharesState,
        _ => {
            return Err(Error::new(
                FailureKind::ContractViolation,
                format!("padagonia relation `{label}` has no colony kind"),
            ))
        }
    })
}

fn kind_rank(kind: &ColonyKind) -> u8 {
    match kind {
        ColonyKind::DependsOn => 0,
        ColonyKind::SharesState => 1,
        ColonyKind::Implements => 2,
        ColonyKind::Tests => 3,
        ColonyKind::Independent => 4,
    }
}

/// Build a one-edge graph and return it. Test helper stays crate-private.
#[cfg(test)]
pub(crate) fn sample_store(path: &Path) {
    use padagonia::{NamespaceId, Provenance, Scalar};
    let mut store = Store::new();
    let provenance = Provenance::new("colony-test", "none", 0.4, 0.0, 0, Vec::new());
    let namespace = NamespaceId::default();
    let audit = store
        .add_node_in_namespace(
            namespace.clone(),
            "dependency-audit",
            "Work",
            vec![("locked", Scalar::Bool(false))],
            None,
            provenance.clone(),
        )
        .unwrap();
    let platform = store
        .add_node_in_namespace(
            namespace,
            "platform-audit",
            "Work",
            vec![("locked", Scalar::Bool(true))],
            None,
            provenance.clone(),
        )
        .unwrap();
    let other = store
        .add_node_in_namespace(
            NamespaceId::default(),
            "unrelated-entity",
            "Work",
            Vec::new(),
            None,
            provenance.clone(),
        )
        .unwrap();
    store.add_edge(
        audit,
        platform,
        "shares_state",
        vec![("strength", Scalar::F64(0.6))],
        None,
        provenance.clone(),
    );
    store.add_edge(
        audit,
        other,
        "depends_on",
        vec![("strength", Scalar::F64(0.9))],
        None,
        provenance,
    );
    store.save(path).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn projects_only_objective_endpoints() {
        let dir = std::env::temp_dir().join(format!("colony-padagonia-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("slice.pad");
        sample_store(&path);
        let source = PadagoniaSource::open(&path).unwrap();
        let slice = source
            .slice("dependency-audit shares state with platform-audit")
            .unwrap();
        assert_eq!(slice.revision.len(), 64);
        assert_eq!(slice.relations.len(), 1);
        assert_eq!(slice.relations[0].from, "dependency-audit");
        assert_eq!(slice.relations[0].to, "platform-audit");
        assert!(matches!(slice.relations[0].kind, ColonyKind::SharesState));
        assert_eq!(slice.relations[0].strength, 0.6);
        assert!(slice.locked_entities.contains("platform-audit"));
        assert!(!slice.locked_entities.contains("unrelated-entity"));
        let _ = fs::remove_dir_all(&dir);
    }
}
