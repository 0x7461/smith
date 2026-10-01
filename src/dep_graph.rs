use std::collections::{HashMap, HashSet, VecDeque};

use crate::package::Package;

pub struct DepGraph {
    /// package name -> set of custom packages it depends on
    pub forward: HashMap<String, HashSet<String>>,
    /// package name -> set of custom packages that depend on it
    pub reverse: HashMap<String, HashSet<String>>,
}

/// Packages [`DepGraph::topological_sort`] could not order: the members of
/// dependency cycles, plus everything downstream of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedDeps {
    /// The packages that could not be ordered, sorted.
    pub packages: Vec<String>,
    /// Build order of the packages that *could* be ordered, so a caller that
    /// only wanted a subset of them does not lose the order to this error.
    pub order: Vec<String>,
}

impl std::fmt::Display for UnresolvedDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.packages.join(", "))
    }
}

impl DepGraph {
    /// Build a dependency graph filtered to only inter-custom-package edges.
    pub fn build(packages: &[Package]) -> Self {
        let custom_names: HashSet<String> = packages.iter().map(|p| p.name.clone()).collect();
        let mut forward: HashMap<String, HashSet<String>> = HashMap::new();
        let mut reverse: HashMap<String, HashSet<String>> = HashMap::new();

        for pkg in packages {
            let mut deps = HashSet::new();
            for dep_name in pkg
                .makedepends
                .iter()
                .chain(pkg.hostmakedepends.iter())
                .chain(pkg.depends.iter())
            {
                // Strip -devel suffix to match base package names
                let base = dep_name
                    .strip_suffix("-devel")
                    .unwrap_or(dep_name)
                    .to_string();
                if custom_names.contains(&base) && base != pkg.name {
                    deps.insert(base.clone());
                    reverse
                        .entry(base)
                        .or_default()
                        .insert(pkg.name.clone());
                }
            }
            forward.insert(pkg.name.clone(), deps);
        }

        // Ensure all packages have entries
        for pkg in packages {
            forward.entry(pkg.name.clone()).or_default();
            reverse.entry(pkg.name.clone()).or_default();
        }

        DepGraph { forward, reverse }
    }

    /// Topological sort of all packages (for build order).
    ///
    /// Returns `Err` naming the packages that could not be ordered — cycle
    /// members and their dependents — rather than a partial order that drops
    /// them. A missing package is a package that never gets built, and the
    /// rest of the order looks fine, so the caller must not be able to ignore
    /// it.
    pub fn topological_sort(&self) -> Result<Vec<String>, UnresolvedDeps> {
        // in_degree[x] = number of custom deps x has
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for (name, deps) in &self.forward {
            in_degree.insert(name.clone(), deps.len());
        }

        let mut queue: VecDeque<String> = VecDeque::new();
        for (name, &deg) in &in_degree {
            if deg == 0 {
                queue.push_back(name.clone());
            }
        }

        let mut result = Vec::new();
        while let Some(name) = queue.pop_front() {
            result.push(name.clone());
            if let Some(dependents) = self.reverse.get(&name) {
                for dep in dependents {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }
        }

        if result.len() != self.forward.len() {
            let ordered: HashSet<&str> = result.iter().map(String::as_str).collect();
            let mut packages: Vec<String> = self
                .forward
                .keys()
                .filter(|n| !ordered.contains(n.as_str()))
                .cloned()
                .collect();
            packages.sort();
            return Err(UnresolvedDeps { packages, order: result });
        }

        Ok(result)
    }

    /// Order `wanted` for building, reporting unresolved packages rather than
    /// letting them block or vanish.
    ///
    /// An unresolved package inside `wanted` fails the whole call: it can never
    /// be built, and `Err` names exactly those packages (the unresolved ones
    /// outside `wanted` are not the caller's problem yet). An unresolved
    /// package outside `wanted` does not block anything — the order of the rest
    /// is still valid — so it comes back alongside the jobs for the caller to
    /// report.
    pub fn plan_build_order(
        &self,
        wanted: &HashSet<String>,
    ) -> Result<(Vec<String>, Vec<String>), UnresolvedDeps> {
        let unresolved = match self.topological_sort() {
            Ok(order) => return Ok((wanted_jobs(order, wanted), Vec::new())),
            Err(unresolved) => unresolved,
        };

        let (blocked, outside): (Vec<String>, Vec<String>) = unresolved
            .packages
            .into_iter()
            .partition(|n| wanted.contains(n));

        if !blocked.is_empty() {
            return Err(UnresolvedDeps { packages: blocked, order: unresolved.order });
        }

        Ok((wanted_jobs(unresolved.order, wanted), outside))
    }

/// Get tree of reverse dependencies for a package (for tree view).
    pub fn reverse_dep_tree(&self, name: &str) -> Vec<TreeNode> {
        self.build_tree(name, &mut HashSet::new())
    }

    fn build_tree(&self, name: &str, visited: &mut HashSet<String>) -> Vec<TreeNode> {
        if visited.contains(name) {
            return vec![];
        }
        visited.insert(name.to_string());

        let mut children = Vec::new();
        if let Some(dependents) = self.reverse.get(name) {
            let mut sorted: Vec<&String> = dependents.iter().collect();
            sorted.sort();
            for dep in sorted {
                let subtree = self.build_tree(dep, visited);
                children.push(TreeNode {
                    name: dep.clone(),
                    children: subtree,
                });
            }
        }
        children
    }
}

/// Filter a full build order down to the packages the caller asked for.
fn wanted_jobs(order: Vec<String>, wanted: &HashSet<String>) -> Vec<String> {
    order.into_iter().filter(|n| wanted.contains(n)).collect()
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub children: Vec<TreeNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A package with only the fields the graph reads. `deps` go in
    /// `makedepends` unless a test needs a specific list.
    fn pkg(name: &str, deps: &[&str]) -> Package {
        Package {
            name: name.into(),
            version: "1.0".into(),
            revision: 1,
            short_desc: String::new(),
            homepage: String::new(),
            build_style: String::new(),
            makedepends: deps.iter().map(|s| s.to_string()).collect(),
            hostmakedepends: vec![],
            depends: vec![],
            distfiles: String::new(),
            changelog: String::new(),
        }
    }

    /// Position of a package in the build order. Panics if it was dropped —
    /// which is the point: a missing package is never a passing assertion.
    fn pos(order: &[String], name: &str) -> usize {
        order
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("{} missing from build order: {:?}", name, order))
    }

    /// The real shape this tool exists for: bumping hyprutils must rebuild the
    /// things that link it.
    fn hypr_stack() -> Vec<Package> {
        vec![
            pkg("hyprutils", &[]),
            pkg("hyprlang", &["hyprutils-devel"]),
            pkg("hyprgraphics", &["hyprutils-devel"]),
            pkg("hyprlock", &["hyprlang-devel", "hyprgraphics-devel", "hyprutils-devel"]),
        ]
    }

    // ── build ───────────────────────────────────────────────────────

    #[test]
    fn devel_suffix_is_stripped_to_the_base_package() {
        // Templates depend on `hyprutils-devel`; the graph is keyed on `hyprutils`.
        // Without the strip every edge in the hypr stack silently disappears.
        let g = DepGraph::build(&hypr_stack());
        assert!(g.forward["hyprlang"].contains("hyprutils"));
        assert!(g.reverse["hyprutils"].contains("hyprlang"));
    }

    #[test]
    fn external_dependencies_are_not_edges() {
        // Only inter-custom edges matter — cmake is not ours to build.
        let g = DepGraph::build(&[pkg("hyprutils", &["cmake", "pixman-devel"])]);
        assert!(g.forward["hyprutils"].is_empty());
    }

    #[test]
    fn a_package_is_not_its_own_dependency() {
        // A self-edge would give it in_degree 1 forever and drop it silently.
        let g = DepGraph::build(&[pkg("zig", &["zig-devel", "zig"])]);
        assert!(g.forward["zig"].is_empty());
        assert_eq!(DepGraph::build(&[pkg("zig", &["zig"])]).topological_sort().unwrap(), vec!["zig"]);
    }

    #[test]
    fn all_three_dependency_lists_are_read() {
        let mut p = pkg("hyprlock", &["hyprutils-devel"]);
        p.hostmakedepends = vec!["hyprwayland-scanner".into()];
        p.depends = vec!["hyprlang-devel".into()];
        let g = DepGraph::build(&[
            pkg("hyprutils", &[]),
            pkg("hyprwayland-scanner", &[]),
            pkg("hyprlang", &[]),
            p,
        ]);
        let deps = &g.forward["hyprlock"];
        assert!(deps.contains("hyprutils"), "makedepends dropped");
        assert!(deps.contains("hyprwayland-scanner"), "hostmakedepends dropped");
        assert!(deps.contains("hyprlang"), "depends dropped");
    }

    #[test]
    fn every_package_gets_an_entry_even_with_no_edges() {
        let g = DepGraph::build(&[pkg("ghostty", &[]), pkg("zed", &[])]);
        assert!(g.forward.contains_key("ghostty") && g.reverse.contains_key("zed"));
    }

    // ── topological_sort ────────────────────────────────────────────

    #[test]
    fn dependencies_are_built_before_dependents() {
        // Relative order only: the sort iterates a HashMap, so the sequence
        // within a tier is not stable and asserting it would flake.
        let order = DepGraph::build(&hypr_stack()).topological_sort().unwrap();
        assert_eq!(order.len(), 4);
        assert!(pos(&order, "hyprutils") < pos(&order, "hyprlang"));
        assert!(pos(&order, "hyprutils") < pos(&order, "hyprgraphics"));
        assert!(pos(&order, "hyprlang") < pos(&order, "hyprlock"));
        assert!(pos(&order, "hyprgraphics") < pos(&order, "hyprlock"));
    }

    #[test]
    fn unrelated_packages_all_appear() {
        let g = DepGraph::build(&[pkg("ghostty", &[]), pkg("zed", &[]), pkg("ollama", &[])]);
        assert_eq!(g.topological_sort().unwrap().len(), 3);
    }

    #[test]
    fn a_dependency_cycle_is_reported_not_dropped() {
        // Kahn's algorithm never reaches in_degree 0 inside a cycle. Returning
        // the partial order made the cycle's packages vanish silently while
        // unrelated ones came through, so nothing looked wrong.
        let g = DepGraph::build(&[
            pkg("a", &["b"]),
            pkg("b", &["a"]),
            pkg("ghostty", &[]),
        ]);
        let err = g
            .topological_sort()
            .expect_err("a cycle must not produce a partial order");
        assert_eq!(err.packages, vec!["a", "b"]);
        assert_eq!(err.to_string(), "a, b");
        assert_eq!(err.order, vec!["ghostty"], "the unaffected order survives the error");
    }

    #[test]
    fn a_cycle_also_reports_what_depends_on_it() {
        // A dependent of a cycle never reaches in_degree 0 either, so it is
        // just as unbuildable and must be named too.
        let g = DepGraph::build(&[
            pkg("a", &["b"]),
            pkg("b", &["a"]),
            pkg("zed", &["a"]),
        ]);
        let err = g.topological_sort().expect_err("cycle must be reported");
        assert_eq!(err.packages, vec!["a", "b", "zed"]);
        assert!(err.order.is_empty(), "nothing was orderable");
    }

    // ── plan_build_order ────────────────────────────────────────────

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_cycle_in_the_build_set_blocks_the_run() {
        let g = DepGraph::build(&[
            pkg("a", &["b"]),
            pkg("b", &["a"]),
            pkg("ghostty", &[]),
        ]);
        // `b` is unresolved too, but it is not being built, so it is not named:
        // the message is about what this run cannot do.
        let err = g
            .plan_build_order(&set(&["a", "ghostty"]))
            .expect_err("an unresolved package in the build set must refuse the run");
        assert_eq!(err.packages, vec!["a"]);
    }

    #[test]
    fn a_cycle_outside_the_build_set_does_not_block_it() {
        let g = DepGraph::build(&[
            pkg("a", &["b"]),
            pkg("b", &["a"]),
            pkg("ghostty", &[]),
        ]);
        let (jobs, unresolved) = g
            .plan_build_order(&set(&["ghostty"]))
            .expect("ghostty does not depend on the cycle");
        assert_eq!(jobs, vec!["ghostty"]);
        assert_eq!(unresolved, vec!["a", "b"], "still reported, not dropped");
    }

    #[test]
    fn a_clean_graph_plans_with_no_unresolved() {
        let g = DepGraph::build(&hypr_stack());
        let (jobs, unresolved) = g.plan_build_order(&set(&["hyprlock"])).expect("acyclic");
        assert_eq!(jobs, vec!["hyprlock"], "only the wanted package is a job");
        assert!(unresolved.is_empty());
    }

    // ── reverse_dep_tree ────────────────────────────────────────────

    #[test]
    fn reverse_tree_lists_what_a_bump_must_rebuild() {
        let g = DepGraph::build(&hypr_stack());
        let tree = g.reverse_dep_tree("hyprutils");
        let names: Vec<&str> = tree.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["hyprgraphics", "hyprlang", "hyprlock"], "children are sorted");
    }

    #[test]
    fn a_leaf_has_no_dependents() {
        assert!(DepGraph::build(&hypr_stack()).reverse_dep_tree("hyprlock").is_empty());
    }

    fn count(nodes: &[TreeNode], name: &str, seen: &mut usize) {
        for n in nodes {
            if n.name == name {
                *seen += 1;
            }
            count(&n.children, name, seen);
        }
    }

    #[test]
    fn a_diamond_lists_the_shared_dependent_once_per_path() {
        // `visited` prunes the sub*tree*, not the node — the node is pushed
        // before the guard is consulted. So in the hypr diamond hyprlock shows
        // three times: under hyprutils directly, and under each of hyprlang and
        // hyprgraphics. Not a defect — each really is a path a rebuild travels —
        // but pinned because it is not what the `visited` set looks like it does.
        let g = DepGraph::build(&hypr_stack());
        let tree = g.reverse_dep_tree("hyprutils");
        let mut seen = 0;
        count(&tree, "hyprlock", &mut seen);
        assert_eq!(seen, 3);
    }

    #[test]
    fn only_the_first_occurrence_carries_its_children() {
        // The consequence of the above, and the sharper edge: a node reached
        // again renders as a leaf even when it has dependents of its own, so a
        // repeat occurrence understates what a bump rebuilds.
        let g = DepGraph::build(&[
            pkg("base", &[]),
            pkg("mid_a", &["base"]),
            pkg("mid_b", &["base"]),
            pkg("shared", &["mid_a", "mid_b"]),
            pkg("top", &["shared"]),
        ]);
        let tree = g.reverse_dep_tree("base");

        let mut with_children = 0;
        fn walk(nodes: &[TreeNode], name: &str, n: &mut usize) {
            for node in nodes {
                if node.name == name && !node.children.is_empty() {
                    *n += 1;
                }
                walk(&node.children, name, n);
            }
        }
        walk(&tree, "shared", &mut with_children);

        let mut total = 0;
        count(&tree, "shared", &mut total);
        assert!(total > with_children, "expected a repeat occurrence rendered as a leaf");
        assert_eq!(with_children, 1, "only one occurrence expands its subtree");
    }

    #[test]
    fn an_unknown_package_has_an_empty_tree() {
        assert!(DepGraph::build(&hypr_stack()).reverse_dep_tree("nope").is_empty());
    }
}
