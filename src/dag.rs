use crate::config::ArigConfig;
use std::collections::{HashMap, HashSet, VecDeque};

/// Returns service names in topological order (respecting depends_on).
/// Services with no dependencies come first. Services in the same "wave"
/// can be started in parallel.
pub fn toposort(config: &ArigConfig) -> anyhow::Result<Vec<Vec<String>>> {
    let names: HashSet<&str> = config.services.keys().map(|s| s.as_str()).collect();

    // Validate that all depends_on references exist
    for (name, service) in &config.services {
        for dep in &service.depends_on {
            if !names.contains(dep.as_str()) {
                anyhow::bail!("service '{name}' depends on unknown service '{dep}'");
            }
        }
    }

    // Build in-degree map
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for name in &names {
        in_degree.insert(name, 0);
    }
    for (name, service) in &config.services {
        in_degree.insert(name.as_str(), service.depends_on.len());
        for dep in &service.depends_on {
            dependents.entry(dep.as_str()).or_default().push(name.as_str());
        }
    }

    // Kahn's algorithm, collecting by wave
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut queue: VecDeque<&str> = VecDeque::new();

    for (name, &deg) in &in_degree {
        if deg == 0 {
            queue.push_back(name);
        }
    }

    let mut visited = 0;
    while !queue.is_empty() {
        let wave: Vec<&str> = queue.drain(..).collect();
        visited += wave.len();

        for &name in &wave {
            if let Some(deps) = dependents.get(name) {
                for &dep in deps {
                    let deg = in_degree.get_mut(dep).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(dep);
                    }
                }
            }
        }

        waves.push(wave.into_iter().map(String::from).collect());
    }

    if visited != names.len() {
        anyhow::bail!("circular dependency detected");
    }

    Ok(waves)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ArigConfig, ServiceConfig};
    use std::collections::HashMap;

    fn svc(command: &str, depends_on: Vec<&str>) -> ServiceConfig {
        ServiceConfig {
            command: command.to_string(),
            service_type: Default::default(),
            working_dir: None,
            env: HashMap::new(),
            depends_on: depends_on.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn no_dependencies() {
        let config = ArigConfig {
            services: HashMap::from([
                ("a".into(), svc("echo a", vec![])),
                ("b".into(), svc("echo b", vec![])),
            ]),
        };
        let waves = toposort(&config).unwrap();
        assert_eq!(waves.len(), 1);
        assert_eq!(waves[0].len(), 2);
    }

    #[test]
    fn linear_chain() {
        let config = ArigConfig {
            services: HashMap::from([
                ("db".into(), svc("echo db", vec![])),
                ("api".into(), svc("echo api", vec!["db"])),
                ("web".into(), svc("echo web", vec!["api"])),
            ]),
        };
        let waves = toposort(&config).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0], vec!["db"]);
        assert_eq!(waves[1], vec!["api"]);
        assert_eq!(waves[2], vec!["web"]);
    }

    #[test]
    fn diamond() {
        let config = ArigConfig {
            services: HashMap::from([
                ("db".into(), svc("echo db", vec![])),
                ("cache".into(), svc("echo cache", vec!["db"])),
                ("api".into(), svc("echo api", vec!["db"])),
                ("web".into(), svc("echo web", vec!["cache", "api"])),
            ]),
        };
        let waves = toposort(&config).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0], vec!["db"]);
        assert!(waves[1].contains(&"cache".to_string()));
        assert!(waves[1].contains(&"api".to_string()));
        assert_eq!(waves[2], vec!["web"]);
    }

    #[test]
    fn cycle_detected() {
        let config = ArigConfig {
            services: HashMap::from([
                ("a".into(), svc("echo a", vec!["b"])),
                ("b".into(), svc("echo b", vec!["a"])),
            ]),
        };
        assert!(toposort(&config).is_err());
    }

    #[test]
    fn unknown_dependency() {
        let config = ArigConfig {
            services: HashMap::from([
                ("a".into(), svc("echo a", vec!["nonexistent"])),
            ]),
        };
        assert!(toposort(&config).is_err());
    }
}
