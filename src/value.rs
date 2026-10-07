//! Flat callable provenance: aggregate depth never becomes Rust call-stack depth.
use crate::config::Limit;
use std::{cell::Cell, collections::BTreeMap};

pub struct Policy {
    pub depth: Limit,
    pub observed_depth: Cell<usize>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Facts<T> {
    pub targets: Vec<T>,
    pub truncated: bool,
    pub unknown: bool,
}

impl<T> Default for Facts<T> {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            truncated: false,
            unknown: false,
        }
    }
}

impl<T: Copy + Eq> Facts<T> {
    fn join(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for target in &other.targets {
            if !self.targets.contains(target) {
                self.targets.push(*target);
                changed = true;
            }
        }
        changed |= other.truncated && !self.truncated;
        changed |= other.unknown && !self.unknown;
        self.truncated |= other.truncated;
        self.unknown |= other.unknown;
        changed
    }
}

/// Only nonempty facts are stored. Paths count aggregate fields from depth zero;
/// references retain their pointee's value and do not add a level.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Value<T> {
    pub root: Facts<T>,
    fields: BTreeMap<Vec<usize>, Facts<T>>,
}

impl<T> Default for Value<T> {
    fn default() -> Self {
        Self {
            root: Facts::default(),
            fields: BTreeMap::new(),
        }
    }
}

impl<T: Copy + Eq> Value<T> {
    pub fn function(target: T) -> Self {
        let mut value = Self::default();
        value.root.targets.push(target);
        value
    }

    pub fn is_truncated(&self) -> bool {
        self.root.truncated || self.fields.values().any(|facts| facts.truncated)
    }

    pub fn join(&mut self, other: &Self, policy: &Policy) -> bool {
        self.assign(&[], other, policy)
    }

    /// Join at a subfield, carrying the root's budget through the entire path.
    pub fn assign(&mut self, prefix: &[usize], other: &Self, policy: &Policy) -> bool {
        let mut changed = false;
        for (suffix, facts) in std::iter::once((&[][..], &other.root)).chain(
            other
                .fields
                .iter()
                .map(|(path, facts)| (path.as_slice(), facts)),
        ) {
            if facts == &Facts::default() {
                continue;
            }
            let depth = prefix.len().saturating_add(suffix.len());
            policy
                .observed_depth
                .set(policy.observed_depth.get().max(depth));
            let mut path: Vec<_> = prefix.iter().chain(suffix).copied().collect();
            let truncated = !policy.depth.allows(depth);
            if let Limit::Finite(limit) = policy.depth {
                path.truncate(limit);
            }
            let destination = if path.is_empty() {
                &mut self.root
            } else {
                self.fields.entry(path).or_default()
            };
            if truncated {
                changed |= !destination.truncated;
                destination.truncated = true;
            } else {
                changed |= destination.join(facts);
            }
        }
        changed
    }

    /// `None` joins all possible array elements. A truncation marker survives
    /// projection through a discarded field so a missing target stays visible.
    pub fn project(&self, field: Option<usize>, policy: &Policy) -> Self {
        let mut result = Self::default();
        result.root.truncated = self.root.truncated;
        for (path, facts) in &self.fields {
            if field.is_none_or(|field| field == path[0]) {
                let mut value = Self::default();
                value.root = facts.clone();
                result.assign(&path[1..], &value, policy);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{DefaultHasher, Hash, Hasher};

    fn policy(depth: Limit) -> Policy {
        Policy {
            depth,
            observed_depth: Cell::new(0),
        }
    }

    #[test]
    fn subfield_assignment_cannot_reset_the_budget() {
        let policy = policy(Limit::Finite(2));
        let mut inner = Value::default();
        inner.assign(&[1, 2], &Value::function(42), &policy);
        let mut outer = Value::default();
        outer.assign(&[0], &inner, &policy);
        assert!(outer.is_truncated());
        let projected = outer
            .project(Some(0), &policy)
            .project(Some(1), &policy)
            .project(Some(2), &policy);
        assert!(projected.is_truncated());
        assert!(projected.root.targets.is_empty());
        assert_eq!(policy.observed_depth.get(), 3);
    }

    #[test]
    fn field_and_array_projection_keep_targets_separate() {
        let policy = policy(Limit::Unlimited);
        let mut value = Value::default();
        value.assign(&[0, 1], &Value::function(42), &policy);
        value.assign(&[1, 1], &Value::function(43), &policy);
        assert_eq!(
            value
                .project(Some(0), &policy)
                .project(Some(1), &policy)
                .root
                .targets,
            [42]
        );
        assert_eq!(
            value
                .project(None, &policy)
                .project(Some(1), &policy)
                .root
                .targets,
            [42, 43]
        );
    }

    #[test]
    fn deep_values_clone_hash_compare_and_drop_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(|| {
                let policy = policy(Limit::Unlimited);
                let mut value = Value::default();
                value.assign(&vec![0; 20_000], &Value::function(42), &policy);
                let cloned = value.clone();
                assert!(value == cloned);
                let mut hasher = DefaultHasher::new();
                value.hash(&mut hasher);
                let _ = hasher.finish();
                assert!(!value.is_truncated());
                assert_eq!(policy.observed_depth.get(), 20_000);
                drop((value, cloned));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
