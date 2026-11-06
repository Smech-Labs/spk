mod resolver {
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
        /// Name of the configured repo this candidate came from ("" for
        /// installed packages and in tests). Installs fetch from exactly this repo.
        pub repo: String,
    }

    impl Pkg {
        pub fn new(name: &str, version: &str, depends: &str, provides: &str, conflicts: &str) -> Pkg {
            Pkg {
                name: name.to_string(),
                version: version.to_string(),
                depends: parse_depends(depends),
                provides: parse_provides(provides),
                conflicts: parse_conflicts(conflicts),
                repo: String::new(),
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
            let owned: Vec<Pkg> = chosen.iter().filter(|c| !c.installed).map(|c| c.pkg.clone()).collect();
            Self::order_packages(&owned)
        }

        /// Dependencies-first order for an already chosen set of new packages.
        /// Shared with the optional libsolv backend, which only picks the set.
        pub fn order_packages(owned: &[Pkg]) -> Vec<Pkg> {
            let new: Vec<&Pkg> = owned.iter().collect();
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
}

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::path::Path;
use std::process::{Command, Stdio, exit};
use yaml_rust2::YamlLoader;

// ── ANSI ─────────────────────────────────────────────────────────────────────
const R: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";

// ── Config ────────────────────────────────────────────────────────────────────
const VERSION: &str = "2.5.0";
// Fallback used only if /etc/spk-repo-conf.yaml is missing or fails to parse
// (e.g. a system that predates this file, or one where it was deleted).
const DEFAULT_RELEASE_URL: &str =
    "https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages";
const REPO_CONF_PATH: &str = "/etc/spk-repo-conf.yaml";
const DOCS_URL: &str = "https://docs.smech.xyz";
// One text file per installed package -- see InstalledPkg below for format.
const INSTALLED_DB_DIR: &str = "/var/lib/spk/installed";
// The public half of nix2spk's signing key (see keys/nix2spk-signing.key,
// never committed). Every ~nix~ package must carry a `signature` file in
// its control.tar.xz that verifies against this key over the canonical
// message (see verify_signature/signing_message_for) -- otherwise it's
// refused, closing the suffix-spoofing hole a crafted control file with a
// fake ~nix~ name and an arbitrary payload used to walk straight through.
const NIX2SPK_PUBKEY: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA/B2iaPNEITKzbMvMd5PXD9Tjxv3dXLd5NaNvIvnNIHE=\n-----END PUBLIC KEY-----\n";

// ── Repo config ───────────────────────────────────────────────────────────────
// /etc/spk-repo-conf.yaml lets an operator point spk at one or more package
// repos without hardcoding a URL into the binary. Parsed with yaml-rust2 (spk
// is no longer a zero-dependency binary, see Cargo.toml) rather than the
// previous hand-rolled line-by-line parser, which only ever approximated a
// small fixed subset of real YAML and would have silently misread anything
// outside it (multi-line strings, flow-style lists/maps, anchors, etc.).
//
// Priority: 1 is tried first, up to 99 for ordinary lower-priority mirrors.
// 100 is a distinct "insecure/untrusted" tier, not just "even lower
// priority" -- those repos are only ever used as a last resort after every
// other configured repo has failed, and only with a loud warning printed
// first, since silently falling back to an operator-marked-untrusted source
// is exactly the kind of thing CONTRIBUTING.md's security section warns
// against normalizing.
//
// Signing. A repo entry may carry `key: /path/to/archive.pub`, the PEM
// Ed25519 public key its indexes are signed with. Such a repo is verified
// strictly and fails closed (see "Repository trust" below). A repo without
// a key is *unsigned*: it still works, with a warning, unless
// `require_signed: true` is set (top level, or on the entry) or
// SPK_REQUIRE_SIGNED is in the environment. `file://` repos are exempt from
// require_signed: a directory on this machine is the operator's own.
struct Repo {
    name: String,
    url: String,
    priority: u8,
    /// Where the public key was read from (kept so the config can be rewritten).
    key_path: Option<String>,
    /// The PEM text of the key; Err = the configured key file is unusable,
    /// which makes the repo unusable rather than silently unsigned.
    key: Result<Option<String>, String>,
    require_signed: bool,
    max_index_age_days: u64,
}

impl Repo {
    fn unsigned(name: &str, url: &str, priority: u8) -> Repo {
        Repo {
            name: name.to_string(),
            url: url.to_string(),
            priority,
            key_path: None,
            key: Ok(None),
            require_signed: false,
            max_index_age_days: DEFAULT_MAX_INDEX_AGE_DAYS,
        }
    }
}

const DEFAULT_MAX_INDEX_AGE_DAYS: u64 = 14;

fn default_repos() -> Vec<Repo> {
    vec![Repo::unsigned("default", DEFAULT_RELEASE_URL, 1)]
}

fn parse_repo_conf(text: &str) -> Vec<Repo> {
    let docs = match YamlLoader::load_from_str(text) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let Some(doc) = docs.first() else { return Vec::new() };
    let Some(entries) = doc["repos"].as_vec() else { return Vec::new() };
    let global_require = doc["require_signed"].as_bool().unwrap_or(false);

    let mut repos = Vec::new();
    for entry in entries {
        let (Some(name), Some(url)) = (entry["name"].as_str(), entry["url"].as_str()) else {
            continue;
        };
        let priority = entry["priority"]
            .as_i64()
            .and_then(|p| u8::try_from(p).ok())
            .unwrap_or(50);
        let key_path = entry["key"].as_str().map(|s| s.to_string());
        let key = match &key_path {
            None => Ok(None),
            Some(path) => match fs::read_to_string(path) {
                Ok(pem) if pem.contains("BEGIN PUBLIC KEY") => Ok(Some(pem)),
                Ok(_) => Err(format!("key file {path} is not a PEM public key")),
                Err(e) => Err(format!("cannot read key file {path}: {e}")),
            },
        };
        repos.push(Repo {
            name: name.to_string(),
            url: url.to_string(),
            priority,
            key_path,
            key,
            require_signed: entry["require_signed"].as_bool().unwrap_or(global_require),
            max_index_age_days: entry["max_index_age_days"]
                .as_i64()
                .and_then(|d| u64::try_from(d).ok())
                .filter(|d| *d > 0)
                .unwrap_or(DEFAULT_MAX_INDEX_AGE_DAYS),
        });
    }
    repos
}

/// The repo config as YAML, preserving signing settings.
fn repos_to_yaml(repos: &[Repo]) -> String {
    let mut out = String::from("repos:\n");
    for r in repos {
        out.push_str(&format!("  - name: {}\n    url: {}\n    priority: {}\n", r.name, r.url, r.priority));
        if let Some(k) = &r.key_path {
            out.push_str(&format!("    key: {k}\n"));
        }
        if r.require_signed {
            out.push_str("    require_signed: true\n");
        }
        if r.max_index_age_days != DEFAULT_MAX_INDEX_AGE_DAYS {
            out.push_str(&format!("    max_index_age_days: {}\n", r.max_index_age_days));
        }
    }
    out
}

fn load_repos() -> Vec<Repo> {
    // SPK_REPO_CONF points spk at another config file (testing, non-root use).
    let conf_path = env::var("SPK_REPO_CONF").unwrap_or_else(|_| REPO_CONF_PATH.to_string());
    match fs::read_to_string(&conf_path) {
        Ok(text) => {
            let mut repos = parse_repo_conf(&text);
            if repos.is_empty() {
                println!(
                    "{YELLOW}[spk] {REPO_CONF_PATH} has no usable repos, using built-in default.{R}"
                );
                repos = default_repos();
            }
            repos.sort_by_key(|r| r.priority);
            if env::var("SPK_REQUIRE_SIGNED").is_ok() {
                for r in &mut repos {
                    r.require_signed = true;
                }
            }
            repos
        }
        Err(_) => default_repos(),
    }
}

/// Tries every configured repo in priority order (1 first), skipping the
/// insecure/untrusted tier (priority 100) unless nothing else worked.
/// Returns true and leaves the package at `dest_tmp` on the first success.
/// A repo's `url` can be `file:///abs/path` (see `local-package-repo`) as
/// well as http(s) -- curl handles both transparently, so this needed no
/// new fetch logic of its own.
// ── Repository trust ──────────────────────────────────────────────────────────
//
// A repo with a `key:` is verified the way APT verifies a signed Release file,
// and fails closed:
//   * index.meta must carry a valid Ed25519 signature (index.meta.sig) from
//     that key, over "smechbuild-v1\nindex\nindex.meta\n<sha256>\n";
//   * it must be fresh (its `generated` header no older than
//     max_index_age_days) and never older than one already seen (rollback);
//   * every package is checked against the sha256 the signed index pins, so a
//     compromised mirror or web server cannot substitute a file.
// If a signed repo fails any of this, spk stops: it does NOT fall back to
// another repo for that package, because a lower-priority unsigned repo could
// then serve a same-named package (dependency confusion).

#[derive(Clone, Debug)]
enum IndexError {
    /// Could not be fetched; another repo may be tried.
    Unreachable(String),
    /// Fetched but not acceptable. Callers must stop, not fall back.
    Untrusted(String),
}

#[derive(Debug)]
struct VerifiedIndex {
    meta: String,
    /// (name, version) -> sha256. Authenticated only when `signed`.
    pins: HashMap<(String, String), String>,
    latest: HashMap<String, String>,
    generated: Option<u64>,
    signed: bool,
}

enum TrustError {
    /// Misconfigured key: stop, do not guess.
    Broken(String),
    /// This repo may not be used under the current policy; others still may.
    Disallowed(String),
}

enum Trust<'a> {
    Signed(&'a str),
    Unsigned,
}

fn is_file_url(url: &str) -> bool {
    url.starts_with("file://")
}

fn trust_of(repo: &Repo) -> Result<Trust<'_>, TrustError> {
    match &repo.key {
        Err(e) => Err(TrustError::Broken(e.clone())),
        Ok(Some(pem)) => Ok(Trust::Signed(pem)),
        Ok(None) if repo.require_signed && !is_file_url(&repo.url) => Err(TrustError::Disallowed(format!(
            "repo '{}' has no `key:` and signatures are required (SPK_REQUIRE_SIGNED / require_signed)",
            repo.name
        ))),
        Ok(None) => Ok(Trust::Unsigned),
    }
}

/// What the archive key signs for an index file. Must stay byte-identical to
/// smechbuild's signing.index_message().
fn index_signing_message(filename: &str, sha256_hex: &str) -> Vec<u8> {
    format!("smechbuild-v1\nindex\n{filename}\n{sha256_hex}\n").into_bytes()
}

fn sha256_hex_of_bytes(data: &[u8]) -> Option<String> {
    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-r"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(data).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().map(|s| s.to_string())
}

/// The `generated:` unix time from index.meta's header stanza.
fn index_generated(text: &str) -> Option<u64> {
    let head = text.split("\n\n").next()?;
    head.lines()
        .find_map(|l| l.strip_prefix("generated:"))
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn parse_index_pins(text: &str) -> (HashMap<(String, String), String>, HashMap<String, String>) {
    let mut pins = HashMap::new();
    let mut latest: HashMap<String, String> = HashMap::new();
    for stanza in text.split("\n\n") {
        let mut f: HashMap<&str, &str> = HashMap::new();
        for line in stanza.lines() {
            if let Some((k, v)) = line.split_once(':') {
                f.insert(k.trim(), v.trim());
            }
        }
        let (Some(name), Some(version)) = (f.get("name"), f.get("version")) else { continue };
        if let Some(sha) = f.get("sha256") {
            pins.insert((name.to_string(), version.to_string()), sha.to_string());
        }
        let newer = match latest.get(*name) {
            Some(cur) => resolver::compare_versions(version, cur) == std::cmp::Ordering::Greater,
            None => true,
        };
        if newer {
            latest.insert(name.to_string(), version.to_string());
        }
    }
    (pins, latest)
}

fn state_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env::var("SPK_STATE_DIR").unwrap_or_else(|_| "/var/lib/spk".to_string()))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Verify a signed index. Pure apart from reading/writing the rollback marker
/// under `state_dir`, so it is testable without a network.
fn check_signed_index(
    repo: &Repo,
    pem: &str,
    meta: &[u8],
    sig: &str,
    now: u64,
    state_dir: &Path,
) -> Result<VerifiedIndex, IndexError> {
    let untrusted = |m: String| IndexError::Untrusted(m);
    let Some(hash) = sha256_hex_of_bytes(meta) else {
        return Err(untrusted("cannot hash index.meta (is openssl installed?)".into()));
    };
    if !verify_signature(pem, &index_signing_message("index.meta", &hash), sig.trim()) {
        return Err(untrusted("index.meta signature is invalid (wrong key, or the file was altered)".into()));
    }
    let text = String::from_utf8(meta.to_vec()).map_err(|_| untrusted("index.meta is not UTF-8".into()))?;
    let Some(generated) = index_generated(&text) else {
        return Err(untrusted("signed index.meta has no `generated` timestamp".into()));
    };
    if generated > now + 86_400 {
        return Err(untrusted("index.meta claims to be generated in the future (clock wrong, or forged)".into()));
    }
    let max_age = repo.max_index_age_days * 86_400;
    if now.saturating_sub(generated) > max_age {
        return Err(untrusted(format!(
            "index.meta is stale: generated {} day(s) ago, limit {} (the archive stopped re-signing, or a mirror serves old data)",
            now.saturating_sub(generated) / 86_400,
            repo.max_index_age_days
        )));
    }
    let marker_dir = state_dir.join("index-generated");
    let safe: String = repo.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let marker = marker_dir.join(safe);
    if let Some(seen) = fs::read_to_string(&marker).ok().and_then(|t| t.trim().parse::<u64>().ok()) {
        if generated < seen {
            return Err(untrusted(format!(
                "index.meta (generated {generated}) is older than one already seen ({seen}): possible rollback"
            )));
        }
    }
    if fs::create_dir_all(&marker_dir).is_ok() {
        let _ = fs::write(&marker, generated.to_string());
    }
    let (pins, latest) = parse_index_pins(&text);
    Ok(VerifiedIndex { meta: text, pins, latest, generated: Some(generated), signed: true })
}

fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = format!(
        "/tmp/spk-fetch-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let ok = Command::new("curl")
        .args(["-sfL", "--max-time", "120", "-o", &tmp, url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let data = if ok { fs::read(&tmp).map_err(|e| e.to_string()) } else { Err(format!("cannot fetch {url}")) };
    let _ = fs::remove_file(&tmp);
    data
}

thread_local! {
    static INDEX_CACHE: std::cell::RefCell<HashMap<String, Result<std::rc::Rc<VerifiedIndex>, IndexError>>> =
        std::cell::RefCell::new(HashMap::new());
    static WARNED: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// name -> (repo, version) the solver chose for this run: installs fetch
    /// exactly that package from exactly that repo.
    static PLANNED: std::cell::RefCell<HashMap<String, (String, String)>> =
        std::cell::RefCell::new(HashMap::new());
    /// Set when the last fetch was refused for a trust reason (bad signature,
    /// hash mismatch, broken key). Callers must then stop, not try another
    /// format or repo.
    static TRUST_REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn refuse_trust() -> bool {
    TRUST_REFUSED.with(|f| f.set(true));
    false
}

/// The (verified, if the repo has a key) index of `repo`, fetched at most once per run.
fn verified_index(repo: &Repo) -> Result<std::rc::Rc<VerifiedIndex>, IndexError> {
    let key = format!("{}|{}", repo.name, repo.url);
    if let Some(hit) = INDEX_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let result = (|| {
        let base = repo.url.trim_end_matches('/');
        let trust = match trust_of(repo) {
            Ok(t) => t,
            Err(TrustError::Broken(m)) => return Err(IndexError::Untrusted(m)),
            Err(TrustError::Disallowed(m)) => return Err(IndexError::Untrusted(m)),
        };
        let meta = fetch_bytes(&format!("{base}/index.meta")).map_err(IndexError::Unreachable)?;
        match trust {
            Trust::Signed(pem) => {
                let sig = fetch_bytes(&format!("{base}/index.meta.sig"))
                    .map_err(|_| IndexError::Untrusted("this repo is configured with a key but serves no index.meta.sig".into()))?;
                let sig = String::from_utf8_lossy(&sig).to_string();
                check_signed_index(repo, pem, &meta, &sig, now_unix(), &state_dir()).map(std::rc::Rc::new)
            }
            Trust::Unsigned => {
                if !is_file_url(&repo.url) {
                    WARNED.with(|w| {
                        if w.borrow_mut().insert(repo.name.clone()) {
                            println!(
                                "{YELLOW}[spk] WARNING: repo '{}' is not signed; its contents are not verified. \
                                 Add `key:` to its entry in {REPO_CONF_PATH}.{R}",
                                repo.name
                            );
                        }
                    });
                }
                let text = String::from_utf8_lossy(&meta).to_string();
                let (_pins, latest) = parse_index_pins(&text);
                Ok(std::rc::Rc::new(VerifiedIndex {
                    generated: index_generated(&text),
                    meta: text,
                    pins: HashMap::new(),
                    latest,
                    signed: false,
                }))
            }
        }
    })();
    INDEX_CACHE.with(|c| c.borrow_mut().insert(key, result.clone()));
    result
}

/// Version recorded in an .spkg's control file.
fn spkg_control_version(path: &str) -> Option<String> {
    let outer = Command::new("tar").args(["-xOf", path, "control.tar.xz"]).output().ok()?;
    if !outer.status.success() {
        return None;
    }
    let mut child = Command::new("tar")
        .args(["-xJOf", "-", "--wildcards", "*control"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(&outer.stdout).ok()?;
    let out = child.wait_with_output().ok()?;
    let meta = parse_control(&String::from_utf8_lossy(&out.stdout));
    if meta.version.is_empty() { None } else { Some(meta.version) }
}

fn curl_to(url: &str, dest: &str) -> bool {
    Command::new("curl")
        .args(["-sfL", "-o", dest, url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn fetch_package_ext(pkg: &str, ext: &str, dest_tmp: &str) -> bool {
    TRUST_REFUSED.with(|f| f.set(false));
    let planned = PLANNED.with(|p| p.borrow().get(pkg).cloned());
    let repos = load_repos();
    let (trusted, untrusted): (Vec<_>, Vec<_>) = repos.iter().partition(|r| r.priority < 100);

    for repo in trusted.iter().chain(untrusted.iter()) {
        if let Some((planned_repo, _)) = &planned {
            if &repo.name != planned_repo {
                continue;
            }
        }
        if repo.priority >= 100 {
            println!(
                "{RED}{BOLD}[spk] WARNING: falling back to untrusted repo '{}' ({}) -- \
                 every configured trusted repo failed.{R}",
                repo.name, repo.url
            );
        }
        let base = repo.url.trim_end_matches('/');
        let trust = match trust_of(repo) {
            Ok(t) => t,
            Err(TrustError::Disallowed(m)) => {
                println!("{YELLOW}[spk] Skipping: {m}{R}");
                continue;
            }
            Err(TrustError::Broken(m)) => {
                println!("{RED}{BOLD}[spk] Refusing to continue: repo '{}' is misconfigured: {m}{R}", repo.name);
                return refuse_trust();
            }
        };

        if let Trust::Signed(_) = trust {
            if ext != "spkg" {
                println!(
                    "{YELLOW}[spk] Legacy .{ext} bundles carry no hash pin and are not accepted from \
                     signed repo '{}'.{R}",
                    repo.name
                );
                continue;
            }
            let vi = match verified_index(repo) {
                Ok(v) => v,
                Err(IndexError::Unreachable(_)) => continue,
                Err(IndexError::Untrusted(m)) => {
                    println!("{RED}{BOLD}[spk] Refusing {pkg}: repo '{}' failed verification: {m}{R}", repo.name);
                    return refuse_trust();
                }
            };
            let want = match &planned {
                Some((_, v)) => v.clone(),
                None => match vi.latest.get(pkg) {
                    Some(v) => v.clone(),
                    None => continue, // not in this repo; the next one may have it
                },
            };
            let Some(pin) = vi.pins.get(&(pkg.to_string(), want.clone())) else {
                if planned.is_some() {
                    println!(
                        "{RED}{BOLD}[spk] Refusing {pkg}: the plan needs {want} but the signed index of '{}' \
                         does not list it.{R}",
                        repo.name
                    );
                    return refuse_trust();
                }
                continue;
            };
            let url = if vi.latest.get(pkg) == Some(&want) {
                format!("{base}/{pkg}.spkg")
            } else {
                format!("{base}/pool/{pkg}_{want}.spkg")
            };
            if !curl_to(&url, dest_tmp) {
                let _ = fs::remove_file(dest_tmp);
                println!(
                    "{RED}{BOLD}[spk] Refusing {pkg}: the signed index of '{}' lists {want} but {url} could not \
                     be fetched. Not falling back to another repo.{R}",
                    repo.name
                );
                return refuse_trust();
            }
            if sha256_of_file(dest_tmp).as_deref() != Some(pin.as_str()) {
                let _ = fs::remove_file(dest_tmp);
                println!(
                    "{RED}{BOLD}[spk] Refusing {pkg} {want}: the downloaded file does not match the sha256 \
                     pinned in the signed index of '{}' (tampered or corrupted).{R}",
                    repo.name
                );
                return refuse_trust();
            }
            println!("{GREEN}[spk] {pkg} {want} verified against the signed index of '{}'.{R}", repo.name);
            return true;
        }

        // Unsigned repo: the old behavior, plus a version check when the solver chose one.
        if !is_file_url(&repo.url) {
            WARNED.with(|w| {
                if w.borrow_mut().insert(repo.name.clone()) {
                    println!(
                        "{YELLOW}[spk] WARNING: repo '{}' is not signed; its contents are not verified.{R}",
                        repo.name
                    );
                }
            });
        }
        let url = format!("{base}/{pkg}.{ext}");
        if curl_to(&url, dest_tmp) {
            if let (Some((_, want)), "spkg") = (&planned, ext) {
                if let Some(got) = spkg_control_version(dest_tmp) {
                    if &got != want {
                        println!(
                            "{RED}[spk] '{}' serves {pkg} {got} but the plan needs {want}; not installing it.{R}",
                            repo.name
                        );
                        let _ = fs::remove_file(dest_tmp);
                        continue;
                    }
                }
            }
            return true;
        }
        let _ = fs::remove_file(dest_tmp);
    }
    false
}

// Packages currently published in the release above. Used by system-upgrade to
// know what to re-fetch; install can fetch any package name (unknown ones just
// produce an HTTP 404 from curl).
//
// Packages are served from a GitHub Release rather than a REST file-content
// API. GitHub Releases serves raw files with no size limit; the GerritHub REST
// API was silently truncating large .tar.xz files.
const SMECHOS_PACKAGES: &[&str] = &[
    "base-system",
    "kernel-modules",
    "firmware",
    "bootloader-grub",
    "kde-frameworks",
    "plasma",
    "qt6",
    "mesa-graphics",
    "plasma-discover",
    "packagekit-spk",
];

// ── Banner / help ─────────────────────────────────────────────────────────────

fn print_banner() {
    println!("{BOLD}{MAGENTA}========================================================================{R}");
    println!("{BOLD}{RED}     ███████╗██████╗ ██╗  ██╗    ███████╗ ██████╗ ██╗  ██╗{R}");
    println!("{BOLD}{RED}     ██╔════╝██╔══██╗██║ ██╔╝    ██╔════╝██╔═══██╗██║ ██╔╝{R}");
    println!("{BOLD}{RED}     ███████╗██████╔╝█████╔╝     ███████╗██║   ██║█████╔╝ {R}");
    println!("{BOLD}{RED}     ╚════██║██╔═══╝ ██╔═██╗     ╚════██║██║   ██║██╔═██╗ {R}");
    println!("{BOLD}{RED}     ███████║██║     ██║  ██╗    ███████║╚██████╔╝██║  ██╗{R}");
    println!("{BOLD}{CYAN}          SMECH SOVEREIGN PACKAGE KEEPER  v{VERSION}{R}");
    println!("{BOLD}{MAGENTA}========================================================================{R}");
}

fn print_help() {
    print_banner();
    println!("{BOLD}USAGE:{R}  spk <COMMAND> [args...]");
    println!();
    println!("{BOLD}PACKAGE MANAGEMENT{R}");
    println!("    {GREEN}install <pkg>...{R}           Fetch and install packages from a repo");
    println!("                                     (--dry-run shows the plan only; packages pulled");
    println!("                                     in as dependencies are marked [auto])");
    println!("    {GREEN}install --local-package <path>{R}  Install a local .spkg file (the only");
    println!("                                     way to install from disk -- a bare path");
    println!("                                     to 'install' is rejected on purpose)");
    println!("    {GREEN}local-package-repo <folder>{R}  Point 'install <name>' at a local folder");
    println!("                                     of .spkg files too (priority 1)");
    println!("    {GREEN}remove <pkg> [-y] [--force]{R}  Uninstall a package (checks reverse deps");
    println!("                                     and never deletes a file another");
    println!("                                     installed package also owns)");
    println!("    {GREEN}list{R}                       Show installed packages (--upgradable, --available,");
    println!("                                     --auto, --manual, --held)");
    println!("    {GREEN}update{R}                     Check every repo's index and count upgrades");
    println!("    {GREEN}upgrade [pkg...] [-y] [-n]{R} Upgrade packages that have a newer version");
    println!("                                     (-n/--dry-run lists them; held ones are skipped)");
    println!("    {GREEN}reinstall <pkg>...{R}         Fetch and install the same package again");
    println!("    {GREEN}autoremove [-y] [-n]{R}       Remove dependencies nothing needs any more");
    println!("    {GREEN}search <term>...{R}           Search installed and published packages");
    println!("    {GREEN}info <pkg>{R}                 Versions, status, dependencies, reverse deps");
    println!("    {GREEN}files <pkg>{R}                List the files an installed package owns");
    println!("    {GREEN}owns <path>{R}                Which installed package owns a file");
    println!("    {GREEN}provides <name>{R}            Which packages provide a (virtual) name");
    println!("    {GREEN}mark-manual|mark-auto <pkg>{R}  Change whether autoremove may remove it");
    println!("    {GREEN}hold|unhold <pkg>{R}          Keep a package at its version during upgrade");
    println!("    {GREEN}verify [pkg...] [-v]{R}       Check that every recorded file is on disk");
    println!("    {GREEN}check{R}                      Look for unmet dependencies and conflicts");
    println!("    {GREEN}history{R}                    Show the install/remove/upgrade log");
    println!("    {GREEN}download <pkg>...{R}          Save .spkg files without installing");
    println!("    {GREEN}clean{R}                      Delete leftover partial downloads");
    println!("    {GREEN}repo list|add|remove{R}       Show or change the configured repos");
    println!("    {GREEN}depends <pkg>{R}              Show a package's dependency tree");
    println!("    {GREEN}repo-status{R}                Show each repo's signing state and whether its index verifies");
    println!("    {GREEN}solve <pkg>{R}                Show what installing a package would do");
    println!("                                     (versions, alternatives, conflicts) without");
    println!("                                     installing; needs the repo's index.meta");
    println!("    {GREEN}create-live-image <out.iso>{R}  Build a bootable image straight from a");
    println!("                                     repo's published packages (needs a repo");
    println!("                                     with index.txt -- see phase_bundle_spkg_packages)");
    println!("    {GREEN}system-upgrade{R}             Re-fetch and reinstall all known packages");
    println!();
    println!("{BOLD}PACKAGE BUILD (pure Rust, no spk-compile.py involved){R}");
    println!("    {GREEN}compile <recipe> [-o out.spkg]{R}  Build ONE package's source into a");
    println!("                                     .spkg (recipe = control-style header +");
    println!("                                     #--build--/#--install-- script blocks)");
    println!();
    println!("{BOLD}IMAGE BUILD (native orchestration via spk-compile.py){R}");
    println!("    {GREEN}build-image smechos{R}        Full SmechOS build");
    println!("    {GREEN}build-image smechos  --phase <p>{R}  Single SmechOS phase");
    println!("    {GREEN}build-image iso smechos{R}    SmechOS install ISO");
    println!("    {GREEN}build-image --list smechos{R} List SmechOS build phases");
    println!();
    println!("{BOLD}PACKAGEKIT BACKEND (called by PackageKit daemon -- not for manual use){R}");
    println!("    {GREEN}packagekit-backend{R}         Speak PackageKit script protocol on stdin/stdout");
    println!();
    println!("{BOLD}OTHER{R}");
    println!("    {GREEN}version{R}                    Print version");
    println!("    {GREEN}about{R}                      Workstation specs + credits");
    println!("    {GREEN}help{R}                       This help");
    println!();
    println!("  Docs: {CYAN}{DOCS_URL}{R}");
    println!();
}

fn print_about() {
    print_banner();
    println!("{BOLD}SPK -- Smech Package Keeper{R}");
    println!("  Version:    {VERSION}");
    println!("  License:    MIT");
    println!("  Homepage:   {CYAN}{DOCS_URL}/smechos.html#spk{R}");
    println!("  Source:     {CYAN}https://github.com/Smech-Labs/spk{R}");
    println!();
    println!("{BOLD}Developed by{R}");
    println!("  Smech Labs -- https://labs.smech.xyz");
    println!("  Lead:  Smech");
    println!("  Co-dev: Gemini (Google DeepMind)");
    println!("  Co-dev: Claude (Anthropic)");
    println!("  First release: 2026");
    println!();
    println!("{BOLD}About{R}");
    println!("  Unified sovereign package keeper for SmechOS.");
    println!("  Minimal dependencies: real YAML parsing (yaml-rust2), everything else is std.");
    println!("  PackageKit backend included: works with Plasma Discover and");
    println!("  any other PackageKit-aware frontend out of the box.");
    println!("  Build orchestration via spk-compile (Project SmechDeployV2).");
    println!();
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn target_root() -> &'static str {
    // SPK_ROOT overrides the target (build sandboxes, tests): an empty root
    // has an empty installed-package database, so nothing is "already there".
    static ROOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        if let Ok(r) = env::var("SPK_ROOT") {
            return r;
        }
        if Path::new("/mnt/smechos").exists() {
            "/mnt/smechos".to_string()
        } else {
            "/".to_string()
        }
    })
    .as_str()
}

// ── .spkg format ──────────────────────────────────────────────────────────────
//
// A .spkg is a plain (uncompressed) outer tar -- no point double-compressing
// what's already xz'd inside -- containing exactly two members, deb-style:
//
//   control.tar.xz   metadata (a `control` key:value file) + optional
//                     preinst/postinst/prerm/postrm executable scripts
//   data.tar.xz       payload, paths relative to the install root
//
// Kept implementable with zero new crates: the outer container and both
// inner members are handled with the same `tar` shell-out already used for
// legacy .tar.xz packages, and `control` is hand-parsed with the same
// "key: value, # comments stripped" scanner style already used for
// spk-repo-conf.yaml (see parse_repo_conf above).

struct ControlMeta {
    name: String,
    version: String,
    architecture: String,
    depends: String,
    description: String,
    provides: String,
    conflicts: String,
}

fn parse_control(text: &str) -> ControlMeta {
    let mut m = ControlMeta {
        name: String::new(),
        version: String::new(),
        architecture: String::new(),
        depends: String::new(),
        description: String::new(),
        provides: String::new(),
        conflicts: String::new(),
    };
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("");
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim().to_string();
            match k.trim() {
                "name" => m.name = v,
                "version" => m.version = v,
                "architecture" => m.architecture = v,
                "depends" => m.depends = v,
                "provides" => m.provides = v,
                "conflicts" => m.conflicts = v,
                "description" => m.description = v,
                _ => {}
            }
        }
    }
    m
}

fn run_hook_script(control_dir: &str, name: &str) {
    let path = format!("{control_dir}/{name}");
    if !Path::new(&path).exists() {
        return;
    }
    let _ = Command::new("chmod").args(["+x", &path]).status();
    match Command::new("sh").arg(&path).status() {
        Ok(s) if s.success() => {}
        _ => println!("{YELLOW}[spk] Warning: {name} script did not exit cleanly.{R}"),
    }
}

/// The exact bytes a converter tool signs -- must stay byte-for-byte
/// identical to nix2spk.py's/apt2spk.py's own copy of this format, since
/// the signature only verifies if both sides built the same message.
fn signing_message_for(name: &str, version: &str, arch: &str, data_sha256_hex: &str) -> Vec<u8> {
    format!(
        "spk-sig-v1\nname={name}\nversion={version}\narchitecture={arch}\nsha256={data_sha256_hex}\n"
    )
    .into_bytes()
}

/// sha256 of a file's raw bytes, as a lowercase hex string. Shells out to
/// `openssl dgst` rather than adding a sha2 crate, for the same
/// zero-new-dependency reason signing itself shells out to openssl.
fn sha256_of_file(path: &str) -> Option<String> {
    let out = Command::new("openssl")
        .args(["dgst", "-sha256", "-r", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // `openssl dgst -r` prints "<hex>  <path>" (or "<hex> *<path>").
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
}

/// Verify a base64-encoded raw Ed25519 signature over `message` against a
/// PEM-encoded public key, by shelling out to `openssl pkeyutl -verify
/// -rawin` (symmetric with how nix2spk.py/apt2spk.py sign: raw Ed25519
/// needs no separate digest algorithm the way RSA/ECDSA do). All three
/// inputs go through temp files since `openssl pkeyutl` takes files, not
/// stdin, for -in/-sigfile/-pubin here.
fn verify_signature(pubkey_pem: &str, message: &[u8], sig_b64: &str) -> bool {
    let Ok(sig_bytes) = base64_decode(sig_b64) else { return false };
    let stage_id = std::process::id();
    let dir = format!("/tmp/spk-verify-{stage_id}-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    if fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let pub_path = format!("{dir}/pub.pem");
    let msg_path = format!("{dir}/message");
    let sig_path = format!("{dir}/sig.bin");
    let ok = fs::write(&pub_path, pubkey_pem).is_ok()
        && fs::write(&msg_path, message).is_ok()
        && fs::write(&sig_path, &sig_bytes).is_ok();
    let verified = ok
        && Command::new("openssl")
            .args([
                "pkeyutl", "-verify", "-pubin", "-inkey", &pub_path,
                "-rawin", "-in", &msg_path, "-sigfile", &sig_path,
            ])
            // openssl chatters on stdout/stderr ("Signature Verified Successfully");
            // that must never leak into machine-readable output such as `solve --porcelain`.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let _ = fs::remove_dir_all(&dir);
    verified
}

/// Minimal standard-alphabet base64 decoder -- just enough to turn the
/// signature file's base64 text back into raw bytes, without adding a
/// base64 crate for one call site.
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let s = s.trim();
    let mut table = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in s.bytes() {
        if b == b'=' {
            break;
        }
        let v = table[b as usize];
        if v == 255 {
            continue; // skip whitespace/newlines
        }
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// Install a .spkg file already sitting on local disk into `root`. Used by
/// both the repo-fetch path (fetch_and_install downloads to a temp file,
/// then calls this) and `spk install --local-package` (calls this
/// directly on the caller-given path, no fetch involved).
fn install_spkg_file(spkg_path: &str, root: &str) -> bool {
    install_spkg_file_inner(spkg_path, root, &mut Vec::new())
}

fn install_spkg_file_inner(spkg_path: &str, root: &str, resolving: &mut Vec<String>) -> bool {
    // Keyed by more than just the process ID: dependency resolution calls
    // this function recursively WITHIN the same process (installing a
    // package's dependency mid-install of that package), and a
    // process-ID-only path collided between the outer and inner call --
    // confirmed the hard way: the inner call's cleanup deleted the outer
    // call's still-in-use staging dir out from under it. A monotonic
    // per-call counter guarantees every nested call gets its own directory
    // regardless of how deep the recursion goes.
    static STAGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stage_id = STAGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stage = format!("/tmp/spk-spkg-stage-{}-{stage_id}", std::process::id());
    let _ = fs::remove_dir_all(&stage);
    if fs::create_dir_all(&stage).is_err() {
        println!("{RED}[spk] Failed to create staging dir {stage}.{R}");
        return false;
    }
    let outer_ok = Command::new("tar")
        .args(["-xf", spkg_path, "-C", &stage])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !outer_ok {
        println!("{RED}[spk] Failed to open {spkg_path} (not a valid .spkg?).{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    let control_tar = format!("{stage}/control.tar.xz");
    let data_tar = format!("{stage}/data.tar.xz");
    if !Path::new(&control_tar).exists() || !Path::new(&data_tar).exists() {
        println!("{RED}[spk] {spkg_path} is missing control.tar.xz or data.tar.xz -- not a valid .spkg.{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    let control_dir = format!("{stage}/control");
    let _ = fs::create_dir_all(&control_dir);
    let control_ok = Command::new("tar")
        .args(["-xf", &control_tar, "-C", &control_dir])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !control_ok {
        println!("{RED}[spk] Failed to extract control.tar.xz from {spkg_path}.{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    let control_text =
        fs::read_to_string(format!("{control_dir}/control")).unwrap_or_default();
    let meta = parse_control(&control_text);
    if meta.name.is_empty() {
        println!("{RED}[spk] {spkg_path}'s control file has no 'name' field -- refusing to install.{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    println!("{BOLD}[spk] {} {} ({}){R}", meta.name, meta.version, meta.architecture);
    if !meta.description.is_empty() {
        println!("  {}", meta.description);
    }

    if is_apt_package(&meta.name) {
        println!(
            "{RED}[spk] Refusing to install {}: '~apt~' packages are apt2spk-converted \
             .deb payloads meant for spk-d (SPK on Debian) -- they assume a real Debian \
             host's shared libraries are already present, which SmechOS doesn't provide. \
             Install this with spk-d instead.{R}",
            meta.name
        );
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    // Signature gate for ~nix~ packages: the suffix alone is just text in a
    // hand-editable control file (demonstrated directly -- a payload with
    // zero /nix/store content, hand-labeled ~nix~, installed without
    // complaint before this check existed). Every real nix2spk package
    // carries a `signature` file alongside `control`; verify it against
    // nix2spk's embedded public key before trusting this package at all,
    // and refuse outright on anything missing or invalid. This runs before
    // dependency resolution and the conflict guard -- an unverified package
    // shouldn't get far enough to recurse into installing its (claimed)
    // dependencies or touch the installed-files database at all.
    if is_nix_package(&meta.name) {
        let sig_path = format!("{control_dir}/signature");
        let Ok(sig_b64) = fs::read_to_string(&sig_path) else {
            println!(
                "{RED}[spk] Refusing to install {}: no 'signature' file -- every ~nix~ \
                 package must be signed by nix2spk. This package may be a forged/retagged \
                 payload, not a real nix2spk output.{R}",
                meta.name
            );
            let _ = fs::remove_dir_all(&stage);
            return false;
        };
        let Some(data_sha256) = sha256_of_file(&data_tar) else {
            println!("{RED}[spk] Refusing to install {}: failed to hash data.tar.xz.{R}", meta.name);
            let _ = fs::remove_dir_all(&stage);
            return false;
        };
        let message = signing_message_for(&meta.name, &meta.version, &meta.architecture, &data_sha256);
        if !verify_signature(NIX2SPK_PUBKEY, &message, &sig_b64) {
            println!(
                "{RED}[spk] Refusing to install {}: signature verification failed -- this \
                 package's control fields or payload don't match what nix2spk actually signed. \
                 Refusing rather than risk installing a tampered or forged package.{R}",
                meta.name
            );
            let _ = fs::remove_dir_all(&stage);
            return false;
        }
        println!("{GREEN}[spk] Signature verified (nix2spk).{R}");
    }

    if !meta.depends.is_empty() {
        println!("  {YELLOW}depends:{R} {}", meta.depends);
        if !resolve_dependencies(&meta.depends, root, resolving) {
            let _ = fs::remove_dir_all(&stage);
            return false;
        }
    }

    if is_nix_package(&meta.name) {
        println!(
            "{CYAN}[spk] {} is a Nix-origin package (vendored via nix2spk) -- \
             its payload bundles its own /nix/store closure; it is not a native spk build.{R}",
            meta.name
        );
    }

    // Conflict guard: a file this package would extract must not already
    // be owned by some OTHER installed package, unless it's a /nix/store
    // path -- multiple ~nix~ packages sharing closure members there is
    // expected and fine. This is the concrete enforcement behind "~nix~
    // packages are treated differently": it runs before any extraction
    // happens, so a clash is refused cleanly instead of one package's
    // files silently clobbering another's.
    let incoming_files = tar_member_files(&data_tar);
    for other_name in list_installed_names(root) {
        if other_name == meta.name {
            continue;
        }
        let Some(other_pkg) = read_installed(root, &other_name) else { continue };
        for f in &incoming_files {
            if f.starts_with("nix/store/") {
                continue;
            }
            if other_pkg.files.iter().any(|of| of == f) {
                println!(
                    "{RED}[spk] Refusing to install {}: file '{}' is already owned by installed package '{}'.{R}",
                    meta.name, f, other_pkg.name
                );
                let _ = fs::remove_dir_all(&stage);
                return false;
            }
        }
    }

    if is_installed(root, &meta.name) {
        println!("{YELLOW}[spk] {} is already installed -- reinstalling/upgrading over it.{R}", meta.name);
    }

    run_hook_script(&control_dir, "preinst");

    let _ = fs::create_dir_all(root);
    println!("{CYAN}[spk] Extracting {} into {root}...{R}", meta.name);
    let extract = format!("tar -xf '{data_tar}' -C '{root}'");
    let result = if is_root() {
        Command::new("sh").arg("-c").arg(&extract).status()
    } else {
        Command::new("sudo")
            .args(["-S", "sh", "-c", &extract])
            .stdin(Stdio::inherit())
            .status()
    };

    let ok = matches!(result, Ok(s) if s.success());
    if ok {
        run_hook_script(&control_dir, "postinst");
        // The archive's own member list is the precise, authoritative
        // record of what this package just put on disk -- read it back
        // from data.tar.xz rather than re-deriving it any other way, so
        // `remove` later knows exactly (and only) what to delete. Same
        // normalization the pre-extraction conflict guard above used, via
        // the shared helper, so both agree on what this package owns.
        let files = tar_member_files(&data_tar);
        if !write_installed_record(root, &meta, &files) {
            println!(
                "{YELLOW}[spk] Warning: {} installed, but failed to write its record to \
                 {INSTALLED_DB_DIR} -- 'spk remove {}' won't know about it later.{R}",
                meta.name, meta.name
            );
        }
        // preinst/postinst already ran; persist prerm/postrm (if any) next
        // to the installed record so `remove` can run them too, since the
        // staging dir they live in is about to be deleted.
        for hook in ["prerm", "postrm"] {
            let src = format!("{control_dir}/{hook}");
            if Path::new(&src).exists() {
                if let Ok(contents) = fs::read_to_string(&src) {
                    write_root_file(&format!("{}.{hook}", installed_db_path(root, &meta.name)), &contents);
                }
            }
        }
        println!("{GREEN}[spk] {} installed.{R}", meta.name);
    } else {
        println!("{RED}[spk] Failed to extract data.tar.xz from {spkg_path}.{R}");
    }
    let _ = fs::remove_dir_all(&stage);
    ok
}

/// Write `content` to `path`, which may require root (installed-db records
/// live under /var/lib, repo-package-repo config under /etc). Shared by
/// every "write as root" call site instead of duplicating the sudo-tee
/// dance at each one.
fn write_root_file(path: &str, content: &str) -> bool {
    if is_root() {
        return fs::write(path, content).is_ok();
    }
    match Command::new("sudo")
        .args(["-S", "tee", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(content.as_bytes());
            }
            matches!(child.wait(), Ok(s) if s.success())
        }
        Err(_) => false,
    }
}

fn remove_root_file(path: &str) {
    if is_root() {
        let _ = fs::remove_file(path);
    } else {
        let _ = Command::new("sudo").args(["-S", "rm", "-f", path]).status();
    }
}

fn ensure_root_dir(path: &str) -> bool {
    if Path::new(path).is_dir() {
        return true;
    }
    if is_root() {
        fs::create_dir_all(path).is_ok()
    } else {
        matches!(
            Command::new("sudo").args(["-S", "mkdir", "-p", path]).status(),
            Ok(s) if s.success()
        )
    }
}

// ── Installed-package database ────────────────────────────────────────────────
//
// One text file per installed package at INSTALLED_DB_DIR/<name>:
//
//   version: 1.2.3
//   architecture: x86_64
//   depends: foo, bar (>= 2.0)
//   files:
//   usr/bin/thing
//   usr/lib/libthing.so
//
// This is the foundation everything else here needs: dependency
// resolution checks it to know what's already satisfied, `remove` reads
// it to know exactly which files it owns (and cross-references every
// OTHER package's file list before deleting anything, so a file two
// packages both ship is never removed out from under the other one).

struct InstalledPkg {
    name: String,
    version: String,
    depends: String,
    provides: String,
    conflicts: String,
    files: Vec<String>,
}

/// The installed-db directory lives under `root` itself, not always the
/// host's absolute /var/lib/spk/installed -- a scratch root being
/// assembled by `create-live-image` needs its OWN db (so the resulting
/// image boots with a real record of what's on it), and even an
/// ordinary `/mnt/smechos` install target should track installs there,
/// not silently in the host's own db. root="/" (the ordinary case)
/// collapses back to the plain absolute path.
fn installed_db_dir(root: &str) -> String {
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        INSTALLED_DB_DIR.to_string()
    } else {
        format!("{trimmed}{INSTALLED_DB_DIR}")
    }
}

fn installed_db_path(root: &str, name: &str) -> String {
    format!("{}/{name}", installed_db_dir(root))
}

fn is_installed(root: &str, name: &str) -> bool {
    Path::new(&installed_db_path(root, name)).exists()
}

fn list_installed_names(root: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(installed_db_dir(root)) {
        for e in entries.flatten() {
            if e.path().is_file() {
                if let Some(n) = e.file_name().to_str() {
                    if n.ends_with(".prerm") || n.ends_with(".postrm") {
                        continue;
                    }
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

fn read_installed(root: &str, name: &str) -> Option<InstalledPkg> {
    let text = fs::read_to_string(installed_db_path(root, name)).ok()?;
    let mut version = String::new();
    let mut depends = String::new();
    let mut provides = String::new();
    let mut conflicts = String::new();
    let mut files = Vec::new();
    let mut in_files = false;
    for line in text.lines() {
        if in_files {
            if !line.is_empty() {
                files.push(line.to_string());
            }
            continue;
        }
        if line.trim() == "files:" {
            in_files = true;
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "version" => version = v.trim().to_string(),
                "depends" => depends = v.trim().to_string(),
                "provides" => provides = v.trim().to_string(),
                "conflicts" => conflicts = v.trim().to_string(),
                _ => {}
            }
        }
    }
    Some(InstalledPkg {
        name: name.to_string(),
        version,
        depends,
        provides,
        conflicts,
        files,
    })
}

fn write_installed_record(root: &str, meta: &ControlMeta, files: &[String]) -> bool {
    if !ensure_root_dir(&installed_db_dir(root)) {
        return false;
    }
    let mut out = String::new();
    out.push_str(&format!("version: {}\n", meta.version));
    out.push_str(&format!("architecture: {}\n", meta.architecture));
    out.push_str(&format!("depends: {}\n", meta.depends));
    out.push_str(&format!("provides: {}\n", meta.provides));
    out.push_str(&format!("conflicts: {}\n", meta.conflicts));
    out.push_str("files:\n");
    for f in files {
        out.push_str(f);
        out.push('\n');
    }
    write_root_file(&installed_db_path(root, &meta.name), &out)
}

/// Parse a `depends:` field into plain package names, stripping any
/// version-constraint annotation in parentheses (e.g. "qt6-base (>= 6.10.0)"
/// -> "qt6-base"). The constraint itself isn't enforced yet -- only
/// presence/absence of the dependency by name -- see README for why that's
/// an acceptable v1 scope (spk has no version-comparison logic anywhere
/// else either).
fn parse_depends(depends: &str) -> Vec<String> {
    depends
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        // Fallback (no index.meta) can't choose between alternatives: take the first.
        .map(|s| s.split('|').next().unwrap_or(s))
        .map(|s| s.split('(').next().unwrap_or(s).trim().to_string())
        .collect()
}

/// Passed in `resolving` when the solver has already ordered the install, so
/// the per-package fallback resolver must not re-resolve (or reject cycles).
const SOLVED_MARKER: &str = "*solved*";

/// Recursively install whatever `depends` names aren't already installed,
/// before the package that declared them. `resolving` is the current
/// dependency chain (for cycle detection, not just "already handled" --
/// an already-installed dependency is skipped entirely, never added
/// here).
fn resolve_dependencies(depends: &str, root: &str, resolving: &mut Vec<String>) -> bool {
    if resolving.iter().any(|r| r == SOLVED_MARKER) {
        return true;
    }
    for dep in parse_depends(depends) {
        if is_installed(root, &dep) {
            continue;
        }
        if resolving.contains(&dep) {
            println!(
                "{RED}[spk] Dependency cycle detected involving '{dep}' -- refusing to continue.{R}"
            );
            return false;
        }
        println!("{CYAN}[spk] Resolving dependency: {dep}{R}");
        resolving.push(dep.clone());
        let ok = fetch_and_install_inner(&dep, root, resolving);
        resolving.pop();
        if !ok {
            println!("{RED}[spk] Failed to install dependency '{dep}'.{R}");
            return false;
        }
    }
    true
}

/// A bare `spk install` argument that looks like a filesystem path rather
/// than a repo package name -- used to reject that form outright instead
/// of silently auto-detecting it. See cmd_install's caller in main() for
/// the actual message; this only decides whether to trigger it.
fn looks_like_local_path(s: &str) -> bool {
    s.contains('/') || s.starts_with('.') || s.starts_with('~')
        || s.ends_with(".spkg") || s.ends_with(".tar.xz")
}

/// A package produced by nix2spk (a Nix binary from cache.nixos.org,
/// repackaged as a .spkg with its own /nix/store closure bundled as inert
/// payload data) -- never a native, from-source spk build. The `~nix~`
/// name suffix is the single source of truth `spk` uses to tell the two
/// apart; this is the one place that check lives.
fn is_nix_package(name: &str) -> bool {
    name.ends_with("~nix~")
}

/// A package produced by apt2spk (a Debian .deb repackaged as a .spkg) --
/// meant for spk-d (SPK on Debian), never for spk itself. Unlike `~nix~`,
/// which spk accepts and gives special handling, `~apt~` packages are
/// refused outright: a .deb's payload dynamically links against the
/// *host's own* shared libraries at ordinary FHS paths (no bundled
/// closure the way nix2spk ships one), which only makes sense on the
/// actual Debian system spk-d targets, not on SmechOS.
fn is_apt_package(name: &str) -> bool {
    name.ends_with("~apt~")
}

/// The normalized list of regular-file/symlink members a data.tar.xz will
/// produce on extraction -- same filtering/normalization
/// `install_spkg_file_inner` already needs twice (once to pre-check for
/// conflicts before extracting, once to write the installed-files record
/// after), pulled out so both call sites agree exactly on what counts as
/// "a file this package owns".
fn tar_member_files(data_tar: &str) -> Vec<String> {
    Command::new("tar")
        .args(["-tf", data_tar])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        // GNU tar lists directory members with a trailing '/' -- excluded
        // outright, not just stripped of the slash: directories are
        // near-universally shared across packages, and the conflict guard
        // (and remove's deletion logic) only cares about files it alone
        // would delete/claim.
        .filter(|l| !l.trim().is_empty() && *l != "." && !l.ends_with('/'))
        // A data.tar.xz built by archiving "." (as `spk compile` does)
        // lists members as "./usr/bin/foo"; one built from an explicit
        // file list has no such prefix. Normalize both to the same form.
        .map(|l| l.trim_start_matches("./").to_string())
        .collect()
}

// ── Package install ───────────────────────────────────────────────────────────

fn fetch_and_install(pkg: &str, root: &str) -> bool {
    fetch_and_install_inner(pkg, root, &mut Vec::new())
}

fn fetch_and_install_inner(pkg: &str, root: &str, resolving: &mut Vec<String>) -> bool {
    // Prefer the new .spkg format.
    let spkg_tmp = format!("/tmp/spk-{pkg}.spkg");
    println!("{CYAN}[spk] Fetching {pkg}...{R}");
    if fetch_package_ext(pkg, "spkg", &spkg_tmp) {
        let ok = install_spkg_file_inner(&spkg_tmp, root, resolving);
        let _ = fs::remove_file(&spkg_tmp);
        return ok;
    }
    let _ = fs::remove_file(&spkg_tmp);
    if TRUST_REFUSED.with(|f| f.get()) {
        // A signed repo was refused (bad signature, hash mismatch, ...). Trying the
        // legacy format or another repo would be a downgrade, so stop here.
        println!("{RED}[spk] Not trying any other format or repo for {pkg}.{R}");
        return false;
    }

    // Legacy bare .tar.xz fallback for repos that haven't migrated to
    // .spkg yet -- kept deliberately during the transition rather than a
    // hard cutover, so an operator's existing repo doesn't break outright.
    let tmp = format!("/tmp/spk-{pkg}.tar.xz");
    println!("{YELLOW}[spk] No {pkg}.spkg found -- trying legacy {pkg}.tar.xz...{R}");
    if !fetch_package_ext(pkg, "tar.xz", &tmp) {
        println!("{RED}[spk] Failed to download {pkg} -- package may not exist on any configured repo, or network is unreachable.{R}");
        let _ = fs::remove_file(&tmp);
        return false;
    }

    let _ = fs::create_dir_all(root);

    // Grab the member list before extracting -- same reasoning as the
    // .spkg path: it's the precise record of what's about to land on
    // disk. Legacy bundles carry no control/depends metadata at all, so
    // this record is minimal (no version, no depends), but it's still
    // enough for `spk remove` to know what to delete later.
    let legacy_files: Vec<String> = Command::new("tar")
        .args(["-tf", &tmp])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        // Directories excluded outright, not just slash-stripped -- see
        // the .spkg path's identical filter above for why.
        .filter(|l| !l.trim().is_empty() && *l != "." && !l.ends_with('/'))
        .map(|l| l.trim_start_matches("./").to_string())
        .collect();

    println!("{CYAN}[spk] Extracting {pkg} into {root}...{R}");
    let extract = format!("tar -xf '{tmp}' -C '{root}'");
    let result = if is_root() {
        Command::new("sh").arg("-c").arg(&extract).status()
    } else {
        Command::new("sudo")
            .args(["-S", "sh", "-c", &extract])
            .stdin(Stdio::inherit())
            .status()
    };
    let _ = fs::remove_file(&tmp);

    match result {
        Ok(s) if s.success() => {
            let legacy_meta = ControlMeta {
                name: pkg.to_string(),
                version: "unknown (legacy .tar.xz, no metadata)".to_string(),
                architecture: "x86_64".to_string(),
                depends: String::new(),
                description: String::new(),
                provides: String::new(),
                conflicts: String::new(),
            };
            write_installed_record(root, &legacy_meta, &legacy_files);
            println!("{GREEN}[spk] {pkg} installed.{R}");
            true
        }
        _ => {
            println!("{RED}[spk] Failed to extract {pkg}.{R}");
            false
        }
    }
}

/// libsolv, the dependency solver behind DNF, is spk's solver and a hard
/// dependency (libsolv.so at run time, libsolv-dev to build). It picks the
/// package set; the in-tree resolver's ordering then puts dependencies first.
/// Hand-written FFI on purpose: spk has no crate dependencies besides yaml-rust2.
mod libsolv_backend {
    use super::resolver::{Dep, Op, Pkg, SolveError, Solver, Universe};
    use std::collections::HashMap;
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_void};

    type Id = c_int;

    #[repr(C)]
    struct Queue {
        elements: *mut Id,
        count: c_int,
        alloc: *mut Id,
        left: c_int,
    }
    #[repr(C)]
    struct Pool {
        _opaque: [u8; 0],
    }
    #[repr(C)]
    struct Repo {
        _opaque: [u8; 0],
    }
    #[repr(C)]
    struct LsSolver {
        _opaque: [u8; 0],
    }

    const DISTTYPE_DEB: c_int = 1;
    const SOLVER_SOLVABLE_PROVIDES: Id = 0x03;
    const SOLVER_INSTALL: Id = 0x0100;
    const TESTCASE_RESULT_TRANSACTION: c_int = 1 << 0;
    const TESTCASE_RESULT_PROBLEMS: c_int = 1 << 1;

    #[link(name = "solv")]
    extern "C" {
        fn pool_create() -> *mut Pool;
        fn pool_free(pool: *mut Pool);
        fn pool_setdisttype(pool: *mut Pool, disttype: c_int) -> c_int;
        fn pool_set_installed(pool: *mut Pool, repo: *mut Repo);
        fn pool_createwhatprovides(pool: *mut Pool);
        fn repo_create(pool: *mut Pool, name: *const c_char) -> *mut Repo;
        fn solver_create(pool: *mut Pool) -> *mut LsSolver;
        fn solver_free(solv: *mut LsSolver);
        fn solver_solve(solv: *mut LsSolver, job: *mut Queue) -> c_int;
        fn queue_init(q: *mut Queue);
        fn queue_free(q: *mut Queue);
        fn queue_insert2(q: *mut Queue, pos: c_int, id1: Id, id2: Id);
    }
    #[link(name = "solvext")]
    extern "C" {
        fn testcase_add_testtags(repo: *mut Repo, fp: *mut c_void, flags: c_int) -> c_int;
        fn testcase_str2dep(pool: *mut Pool, s: *const c_char) -> Id;
        fn testcase_solverresult(solv: *mut LsSolver, flags: c_int) -> *mut c_char;
    }
    extern "C" {
        fn tmpfile() -> *mut c_void;
        fn fwrite(ptr: *const c_void, size: usize, n: usize, fp: *mut c_void) -> usize;
        fn rewind(fp: *mut c_void);
        fn fclose(fp: *mut c_void) -> c_int;
        fn free(p: *mut c_void);
    }

    fn dep_str(d: &Dep) -> String {
        match &d.constraint {
            None => d.name.clone(),
            Some((op, v)) => {
                let o = match op {
                    Op::Ge => ">=",
                    Op::Le => "<=",
                    Op::Gt => ">",
                    Op::Lt => "<",
                    Op::Eq => "=",
                };
                format!("{} {} {}", d.name, o, v)
            }
        }
    }

    fn alts_str(alts: &[Dep]) -> String {
        alts.iter().map(dep_str).collect::<Vec<_>>().join(" | ")
    }

    /// libsolv's text package format. It splits a version at its last '-'
    /// into version and release and joins them back, so "1.2-3" round-trips.
    fn tags(pkgs: &[Pkg]) -> String {
        let mut s = String::new();
        for p in pkgs {
            let (ver, rel) = match p.version.rsplit_once('-') {
                Some((v, r)) if !v.is_empty() && !r.is_empty() => (v, r),
                _ => (p.version.as_str(), "-"),
            };
            s += &format!("=Pkg: {} {} {} noarch\n", p.name, ver, rel);
            s += &format!("=Prv: {} = {}\n", p.name, p.version);
            for (n, v) in &p.provides {
                match v {
                    Some(v) => s += &format!("=Prv: {n} = {v}\n"),
                    None => s += &format!("=Prv: {n}\n"),
                }
            }
            for alts in &p.depends {
                if !alts.is_empty() {
                    s += &format!("=Req: {}\n", alts_str(alts));
                }
            }
            for c in &p.conflicts {
                s += &format!("=Con: {}\n", dep_str(c));
            }
        }
        s
    }

    unsafe fn add_repo(pool: *mut Pool, name: &str, pkgs: &[Pkg]) -> Result<*mut Repo, String> {
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        let repo = repo_create(pool, cname.as_ptr());
        let text = tags(pkgs);
        let fp = tmpfile();
        if fp.is_null() {
            return Err("libsolv: tmpfile() failed".to_string());
        }
        fwrite(text.as_ptr() as *const c_void, 1, text.len(), fp);
        rewind(fp);
        let rc = testcase_add_testtags(repo, fp, 0);
        fclose(fp);
        if rc != 0 {
            return Err(format!("libsolv: could not load the {name} package list"));
        }
        Ok(repo)
    }

    /// Resolve `requests` with libsolv; same contract as Solver::solve.
    pub fn solve(uni: &Universe, requests: &[Vec<Dep>]) -> Result<Vec<Pkg>, SolveError> {
        let by_id: HashMap<String, &Pkg> = uni
            .available
            .iter()
            .map(|p| (format!("{}-{}.noarch", p.name, p.version), p))
            .collect();
        let text = unsafe {
            let pool = pool_create();
            pool_setdisttype(pool, DISTTYPE_DEB);
            let installed = add_repo(pool, "@System", &uni.installed);
            let available = add_repo(pool, "available", &uni.available);
            let (installed, _available) = match (installed, available) {
                (Ok(i), Ok(a)) => (i, a),
                (Err(e), _) | (_, Err(e)) => {
                    pool_free(pool);
                    return Err(SolveError::Unsatisfiable(e));
                }
            };
            pool_set_installed(pool, installed);
            pool_createwhatprovides(pool);
            let mut job = Queue { elements: std::ptr::null_mut(), count: 0, alloc: std::ptr::null_mut(), left: 0 };
            queue_init(&mut job);
            for alts in requests {
                let c = CString::new(alts_str(alts)).unwrap_or_default();
                let id = testcase_str2dep(pool, c.as_ptr());
                queue_insert2(&mut job, job.count, SOLVER_INSTALL | SOLVER_SOLVABLE_PROVIDES, id);
            }
            let solv = solver_create(pool);
            let problems = solver_solve(solv, &mut job);
            let flags = if problems == 0 { TESTCASE_RESULT_TRANSACTION } else { TESTCASE_RESULT_PROBLEMS };
            let raw = testcase_solverresult(solv, flags);
            let text = if raw.is_null() { String::new() } else { CStr::from_ptr(raw).to_string_lossy().into_owned() };
            if !raw.is_null() {
                free(raw as *mut c_void);
            }
            queue_free(&mut job);
            solver_free(solv);
            pool_free(pool);
            (problems, text)
        };
        let (problems, text) = text;
        if problems != 0 {
            let first = requests.first().and_then(|a| a.first()).map(|d| d.name.clone()).unwrap_or_default();
            if let Some(i) = text.find("nothing provides ") {
                let rest = &text[i + "nothing provides ".len()..];
                let what = rest.split(" needed by").next().unwrap_or(rest).lines().next().unwrap_or("").trim();
                let what = what.strip_prefix("requested ").unwrap_or(what);
                if what == first || requests.iter().flatten().any(|d| d.name == what) {
                    return Err(SolveError::NotFound(what.to_string()));
                }
            }
            return Err(SolveError::Unsatisfiable(text.trim().to_string()));
        }
        let mut chosen: Vec<Pkg> = Vec::new();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            match (it.next(), it.next()) {
                (Some("install"), Some(id)) | (Some("upgrade"), Some(id)) => {
                    let key = id.rsplit_once('@').map(|(a, _)| a).unwrap_or(id);
                    if let Some(p) = by_id.get(key) {
                        chosen.push((*p).clone());
                    }
                }
                (Some("erase"), Some(id)) => {
                    return Err(SolveError::Unsatisfiable(format!("libsolv would remove {id}, which spk never does")));
                }
                _ => {}
            }
        }
        Ok(Solver::order_packages(&chosen))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn p(name: &str, version: &str, depends: &str) -> Pkg {
            Pkg::new(name, version, depends, "", "")
        }

        fn uni(available: Vec<Pkg>, installed: Vec<Pkg>) -> Universe {
            Universe { available, installed }
        }

        fn names(r: Result<Vec<Pkg>, SolveError>) -> Vec<String> {
            r.expect("solve failed").into_iter().map(|p| p.name).collect()
        }

        fn want(n: &str) -> Vec<Vec<Dep>> {
            vec![vec![super::super::resolver::parse_dep(n).unwrap()]]
        }

        #[test]
        fn installs_dependencies_first() {
            let u = uni(vec![p("app", "1.0-1", "lib (>= 1.0)"), p("lib", "1.2-3", "")], vec![]);
            assert_eq!(names(solve(&u, &want("app"))), vec!["lib", "app"]);
        }

        #[test]
        fn skips_installed_packages() {
            let u = uni(vec![p("app", "1.0-1", "lib"), p("lib", "1.0-1", "")], vec![p("lib", "1.0-1", "")]);
            assert_eq!(names(solve(&u, &want("app"))), vec!["app"]);
        }

        #[test]
        fn picks_the_newest_satisfying_version() {
            let u = uni(vec![p("lib", "1.0-1", ""), p("lib", "2.0-1", ""), p("app", "1-1", "lib (<< 2.0)")], vec![]);
            let r = solve(&u, &want("app")).unwrap();
            assert!(r.iter().any(|x| x.name == "lib" && x.version == "1.0-1"));
        }

        #[test]
        fn resolves_virtual_packages() {
            let v = Pkg::new("real", "1.0-1", "", "virt", "");
            let u = uni(vec![p("app", "1-1", "virt"), v], vec![]);
            assert_eq!(names(solve(&u, &want("app"))), vec!["real", "app"]);
        }

        #[test]
        fn missing_dependency_is_not_found() {
            let u = uni(vec![p("app", "1-1", "ghost")], vec![]);
            let r = solve(&u, &want("ghost"));
            assert!(matches!(r, Err(SolveError::NotFound(_))), "{:?}", r.map(|v| v.len()));
        }

        #[test]
        fn agrees_with_the_builtin_solver() {
            let u = uni(
                vec![p("a", "1-1", "b, c (>= 2)"), p("b", "1-1", "c"), p("c", "2.1-1", ""), p("c", "1.0-1", "")],
                vec![],
            );
            let mine = names(solve(&u, &want("a")));
            let theirs: Vec<String> = Solver::new(&u).solve(&want("a")).unwrap().into_iter().map(|p| p.name).collect();
            assert_eq!(mine, theirs);
        }
    }
}

/// Pick the solver. libsolv is the default. SPK_SOLVER=builtin uses the older
/// in-tree solver; SPK_SOLVER=compare runs both, reports differences on stderr
/// and uses libsolv's answer. If libsolv itself fails to start (it could not
/// load the package list, which says nothing about the dependencies), the
/// in-tree solver answers so installs keep working.
fn solve_with_backend(
    uni: &resolver::Universe,
    req: &[Vec<resolver::Dep>],
) -> Result<Vec<resolver::Pkg>, resolver::SolveError> {
    let builtin = || resolver::Solver::new(uni).solve(req);
    match std::env::var("SPK_SOLVER").as_deref() {
        Ok("builtin") => builtin(),
        Ok("compare") => {
            let b = libsolv_backend::solve(uni, req);
            let a = builtin();
            let names = |r: &Result<Vec<resolver::Pkg>, resolver::SolveError>| -> Option<Vec<String>> {
                r.as_ref().ok().map(|v| v.iter().map(|p| format!("{} {}", p.name, p.version)).collect())
            };
            if names(&a) != names(&b) {
                eprintln!("[spk] solver mismatch: built-in={:?} libsolv={:?}", names(&a), names(&b));
            }
            b
        }
        _ => match libsolv_backend::solve(uni, req) {
            Err(resolver::SolveError::Unsatisfiable(m)) if m.starts_with("libsolv:") => {
                eprintln!("[spk] {m}; using the built-in solver");
                builtin()
            }
            r => r,
        },
    }
}

/// Candidate packages from the `index.meta` of every *trusted* repo. None when
/// no trusted repo publishes one, in which case spk falls back to the older
/// per-package resolver. Untrusted repos are deliberately not consulted: the
/// plan must only name packages that fetch_package_ext will fetch from a
/// trusted repo.
fn fetch_index_meta() -> Result<Option<Vec<resolver::Pkg>>, String> {
    let mut all: Vec<resolver::Pkg> = Vec::new();
    for repo in load_repos().iter().filter(|r| r.priority < 100) {
        match trust_of(repo) {
            Ok(_) => {}
            Err(TrustError::Disallowed(m)) => {
                println!("{YELLOW}[spk] Skipping: {m}{R}");
                continue;
            }
            Err(TrustError::Broken(m)) => return Err(format!("repo '{}' is misconfigured: {m}", repo.name)),
        }
        let vi = match verified_index(repo) {
            Ok(v) => v,
            Err(IndexError::Unreachable(_)) => continue,
            Err(IndexError::Untrusted(m)) => {
                return Err(format!("repo '{}' failed verification: {m}", repo.name));
            }
        };
        for mut p in resolver::parse_index_meta(&vi.meta) {
            if !all.iter().any(|q| q.name == p.name && q.version == p.version) {
                p.repo = repo.name.clone();
                all.push(p);
            }
        }
    }
    Ok(if all.is_empty() { None } else { Some(all) })
}

/// Solve `request` against the repo metadata and the installed set.
/// None = no usable metadata (or the request isn't in it): use the fallback.
fn plan_install(request: &str, root: &str) -> Option<Result<Vec<resolver::Pkg>, String>> {
    if env::var("SPK_NO_SOLVER").is_ok()
        || looks_like_local_path(request)
        || is_nix_package(request)
        || is_apt_package(request)
    {
        return None;
    }
    let dep = resolver::parse_dep(request)?;
    let available = match fetch_index_meta() {
        Ok(Some(a)) => a,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    let installed: Vec<resolver::Pkg> = list_installed_names(root)
        .iter()
        .filter_map(|n| read_installed(root, n))
        .map(|p| resolver::Pkg::new(&p.name, &p.version, &p.depends, &p.provides, &p.conflicts))
        .collect();
    let uni = resolver::Universe { available, installed };
    match solve_with_backend(&uni, &[vec![dep]]) {
        Ok(plan) => Some(Ok(plan)),
        Err(resolver::SolveError::NotFound(_)) => None,
        Err(e) => Some(Err(e.message())),
    }
}

fn describe_plan(plan: &[resolver::Pkg]) -> String {
    plan.iter()
        .map(|p| format!("{} {}", p.name, p.version))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `spk repo-status`: for every configured repo, say whether it is signed and
/// whether its index verifies right now. Exit status 1 if any repo that has a
/// key fails verification.
fn cmd_repo_status() {
    let mut bad = false;
    for repo in load_repos() {
        let policy = match trust_of(&repo) {
            Ok(Trust::Signed(_)) => format!("signed (key {})", repo.key_path.clone().unwrap_or_default()),
            Ok(Trust::Unsigned) => "unsigned".to_string(),
            Err(TrustError::Disallowed(m)) => {
                println!("{YELLOW}{:<14}{R} {} -- {m}", repo.name, repo.url);
                continue;
            }
            Err(TrustError::Broken(m)) => {
                println!("{RED}{:<14}{R} {} -- BROKEN: {m}", repo.name, repo.url);
                bad = true;
                continue;
            }
        };
        print!("{BOLD}{:<14}{R} {} [{policy}, priority {}] ", repo.name, repo.url, repo.priority);
        match verified_index(&repo) {
            Ok(vi) => {
                let age = vi
                    .generated
                    .map(|g| format!(", generated {} h ago", now_unix().saturating_sub(g) / 3600))
                    .unwrap_or_default();
                let tag = if vi.signed { format!("{GREEN}verified{R}") } else { format!("{YELLOW}NOT verified{R}") };
                println!("-> {tag}: {} package(s){age}", vi.latest.len());
            }
            Err(IndexError::Unreachable(m)) => println!("-> {YELLOW}unreachable{R}: {m}"),
            Err(IndexError::Untrusted(m)) => {
                println!("-> {RED}{BOLD}REJECTED{R}: {m}");
                bad = true;
            }
        }
    }
    if bad {
        exit(1);
    }
}

/// `spk solve <pkg>`: show what an install would do, without installing.
fn cmd_solve(request: &str, porcelain: bool) {
    if porcelain {
        // Machine-readable: one "name version" line per package, install
        // order, nothing else on stdout. Failures go to stderr with exit 1.
        match plan_install(request, target_root()) {
            Some(Ok(plan)) => {
                for p in &plan {
                    println!("{} {}", p.name, p.version);
                }
                return;
            }
            Some(Err(msg)) => {
                eprintln!("{msg}");
                exit(1);
            }
            None => {
                eprintln!("no dependency metadata (index.meta) available");
                exit(2);
            }
        }
    }
    match plan_install(request, target_root()) {
        Some(Ok(plan)) if plan.is_empty() => {
            println!("{GREEN}[spk] Nothing to install: {request} and its dependencies are already satisfied.{R}");
        }
        Some(Ok(plan)) => {
            println!("{CYAN}[spk] {request} would install {} package(s), in this order:{R}", plan.len());
            for p in &plan {
                println!("  {GREEN}{}{R} {}", p.name, p.version);
            }
        }
        Some(Err(msg)) => {
            println!("{RED}[spk] Cannot resolve {request}: {msg}{R}");
            exit(1);
        }
        None => {
            println!("{YELLOW}[spk] No dependency metadata (index.meta) available for {request}; \
                      install would use the basic per-package resolver.{R}");
        }
    }
}

fn cmd_install(pkg: &str) {
    println!("{BOLD}[spk] Installing: {pkg}{R}");
    let root = target_root();
    match plan_install(pkg, root) {
        Some(Err(msg)) => {
            println!("{RED}{BOLD}[spk] Cannot resolve dependencies for {pkg}: {msg}{R}");
            println!("{YELLOW}[spk] Set SPK_NO_SOLVER=1 to skip the solver and use the basic resolver.{R}");
            exit(1);
        }
        Some(Ok(plan)) if !plan.is_empty() => {
            println!("{CYAN}[spk] Resolved {} package(s): {}{R}", plan.len(), describe_plan(&plan));
            PLANNED.with(|m| {
                let mut m = m.borrow_mut();
                m.clear();
                for p in &plan {
                    if !p.repo.is_empty() {
                        m.insert(p.name.clone(), (p.repo.clone(), p.version.clone()));
                    }
                }
            });
            let before: std::collections::HashSet<String> = list_installed_names(root).into_iter().collect();
            for p in &plan {
                let mut marker = vec![SOLVED_MARKER.to_string()];
                if !fetch_and_install_inner(&p.name, root, &mut marker) {
                    println!("{RED}{BOLD}[spk] Installation failed for {} (needed by {pkg}).{R}", p.name);
                    exit(1);
                }
            }
            for p in &plan {
                if p.name == pkg {
                    set_auto_mark(root, &p.name, false);
                } else if !before.contains(&p.name) {
                    set_auto_mark(root, &p.name, true);
                }
            }
            log_history(root, "install", &describe_plan(&plan));
            println!("{GREEN}{BOLD}[spk] {pkg} installed successfully.{R}");
            return;
        }
        // Empty plan (already installed, so reinstall as before) or no metadata:
        _ => {}
    }
    let before_fb: std::collections::HashSet<String> = list_installed_names(target_root()).into_iter().collect();
    if !fetch_and_install(pkg, target_root()) {
        println!("{RED}{BOLD}[spk] Installation failed for {pkg}.{R}");
        exit(1);
    }
    for n in list_installed_names(target_root()) {
        if n == pkg {
            set_auto_mark(target_root(), &n, false);
        } else if !before_fb.contains(&n) {
            set_auto_mark(target_root(), &n, true);
        }
    }
    log_history(target_root(), "install", pkg);
    println!("{GREEN}{BOLD}[spk] {pkg} installed successfully.{R}");
}

fn cmd_depends(name: &str) {
    fn print_tree(root: &str, name: &str, depth: usize, seen: &mut std::collections::HashSet<String>) {
        let indent = "  ".repeat(depth);
        if seen.contains(name) {
            println!("{indent}{YELLOW}{name} (already shown above -- shared dependency){R}");
            return;
        }
        seen.insert(name.to_string());
        match read_installed(root, name) {
            Some(p) => {
                let badge = if is_nix_package(&p.name) { format!(" {YELLOW}[nix]{R}") } else { String::new() };
                println!("{indent}{GREEN}{}{R} {}{badge}", p.name, p.version);
                for dep in parse_depends(&p.depends) {
                    print_tree(root, &dep, depth + 1, seen);
                }
            }
            None => println!("{indent}{RED}{name} (not installed){R}"),
        }
    }
    let root = target_root();
    let mut seen = std::collections::HashSet::new();
    print_tree(root, name, 0, &mut seen);
}

/// Everything installed that declares `name` in its own `depends:` --
/// i.e. what breaks if `name` is removed. Used by cmd_remove's
/// reverse-dependency check.
fn find_dependents(root: &str, name: &str) -> Vec<String> {
    list_installed_names(root)
        .into_iter()
        .filter(|other| other != name)
        .filter(|other| {
            read_installed(root, other)
                .map(|p| parse_depends(&p.depends).iter().any(|d| d == name))
                .unwrap_or(false)
        })
        .collect()
}

fn cmd_remove(name: &str, force: bool, yes: bool) {
    let root = target_root();
    let pkg = match read_installed(root, name) {
        Some(p) => p,
        None => {
            println!(
                "{RED}[spk] '{name}' is not installed (no record at {}).{R}",
                installed_db_path(root, name)
            );
            exit(1);
        }
    };

    let dependents = find_dependents(root, name);
    if !dependents.is_empty() {
        if !force {
            println!(
                "{RED}[spk] Refusing to remove '{name}': still required by: {}{R}",
                dependents.join(", ")
            );
            println!("  Use --force to remove anyway (may break those packages).");
            exit(1);
        }
        println!(
            "{YELLOW}{BOLD}[spk] WARNING: '{name}' is still required by: {} -- removing anyway (--force).{R}",
            dependents.join(", ")
        );
    }

    // Shared-file safety: a file is only actually deleted if NO other
    // installed package's own record also lists it. This is the one
    // place a mistake is genuinely destructive (deleting a shared
    // library out from under an unrelated package), so it's checked
    // fresh against every other installed record here rather than
    // trusted from anywhere else.
    let mut other_owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for other in list_installed_names(root) {
        if other == name {
            continue;
        }
        if let Some(p) = read_installed(root, &other) {
            other_owned.extend(p.files);
        }
    }
    let to_delete: Vec<&String> = pkg.files.iter().filter(|f| !other_owned.contains(*f)).collect();
    let kept: Vec<&String> = pkg.files.iter().filter(|f| other_owned.contains(*f)).collect();

    println!("{BOLD}[spk] Removing: {} {}{R}", pkg.name, pkg.version);
    println!("  {} file(s) will be deleted.", to_delete.len());
    if !kept.is_empty() {
        println!(
            "  {YELLOW}{} file(s) kept -- still owned by another installed package.{R}",
            kept.len()
        );
    }

    if !yes {
        print!("  Proceed? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if answer.trim().to_lowercase() != "y" {
            println!("{YELLOW}[spk] Aborted -- nothing removed.{R}");
            exit(0);
        }
    }

    let prerm_path = format!("{}.prerm", installed_db_path(root, name));
    if Path::new(&prerm_path).exists() {
        let _ = Command::new("chmod").args(["+x", &prerm_path]).status();
        match Command::new("sh").arg(&prerm_path).status() {
            Ok(s) if s.success() => {}
            _ => println!("{YELLOW}[spk] Warning: prerm script did not exit cleanly.{R}"),
        }
    }

    for f in &to_delete {
        let path = format!("{}/{}", root.trim_end_matches('/'), f);
        remove_root_file(&path);
    }

    let postrm_path = format!("{}.postrm", installed_db_path(root, name));
    if Path::new(&postrm_path).exists() {
        let _ = Command::new("chmod").args(["+x", &postrm_path]).status();
        match Command::new("sh").arg(&postrm_path).status() {
            Ok(s) if s.success() => {}
            _ => println!("{YELLOW}[spk] Warning: postrm script did not exit cleanly.{R}"),
        }
    }

    remove_root_file(&prerm_path);
    remove_root_file(&postrm_path);
    remove_root_file(&installed_db_path(root, name));
    set_auto_mark(root, name, false);
    log_history(root, "remove", &format!("{} {}", pkg.name, pkg.version));
    println!("{GREEN}[spk] {} removed.{R}", pkg.name);
}

fn cmd_install_local(path: &str) {
    if !Path::new(path).exists() {
        println!("{RED}[spk] Error: '{path}' does not exist.{R}");
        exit(1);
    }
    if !install_spkg_file(path, target_root()) {
        println!("{RED}{BOLD}[spk] Local package installation failed.{R}");
        exit(1);
    }
}

/// Point `spk install <name>` (the ordinary, no-flag form) at a local
/// directory of .spkg files too, by adding it to spk-repo-conf.yaml as a
/// `file://` repo entry at the highest priority. Reuses fetch_package_ext
/// verbatim -- curl already speaks file:// -- so this needed no new fetch
/// code, just a config-writing subcommand.
fn cmd_local_package_repo(folder: &str) {
    let abs = match fs::canonicalize(folder) {
        Ok(p) => p,
        Err(_) => {
            println!("{RED}[spk] Error: '{folder}' does not exist or is not accessible.{R}");
            exit(1);
        }
    };
    if !abs.is_dir() {
        println!("{RED}[spk] Error: '{folder}' is not a directory.{R}");
        exit(1);
    }
    let url = format!("file://{}", abs.display());

    let mut repos = load_repos();
    // Replace any existing "local" entry rather than accumulating
    // duplicates -- only one local-package-repo is active at a time.
    repos.retain(|r| r.name != "local");
    repos.push(Repo::unsigned("local", &url, 1));
    repos.sort_by_key(|r| r.priority);

    let out = repos_to_yaml(&repos);

    let write_ok = if is_root() {
        fs::write(REPO_CONF_PATH, &out).is_ok()
    } else {
        match Command::new("sudo")
            .args(["-S", "tee", REPO_CONF_PATH])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(out.as_bytes());
                }
                matches!(child.wait(), Ok(s) if s.success())
            }
            Err(_) => false,
        }
    };

    if write_ok {
        println!("{GREEN}[spk] Local package repo set: {}{R}", abs.display());
        println!("  'spk install <name>' will check this folder first (priority 1).");
    } else {
        println!("{RED}[spk] Failed to write {REPO_CONF_PATH}.{R}");
        exit(1);
    }
}

fn cmd_system_upgrade() {
    println!("{BOLD}{MAGENTA}[spk] System upgrade starting...{R}");

    let mut failures: Vec<String> = Vec::new();

    let root = target_root();
    for (i, pkg) in SMECHOS_PACKAGES.iter().enumerate() {
        println!("{BOLD}[{}/{}] Re-fetching {pkg}...{R}", i + 1, SMECHOS_PACKAGES.len());
        if !fetch_and_install(pkg, root) {
            failures.push(pkg.to_string());
        }
    }

    if failures.is_empty() {
        println!("{GREEN}{BOLD}[spk] System upgrade complete. Sovereignty verified.{R}");
    } else {
        println!("{YELLOW}{BOLD}[spk] Upgrade complete with failures: {failures:?}{R}");
        exit(1);
    }
}

// ── apt/dnf-style commands ────────────────────────────────────────────────────
//
// Everything below works on the same installed-package records and repo
// indexes as install/remove. The extra state it keeps lives next to the
// records, under <root>/var/lib/spk/:
//   auto-installed   packages that were pulled in only as dependencies
//   held             packages `upgrade` must leave alone
//   history.log      one line per install / remove / upgrade
// Packages that were already in an image have no auto mark, so they count as
// explicitly installed and `autoremove` never touches them.

use std::collections::BTreeSet;

fn spk_state_path(root: &str, name: &str) -> String {
    format!("{}/var/lib/spk/{name}", root.trim_end_matches('/'))
}

fn ensure_parent_dir(path: &str) -> bool {
    match Path::new(path).parent() {
        Some(d) if !d.as_os_str().is_empty() => ensure_root_dir(&d.to_string_lossy()),
        _ => true,
    }
}

fn read_name_set(path: &str) -> BTreeSet<String> {
    fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn write_name_set(path: &str, set: &BTreeSet<String>) -> bool {
    if !ensure_parent_dir(path) {
        return false;
    }
    let mut out = String::new();
    for n in set {
        out.push_str(n);
        out.push('\n');
    }
    write_root_file(path, &out)
}

fn set_auto_mark(root: &str, name: &str, auto: bool) {
    let path = spk_state_path(root, "auto-installed");
    let mut set = read_name_set(&path);
    let changed = if auto { set.insert(name.to_string()) } else { set.remove(name) };
    if changed {
        write_name_set(&path, &set);
    }
}

fn log_history(root: &str, action: &str, detail: &str) {
    let stamp = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let path = spk_state_path(root, "history.log");
    let mut text = fs::read_to_string(&path).unwrap_or_default();
    text.push_str(&format!("{stamp} {action} {detail}\n"));
    if ensure_parent_dir(&path) {
        write_root_file(&path, &text);
    }
}

fn all_records(root: &str) -> Vec<InstalledPkg> {
    list_installed_names(root)
        .iter()
        .filter_map(|n| read_installed(root, n))
        .collect()
}

/// The newest published version of every package, across all trusted repos.
fn latest_available() -> Result<Vec<resolver::Pkg>, String> {
    let all = match fetch_index_meta()? {
        Some(a) => a,
        None => return Err("no configured repo publishes an index.meta".to_string()),
    };
    let mut best: HashMap<String, resolver::Pkg> = HashMap::new();
    for p in all {
        let keep_old = matches!(
            best.get(&p.name),
            Some(b) if resolver::compare_versions(&b.version, &p.version) != std::cmp::Ordering::Less
        );
        if !keep_old {
            best.insert(p.name.clone(), p);
        }
    }
    let mut v: Vec<resolver::Pkg> = best.into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
}

struct Upgrade {
    name: String,
    from: String,
    to: resolver::Pkg,
}

fn find_upgrades(root: &str) -> Result<Vec<Upgrade>, String> {
    let avail = latest_available()?;
    let mut out = Vec::new();
    for rec in all_records(root) {
        // Legacy bundles recorded no real version, so there is nothing to compare.
        if rec.version.starts_with("unknown") {
            continue;
        }
        if let Some(p) = avail.iter().find(|p| p.name == rec.name) {
            if resolver::compare_versions(&p.version, &rec.version) == std::cmp::Ordering::Greater {
                out.push(Upgrade { name: rec.name.clone(), from: rec.version.clone(), to: p.clone() });
            }
        }
    }
    Ok(out)
}

fn confirm(prompt: &str) -> bool {
    print!("  {prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    answer.trim().to_lowercase() == "y"
}

fn names_in(args: &[String]) -> Vec<String> {
    args.iter().filter(|a| !a.starts_with('-')).cloned().collect()
}

fn has_flag(args: &[String], long: &str, short: &str) -> bool {
    args.iter().any(|a| a == long || (!short.is_empty() && a == short))
}

/// `spk update`: verify every repo's index and say what could be upgraded.
/// spk reads indexes on demand rather than keeping a local copy, so this is
/// the check APT's `update` does for a fresh cache, not a download step.
fn cmd_update() {
    println!("{BOLD}[spk] Checking repository indexes...{R}");
    match fetch_index_meta() {
        Err(e) => {
            println!("{RED}{BOLD}[spk] {e}{R}");
            exit(1);
        }
        Ok(None) => println!("{YELLOW}[spk] No configured repo publishes an index.meta.{R}"),
        Ok(Some(all)) => {
            let names: std::collections::HashSet<&str> = all.iter().map(|p| p.name.as_str()).collect();
            println!(
                "{GREEN}[spk] {} version(s) of {} package(s) available.{R}",
                all.len(),
                names.len()
            );
        }
    }
    cmd_repo_status();
    if let Ok(ups) = find_upgrades(target_root()) {
        if ups.is_empty() {
            println!("{GREEN}[spk] Everything is up to date.{R}");
        } else {
            println!("{CYAN}[spk] {} package(s) can be upgraded.   spk upgrade{R}", ups.len());
        }
    }
}

fn cmd_search(terms: &[String]) {
    let terms = names_in(terms);
    if terms.is_empty() {
        println!("{RED}[spk] Error: specify a search term.   spk search <term>...{R}");
        exit(1);
    }
    let root = target_root();
    let avail = match latest_available() {
        Ok(a) => a,
        Err(e) => {
            println!("{YELLOW}[spk] No repo index to search ({e}); showing installed packages only.{R}");
            Vec::new()
        }
    };
    let recs = all_records(root);
    let upgrades: Vec<String> = find_upgrades(root).map(|u| u.into_iter().map(|x| x.name).collect()).unwrap_or_default();

    let matches = |name: &str, provides: &str| -> bool {
        let hay = format!("{} {}", name, provides).to_lowercase();
        terms.iter().all(|t| hay.contains(&t.to_lowercase()))
    };
    let mut rows: BTreeSet<String> = BTreeSet::new();
    for p in &avail {
        let prov: Vec<String> = p.provides.iter().map(|(n, _)| n.clone()).collect();
        if matches(&p.name, &prov.join(" ")) {
            rows.insert(p.name.clone());
        }
    }
    for r in &recs {
        if matches(&r.name, &r.provides) {
            rows.insert(r.name.clone());
        }
    }
    if rows.is_empty() {
        println!("{YELLOW}[spk] Nothing matches '{}'.{R}", terms.join(" "));
        return;
    }
    for name in &rows {
        let newest = avail.iter().find(|p| &p.name == name);
        let installed = recs.iter().find(|r| &r.name == name);
        let version = newest
            .map(|p| p.version.clone())
            .or_else(|| installed.map(|r| r.version.clone()))
            .unwrap_or_default();
        let badge = match installed {
            Some(_) if upgrades.contains(name) => format!(" {YELLOW}[installed, upgradable]{R}"),
            Some(_) => format!(" {CYAN}[installed]{R}"),
            None => String::new(),
        };
        println!("  {GREEN}{name}{R}  {version}{badge}");
    }
}

fn dep_groups_to_string(groups: &[Vec<resolver::Dep>]) -> String {
    groups
        .iter()
        .map(|g| g.iter().map(|d| d.name.clone()).collect::<Vec<_>>().join(" | "))
        .collect::<Vec<_>>()
        .join(", ")
}

fn cmd_info(name: &str) {
    let root = target_root();
    let rec = read_installed(root, name);
    let avail = latest_available().ok().and_then(|a| a.into_iter().find(|p| p.name == name));
    if rec.is_none() && avail.is_none() {
        println!("{RED}[spk] No package called '{name}' is installed or published.{R}");
        exit(1);
    }
    let auto = read_name_set(&spk_state_path(root, "auto-installed"));
    let held = read_name_set(&spk_state_path(root, "held"));
    println!("{BOLD}Package:{R}    {name}");
    match &rec {
        Some(r) => println!("{BOLD}Installed:{R}  {}", r.version),
        None => println!("{BOLD}Installed:{R}  no"),
    }
    if let Some(a) = &avail {
        println!("{BOLD}Available:{R}  {} (repo {})", a.version, a.repo);
    }
    if rec.is_some() {
        let how = if auto.contains(name) { "installed as a dependency" } else { "installed explicitly" };
        let hold = if held.contains(name) { ", held" } else { "" };
        println!("{BOLD}Status:{R}     {how}{hold}");
    }
    let depends = match (&rec, &avail) {
        (Some(r), _) if !r.depends.is_empty() => r.depends.clone(),
        (_, Some(a)) => dep_groups_to_string(&a.depends),
        _ => String::new(),
    };
    if !depends.is_empty() {
        println!("{BOLD}Depends:{R}    {depends}");
    }
    if let Some(r) = &rec {
        if !r.provides.is_empty() {
            println!("{BOLD}Provides:{R}   {}", r.provides);
        }
        if !r.conflicts.is_empty() {
            println!("{BOLD}Conflicts:{R}  {}", r.conflicts);
        }
        println!("{BOLD}Files:{R}      {}", r.files.len());
        let rdeps = find_dependents(root, name);
        if !rdeps.is_empty() {
            println!("{BOLD}Needed by:{R}  {}", rdeps.join(", "));
        }
    }
}

fn cmd_files(name: &str) {
    match read_installed(target_root(), name) {
        Some(r) => {
            for f in &r.files {
                println!("/{}", f.trim_start_matches('/'));
            }
        }
        None => {
            println!("{RED}[spk] '{name}' is not installed.{R}");
            exit(1);
        }
    }
}

fn normalize_owned_path(p: &str) -> String {
    p.trim_start_matches("./").trim_start_matches('/').to_string()
}

fn cmd_owns(path: &str) {
    let want = normalize_owned_path(path);
    let owners: Vec<String> = all_records(target_root())
        .into_iter()
        .filter(|r| r.files.iter().any(|f| normalize_owned_path(f) == want))
        .map(|r| r.name)
        .collect();
    if owners.is_empty() {
        println!("{YELLOW}[spk] No installed package owns /{want}.{R}");
        exit(1);
    }
    for o in owners {
        println!("{o}: /{want}");
    }
}

fn cmd_provides(name: &str) {
    let root = target_root();
    let mut found = false;
    for r in all_records(root) {
        let provided: Vec<String> = resolver::parse_provides(&r.provides).into_iter().map(|(n, _)| n).collect();
        if r.name == name || provided.iter().any(|p| p == name) {
            println!("{GREEN}{}{R}  {}  (installed)", r.name, r.version);
            found = true;
        }
    }
    if let Ok(avail) = latest_available() {
        for p in avail {
            if (p.name == name || p.provides.iter().any(|(n, _)| n == name))
                && !is_installed(root, &p.name)
            {
                println!("{GREEN}{}{R}  {}  (available, repo {})", p.name, p.version, p.repo);
                found = true;
            }
        }
    }
    if !found {
        println!("{YELLOW}[spk] Nothing provides '{name}'.{R}");
        exit(1);
    }
}

fn cmd_list(args: &[String]) {
    let root = target_root();
    if has_flag(args, "--available", "") {
        match latest_available() {
            Ok(avail) => {
                println!("{BOLD}Available packages ({}){R}", avail.len());
                for p in avail {
                    println!("  {GREEN}{}{R}  {}  (repo {})", p.name, p.version, p.repo);
                }
            }
            Err(e) => {
                println!("{RED}[spk] {e}{R}");
                exit(1);
            }
        }
        return;
    }
    if has_flag(args, "--upgradable", "") {
        match find_upgrades(root) {
            Ok(ups) => {
                println!("{BOLD}Upgradable packages ({}){R}", ups.len());
                for u in ups {
                    println!("  {GREEN}{}{R}  {} -> {}", u.name, u.from, u.to.version);
                }
            }
            Err(e) => {
                println!("{RED}[spk] {e}{R}");
                exit(1);
            }
        }
        return;
    }
    let auto = read_name_set(&spk_state_path(root, "auto-installed"));
    let held = read_name_set(&spk_state_path(root, "held"));
    let mut names = list_installed_names(root);
    if has_flag(args, "--auto", "") {
        names.retain(|n| auto.contains(n));
    }
    if has_flag(args, "--manual", "") {
        names.retain(|n| !auto.contains(n));
    }
    if has_flag(args, "--held", "") {
        names.retain(|n| held.contains(n));
    }
    if names.is_empty() {
        println!(
            "{YELLOW}[spk] No packages installed (or {} doesn't exist yet).{R}",
            installed_db_dir(root)
        );
        return;
    }
    println!("{BOLD}Installed packages ({}){R}", names.len());
    for name in &names {
        if let Some(p) = read_installed(root, name) {
            let mut badge = String::new();
            if is_nix_package(&p.name) {
                badge.push_str(&format!(" {YELLOW}[nix]{R}"));
            }
            if auto.contains(name) {
                badge.push_str(&format!(" {CYAN}[auto]{R}"));
            }
            if held.contains(name) {
                badge.push_str(&format!(" {YELLOW}[held]{R}"));
            }
            println!("  {GREEN}{}{R}  {}{badge}", p.name, p.version);
        }
    }
}

/// Files the old version owned that the new version no longer ships, and that
/// no other package owns, are deleted after an upgrade -- what APT and DNF do.
fn prune_dropped_files(root: &str, name: &str, old_files: &[String]) -> usize {
    let new_files: std::collections::HashSet<String> = read_installed(root, name)
        .map(|p| p.files.into_iter().collect())
        .unwrap_or_default();
    let mut owned_elsewhere: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in all_records(root) {
        if r.name != name {
            owned_elsewhere.extend(r.files);
        }
    }
    let mut n = 0;
    for f in old_files {
        if !new_files.contains(f) && !owned_elsewhere.contains(f) {
            remove_root_file(&format!("{}/{}", root.trim_end_matches('/'), f));
            n += 1;
        }
    }
    n
}

fn cmd_upgrade(args: &[String]) {
    let root = target_root();
    let yes = has_flag(args, "--yes", "-y");
    let dry = has_flag(args, "--dry-run", "-n");
    let wanted = names_in(args);
    let held = read_name_set(&spk_state_path(root, "held"));
    let ups = match find_upgrades(root) {
        Ok(u) => u,
        Err(e) => {
            println!("{RED}[spk] Cannot check for upgrades: {e}{R}");
            exit(1);
        }
    };
    for w in &wanted {
        if !is_installed(root, w) {
            println!("{RED}[spk] '{w}' is not installed.{R}");
            exit(1);
        }
    }
    let todo: Vec<&Upgrade> = ups
        .iter()
        .filter(|u| if wanted.is_empty() { !held.contains(&u.name) } else { wanted.contains(&u.name) })
        .collect();
    if todo.is_empty() {
        println!("{GREEN}[spk] Nothing to upgrade.{R}");
        return;
    }
    println!("{BOLD}[spk] {} package(s) will be upgraded:{R}", todo.len());
    for u in &todo {
        println!("  {GREEN}{}{R}  {} -> {}", u.name, u.from, u.to.version);
    }
    if dry {
        return;
    }
    if !yes && !confirm("Proceed?") {
        println!("{YELLOW}[spk] Aborted -- nothing upgraded.{R}");
        exit(0);
    }
    let mut failures = Vec::new();
    for u in &todo {
        let old_files = read_installed(root, &u.name).map(|p| p.files).unwrap_or_default();
        PLANNED.with(|m| {
            let mut m = m.borrow_mut();
            m.clear();
            m.insert(u.name.clone(), (u.to.repo.clone(), u.to.version.clone()));
        });
        println!("{BOLD}[spk] Upgrading {} {} -> {}{R}", u.name, u.from, u.to.version);
        if fetch_and_install_inner(&u.name, root, &mut Vec::new()) {
            let dropped = prune_dropped_files(root, &u.name, &old_files);
            if dropped > 0 {
                println!("  removed {dropped} file(s) the new version no longer ships.");
            }
            log_history(root, "upgrade", &format!("{} {} -> {}", u.name, u.from, u.to.version));
        } else {
            failures.push(u.name.clone());
        }
    }
    if failures.is_empty() {
        println!("{GREEN}{BOLD}[spk] Upgrade complete.{R}");
    } else {
        println!("{RED}{BOLD}[spk] Upgrade finished with failures: {}{R}", failures.join(", "));
        exit(1);
    }
}

fn cmd_reinstall(names: &[String]) {
    let root = target_root();
    if names.is_empty() {
        println!("{RED}[spk] Error: specify a package.   spk reinstall <pkg>{R}");
        exit(1);
    }
    for n in names {
        if !is_installed(root, n) {
            println!("{RED}[spk] '{n}' is not installed; use 'spk install {n}'.{R}");
            exit(1);
        }
        let old_files = read_installed(root, n).map(|p| p.files).unwrap_or_default();
        if !fetch_and_install_inner(n, root, &mut Vec::new()) {
            println!("{RED}{BOLD}[spk] Reinstall failed for {n}.{R}");
            exit(1);
        }
        prune_dropped_files(root, n, &old_files);
        log_history(root, "reinstall", n);
        println!("{GREEN}[spk] {n} reinstalled.{R}");
    }
}

/// Everything reachable from the explicitly installed packages through
/// `depends` (every alternative that is installed counts as needed).
fn needed_set(recs: &[InstalledPkg], auto: &BTreeSet<String>) -> std::collections::HashSet<String> {
    let pkgs: Vec<resolver::Pkg> = recs
        .iter()
        .map(|p| resolver::Pkg::new(&p.name, &p.version, &p.depends, &p.provides, &p.conflicts))
        .collect();
    let mut needed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack: Vec<String> = recs.iter().filter(|r| !auto.contains(&r.name)).map(|r| r.name.clone()).collect();
    while let Some(n) = stack.pop() {
        if !needed.insert(n.clone()) {
            continue;
        }
        let Some(p) = pkgs.iter().find(|p| p.name == n) else { continue };
        for group in &p.depends {
            for alt in group {
                for q in pkgs.iter().filter(|q| q.satisfies(alt)) {
                    stack.push(q.name.clone());
                }
            }
        }
    }
    needed
}

fn cmd_autoremove(args: &[String]) {
    let root = target_root();
    let yes = has_flag(args, "--yes", "-y");
    let dry = has_flag(args, "--dry-run", "-n");
    let auto = read_name_set(&spk_state_path(root, "auto-installed"));
    let recs = all_records(root);
    let needed = needed_set(&recs, &auto);
    let removable: Vec<String> = recs
        .iter()
        .filter(|r| auto.contains(&r.name) && !needed.contains(&r.name))
        .map(|r| r.name.clone())
        .collect();
    if removable.is_empty() {
        println!("{GREEN}[spk] Nothing to remove: no dependency is left without a package needing it.{R}");
        return;
    }
    println!("{BOLD}[spk] {} package(s) are no longer needed:{R}", removable.len());
    for n in &removable {
        println!("  {GREEN}{n}{R}");
    }
    if dry {
        return;
    }
    if !yes && !confirm("Remove them?") {
        println!("{YELLOW}[spk] Aborted -- nothing removed.{R}");
        exit(0);
    }
    let exe = env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("spk"));
    for n in &removable {
        let ok = Command::new(&exe)
            .args(["remove", n, "-y", "--force"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            println!("{RED}{BOLD}[spk] Could not remove {n}; stopping.{R}");
            exit(1);
        }
        set_auto_mark(root, n, false);
    }
    log_history(root, "autoremove", &removable.join(" "));
    println!("{GREEN}{BOLD}[spk] Autoremove complete.{R}");
}

/// `mark-manual`, `mark-auto`, `hold`, `unhold` (APT's apt-mark).
fn cmd_mark(action: &str, names: &[String]) {
    let root = target_root();
    if names.is_empty() {
        println!("{RED}[spk] Error: specify a package.   spk {action} <pkg>...{R}");
        exit(1);
    }
    for n in names {
        if !is_installed(root, n) {
            println!("{RED}[spk] '{n}' is not installed.{R}");
            exit(1);
        }
        match action {
            "mark-manual" => set_auto_mark(root, n, false),
            "mark-auto" => set_auto_mark(root, n, true),
            _ => {
                let path = spk_state_path(root, "held");
                let mut set = read_name_set(&path);
                if action == "hold" {
                    set.insert(n.clone());
                } else {
                    set.remove(n);
                }
                write_name_set(&path, &set);
            }
        }
        println!("{GREEN}[spk] {n}: {action} done.{R}");
    }
}

/// `spk verify [pkg...]`: every file a package's record lists must still be
/// on disk. Records hold no checksums yet, so this finds missing files, not
/// modified ones.
fn cmd_verify(args: &[String]) {
    let root = target_root();
    let wanted = names_in(args);
    let verbose = has_flag(args, "--verbose", "-v");
    let names = if wanted.is_empty() { list_installed_names(root) } else { wanted };
    let mut bad = 0;
    for n in &names {
        let Some(r) = read_installed(root, n) else {
            println!("{RED}[spk] '{n}' is not installed.{R}");
            exit(1);
        };
        let missing: Vec<&String> = r
            .files
            .iter()
            .filter(|f| fs::symlink_metadata(format!("{}/{}", root.trim_end_matches('/'), f)).is_err())
            .collect();
        if !missing.is_empty() {
            bad += 1;
            println!("{YELLOW}{n}{R}: {} of {} file(s) missing", missing.len(), r.files.len());
            for f in missing.iter().take(if verbose { usize::MAX } else { 3 }) {
                println!("    /{f}");
            }
        }
    }
    if bad == 0 {
        println!("{GREEN}[spk] All files of {} package(s) are present.{R}", names.len());
    } else {
        println!("{YELLOW}[spk] {bad} package(s) have missing files. (-v lists them all){R}");
        exit(1);
    }
}

/// `spk check`: every installed package's dependencies are satisfied and no
/// installed pair conflicts.
fn cmd_check() {
    let root = target_root();
    let recs = all_records(root);
    let pkgs: Vec<resolver::Pkg> = recs
        .iter()
        .map(|p| resolver::Pkg::new(&p.name, &p.version, &p.depends, &p.provides, &p.conflicts))
        .collect();
    let mut problems = 0;
    for p in &pkgs {
        for group in &p.depends {
            if !group.iter().any(|alt| pkgs.iter().any(|q| q.satisfies(alt))) {
                problems += 1;
                println!("{YELLOW}{}{R}: unmet dependency: {}", p.name, dep_groups_to_string(std::slice::from_ref(group)));
            }
        }
        for c in &p.conflicts {
            if let Some(q) = pkgs.iter().find(|q| q.name != p.name && q.satisfies(c)) {
                problems += 1;
                println!("{RED}{}{R}: conflicts with installed {}", p.name, q.name);
            }
        }
    }
    if problems == 0 {
        println!("{GREEN}[spk] No broken dependencies among {} package(s).{R}", pkgs.len());
    } else {
        println!("{YELLOW}[spk] {problems} problem(s) found.{R}");
        exit(1);
    }
}

fn cmd_history() {
    match fs::read_to_string(spk_state_path(target_root(), "history.log")) {
        Ok(t) if !t.trim().is_empty() => print!("{t}"),
        _ => println!("{YELLOW}[spk] No history yet.{R}"),
    }
}

/// Download packages into the current directory without installing them.
fn cmd_download(names: &[String]) {
    if names.is_empty() {
        println!("{RED}[spk] Error: specify a package.   spk download <pkg>...{R}");
        exit(1);
    }
    for n in names {
        let dest = format!("{n}.spkg");
        if fetch_package_ext(n, "spkg", &dest) {
            println!("{GREEN}[spk] Saved {dest}{R}");
        } else {
            println!("{RED}[spk] Could not download {n}.{R}");
            exit(1);
        }
    }
}

/// Remove the partial downloads an interrupted install can leave in /tmp.
fn cmd_clean() {
    let mut n = 0;
    if let Ok(entries) = fs::read_dir("/tmp") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("spk-") && (name.ends_with(".spkg") || name.ends_with(".tar.xz")) {
                if fs::remove_file(e.path()).is_ok() {
                    n += 1;
                }
            }
        }
    }
    println!("{GREEN}[spk] Removed {n} leftover download(s).{R}");
}

fn write_repo_conf(repos: &[Repo]) -> bool {
    let path = env::var("SPK_REPO_CONF").unwrap_or_else(|_| REPO_CONF_PATH.to_string());
    write_root_file(&path, &repos_to_yaml(repos))
}

/// `spk repo list|add|remove`. Rewrites the config file from the parsed repos,
/// so hand-written comments in it are not kept.
fn cmd_repo(args: &[String]) {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "list" => cmd_repo_status(),
        "add" => {
            let pos = names_in(&rest.iter().filter(|a| !a.starts_with("--")).cloned().collect::<Vec<_>>());
            let flag_val = |flag: &str| rest.iter().position(|a| a == flag).and_then(|i| rest.get(i + 1)).cloned();
            let value_args: Vec<String> = ["--priority", "--key"].iter().filter_map(|f| flag_val(f)).collect();
            let pos: Vec<String> = pos.into_iter().filter(|p| !value_args.contains(p)).collect();
            if pos.len() != 2 {
                println!("{RED}[spk] Error: spk repo add <name> <url> [--priority N] [--key /path/key.pub]{R}");
                exit(1);
            }
            let priority = flag_val("--priority").and_then(|p| p.parse::<u8>().ok()).unwrap_or(50);
            let mut repos = load_repos();
            if repos.iter().any(|r| r.name == pos[0]) {
                println!("{RED}[spk] A repo called '{}' already exists.{R}", pos[0]);
                exit(1);
            }
            let mut repo = Repo::unsigned(&pos[0], &pos[1], priority);
            if let Some(path) = flag_val("--key") {
                repo.key = match fs::read_to_string(&path) {
                    Ok(pem) if pem.contains("BEGIN PUBLIC KEY") => Ok(Some(pem)),
                    Ok(_) => Err(format!("key file {path} is not a PEM public key")),
                    Err(e) => Err(format!("cannot read key file {path}: {e}")),
                };
                if let Err(e) = &repo.key {
                    println!("{RED}[spk] {e}{R}");
                    exit(1);
                }
                repo.key_path = Some(path);
            }
            repos.push(repo);
            repos.sort_by_key(|r| r.priority);
            if !write_repo_conf(&repos) {
                println!("{RED}[spk] Failed to write the repo config.{R}");
                exit(1);
            }
            println!("{GREEN}[spk] Repo '{}' added (priority {priority}).{R}", pos[0]);
        }
        "remove" => {
            let Some(name) = names_in(rest).into_iter().next() else {
                println!("{RED}[spk] Error: spk repo remove <name>{R}");
                exit(1);
            };
            let mut repos = load_repos();
            let before = repos.len();
            repos.retain(|r| r.name != name);
            if repos.len() == before {
                println!("{RED}[spk] No repo called '{name}'.{R}");
                exit(1);
            }
            if repos.is_empty() {
                println!("{RED}[spk] Refusing to remove the last repo.{R}");
                exit(1);
            }
            if !write_repo_conf(&repos) {
                println!("{RED}[spk] Failed to write the repo config.{R}");
                exit(1);
            }
            println!("{GREEN}[spk] Repo '{name}' removed.{R}");
        }
        other => {
            println!("{RED}[spk] Unknown repo command '{other}'. Use: list, add, remove.{R}");
            exit(1);
        }
    }
}

// ── create-live-image ──────────────────────────────────────────────────────────
//
// Assembles a bootable image straight from packages already sitting on a
// repo, instead of spk-compile.py's from-source build. Needs a repo to
// publish index.txt (see phase_bundle_spkg_packages) -- one "name
// version" pair per line -- so spk can discover what's available without
// anything hardcoded into this binary.
//
// Honest current limitation: kernel, firmware, GRUB, and the live
// initramfs aren't published as .spkg packages yet (only KF6/Plasma/Qt6
// modules are, as of this writing -- see phase_bundle_spkg_packages's own
// docstring). This still produces a real squashfs of everything the repo
// actually has; it only produces a bootable ISO on top of that if
// packages providing /boot/vmlinuz, /boot/live-initrd.img, and a real
// grub-mkrescue end up in the installed set. That's not a bug in this
// command -- it's an accurate reflection of what's published today.

fn fetch_repo_index() -> Option<(String, Vec<String>)> {
    for repo in load_repos() {
        match trust_of(&repo) {
            Ok(Trust::Signed(_)) => {
                // Names come from the verified index.meta, never from an unsigned index.txt.
                match verified_index(&repo) {
                    Ok(vi) => {
                        let mut names: Vec<String> = vi.latest.keys().cloned().collect();
                        names.sort();
                        if !names.is_empty() {
                            return Some((repo.name.clone(), names));
                        }
                    }
                    Err(IndexError::Unreachable(_)) => {}
                    Err(IndexError::Untrusted(m)) => {
                        println!("{RED}{BOLD}[spk] Repo '{}' failed verification: {m}{R}", repo.name);
                        return None;
                    }
                }
                continue;
            }
            Err(TrustError::Disallowed(m)) => {
                println!("{YELLOW}[spk] Skipping: {m}{R}");
                continue;
            }
            Err(TrustError::Broken(m)) => {
                println!("{RED}{BOLD}[spk] Repo '{}' is misconfigured: {m}{R}", repo.name);
                return None;
            }
            Ok(Trust::Unsigned) => {}
        }
        let idx_tmp = format!("/tmp/spk-index-{}.txt", std::process::id());
        let url = format!("{}/index.txt", repo.url.trim_end_matches('/'));
        if !curl_to(&url, &idx_tmp) {
            let _ = fs::remove_file(&idx_tmp);
            continue;
        }
        let names: Vec<String> = fs::read_to_string(&idx_tmp)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|s| s.to_string())
            .collect();
        let _ = fs::remove_file(&idx_tmp);
        if !names.is_empty() {
            return Some((repo.name.clone(), names));
        }
    }
    None
}

fn cmd_create_live_image(out_iso: &str, extra_names: &[String]) {
    let mut names: Vec<String> = Vec::new();
    match fetch_repo_index() {
        Some((repo_name, index_names)) => {
            println!(
                "{GREEN}[spk] Using package index from repo '{repo_name}' ({} packages).{R}",
                index_names.len()
            );
            names = index_names;
        }
        None => {
            println!(
                "{YELLOW}[spk] No configured repo has an index.txt.{R}"
            );
        }
    }
    for n in extra_names {
        if !names.contains(n) {
            names.push(n.clone());
        }
    }
    if names.is_empty() {
        println!("{RED}[spk] No packages to install -- no repo index found and no extra package names given.{R}");
        println!("  Usage: spk create-live-image <output.iso> [extra-package-name...]");
        exit(1);
    }

    let scratch = format!("/tmp/spk-live-image-root-{}", std::process::id());
    let _ = fs::remove_dir_all(&scratch);
    if fs::create_dir_all(&scratch).is_err() {
        println!("{RED}[spk] Failed to create scratch root {scratch}.{R}");
        exit(1);
    }

    println!("{BOLD}[spk] Installing {} package(s) into {scratch}...{R}", names.len());
    let mut failures = Vec::new();
    for (i, name) in names.iter().enumerate() {
        println!("{BOLD}[{}/{}] {name}{R}", i + 1, names.len());
        // Dependency resolution and the installed-db it consults are
        // both scoped to `scratch` (see installed_db_dir), so the
        // resulting image itself boots with a real, working spk
        // database -- not just the files, the package metadata too.
        if !fetch_and_install_inner(name, &scratch, &mut Vec::new()) {
            failures.push(name.clone());
        }
    }
    if !failures.is_empty() {
        println!(
            "{YELLOW}[spk] {} package(s) failed to install: {}{R}",
            failures.len(),
            failures.join(", ")
        );
    }

    let vmlinuz = format!("{scratch}/boot/vmlinuz");
    let initrd = format!("{scratch}/boot/live-initrd.img");
    let have_boot = Path::new(&vmlinuz).exists() && Path::new(&initrd).exists();
    if !have_boot {
        println!(
            "{YELLOW}[spk] {scratch}/boot/vmlinuz and/or live-initrd.img are not present in \
             the installed package set -- kernel/firmware/live-initramfs aren't published as \
             .spkg packages yet. Producing the squashfs only, not a bootable ISO.{R}"
        );
    }

    let squashfs_path = format!("{scratch}.squashfs");
    println!("{CYAN}[spk] Building squashfs...{R}");
    let squash_ok = Command::new("mksquashfs")
        .args([scratch.as_str(), squashfs_path.as_str(), "-comp", "xz",
               "-e", &format!("{scratch}/boot"), "-noappend"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !squash_ok {
        println!("{RED}[spk] mksquashfs failed.{R}");
        exit(1);
    }
    println!("{GREEN}[spk] squashfs: {squashfs_path}{R}");

    if have_boot {
        let iso_root = format!("{scratch}-iso-root");
        let _ = fs::remove_dir_all(&iso_root);
        let _ = fs::create_dir_all(format!("{iso_root}/boot/grub"));
        let _ = fs::create_dir_all(format!("{iso_root}/live"));
        let _ = fs::copy(&vmlinuz, format!("{iso_root}/boot/vmlinuz"));
        let _ = fs::copy(&initrd, format!("{iso_root}/boot/live-initrd.img"));
        let _ = fs::rename(&squashfs_path, format!("{iso_root}/live/filesystem.squashfs"));
        let grub_cfg = "set timeout=10\nset default=0\n\n\
            menuentry \"SmechOS (from repo image)\" {\n\
            \x20\x20\x20\x20linux  /boot/vmlinuz boot=live quiet splash loglevel=3\n\
            \x20\x20\x20\x20initrd /boot/live-initrd.img\n}\n";
        let _ = fs::write(format!("{iso_root}/boot/grub/grub.cfg"), grub_cfg);

        let grub_mkrescue = format!("{scratch}/usr/bin/grub-mkrescue");
        let mkrescue_bin = if Path::new(&grub_mkrescue).exists() {
            grub_mkrescue
        } else {
            "grub-mkrescue".to_string()
        };
        println!("{CYAN}[spk] Building ISO with {mkrescue_bin}...{R}");
        let iso_ok = Command::new(&mkrescue_bin)
            .args(["-o", out_iso, &iso_root, "--", "-volid", "SMECHOS_LIVE"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let _ = fs::remove_dir_all(&iso_root);
        if iso_ok {
            println!("{GREEN}{BOLD}[spk] Bootable image: {out_iso}{R}");
        } else {
            println!("{RED}[spk] grub-mkrescue failed -- squashfs was still produced at {squashfs_path} before this step moved it into the ISO root.{R}");
            exit(1);
        }
    } else {
        println!(
            "{YELLOW}[spk] Done, but no ISO was produced (see above). \
             Publish kernel/live-initramfs/grub as .spkg packages to close this gap.{R}"
        );
    }
}

// ── Compile (build orchestration) ─────────────────────────────────────────────

fn find_spk_compile() -> Option<String> {
    let candidates = [
        "/usr/share/spk/spk-compile.py",
        "/opt/smechdeploy/spk-compile.py",
        "spk-compile.py",
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    None
}

fn cmd_build_image(args: &[String]) {
    let script = match find_spk_compile() {
        Some(s) => s,
        None => {
            println!("{RED}[spk] spk-compile.py not found.{R}");
            println!("  Install it to one of:");
            println!("    /usr/share/spk/spk-compile.py");
            println!("    /opt/smechdeploy/spk-compile.py");
            println!("  Or get spk-compile: https://github.com/Smech-Labs/spk-compile");
            exit(1);
        }
    };
    println!("{CYAN}[spk] Running build orchestrator: {script}{R}");
    let status = Command::new("python3")
        .arg(&script)
        .args(args)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(e) => {
            println!("{RED}[spk] Failed to run spk-compile.py: {e}{R}");
            exit(1);
        }
    }
}

// ── spk compile: build ONE package's source into a .spkg ────────────────────
//
// Not to be confused with `spk build-image`, which forwards to
// spk-compile.py to build an entire SmechOS image from source -- a
// different job by an order of magnitude. `compile` here is the verb a
// package manager actually owns: take a recipe describing how to build and
// stage one component, run it, and package the result. Pure Rust, no
// spk-compile.py involved -- same "shell out to real build tools, don't
// link crates for it" philosophy as the rest of spk.

/// A recipe is the same key:value header as a .spkg control file, followed
/// by one or more `#--<section>--` marked shell script blocks. Header lines
/// are parsed with `parse_control`; script content is never scanned for
/// `key:` pairs, so a build command containing a literal colon can't be
/// misread as metadata.
fn parse_recipe(text: &str) -> (ControlMeta, HashMap<String, String>) {
    let mut header = String::new();
    let mut sections: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#--") && trimmed.ends_with("--") && trimmed.len() > 5 {
            let name = trimmed[3..trimmed.len() - 2].trim().to_lowercase();
            current = Some(name.clone());
            sections.entry(name).or_default();
            continue;
        }
        match &current {
            Some(name) => {
                let buf = sections.get_mut(name).unwrap();
                buf.push_str(line);
                buf.push('\n');
            }
            None => {
                header.push_str(line);
                header.push('\n');
            }
        }
    }
    (parse_control(&header), sections)
}

fn write_script(dir: &str, name: &str, content: &str) -> String {
    let path = format!("{dir}/{name}");
    let _ = fs::write(&path, content);
    let _ = Command::new("chmod").args(["+x", &path]).status();
    path
}

fn cmd_compile_package(recipe_path: &str, out_override: Option<&str>) {
    let text = match fs::read_to_string(recipe_path) {
        Ok(t) => t,
        Err(e) => {
            println!("{RED}[spk] Failed to read recipe {recipe_path}: {e}{R}");
            exit(1);
        }
    };
    let (mut meta, sections) = parse_recipe(&text);
    if meta.name.is_empty() || meta.version.is_empty() {
        println!("{RED}[spk] Recipe is missing 'name' or 'version' in its header.{R}");
        exit(1);
    }
    if meta.architecture.is_empty() {
        meta.architecture = "x86_64".to_string();
    }
    let build_script = sections.get("build").map(|s| s.trim()).unwrap_or("");
    let install_script = sections.get("install").map(|s| s.trim()).unwrap_or("");
    if build_script.is_empty() || install_script.is_empty() {
        println!(
            "{RED}[spk] Recipe needs both a #--build-- and a #--install-- section.{R}"
        );
        exit(1);
    }

    static BUILD_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let build_id = BUILD_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let build_root = format!(
        "/tmp/spk-compile-{}-{}-{}",
        meta.name,
        std::process::id(),
        build_id
    );
    let control_dir = format!("{build_root}/control");
    let stage_dir = format!("{build_root}/stage");
    let _ = fs::remove_dir_all(&build_root);
    if fs::create_dir_all(&control_dir).is_err() || fs::create_dir_all(&stage_dir).is_err() {
        println!("{RED}[spk] Failed to create build dir {build_root}.{R}");
        exit(1);
    }

    println!(
        "{CYAN}[spk] Building {} {}...{R}",
        meta.name, meta.version
    );
    let build_sh = write_script(&build_root, "build.sh", build_script);
    let build_ok = Command::new("sh")
        .arg(&build_sh)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !build_ok {
        println!("{RED}[spk] Build step failed for {}.{R}", meta.name);
        println!("  (left for inspection: {build_root})");
        exit(1);
    }

    println!("{CYAN}[spk] Staging install into {stage_dir}...{R}");
    let install_sh = write_script(&build_root, "install.sh", install_script);
    let install_ok = Command::new("sh")
        .arg(&install_sh)
        .env("SPK_STAGE", &stage_dir)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !install_ok {
        println!("{RED}[spk] Install step failed for {}.{R}", meta.name);
        println!("  (left for inspection: {build_root})");
        exit(1);
    }
    let staged_anything = fs::read_dir(&stage_dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if !staged_anything {
        println!(
            "{RED}[spk] Install step produced no files under $SPK_STAGE ({stage_dir}).{R}"
        );
        println!("  Nothing to package -- check the #--install-- section.");
        exit(1);
    }

    let control_text = format!(
        "name: {}\nversion: {}\narchitecture: {}\ndepends: {}\ndescription: {}\n",
        meta.name, meta.version, meta.architecture, meta.depends, meta.description
    );
    let _ = fs::write(format!("{control_dir}/control"), control_text);
    for hook in ["preinst", "postinst", "prerm", "postrm"] {
        if let Some(script) = sections.get(hook) {
            if !script.trim().is_empty() {
                write_script(&control_dir, hook, script);
            }
        }
    }

    let control_tar = format!("{build_root}/control.tar.xz");
    let data_tar = format!("{build_root}/data.tar.xz");
    let control_tar_ok = Command::new("tar")
        .args(["-cJf", &control_tar, "-C", &control_dir, "."])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let data_tar_ok = Command::new("tar")
        .args(["-cJf", &data_tar, "-C", &stage_dir, "."])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !control_tar_ok || !data_tar_ok {
        println!("{RED}[spk] Failed to build control.tar.xz/data.tar.xz for {}.{R}", meta.name);
        exit(1);
    }

    let out_path = out_override
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}-{}.spkg", meta.name, meta.version));
    let outer_ok = Command::new("tar")
        .args([
            "-cf",
            &out_path,
            "-C",
            &build_root,
            "control.tar.xz",
            "data.tar.xz",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = fs::remove_dir_all(&build_root);
    if !outer_ok {
        println!("{RED}[spk] Failed to assemble {out_path}.{R}");
        exit(1);
    }
    println!("{GREEN}[spk] Built {out_path}{R}");
}

// ── PackageKit backend (script protocol) ─────────────────────────────────────
//
// PackageKit spawns this as `spk packagekit-backend` and communicates via
// stdin/stdout using a line-based protocol. Plasma Discover and any other
// PackageKit frontend can then use SPK to install/list packages.
//
// Response format per line:
//   package\t<status>\t<id>\t<summary>     (id = name;version;arch;repo)
//   progress\t<percent>
//   status\t<status_string>
//   error\t<errorcode>\t<message>
//   finished
//
fn cmd_packagekit_backend() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    macro_rules! pk_write {
        ($($arg:tt)*) => {
            let _ = writeln!(stdout, $($arg)*);
            let _ = stdout.flush();
        };
    }

    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let parts: Vec<&str> = line.trim().splitn(5, '\t').collect();
        if parts.is_empty() {
            continue;
        }

        let active_pkgs: Vec<&str> = SMECHOS_PACKAGES.to_vec();
        let repo_id = "smechos";

        match parts[0] {
            "get-packages" => {
                pk_write!("status\tquery");
                for pkg in &active_pkgs {
                    pk_write!("package\tavailable\t{pkg};2.0.0;x86_64;{repo_id}\tSmech Labs package: {pkg}");
                }
                pk_write!("finished");
            }
            "resolve" => {
                let pkg = if parts.len() > 2 { parts[2] } else { "" };
                pk_write!("status\tquery");
                if active_pkgs.contains(&pkg) {
                    pk_write!("package\tavailable\t{pkg};2.0.0;x86_64;{repo_id}\tSmech Labs package: {pkg}");
                } else {
                    pk_write!("error\tpackage-not-found\tPackage '{pkg}' not found in SPK repos");
                }
                pk_write!("finished");
            }
            "install-packages" => {
                let pkg_id = if parts.len() > 2 { parts[2] } else { "" };
                let pkg_name = pkg_id.split(';').next().unwrap_or(pkg_id);
                pk_write!("status\tinstall");
                pk_write!("package\tinstalling\t{pkg_id}\tInstalling {pkg_name}...");
                pk_write!("progress\t10");
                if fetch_and_install(pkg_name, target_root()) {
                    pk_write!("progress\t100");
                    pk_write!("package\tinstalled\t{pkg_id}\t{pkg_name} installed");
                    pk_write!("finished");
                } else {
                    pk_write!("error\tpackage-install-failed\tFailed to install {pkg_name}");
                    pk_write!("finished");
                }
            }
            "remove-packages" => {
                pk_write!("error\tnot-supported\tSPK does not support package removal in this version");
                pk_write!("finished");
            }
            "update-packages" | "get-updates" => {
                pk_write!("status\tquery");
                for pkg in &active_pkgs {
                    pk_write!("package\tavailable\t{pkg};2.0.0;x86_64;{repo_id}\tUpdate available: {pkg}");
                }
                pk_write!("finished");
            }
            "refresh-cache" => {
                pk_write!("status\trefresh-cache");
                pk_write!("finished");
            }
            "get-repo-list" => {
                pk_write!("status\tquery");
                pk_write!("repo-detail\t{repo_id}\tSmech Labs Package Repository\ttrue");
                pk_write!("finished");
            }
            "quit" | "" => break,
            other => {
                pk_write!("error\tnot-supported\tUnknown command: {other}");
                pk_write!("finished");
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_help();
        exit(0);
    }

    match args[1].as_str() {
        "help" | "--help" | "-h" => print_help(),

        "version" | "--version" | "-v" => println!("spk {VERSION}"),

        "about" => print_about(),

        "install" => {
            let rest = &args[2..];
            // --local-package <path>: the ONLY sanctioned way to install a
            // local .spkg file. No warning here on success -- the flag
            // itself already is the explicit "yes, I mean a local file"
            // signal, so there's nothing left to confirm.
            if let Some(pos) = rest.iter().position(|a| a == "--local-package") {
                match rest.get(pos + 1) {
                    Some(path) => cmd_install_local(path),
                    None => {
                        println!(
                            "{RED}[spk] Error: --local-package requires a path.   \
                             spk install --local-package /path/to/pkg.spkg{R}"
                        );
                        exit(1);
                    }
                }
                return;
            }

            let names = names_in(rest);
            if names.is_empty() {
                println!("{RED}[spk] Error: specify a package.   spk install <pkg>...{R}");
                exit(1);
            }
            // A bare argument that looks like a filesystem path is a
            // mistake worth catching explicitly, not silently guessing at
            // -- "spk install <name>" only ever means "fetch from a
            // configured repo".
            for pkg in &names {
                if looks_like_local_path(pkg) {
                    println!("{RED}[spk] '{pkg}' looks like a local file, not a repo package name.{R}");
                    println!("  'spk install <name>' only fetches from configured repos.");
                    println!("  To install a local .spkg file, use:");
                    println!("    {BOLD}spk install --local-package {pkg}{R}");
                    exit(1);
                }
            }
            let dry = has_flag(rest, "--dry-run", "-n");
            for pkg in &names {
                if dry {
                    cmd_solve(pkg, false);
                } else {
                    cmd_install(pkg);
                }
            }
        }

        "local-package-repo" => {
            if args.len() < 3 {
                println!(
                    "{RED}[spk] Error: specify a folder.   spk local-package-repo <folder>{R}"
                );
                exit(1);
            }
            cmd_local_package_repo(&args[2]);
        }

        "remove" | "uninstall" | "purge" => {
            let rest = &args[2..];
            let force = rest.iter().any(|a| a == "--force");
            let yes = rest.iter().any(|a| a == "-y" || a == "--yes");
            match rest.iter().find(|a| !a.starts_with('-')) {
                Some(name) => cmd_remove(name, force, yes),
                None => {
                    println!("{RED}[spk] Error: specify a package.   spk remove <pkg>{R}");
                    exit(1);
                }
            }
        }

        "list" => cmd_list(&args[2..]),

        "update" | "refresh" => cmd_update(),
        "search" => cmd_search(&args[2..]),
        "info" | "show" => match names_in(&args[2..]).first() {
            Some(n) => cmd_info(n),
            None => {
                println!("{RED}[spk] Error: specify a package.   spk info <pkg>{R}");
                exit(1);
            }
        },
        "files" => match args.get(2) {
            Some(n) => cmd_files(n),
            None => {
                println!("{RED}[spk] Error: specify a package.   spk files <pkg>{R}");
                exit(1);
            }
        },
        "owns" | "which" => match args.get(2) {
            Some(path) => cmd_owns(path),
            None => {
                println!("{RED}[spk] Error: specify a path.   spk owns /usr/bin/foo{R}");
                exit(1);
            }
        },
        "provides" => match args.get(2) {
            Some(n) => cmd_provides(n),
            None => {
                println!("{RED}[spk] Error: specify a name.   spk provides <name>{R}");
                exit(1);
            }
        },
        "upgrade" | "full-upgrade" | "dist-upgrade" => cmd_upgrade(&args[2..]),
        "reinstall" => cmd_reinstall(&names_in(&args[2..])),
        "autoremove" => cmd_autoremove(&args[2..]),
        "mark-manual" | "mark-auto" | "hold" | "unhold" => cmd_mark(args[1].as_str(), &names_in(&args[2..])),
        "verify" => cmd_verify(&args[2..]),
        "check" => cmd_check(),
        "history" => cmd_history(),
        "download" => cmd_download(&names_in(&args[2..])),
        "clean" | "autoclean" => cmd_clean(),
        "repo" => cmd_repo(&args[2..]),

        "depends" => {
            if args.len() < 3 {
                println!("{RED}[spk] Error: specify a package.   spk depends <pkg>{R}");
                exit(1);
            }
            cmd_depends(&args[2]);
        }

        "repo-status" => cmd_repo_status(),

        "solve" => {
            let porcelain = args.iter().skip(2).any(|a| a == "--porcelain");
            let reqs: Vec<&String> = args.iter().skip(2).filter(|a| *a != "--porcelain").collect();
            if reqs.len() != 1 {
                println!("{RED}[spk] Error: specify one package.   spk solve [--porcelain] <pkg>{R}");
                exit(1);
            }
            cmd_solve(reqs[0], porcelain);
        }

        "create-live-image" => {
            if args.len() < 3 {
                println!(
                    "{RED}[spk] Error: specify an output path.   \
                     spk create-live-image <output.iso> [extra-package-name...]{R}"
                );
                exit(1);
            }
            cmd_create_live_image(&args[2], &args[3..]);
        }

        "system-upgrade" => cmd_system_upgrade(),

        "compile" => {
            let rest = &args[2..];
            if rest.is_empty() {
                println!(
                    "{RED}[spk] Error: specify a recipe.   spk compile <recipe> [-o out.spkg]{R}"
                );
                exit(1);
            }
            let out = rest
                .iter()
                .position(|a| a == "-o" || a == "--output")
                .and_then(|i| rest.get(i + 1))
                .map(|s| s.as_str());
            let recipe = match rest.iter().find(|a| !a.starts_with('-')) {
                Some(r) => r,
                None => {
                    println!(
                        "{RED}[spk] Error: specify a recipe.   spk compile <recipe> [-o out.spkg]{R}"
                    );
                    exit(1);
                }
            };
            cmd_compile_package(recipe, out);
        }

        "build-image" => {
            // Forward everything after "build-image" to spk-compile.py --
            // this is the whole-image, from-source build orchestrator, not
            // a single package. See `compile` above for that.
            cmd_build_image(&args[2..].to_vec());
        }

        "packagekit-backend" => cmd_packagekit_backend(),

        // ── Legacy aliases (v1.x compat) ─────────────────────────────────────
        "system-install" | "userland-install" => {
            println!("{YELLOW}[spk] '{} <pkg>' is now 'spk install <pkg>'{R}", args[1]);
            if args.len() < 3 {
                println!("{RED}[spk] Error: specify a package.{R}");
                exit(1);
            }
            cmd_install(&args[2]);
        }

        "entire-system-upgrade" => {
            println!("{YELLOW}[spk] 'entire-system-upgrade' is now 'spk system-upgrade'{R}");
            cmd_system_upgrade();
        }

        unknown => {
            println!("{RED}[spk] Unknown command: '{unknown}'{R}");
            println!("  Use '{BOLD}spk help{R}' to see valid commands.");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_message_matches_python_converter_format() {
        let msg = signing_message_for("hello~nix~", "2.12.3", "x86_64", "abc123");
        assert_eq!(
            msg,
            b"spk-sig-v1\nname=hello~nix~\nversion=2.12.3\narchitecture=x86_64\nsha256=abc123\n"
        );
    }

    #[test]
    fn base64_decode_round_trips_known_vectors() {
        // "hello" -> "aGVsbG8=" is a standard, widely-known vector.
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        // Real-shaped Ed25519 raw signature is 64 bytes -- decode one of
        // our own real public keys (base64 body only) and check the length
        // comes out byte-exact rather than off-by-one from padding math.
        let decoded = base64_decode("/B2iaPNEITKzbMvMd5PXD9Tjxv3dXLd5NaNvIvnNIHE=").unwrap();
        assert_eq!(decoded.len(), 32);
    }

    #[test]
    fn verify_signature_rejects_tampered_message() {
        // Without a real matching private key in the test environment, this
        // just confirms the plumbing fails closed on garbage input rather
        // than panicking or (worse) succeeding.
        assert!(!verify_signature(NIX2SPK_PUBKEY, b"not signed", "not-base64-sig"));
    }

    #[test]
    fn parses_multiple_repos_with_priority_order() {
        let yaml = r#"
repos:
  - name: primary
    url: https://pkg.smech.xyz
    priority: 1
  - name: github-fallback
    url: https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages
    priority: 50
  - name: sketchy-mirror
    url: http://example.com/mirror
    priority: 100
"#;
        let mut repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 3);
        repos.sort_by_key(|r| r.priority);
        assert_eq!(repos[0].name, "primary");
        assert_eq!(repos[0].priority, 1);
        assert_eq!(repos[1].name, "github-fallback");
        assert_eq!(repos[1].priority, 50);
        assert_eq!(repos[2].name, "sketchy-mirror");
        assert_eq!(repos[2].priority, 100);
    }

    #[test]
    fn defaults_missing_priority_to_50() {
        let yaml = r#"
repos:
  - name: no-priority
    url: https://example.com
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].priority, 50);
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let yaml = r#"
# top comment
repos:
  # a comment inside the list too
  - name: one

    url: https://example.com/one
    priority: 5
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "one");
        assert_eq!(repos[0].url, "https://example.com/one");
        assert_eq!(repos[0].priority, 5);
    }

    #[test]
    fn handles_quoted_values() {
        let yaml = r#"
repos:
  - name: "quoted"
    url: 'https://example.com/single'
    priority: 3
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos[0].name, "quoted");
        assert_eq!(repos[0].url, "https://example.com/single");
    }

    #[test]
    fn empty_input_yields_no_repos() {
        assert!(parse_repo_conf("").is_empty());
        assert!(parse_repo_conf("some: other\nyaml: entirely").is_empty());
    }

    #[test]
    fn single_repo_no_list_wrapper_needed() {
        // The user explicitly said "just a single repo" should be fine too --
        // confirm one-entry lists parse identically to multi-entry ones.
        let yaml = r#"
repos:
  - name: only
    url: https://pkg.smech.xyz
    priority: 1
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].url, "https://pkg.smech.xyz");
    }

    #[test]
    fn parses_flow_style_yaml() {
        // The old hand-rolled parser only ever understood block-style `-
        // name: ...` list items; real YAML also allows this compact
        // flow-style form. Proving this works is the actual point of
        // moving to yaml-rust2 instead of keeping the subset parser.
        let yaml = r#"repos: [{name: flow, url: "https://example.com/flow", priority: 7}]"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "flow");
        assert_eq!(repos[0].url, "https://example.com/flow");
        assert_eq!(repos[0].priority, 7);
    }

    #[test]
    fn parses_control_file() {
        let text = "name: kcoreaddons\nversion: 6.24.0\narchitecture: x86_64\ndepends: qt6-base\ndescription: KDE Frameworks - KCoreAddons\n";
        let m = parse_control(text);
        assert_eq!(m.name, "kcoreaddons");
        assert_eq!(m.version, "6.24.0");
        assert_eq!(m.architecture, "x86_64");
        assert_eq!(m.depends, "qt6-base");
        assert_eq!(m.description, "KDE Frameworks - KCoreAddons");
    }

    #[test]
    fn control_file_ignores_comments() {
        let text = "# a comment\nname: foo\n# depends: ignored\nversion: 1.0\n";
        let m = parse_control(text);
        assert_eq!(m.name, "foo");
        assert_eq!(m.version, "1.0");
        assert!(m.depends.is_empty());
    }

    #[test]
    fn detects_local_paths() {
        assert!(looks_like_local_path("./foo.spkg"));
        assert!(looks_like_local_path("/home/smech/foo.spkg"));
        assert!(looks_like_local_path("~/foo.spkg"));
        assert!(looks_like_local_path("foo.spkg"));
        assert!(looks_like_local_path("foo.tar.xz"));
        assert!(!looks_like_local_path("firefox"));
        assert!(!looks_like_local_path("kde-frameworks"));
    }

    #[test]
    fn detects_origin_suffixes() {
        assert!(is_nix_package("hello~nix~"));
        assert!(!is_nix_package("hello~apt~"));
        assert!(!is_nix_package("hello"));

        assert!(is_apt_package("hello~apt~"));
        assert!(!is_apt_package("hello~nix~"));
        assert!(!is_apt_package("hello"));
    }

    // ── Repository trust ──────────────────────────────────────────────────────
    mod trust_tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);

        fn tmp(tag: &str) -> std::path::PathBuf {
            let d = std::env::temp_dir().join(format!(
                "spk-trust-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            d
        }

        fn run(args: &[&str]) {
            let st = Command::new("openssl").args(args).status().unwrap();
            assert!(st.success(), "openssl {args:?}");
        }

        /// (private key path, public PEM)
        fn keypair(dir: &Path) -> (String, String) {
            let k = dir.join("k.pem");
            let kp = dir.join("k.pub");
            run(&["genpkey", "-algorithm", "ed25519", "-out", k.to_str().unwrap()]);
            run(&["pkey", "-in", k.to_str().unwrap(), "-pubout", "-out", kp.to_str().unwrap()]);
            (k.to_str().unwrap().to_string(), fs::read_to_string(kp).unwrap())
        }

        fn sign(dir: &Path, key: &str, meta: &[u8]) -> String {
            let hash = sha256_hex_of_bytes(meta).unwrap();
            let msg = dir.join("m");
            let sigf = dir.join("s");
            fs::write(&msg, index_signing_message("index.meta", &hash)).unwrap();
            run(&["pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", msg.to_str().unwrap(), "-out", sigf.to_str().unwrap()]);
            let out = Command::new("openssl").args(["base64", "-A", "-in", sigf.to_str().unwrap()]).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        }

        fn repo(name: &str, max_age: u64) -> Repo {
            let mut r = Repo::unsigned(name, "https://example.org/repo", 1);
            r.max_index_age_days = max_age;
            r
        }

        fn meta_at(generated: u64) -> String {
            format!(
                "format: 1\ngenerated: {generated}\n\nname: a\nversion: 1.0-9\nsha256: {}\n\n\
                 name: a\nversion: 1.0-10\nsha256: {}\n\nname: b\nversion: 2\nsha256: {}\n",
                "11".repeat(32), "22".repeat(32), "33".repeat(32)
            )
        }

        const NOW: u64 = 1_800_000_000;

        #[test]
        fn accepts_a_valid_index_and_pins_by_version() {
            let d = tmp("ok");
            let (key, pem) = keypair(&d);
            let meta = meta_at(NOW - 3600);
            let sig = sign(&d, &key, meta.as_bytes());
            let vi = check_signed_index(&repo("r", 14), &pem, meta.as_bytes(), &sig, NOW, &d).unwrap();
            assert!(vi.signed);
            assert_eq!(vi.generated, Some(NOW - 3600));
            assert_eq!(vi.pins[&("a".to_string(), "1.0-10".to_string())], "22".repeat(32));
            // dpkg ordering, not string ordering: 1.0-10 is newer than 1.0-9
            assert_eq!(vi.latest["a"], "1.0-10");
            assert_eq!(vi.latest["b"], "2");
        }

        #[test]
        fn rejects_a_tampered_index() {
            let d = tmp("tamper");
            let (key, pem) = keypair(&d);
            let meta = meta_at(NOW);
            let sig = sign(&d, &key, meta.as_bytes());
            let evil = meta.replace("version: 2", "version: 3");
            match check_signed_index(&repo("r", 14), &pem, evil.as_bytes(), &sig, NOW, &d) {
                Err(IndexError::Untrusted(m)) => assert!(m.contains("signature is invalid"), "{m}"),
                other => panic!("expected rejection, got {other:?}"),
            }
        }

        #[test]
        fn rejects_wrong_key_and_garbage_signatures() {
            let d = tmp("key");
            let (key, _pem) = keypair(&d);
            let d2 = tmp("key2");
            let (_k2, other_pem) = keypair(&d2);
            let meta = meta_at(NOW);
            let sig = sign(&d, &key, meta.as_bytes());
            for (pem, s) in [(&other_pem, sig.as_str()), (&other_pem, "not base64 !!"), (&other_pem, "")] {
                assert!(matches!(
                    check_signed_index(&repo("r", 14), pem, meta.as_bytes(), s, NOW, &d),
                    Err(IndexError::Untrusted(_))
                ));
            }
        }

        #[test]
        fn rejects_stale_and_future_indexes() {
            let d = tmp("age");
            let (key, pem) = keypair(&d);
            let stale = meta_at(NOW - 15 * 86_400);
            let sig = sign(&d, &key, stale.as_bytes());
            match check_signed_index(&repo("r", 14), &pem, stale.as_bytes(), &sig, NOW, &d) {
                Err(IndexError::Untrusted(m)) => assert!(m.contains("stale"), "{m}"),
                other => panic!("{other:?}"),
            }
            // the same index is fine under a longer limit
            assert!(check_signed_index(&repo("r2", 30), &pem, stale.as_bytes(), &sig, NOW, &tmp("age2")).is_ok());
            let future = meta_at(NOW + 3 * 86_400);
            let sig = sign(&d, &key, future.as_bytes());
            match check_signed_index(&repo("r3", 14), &pem, future.as_bytes(), &sig, NOW, &tmp("age3")) {
                Err(IndexError::Untrusted(m)) => assert!(m.contains("future"), "{m}"),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn rejects_rollback_to_an_older_signed_index() {
            let d = tmp("rb");
            let state = tmp("rb-state");
            let (key, pem) = keypair(&d);
            let newer = meta_at(NOW - 100);
            let older = meta_at(NOW - 5_000);
            let sig_new = sign(&d, &key, newer.as_bytes());
            let sig_old = sign(&d, &key, older.as_bytes());
            let r = repo("r", 14);
            check_signed_index(&r, &pem, newer.as_bytes(), &sig_new, NOW, &state).unwrap();
            match check_signed_index(&r, &pem, older.as_bytes(), &sig_old, NOW, &state) {
                Err(IndexError::Untrusted(m)) => assert!(m.contains("rollback"), "{m}"),
                other => panic!("{other:?}"),
            }
            // the same generation again and a newer one are fine
            check_signed_index(&r, &pem, newer.as_bytes(), &sig_new, NOW, &state).unwrap();
            let newest = meta_at(NOW - 10);
            let sig_newest = sign(&d, &key, newest.as_bytes());
            check_signed_index(&r, &pem, newest.as_bytes(), &sig_newest, NOW, &state).unwrap();
        }

        #[test]
        fn a_signed_index_without_a_timestamp_is_refused() {
            let d = tmp("nots");
            let (key, pem) = keypair(&d);
            let meta = "name: a\nversion: 1\nsha256: aa\n".to_string();
            let sig = sign(&d, &key, meta.as_bytes());
            match check_signed_index(&repo("r", 14), &pem, meta.as_bytes(), &sig, NOW, &d) {
                Err(IndexError::Untrusted(m)) => assert!(m.contains("generated"), "{m}"),
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn python_signed_fixture_verifies_in_rust() {
            // Produced by smechbuild's signing.py: the two implementations must agree on the message format.
            let pem = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAcmhlA+WYL/5PMn3BguGV9NrySMxkxoPWD5jaKAiG2bI=\n-----END PUBLIC KEY-----\n";
            let meta = "format: 1\ngenerated: 1792935770\n\nname: libx\nversion: 1.0-1\nsha256: abababababababababababababababababababababababababababababababab\n\nname: libx\nversion: 1.0-10\nsha256: cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd\n";
            let sig = "TssiUEjdS+J7RxRD4+9/5at30bS1CN9huD/5eC9sbOht7rcCnt/PNyO5bK15Pm60kP00IGJzfDAgbzle3JtBDg==";
            let d = tmp("xlang");
            let vi = check_signed_index(&repo("r", 14), pem, meta.as_bytes(), sig, 1_792_935_770 + 60, &d).unwrap();
            assert_eq!(vi.latest["libx"], "1.0-10");
            assert_eq!(vi.pins[&("libx".to_string(), "1.0-1".to_string())], "ab".repeat(32));
            let tampered = meta.replace("1.0-10", "1.0-11");
            assert!(check_signed_index(&repo("r", 14), pem, tampered.as_bytes(), sig, 1_792_935_770 + 60, &tmp("xlang2")).is_err());
        }

        #[test]
        fn trust_policy() {
            let mut r = Repo::unsigned("r", "https://example.org/x", 1);
            assert!(matches!(trust_of(&r), Ok(Trust::Unsigned)));
            r.require_signed = true;
            assert!(matches!(trust_of(&r), Err(TrustError::Disallowed(_))));
            r.url = "file:///srv/local".to_string();
            assert!(matches!(trust_of(&r), Ok(Trust::Unsigned)), "file:// repos are the operator's own");
            r.key = Ok(Some("PEM".into()));
            assert!(matches!(trust_of(&r), Ok(Trust::Signed(_))));
            r.key = Err("cannot read key".into());
            assert!(matches!(trust_of(&r), Err(TrustError::Broken(_))), "a broken key must not degrade to unsigned");
        }

        #[test]
        fn config_parses_signing_settings_and_round_trips() {
            let d = tmp("conf");
            let (_k, pem) = keypair(&d);
            let keyfile = d.join("archive.pub");
            fs::write(&keyfile, &pem).unwrap();
            let conf = format!(
                "require_signed: true\nrepos:\n  - name: smech\n    url: https://a.example/repo\n    priority: 1\n    key: {}\n    max_index_age_days: 3\n  - name: bad\n    url: https://b.example\n    key: /nonexistent/key.pub\n  - name: plain\n    url: https://c.example\n",
                keyfile.display()
            );
            let repos = parse_repo_conf(&conf);
            assert_eq!(repos.len(), 3);
            assert!(matches!(trust_of(&repos[0]), Ok(Trust::Signed(p)) if p == pem));
            assert_eq!(repos[0].max_index_age_days, 3);
            assert!(repos.iter().all(|r| r.require_signed), "top-level require_signed applies to every entry");
            assert!(matches!(trust_of(&repos[1]), Err(TrustError::Broken(_))));
            assert!(matches!(trust_of(&repos[2]), Err(TrustError::Disallowed(_))));
            // rewriting the config keeps key, require_signed and the age limit
            let again = parse_repo_conf(&repos_to_yaml(&repos));
            assert_eq!(again[0].key_path.as_deref(), Some(keyfile.to_str().unwrap()));
            assert_eq!(again[0].max_index_age_days, 3);
            assert!(again[2].require_signed);
        }

        #[test]
        fn non_pem_key_file_is_rejected() {
            let d = tmp("badkey");
            let f = d.join("k.pub");
            fs::write(&f, "ssh-ed25519 AAAA... not a pem").unwrap();
            let repos = parse_repo_conf(&format!("repos:\n  - name: r\n    url: https://x\n    key: {}\n", f.display()));
            assert!(matches!(trust_of(&repos[0]), Err(TrustError::Broken(_))));
        }
    }

}
