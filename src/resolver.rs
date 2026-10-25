//! Dependency resolver for spk.
//!
//! Debian-style versions, version constraints, alternatives (`a | b`),
//! virtual packages (`provides`), `conflicts`, and a backtracking solver that
//! produces an install order or an explanation of why no solution exists.
//!
//! Installed packages are treated as fixed: the solver never upgrades or
//! removes anything that is already installed, it only adds packages.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

// ── Versions ─────────────────────────────────────────────────────────────────

/// Compare two version strings with dpkg's algorithm:
/// `[epoch:]upstream[-revision]`, where `~` sorts before everything
/// (including the end of the string).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (ea, ra) = split_epoch(a);
    let (eb, rb) = split_epoch(b);
    if ea != eb {
        return ea.cmp(&eb);
    }
    let (ua, da) = split_revision(ra);
    let (ub, db) = split_revision(rb);
    match compare_part(ua, ub) {
        Ordering::Equal => compare_part(da, db),
        other => other,
    }
}

fn split_epoch(v: &str) -> (u64, &str) {
    if let Some((e, rest)) = v.split_once(':') {
        if let Ok(n) = e.parse::<u64>() {
            return (n, rest);
        }
    }
    (0, v)
}

fn split_revision(v: &str) -> (&str, &str) {
    match v.rsplit_once('-') {
        Some((u, r)) => (u, r),
        None => (v, ""),
    }
}

fn char_order(c: u8) -> i32 {
    if c == 0 {
        0
    } else if c.is_ascii_digit() {
        0
    } else if c.is_ascii_alphabetic() {
        c as i32
    } else if c == b'~' {
        -1
    } else {
        c as i32 + 256
    }
}

fn compare_part(a: &str, b: &str) -> Ordering {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let at = |s: &[u8], i: usize| -> u8 { if i < s.len() { s[i] } else { 0 } };
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() || j < b.len() {
        // Non-digit run.
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let (ac, bc) = (char_order(at(a, i)), char_order(at(b, j)));
            if ac != bc {
                return ac.cmp(&bc);
            }
            i += 1;
            j += 1;
        }
        // Digit run: skip leading zeros, then the longer number wins, ties
        // broken by the first differing digit.
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let mut first_diff = Ordering::Equal;
        while i < a.len() && a[i].is_ascii_digit() && j < b.len() && b[j].is_ascii_digit() {
            if first_diff == Ordering::Equal {
                first_diff = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if i < a.len() && a[i].is_ascii_digit() {
            return Ordering::Greater;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            return Ordering::Less;
        }
        if first_diff != Ordering::Equal {
            return first_diff;
        }
    }
    Ordering::Equal
}

// ── Dependency expressions ───────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
}

impl Op {
    fn holds(&self, ord: Ordering) -> bool {
        match self {
            Op::Ge => ord != Ordering::Less,
            Op::Le => ord != Ordering::Greater,
            Op::Gt => ord == Ordering::Greater,
            Op::Lt => ord == Ordering::Less,
            Op::Eq => ord == Ordering::Equal,
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            Op::Ge => ">=",
            Op::Le => "<=",
            Op::Gt => ">>",
            Op::Lt => "<<",
            Op::Eq => "=",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dep {
    pub name: String,
    pub constraint: Option<(Op, String)>,
}

impl Dep {
    pub fn satisfied_by_version(&self, version: &str) -> bool {
        match &self.constraint {
            None => true,
            Some((op, v)) => op.holds(compare_versions(version, v)),
        }
    }

    pub fn display(&self) -> String {
        match &self.constraint {
            None => self.name.clone(),
            Some((op, v)) => format!("{} ({} {})", self.name, op.symbol(), v),
        }
    }
}

/// Parse one dependency, e.g. `libfoo` or `libfoo (>= 1.2-3)`.
pub fn parse_dep(s: &str) -> Option<Dep> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match s.split_once('(') {
        None => Some(Dep { name: s.to_string(), constraint: None }),
        Some((name, rest)) => {
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let inner = rest.trim().trim_end_matches(')').trim();
            // Longest operators first so ">=" isn't read as ">" + "=".
            let ops: [(&str, Op); 8] = [
                (">=", Op::Ge),
                ("<=", Op::Le),
                (">>", Op::Gt),
                ("<<", Op::Lt),
                ("==", Op::Eq),
                (">", Op::Gt),
                ("<", Op::Lt),
                ("=", Op::Eq),
            ];
            for (sym, op) in ops {
                if let Some(v) = inner.strip_prefix(sym) {
                    let v = v.trim();
                    if v.is_empty() {
                        return None;
                    }
                    return Some(Dep {
                        name: name.to_string(),
                        constraint: Some((op, v.to_string())),
                    });
                }
            }
            // Unrecognised constraint: keep the name, ignore the annotation.
            Some(Dep { name: name.to_string(), constraint: None })
        }
    }
}

/// Parse a `depends:` value: comma = AND, pipe = OR.
/// `"a (>= 1), b | c"` -> `[[a>=1], [b, c]]`.
pub fn parse_depends(s: &str) -> Vec<Vec<Dep>> {
    s.split(',')
        .filter_map(|group| {
            let alts: Vec<Dep> = group.split('|').filter_map(parse_dep).collect();
            if alts.is_empty() { None } else { Some(alts) }
        })
        .collect()
}

/// Parse a `conflicts:` value: every entry is an independent conflict.
pub fn parse_conflicts(s: &str) -> Vec<Dep> {
    s.split(',').filter_map(parse_dep).collect()
}

/// Parse a `provides:` value: `name` or `name (= version)`.
pub fn parse_provides(s: &str) -> Vec<(String, Option<String>)> {
    s.split(',')
        .filter_map(parse_dep)
        .map(|d| (d.name, d.constraint.map(|(_, v)| v)))
        .collect()
}

// ── Packages ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub depends: Vec<Vec<Dep>>,
    pub provides: Vec<(String, Option<String>)>,
    pub conflicts: Vec<Dep>,
}

impl Pkg {
    pub fn new(name: &str, version: &str, depends: &str, provides: &str, conflicts: &str) -> Pkg {
        Pkg {
            name: name.to_string(),
            version: version.to_string(),
            depends: parse_depends(depends),
            provides: parse_provides(provides),
            conflicts: parse_conflicts(conflicts),
        }
    }

    /// Does this package satisfy `dep`, either under its own name or through
    /// a `provides` entry? An unversioned `provides` does not satisfy a
    /// versioned dependency.
    pub fn satisfies(&self, dep: &Dep) -> bool {
        if self.name == dep.name && dep.satisfied_by_version(&self.version) {
            return true;
        }
        self.provides.iter().any(|(n, v)| {
            n == &dep.name
                && match (&dep.constraint, v) {
                    (None, _) => true,
                    (Some(_), Some(pv)) => dep.satisfied_by_version(pv),
                    (Some(_), None) => false,
                }
        })
    }
}

/// Parse an `index.meta` file: blank-line separated stanzas of `key: value`
/// lines (`name`, `version`, `depends`, `provides`, `conflicts`).
pub fn parse_index_meta(text: &str) -> Vec<Pkg> {
    let mut out = Vec::new();
    for stanza in text.split("\n\n") {
        let mut f: HashMap<&str, &str> = HashMap::new();
        for line in stanza.lines() {
            if let Some((k, v)) = line.split_once(':') {
                f.insert(k.trim(), v.trim());
            }
        }
        let (Some(name), Some(version)) = (f.get("name"), f.get("version")) else {
            continue;
        };
        out.push(Pkg::new(
            name,
            version,
            f.get("depends").copied().unwrap_or(""),
            f.get("provides").copied().unwrap_or(""),
            f.get("conflicts").copied().unwrap_or(""),
        ));
    }
    out
}

// ── Solver ───────────────────────────────────────────────────────────────────

const STEP_LIMIT: usize = 200_000;

pub struct Universe {
    /// Everything the repositories offer (all versions).
    pub available: Vec<Pkg>,
    /// What is installed now; these are never changed by the solver.
    pub installed: Vec<Pkg>,
}

#[derive(Debug)]
pub enum SolveError {
    /// No package (by name or `provides`) exists for this dependency at all.
    NotFound(String),
    /// Candidates exist but none can be chosen; the string explains why.
    Unsatisfiable(String),
    /// The search exceeded its step limit.
    TooComplex,
}

impl SolveError {
    pub fn message(&self) -> String {
        match self {
            SolveError::NotFound(d) => format!("no package provides '{d}'"),
            SolveError::Unsatisfiable(why) => why.clone(),
            SolveError::TooComplex => {
                "the dependency search became too large to finish; try installing the \
                 pieces separately"
                    .to_string()
            }
        }
    }
}

struct Chosen {
    pkg: Pkg,
    installed: bool,
}

struct Ctx {
    steps: usize,
    aborted: bool,
    fail_depth: usize,
    fail_msg: Option<String>,
}

type Goal = (Vec<Dep>, String);

pub struct Solver<'a> {
    uni: &'a Universe,
    by_name: HashMap<&'a str, Vec<usize>>,
    by_provide: HashMap<&'a str, Vec<usize>>,
}

impl<'a> Solver<'a> {
    pub fn new(uni: &'a Universe) -> Solver<'a> {
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut by_provide: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, p) in uni.available.iter().enumerate() {
            by_name.entry(p.name.as_str()).or_default().push(i);
            for (n, _) in &p.provides {
                by_provide.entry(n.as_str()).or_default().push(i);
            }
        }
        Solver { uni, by_name, by_provide }
    }

    /// Resolve `requests` (each a disjunction of alternatives) against the
    /// universe and return the packages to install, dependencies first.
    pub fn solve(&self, requests: &[Vec<Dep>]) -> Result<Vec<Pkg>, SolveError> {
        // A request nobody can ever satisfy is a clearer error than a failed search.
        for alts in requests {
            if alts.iter().all(|d| self.candidates(d).is_empty() && !self.installed_satisfies(d)) {
                // Distinguish "nothing by that name exists" from "it exists
                // but not in a version that fits".
                if let Some(d) = alts.iter().find(|d| self.versions_of(&d.name).is_some()) {
                    return Err(SolveError::Unsatisfiable(format!(
                        "request needs {}: {}",
                        d.display(),
                        self.versions_of(&d.name).unwrap_or_default()
                    )));
                }
                return Err(SolveError::NotFound(alts[0].display()));
            }
        }
        let mut chosen: Vec<Chosen> = self
            .uni
            .installed
            .iter()
            .map(|p| Chosen { pkg: p.clone(), installed: true })
            .collect();
        let goals: Vec<Goal> = requests
            .iter()
            .rev()
            .map(|alts| (alts.clone(), "request".to_string()))
            .collect();
        let mut ctx = Ctx { steps: 0, aborted: false, fail_depth: 0, fail_msg: None };
        if self.search(&mut chosen, goals, &mut ctx) {
            Ok(Self::order(&chosen))
        } else if ctx.aborted {
            Err(SolveError::TooComplex)
        } else {
            Err(SolveError::Unsatisfiable(
                ctx.fail_msg.unwrap_or_else(|| "no consistent set of packages exists".to_string()),
            ))
        }
    }

    fn installed_satisfies(&self, dep: &Dep) -> bool {
        self.uni.installed.iter().any(|p| p.satisfies(dep))
    }

    /// "available versions: a, b" for a package name, or None if no package
    /// of that name exists (installed or available).
    fn versions_of(&self, name: &str) -> Option<String> {
        let mut vs: Vec<&str> = self
            .by_name
            .get(name)
            .map(|v| v.iter().map(|&i| self.uni.available[i].version.as_str()).collect())
            .unwrap_or_default();
        vs.sort_by(|a, b| compare_versions(b, a));
        let installed = self.uni.installed.iter().find(|p| p.name == name);
        if vs.is_empty() && installed.is_none() {
            return None;
        }
        let mut parts = Vec::new();
        if !vs.is_empty() {
            parts.push(format!("available versions: {}", vs.join(", ")));
        }
        if let Some(p) = installed {
            parts.push(format!("installed: {}", p.version));
        }
        Some(parts.join("; "))
    }

    /// Candidates for `dep`: packages with that exact name first (newest
    /// version first), then packages that provide it.
    fn candidates(&self, dep: &Dep) -> Vec<&'a Pkg> {
        let mut named: Vec<&Pkg> = self
            .by_name
            .get(dep.name.as_str())
            .map(|v| v.iter().map(|&i| &self.uni.available[i]).collect())
            .unwrap_or_default();
        named.retain(|p| p.satisfies(dep));
        named.sort_by(|a, b| compare_versions(&b.version, &a.version));

        let mut provided: Vec<&Pkg> = self
            .by_provide
            .get(dep.name.as_str())
            .map(|v| v.iter().map(|&i| &self.uni.available[i]).collect())
            .unwrap_or_default();
        provided.retain(|p| p.name != dep.name && p.satisfies(dep));
        provided.sort_by(|a, b| a.name.cmp(&b.name).then(compare_versions(&b.version, &a.version)));

        named.extend(provided);
        named
    }

    fn conflict_with(chosen: &[Chosen], cand: &Pkg) -> Option<String> {
        for c in chosen {
            if c.pkg.name == cand.name {
                continue;
            }
            if let Some(d) = c.pkg.conflicts.iter().find(|d| cand.satisfies(d)) {
                return Some(format!(
                    "{} {} conflicts with {} (declared by {})",
                    cand.name, cand.version, d.display(), c.pkg.name
                ));
            }
            if let Some(d) = cand.conflicts.iter().find(|d| c.pkg.satisfies(d)) {
                return Some(format!(
                    "{} {} conflicts with installed/selected {} {} ({})",
                    cand.name, cand.version, c.pkg.name, c.pkg.version, d.display()
                ));
            }
        }
        None
    }

    fn search(&self, chosen: &mut Vec<Chosen>, mut goals: Vec<Goal>, ctx: &mut Ctx) -> bool {
        ctx.steps += 1;
        if ctx.steps > STEP_LIMIT {
            ctx.aborted = true;
            return false;
        }
        let Some((alts, why)) = goals.pop() else {
            return true;
        };
        if alts.iter().any(|d| chosen.iter().any(|c| c.pkg.satisfies(d))) {
            return self.search(chosen, goals, ctx);
        }

        let mut reasons: Vec<String> = Vec::new();
        for dep in &alts {
            for cand in self.candidates(dep) {
                if let Some(held) = chosen.iter().find(|c| c.pkg.name == cand.name) {
                    reasons.push(format!(
                        "{} {} is {} but {} is needed",
                        held.pkg.name,
                        held.pkg.version,
                        if held.installed { "installed" } else { "already selected" },
                        dep.display()
                    ));
                    continue;
                }
                if let Some(why_not) = Self::conflict_with(chosen, cand) {
                    reasons.push(why_not);
                    continue;
                }
                chosen.push(Chosen { pkg: cand.clone(), installed: false });
                let mut next = goals.clone();
                for alts in cand.depends.iter().rev() {
                    next.push((alts.clone(), cand.name.clone()));
                }
                if self.search(chosen, next, ctx) {
                    return true;
                }
                chosen.pop();
                if ctx.aborted {
                    return false;
                }
            }
        }

        let wanted = alts.iter().map(|d| d.display()).collect::<Vec<_>>().join(" | ");
        let mut msg = format!("{why} needs {wanted}");
        if reasons.is_empty() {
            for d in &alts {
                if let Some(p) = self.uni.installed.iter().find(|p| p.name == d.name) {
                    msg.push_str(&format!(": installed {} {} does not satisfy it", p.name, p.version));
                }
            }
            if !msg.contains(':') {
                match alts.iter().find_map(|d| self.versions_of(&d.name)) {
                    Some(v) => msg.push_str(&format!(": no version fits ({v})")),
                    None => msg.push_str(": no available package satisfies it"),
                }
            }
        } else {
            msg.push_str(&format!(": {}", reasons.join("; ")));
        }
        if chosen.len() >= ctx.fail_depth {
            ctx.fail_depth = chosen.len();
            ctx.fail_msg = Some(msg);
        }
        false
    }

    /// Order the newly selected packages so dependencies come first.
    /// Dependency cycles are legal: back-edges are ignored.
    fn order(chosen: &[Chosen]) -> Vec<Pkg> {
        let new: Vec<&Pkg> = chosen.iter().filter(|c| !c.installed).map(|c| &c.pkg).collect();
        let mut done: HashSet<String> = HashSet::new();
        let mut out: Vec<Pkg> = Vec::new();
        fn visit<'a>(
            p: &'a Pkg,
            new: &[&'a Pkg],
            done: &mut HashSet<String>,
            out: &mut Vec<Pkg>,
        ) {
            if !done.insert(p.name.clone()) {
                return;
            }
            for alts in &p.depends {
                if let Some(dep_pkg) = new.iter().find(|q| alts.iter().any(|d| q.satisfies(d))) {
                    visit(dep_pkg, new, done, out);
                }
            }
            out.push(p.clone());
        }
        for p in &new {
            visit(p, &new, &mut done, &mut out);
        }
        out
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, version: &str, depends: &str) -> Pkg {
        Pkg::new(name, version, depends, "", "")
    }

    fn uni(available: Vec<Pkg>, installed: Vec<Pkg>) -> Universe {
        Universe { available, installed }
    }

    fn names(plan: &[Pkg]) -> Vec<String> {
        plan.iter().map(|p| p.name.clone()).collect()
    }

    #[test]
    fn version_ordering_follows_dpkg() {
        use Ordering::*;
        assert_eq!(compare_versions("1.0", "1.0"), Equal);
        assert_eq!(compare_versions("1.0", "1.0-1"), Less);
        assert_eq!(compare_versions("1.0~rc1", "1.0"), Less);
        assert_eq!(compare_versions("1:0.5", "9.9"), Greater);
        assert_eq!(compare_versions("1.10", "1.9"), Greater);
        assert_eq!(compare_versions("1.0a", "1.0"), Greater);
        assert_eq!(compare_versions("1.0+b1", "1.0"), Greater);
        assert_eq!(compare_versions("6.11.2", "6.10.3"), Greater);
        assert_eq!(compare_versions("007", "7"), Equal);
        assert_eq!(compare_versions("25.08.3", "25.08.10"), Less);
    }

    #[test]
    fn parses_constraints_and_alternatives() {
        let d = parse_depends("a (>= 1.2), b | c (<< 2), d");
        assert_eq!(d.len(), 3);
        assert_eq!(d[0][0].constraint, Some((Op::Ge, "1.2".to_string())));
        assert_eq!(d[1].len(), 2);
        assert_eq!(d[1][1].constraint, Some((Op::Lt, "2".to_string())));
        assert!(d[2][0].constraint.is_none());
        assert!(parse_depends("").is_empty());
    }

    #[test]
    fn installs_dependencies_first() {
        let u = uni(vec![p("app", "1", "lib"), p("lib", "1", "base"), p("base", "1", "")], vec![]);
        let plan = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap();
        assert_eq!(names(&plan), vec!["base", "lib", "app"]);
    }

    #[test]
    fn skips_installed_packages() {
        let u = uni(vec![p("app", "1", "lib"), p("lib", "1", "")], vec![p("lib", "1", "")]);
        let plan = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap();
        assert_eq!(names(&plan), vec!["app"]);
    }

    #[test]
    fn picks_newest_version_that_fits_constraint() {
        let u = uni(
            vec![p("app", "1", "lib (<< 2)"), p("lib", "1.5", ""), p("lib", "2.1", ""), p("lib", "1.9", "")],
            vec![],
        );
        let plan = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap();
        let lib = plan.iter().find(|x| x.name == "lib").unwrap();
        assert_eq!(lib.version, "1.9");
    }

    #[test]
    fn falls_back_to_alternative() {
        let u = uni(vec![p("app", "1", "missing | other"), p("other", "1", "")], vec![]);
        let plan = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap();
        assert_eq!(names(&plan), vec!["other", "app"]);
    }

    #[test]
    fn resolves_virtual_packages() {
        let mta = Pkg::new("postfix", "3", "", "mail-agent", "");
        let u = uni(vec![p("app", "1", "mail-agent"), mta], vec![]);
        let plan = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap();
        assert_eq!(names(&plan), vec!["postfix", "app"]);
    }

    #[test]
    fn unversioned_provides_do_not_satisfy_versioned_deps() {
        let u = uni(
            vec![p("app", "1", "mail-agent (>= 2)"), Pkg::new("postfix", "3", "", "mail-agent", "")],
            vec![],
        );
        assert!(Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).is_err());
    }

    #[test]
    fn backtracks_around_conflicts() {
        // app needs x | y and z. x conflicts with z, so y must be chosen.
        let x = Pkg::new("x", "1", "", "", "z");
        let u = uni(vec![p("app", "1", "x | y, z"), x, p("y", "1", ""), p("z", "1", "")], vec![]);
        let plan = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap();
        let n = names(&plan);
        assert!(n.contains(&"y".to_string()) && !n.contains(&"x".to_string()));
    }

    #[test]
    fn conflict_with_installed_package_is_reported() {
        let installed = Pkg::new("old", "1", "", "", "app");
        let u = uni(vec![p("app", "1", "")], vec![installed]);
        let err = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap_err();
        assert!(err.message().contains("conflicts"), "{}", err.message());
    }

    #[test]
    fn missing_dependency_names_the_culprit() {
        let u = uni(vec![p("app", "1", "ghost")], vec![]);
        let err = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap_err();
        let m = err.message();
        assert!(m.contains("ghost") && m.contains("app"), "{m}");
    }

    #[test]
    fn unknown_request_is_not_found() {
        let u = uni(vec![], vec![]);
        match Solver::new(&u).solve(&[vec![parse_dep("nothing").unwrap()]]) {
            Err(SolveError::NotFound(n)) => assert_eq!(n, "nothing"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unmet_version_lists_what_exists() {
        let u = uni(vec![p("app", "1", "lib (<< 1)"), p("lib", "2.0", ""), p("lib", "3.0", "")], vec![]);
        let err = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap_err();
        assert!(err.message().contains("available versions: 3.0, 2.0"), "{}", err.message());
        let err = Solver::new(&u).solve(&[vec![parse_dep("lib (>= 9)").unwrap()]]).unwrap_err();
        assert!(matches!(err, SolveError::Unsatisfiable(_)), "{err:?}");
        assert!(err.message().contains("3.0"), "{}", err.message());
    }

    #[test]
    fn installed_version_too_old_is_explained() {
        let u = uni(vec![p("app", "1", "lib (>= 2)")], vec![p("lib", "1", "")]);
        let err = Solver::new(&u).solve(&[vec![parse_dep("app").unwrap()]]).unwrap_err();
        assert!(err.message().contains("installed lib 1"), "{}", err.message());
    }

    #[test]
    fn dependency_cycles_are_allowed() {
        let u = uni(vec![p("a", "1", "b"), p("b", "1", "a")], vec![]);
        let plan = Solver::new(&u).solve(&[vec![parse_dep("a").unwrap()]]).unwrap();
        let mut n = names(&plan);
        n.sort();
        assert_eq!(n, vec!["a", "b"]);
    }

    #[test]
    fn index_meta_round_trips() {
        let text = "name: a\nversion: 1.0\ndepends: b (>= 2)\n\nname: b\nversion: 2.0\nprovides: virt\n";
        let pkgs = parse_index_meta(text);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].depends[0][0].name, "b");
        assert_eq!(pkgs[1].provides[0].0, "virt");
    }
}
