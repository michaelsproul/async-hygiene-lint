use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub prohibited: Vec<Prohibited>,
    pub insulators: Vec<Insulator>,
    pub max_instances: usize,
    pub max_iterations: usize,
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
                    path: "tokio::runtime::*::block_on".into(),
                    reason: "cannot start a runtime from within a runtime".into(),
                },
                Prohibited {
                    path: "futures_executor::*block_on".into(),
                    reason: "blocks the executor thread".into(),
                },
            ],
            insulators: vec![
                Insulator {
                    path: "tokio::task::blocking::spawn_blocking".into(),
                    callback_args: vec![0],
                },
                Insulator {
                    path: "tokio::runtime::*::spawn_blocking".into(),
                    callback_args: vec![1],
                },
                Insulator {
                    path: "tokio::task::blocking::block_in_place".into(),
                    callback_args: vec![0],
                },
                Insulator {
                    path: "std::thread::spawn".into(),
                    callback_args: vec![0],
                },
            ],
            max_instances: 10_000,
            max_iterations: 100,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_instances == 0 || self.max_iterations == 0 {
            return Err("analysis limits must be greater than zero".into());
        }
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
        assert!(
            toml::from_str::<Config>("max-instances = 0")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            toml::from_str::<Config>("[[insulators]]\npath = 'test::spawn'\ncallback-args = []")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(Config::default().validate().is_ok());
    }
}
