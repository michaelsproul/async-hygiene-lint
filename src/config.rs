use serde::{Deserialize, Deserializer, de};
use std::fmt;

/// An absent budget check, rather than a large sentinel, represents unlimited work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    Finite(usize),
    Unlimited,
}

impl Limit {
    pub fn allows(self, count: usize) -> bool {
        match self {
            Self::Finite(limit) => count <= limit,
            Self::Unlimited => true,
        }
    }

    pub fn exhausted(self, count: usize) -> bool {
        match self {
            Self::Finite(limit) => count >= limit,
            Self::Unlimited => false,
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finite(value) => value.fmt(f),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(usize),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Number(n) if n > 0 => Ok(Self::Finite(n)),
            Raw::Text(text) if text == "unlimited" => Ok(Self::Unlimited),
            _ => Err(de::Error::custom(
                "expected a positive integer or \"unlimited\"",
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub prohibited: Vec<Prohibited>,
    pub insulators: Vec<Insulator>,
    pub max_instances: Limit,
    pub max_iterations: Limit,
    pub max_dataflow_iterations: Option<Limit>,
    pub max_aggregate_depth: Limit,
    pub max_recursive_instances: Limit,
    pub max_incomplete_notes: Limit,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prohibited {
    pub path: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Insulator {
    pub path: String,
    pub callback_args: Vec<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prohibited: vec![
                Prohibited {
                    path: "std::thread::sleep".into(),
                    reason: "blocks the executor thread".into(),
                },
                Prohibited {
                    path: "std::thread::park".into(),
                    reason: "blocks the executor thread".into(),
                },
                Prohibited {
                    path: "tokio::runtime::Runtime::block_on".into(),
                    reason: "cannot start a runtime from within a runtime".into(),
                },
                Prohibited {
                    path: "tokio::runtime::Handle::block_on".into(),
                    reason: "cannot start a runtime from within a runtime".into(),
                },
                Prohibited {
                    path: "futures_executor::block_on".into(),
                    reason: "blocks the executor thread".into(),
                },
            ],
            insulators: vec![
                Insulator {
                    path: "tokio::task::spawn_blocking".into(),
                    callback_args: vec![0],
                },
                Insulator {
                    path: "tokio::runtime::Handle::spawn_blocking".into(),
                    callback_args: vec![1],
                },
                Insulator {
                    path: "tokio::runtime::Runtime::spawn_blocking".into(),
                    callback_args: vec![1],
                },
                Insulator {
                    path: "tokio::task::block_in_place".into(),
                    callback_args: vec![0],
                },
                Insulator {
                    path: "std::thread::spawn".into(),
                    callback_args: vec![0],
                },
            ],
            max_instances: Limit::Finite(10_000),
            max_iterations: Limit::Finite(100),
            max_dataflow_iterations: None,
            max_aggregate_depth: Limit::Finite(8),
            max_recursive_instances: Limit::Finite(8),
            max_incomplete_notes: Limit::Finite(5),
        }
    }
}

impl Config {
    pub fn dataflow_iterations(&self) -> Limit {
        self.max_dataflow_iterations.unwrap_or(self.max_iterations)
    }

    pub fn validate(&self) -> Result<(), String> {
        for path in self
            .prohibited
            .iter()
            .map(|r| &r.path)
            .chain(self.insulators.iter().map(|r| &r.path))
        {
            if path.is_empty() || !path.contains("::") || path.chars().any(char::is_whitespace) {
                return Err(format!(
                    "expected a crate-qualified function path, got `{path}`"
                ));
            }
        }
        for rule in &self.insulators {
            if rule.callback_args.is_empty() {
                return Err(format!(
                    "insulator `{}` needs at least one callback-args index",
                    rule.path
                ));
            }
        }
        Ok(())
    }
}

/// A deliberately small glob language: `*` matches zero or more characters,
/// including path separators. Everything else is literal.
pub fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, path) = (pattern.as_bytes(), path.as_bytes());
    let (mut p, mut s, mut star, mut retry) = (0, 0, None, 0);
    while s < path.len() {
        if p < pattern.len() && pattern[p] == path[s] {
            p += 1;
            s += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = s;
        } else if let Some(index) = star {
            retry += 1;
            s = retry;
            p = index + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_preserve_defaults_and_dataflow_fallback() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.max_instances, Limit::Finite(10_000));
        assert_eq!(config.max_iterations, Limit::Finite(100));
        assert_eq!(config.dataflow_iterations(), Limit::Finite(100));
        assert_eq!(config.max_aggregate_depth, Limit::Finite(8));
        assert_eq!(config.max_recursive_instances, Limit::Finite(8));
        assert_eq!(config.max_incomplete_notes, Limit::Finite(5));
        for (text, expected) in [
            ("123", Limit::Finite(123)),
            ("'unlimited'", Limit::Unlimited),
        ] {
            let config: Config = toml::from_str(&format!("max-iterations = {text}")).unwrap();
            assert_eq!(config.dataflow_iterations(), expected);
            let config: Config = toml::from_str(&format!(
                "max-iterations = {text}\nmax-dataflow-iterations = 7"
            ))
            .unwrap();
            assert_eq!(config.dataflow_iterations(), Limit::Finite(7));
        }
        let config: Config =
            toml::from_str("max-iterations = 7\nmax-dataflow-iterations = 'unlimited'").unwrap();
        assert_eq!(config.max_iterations, Limit::Finite(7));
        assert_eq!(config.dataflow_iterations(), Limit::Unlimited);
    }

    #[test]
    fn every_budget_accepts_positive_integers_or_unlimited_only() {
        for key in [
            "max-instances",
            "max-iterations",
            "max-dataflow-iterations",
            "max-aggregate-depth",
            "max-recursive-instances",
            "max-incomplete-notes",
        ] {
            for value in ["1", "100000", "'unlimited'"] {
                let config: Config = toml::from_str(&format!("{key} = {value}")).unwrap();
                assert!(config.validate().is_ok());
            }
            for value in ["0", "-1", "'Unlimited'", "'100'", "''", "true", "1.5", "[]"] {
                assert!(
                    toml::from_str::<Config>(&format!("{key} = {value}")).is_err(),
                    "{key} = {value}"
                );
            }
            assert!(toml::from_str::<Config>(&format!("{key}-typo = 1")).is_err());
        }
        assert!(Limit::Unlimited.allows(usize::MAX));
        assert!(!Limit::Unlimited.exhausted(usize::MAX));
    }

    #[test]
    fn rules_are_anchored_and_globs_backtrack() {
        assert!(matches(
            "tokio::runtime::*::block_on",
            "tokio::runtime::runtime::Runtime::block_on"
        ));
        assert!(matches("a::*b*c", "a::bbbc"));
        assert!(!matches("a::b", "x::a::b"));
        assert!(!matches("a::*::b", "a::b"));
    }

    #[test]
    fn config_rejects_typos_and_invalid_contracts() {
        assert!(toml::from_str::<Config>("prohibitted = []").is_err());
        assert!(toml::from_str::<Config>("max-instances = 0").is_err());
        assert!(
            toml::from_str::<Config>("[[insulators]]\npath = 'test::spawn'\ncallback-args = []")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(Config::default().validate().is_ok());
    }
}
