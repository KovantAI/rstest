//! Import-graph selection: build a reverse-import index of the project's
//! `.py` files, then BFS from the changed files to the test files that reach
//! them. Conservative by construction (over-selection is always safe).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{rule1_full_run, Selection};
use crate::config::ProjectConfig;

/// Map changed files to the affected test files. `strict`: any changed source
/// file whose reverse import reach contains NO test file falls back to a full run
/// instead of silently selecting nothing (dynamic-import target, unused module).
pub fn affected_tests(
    rootdir: &Path,
    project: &ProjectConfig,
    changed: &[PathBuf],
    strict: bool,
) -> Result<Selection> {
    // Rule 1: anything that isn't a Python file defeats the graph.
    if let Some(full) = rule1_full_run(changed) {
        return Ok(full);
    }
    let index = ProjectIndex::build(rootdir)?;
    select_from_index(rootdir, project, changed, strict, &index)
}

/// [`affected_tests`], but reusing `cache` for the per-file import scan so a
/// repeated call (a `--watch` reselection) re-reads only the files whose mtime
/// changed. The result is identical to a fresh build; only the cost differs.
pub fn affected_tests_cached(
    rootdir: &Path,
    project: &ProjectConfig,
    changed: &[PathBuf],
    strict: bool,
    cache: &mut CollectionCache,
) -> Result<Selection> {
    if let Some(full) = rule1_full_run(changed) {
        return Ok(full);
    }
    let index = cache.index(rootdir, changed);
    select_from_index(rootdir, project, changed, strict, &index)
}

/// The reverse-BFS selection over a built index: changed files -> the test files
/// that reach them. Split out so the fresh and cached index paths share it.
fn select_from_index(
    rootdir: &Path,
    project: &ProjectConfig,
    changed: &[PathBuf],
    strict: bool,
    index: &ProjectIndex,
) -> Result<Selection> {
    let mut affected: HashSet<PathBuf> = HashSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for c in changed {
        let abs = rootdir.join(c);
        let canon = abs.canonicalize().unwrap_or(abs);
        // Rule 2: conftest.py affects every test below its directory.
        if canon.file_name().and_then(|n| n.to_str()) == Some("conftest.py") {
            if let Some(dir) = canon.parent() {
                for f in &index.files {
                    if f.starts_with(dir) {
                        affected.insert(f.clone());
                    }
                }
            }
            continue;
        }
        queue.push_back(canon);
    }

    // Strict: every changed SOURCE file must provably reach a test.
    // (Tests select themselves; conftest covers its subtree by rule 2.)
    if strict {
        for file in &queue {
            if crate::collect::is_test_file(file, project) {
                continue;
            }
            let mut reach: VecDeque<&PathBuf> = VecDeque::from([file]);
            let mut seen: HashSet<&PathBuf> = HashSet::from([file]);
            let mut covered = false;
            while let Some(f) = reach.pop_front() {
                if crate::collect::is_test_file(f, project) {
                    covered = true;
                    break;
                }
                if let Some(importers) = index.reverse.get(f) {
                    for imp in importers {
                        if seen.insert(imp) {
                            reach.push_back(imp);
                        }
                    }
                }
            }
            if !covered {
                return Ok(Selection::FullRun(format!(
                    "--changed-strict: {} reaches no tests via the import \
                     graph (dynamic import target, unused module, or deleted \
                     file) — running everything instead of risking a false skip",
                    file.strip_prefix(rootdir).unwrap_or(file).display()
                )));
            }
        }
    }

    // Reverse BFS over the import graph.
    let mut seen: HashSet<PathBuf> = queue.iter().cloned().collect();
    while let Some(file) = queue.pop_front() {
        affected.insert(file.clone());
        if let Some(importers) = index.reverse.get(&file) {
            for imp in importers {
                if seen.insert(imp.clone()) {
                    queue.push_back(imp.clone());
                }
            }
        }
    }

    let tests: Vec<PathBuf> = affected
        .into_iter()
        // is_test_file is a name-pattern match, so a DELETED test file still
        // matches; drop anything no longer on disk so pytest is never handed a
        // missing path (importers of a deleted source still exist and remain).
        .filter(|f| crate::collect::is_test_file(f, project) && f.exists())
        .collect();
    let mut tests: Vec<PathBuf> = tests
        .into_iter()
        .map(|f| f.strip_prefix(rootdir).map(PathBuf::from).unwrap_or(f))
        .collect();
    tests.sort();
    Ok(Selection::Tests(tests))
}

struct ProjectIndex {
    files: Vec<PathBuf>,
    /// imported file -> files importing it
    reverse: HashMap<PathBuf, Vec<PathBuf>>,
}

/// All project `.py` files (canonical), pruning the same dirs as the test-file
/// walker. This is the whole-tree traversal a `--watch` reselection repeats; the
/// incremental cache below avoids re-reading their contents.
fn walk_py_files(rootdir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(rootdir)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(|e| {
            let n = e.file_name().to_str().unwrap_or("");
            n != "__pycache__" && n != ".git" && !e.path().join("pyvenv.cfg").exists()
        })
        .build();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|t| t.is_file())
            && entry.path().extension().and_then(|e| e.to_str()) == Some("py")
        {
            files.push(
                entry
                    .path()
                    .canonicalize()
                    .unwrap_or_else(|_| entry.into_path()),
            );
        }
    }
    files
}

/// The dotted module name of `file` relative to `rootdir` (canonical), e.g.
/// `pkg/sub/mod.py` -> `pkg.sub.mod`, `pkg/__init__.py` -> `pkg`.
fn dotted_of(rootdir_canon: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(rootdir_canon).unwrap_or(file);
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = parts.last_mut() {
        *last = last.trim_end_matches(".py").to_string();
    }
    if parts.last().map(String::as_str) == Some("__init__") {
        parts.pop();
    }
    parts.join(".")
}

/// Resolves an imported module name to the project files it could refer to.
/// Lookup is by suffix, so `src/` layouts and ambiguous short names resolve to
/// every candidate (over-selection is safe). Indexed by the module's LAST dotted
/// segment so a lookup scans only files sharing that leaf, not the whole project
/// — the old linear scan was O(files x imports) and dominated large reselections.
struct Resolver {
    by_leaf: HashMap<String, Vec<(String, PathBuf)>>,
}

impl Resolver {
    fn build<'a>(dotted: impl Iterator<Item = (&'a String, &'a PathBuf)>) -> Self {
        let mut by_leaf: HashMap<String, Vec<(String, PathBuf)>> = HashMap::new();
        for (d, f) in dotted {
            let leaf = d.rsplit('.').next().unwrap_or(d.as_str()).to_string();
            by_leaf
                .entry(leaf)
                .or_default()
                .push((d.clone(), f.clone()));
        }
        Self { by_leaf }
    }

    fn resolve(&self, module: &str) -> Vec<PathBuf> {
        let leaf = module.rsplit('.').next().unwrap_or(module);
        let suffix = format!(".{module}");
        self.by_leaf
            .get(leaf)
            .into_iter()
            .flatten()
            .filter(|(d, _)| d == module || d.ends_with(&suffix))
            .map(|(_, f)| f.clone())
            .collect()
    }
}

/// Read + scan `file`'s imports. `None` when unreadable (no edges, as before).
fn parse_imports(file: &Path, dotted: &str) -> Option<Vec<String>> {
    let src = std::fs::read_to_string(file).ok()?;
    Some(imports_of(&src, dotted))
}

impl ProjectIndex {
    /// Build the reverse-import index fresh (one-shot runs: `--changed`,
    /// `since-green`, non-watch selection). Reads and parses every project `.py`.
    fn build(rootdir: &Path) -> Result<Self> {
        let files = walk_py_files(rootdir);
        let rootdir = rootdir
            .canonicalize()
            .unwrap_or_else(|_| rootdir.to_path_buf());
        let dotted: Vec<(String, PathBuf)> = files
            .iter()
            .map(|f| (dotted_of(&rootdir, f), f.clone()))
            .collect();
        let resolver = Resolver::build(dotted.iter().map(|(d, f)| (d, f)));
        let mut reverse: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for (importer_dotted, f) in &dotted {
            let Some(modules) = parse_imports(f, importer_dotted) else {
                continue;
            };
            for module in modules {
                for target in resolver.resolve(&module) {
                    if &target != f {
                        reverse.entry(target).or_default().push(f.clone());
                    }
                }
            }
        }
        Ok(Self { files, reverse })
    }
}

/// One cached file: its mtime and the resolved target files it imports (the
/// forward edges). Keeping resolved targets lets an edit patch the reverse index
/// by removing the file's old out-edges and adding its new ones, no full rebuild.
#[derive(Clone, Default)]
struct CachedFile {
    mtime: Option<std::time::SystemTime>,
    out: Vec<PathBuf>,
}

/// A stateful import-graph index that survives across `--watch` reselections.
///
/// Cold or on any file-set change (a file added or deleted) it does a full
/// rebuild, reusing the cached parse of every file whose mtime is unchanged. On
/// the common case — a content edit to existing files — it re-reads only those
/// files and patches their forward/reverse edges in place, skipping the
/// whole-tree read entirely. Correctness is preserved because a file-set change
/// (which could create new import targets for unchanged importers) always forces
/// the full path; edits never change which files exist, only their edges.
#[derive(Default)]
pub struct CollectionCache {
    rootdir: Option<PathBuf>,
    /// canonical file -> (mtime, resolved out-edges)
    files: HashMap<PathBuf, CachedFile>,
    /// canonical file -> dotted module name
    dotted: HashMap<PathBuf, String>,
    /// imported file -> importers (the reverse graph, maintained incrementally)
    reverse: HashMap<PathBuf, HashSet<PathBuf>>,
}

impl CollectionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Produce the current [`ProjectIndex`], reusing cached work. `changed` is
    /// the watcher's change set (root-relative); when every changed path is an
    /// edit to a file already in the graph, the whole-tree walk is skipped and
    /// only those files are re-read. Any path the graph doesn't already know
    /// (a create) or that no longer exists (a delete) means the file set moved,
    /// so a full walk+rebuild runs to keep selection sound (a new import target
    /// could now be reached by an unchanged importer).
    fn index(&mut self, rootdir: &Path, changed: &[PathBuf]) -> ProjectIndex {
        let rootdir_canon = rootdir
            .canonicalize()
            .unwrap_or_else(|_| rootdir.to_path_buf());
        // A different root (or first call) invalidates everything.
        if self.rootdir.as_deref() != Some(rootdir_canon.as_path()) {
            self.rootdir = Some(rootdir_canon.clone());
            self.files.clear();
            self.dotted.clear();
            self.reverse.clear();
        }

        // Cold start: no cache yet, so a full walk+build is unavoidable.
        if self.files.is_empty() {
            let present = walk_py_files(rootdir);
            self.rebuild(&rootdir_canon, &present);
            return self.materialize();
        }

        // Classify the change set: an edit to a known file stays on the fast
        // path; a create (canonicalizes but unknown) or delete (no longer
        // canonicalizes) forces a full walk.
        let mut edits: Vec<PathBuf> = Vec::new();
        let mut structural = false;
        for c in changed {
            let abs = rootdir.join(c);
            match abs.canonicalize() {
                Ok(p) if self.files.contains_key(&p) => edits.push(p),
                _ => {
                    structural = true;
                    break;
                }
            }
        }

        if structural {
            // File set moved: rebuild the whole graph, reusing the cached parse
            // of every file whose mtime is unchanged (only new/edited files read).
            let present = walk_py_files(rootdir);
            self.rebuild(&rootdir_canon, &present);
        } else {
            // Edit-only cycle: re-read and re-edge just the changed files, no walk.
            for f in &edits {
                self.repatch_file(&rootdir_canon, f);
            }
        }
        self.materialize()
    }

    /// Full rebuild over `files`, reusing each unchanged file's cached out-edges.
    fn rebuild(&mut self, rootdir_canon: &Path, files: &[PathBuf]) {
        // Refresh dotted names for the current file set.
        self.dotted = files
            .iter()
            .map(|f| (f.clone(), dotted_of(rootdir_canon, f)))
            .collect();
        let resolver = Resolver::build(self.dotted.iter().map(|(f, d)| (d, f)));
        let mut fresh: HashMap<PathBuf, CachedFile> = HashMap::with_capacity(files.len());
        for f in files {
            let now = mtime_of(f);
            // Reuse the cached out-edges when the mtime is unchanged.
            if let Some(prev) = self.files.get(f) {
                if prev.mtime == now && now.is_some() {
                    fresh.insert(f.clone(), prev.clone());
                    continue;
                }
            }
            let dotted = self.dotted.get(f).cloned().unwrap_or_default();
            let out = parse_imports(f, &dotted)
                .unwrap_or_default()
                .iter()
                .flat_map(|m| resolver.resolve(m))
                .filter(|t| t != f)
                .collect();
            fresh.insert(f.clone(), CachedFile { mtime: now, out });
        }
        self.files = fresh;
        self.rebuild_reverse();
    }

    /// Re-read one file and patch its forward/reverse edges in place. Used on the
    /// edit-only fast path (file set unchanged), so no other file is touched.
    fn repatch_file(&mut self, rootdir_canon: &Path, file: &Path) {
        // Nothing to do if the mtime is unchanged (e.g. a touch, or a duplicate
        // event): the cached edges are still valid.
        if self.files.get(file).map(|c| c.mtime) == Some(mtime_of(file)) {
            return;
        }
        // Remove the file's old out-edges from the reverse graph.
        if let Some(prev) = self.files.get(file) {
            for target in &prev.out {
                if let Some(importers) = self.reverse.get_mut(target) {
                    importers.remove(file);
                }
            }
        }
        // Recompute its out-edges against the current file set.
        let dotted = self
            .dotted
            .entry(file.to_path_buf())
            .or_insert_with(|| dotted_of(rootdir_canon, file))
            .clone();
        let resolver = Resolver::build(self.dotted.iter().map(|(f, d)| (d, f)));
        let out: Vec<PathBuf> = parse_imports(file, &dotted)
            .unwrap_or_default()
            .iter()
            .flat_map(|m| resolver.resolve(m))
            .filter(|t| t != file)
            .collect();
        for target in &out {
            self.reverse
                .entry(target.clone())
                .or_default()
                .insert(file.to_path_buf());
        }
        self.files.insert(
            file.to_path_buf(),
            CachedFile {
                mtime: mtime_of(file),
                out,
            },
        );
    }

    /// Rebuild the reverse graph from every file's cached out-edges.
    fn rebuild_reverse(&mut self) {
        let mut reverse: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        for (f, cached) in &self.files {
            for target in &cached.out {
                reverse.entry(target.clone()).or_default().insert(f.clone());
            }
        }
        self.reverse = reverse;
    }

    /// Snapshot the cache as a plain [`ProjectIndex`] for the BFS.
    fn materialize(&self) -> ProjectIndex {
        let reverse = self
            .reverse
            .iter()
            .map(|(t, importers)| (t.clone(), importers.iter().cloned().collect()))
            .collect();
        ProjectIndex {
            files: self.files.keys().cloned().collect(),
            reverse,
        }
    }
}

/// A file's modified time, or `None` if its metadata is unreadable.
fn mtime_of(file: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(file).and_then(|m| m.modified()).ok()
}

/// Modules imported by `src`. Includes indented (function-local /
/// conditional) imports - extra edges only ever widen the selection.
pub(crate) fn imports_of(src: &str, importer_dotted: &str) -> Vec<String> {
    let mut modules = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("import ") {
            for part in rest.split(',') {
                let m = part.split_whitespace().next().unwrap_or("");
                if !m.is_empty() {
                    modules.push(m.to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("from ") {
            let Some((module_part, names)) = rest.split_once(" import ") else {
                continue;
            };
            let module_part = module_part.trim();
            let level = module_part.chars().take_while(|&c| c == '.').count();
            let named = &module_part[level..];
            let base = if level > 0 {
                // Relative: resolve against the importer's package.
                let mut pkg: Vec<&str> = importer_dotted.split('.').collect();
                for _ in 0..level {
                    pkg.pop();
                }
                let mut base = pkg.join(".");
                if !named.is_empty() {
                    if !base.is_empty() {
                        base.push('.');
                    }
                    base.push_str(named);
                }
                base
            } else {
                named.to_string()
            };
            if !base.is_empty() {
                modules.push(base.clone());
            }
            // `from pkg import x` may import the submodule pkg.x: add a
            // candidate per imported name (misses just don't resolve).
            for name in names.trim_start_matches('(').split(',') {
                let n = name
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(')');
                if !n.is_empty() && n != "*" && !base.is_empty() {
                    modules.push(format!("{base}.{n}"));
                }
            }
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::{affected_tests, affected_tests_cached, imports_of, CollectionCache, Selection};
    use crate::config::ProjectConfig;
    use std::path::{Path, PathBuf};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rstest-graph-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // Canonicalize: the walker canonicalizes indexed files, so the rootdir
        // must too or strip_prefix can't make results rootdir-relative.
        d.canonicalize().unwrap()
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn plain_and_comma_imports() {
        let mods = imports_of("import os, mypkg.utils\nimport json as j\n", "tests.test_x");
        assert!(mods.contains(&"os".to_string()));
        assert!(mods.contains(&"mypkg.utils".to_string()));
        assert!(mods.contains(&"json".to_string()));
    }

    #[test]
    fn from_imports_add_submodule_candidates() {
        let mods = imports_of("from mypkg import utils, helpers\n", "tests.test_x");
        // the package itself AND each name as a possible submodule
        assert!(mods.contains(&"mypkg".to_string()));
        assert!(mods.contains(&"mypkg.utils".to_string()));
        assert!(mods.contains(&"mypkg.helpers".to_string()));
    }

    #[test]
    fn relative_imports_resolve_against_importer_package() {
        // tests/sub/test_a.py doing `from ..core import thing`
        let mods = imports_of("from ..core import thing\n", "tests.sub.test_a");
        assert!(mods.contains(&"tests.core".to_string()), "{mods:?}");
        assert!(mods.contains(&"tests.core.thing".to_string()));
        // `from . import sibling`
        let mods = imports_of("from . import sibling\n", "tests.sub.test_a");
        assert!(mods.contains(&"tests.sub.sibling".to_string()), "{mods:?}");
    }

    #[test]
    fn from_without_import_keyword_is_skipped() {
        // `from x` with no ` import ` clause is incomplete: it contributes no
        // module and must not panic (the split_once returns None -> continue).
        let mods = imports_of("from x\nfrom y import z\n", "tests.test_x");
        assert!(!mods.iter().any(|m| m == "x"), "{mods:?}");
        assert!(mods.contains(&"y".to_string()), "{mods:?}");
    }

    #[test]
    fn conftest_change_selects_every_test_in_its_subtree() {
        // Rule 2: a changed conftest.py affects all tests below its directory,
        // with no import edge needed (the conftest branch's `continue`).
        let root = tmp("conftest");
        write(&root, "pkg/conftest.py", "");
        write(&root, "pkg/test_a.py", "def test_a():\n    pass\n");
        write(&root, "pkg/sub/test_b.py", "def test_b():\n    pass\n");
        write(&root, "other/test_c.py", "def test_c():\n    pass\n");

        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("pkg/conftest.py")],
            false,
        )
        .unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("pkg/test_a.py")), "{tests:?}");
                assert!(
                    tests.contains(&PathBuf::from("pkg/sub/test_b.py")),
                    "{tests:?}"
                );
                // A test outside the conftest's subtree is not pulled in.
                assert!(
                    !tests.contains(&PathBuf::from("other/test_c.py")),
                    "{tests:?}"
                );
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    // An unreadable .py file must be walked over (read_to_string errors ->
    // `continue`) rather than aborting the whole index build.
    #[cfg(unix)]
    #[test]
    fn unreadable_source_file_is_skipped_during_build() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("unreadable");
        write(&root, "pkg/mod.py", "x = 1\n");
        write(&root, "pkg/test_a.py", "from pkg import mod\n");
        let bad = root.join("pkg/bad.py");
        std::fs::write(&bad, "import pkg.mod\n").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();

        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("pkg/mod.py")],
            false,
        );
        // Restore perms so the temp dir can be cleaned up regardless.
        let _ = std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o644));
        let sel = sel.unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("pkg/test_a.py")), "{tests:?}");
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    #[test]
    fn star_imports_keep_base_only() {
        let mods = imports_of("from mypkg.core import *\n", "tests.test_x");
        assert_eq!(mods, vec!["mypkg.core".to_string()]);
    }

    #[test]
    fn indented_imports_inside_functions_count() {
        // over-selection by design: function-local imports still create edges
        let mods = imports_of("def test_x():\n    import lazy_dep\n", "tests.test_x");
        assert!(mods.contains(&"lazy_dep".to_string()));
    }

    #[test]
    fn source_change_selects_importing_test_across_multiple_hops() {
        // leaf.py <- mid.py <- test_leaf.py: a change to leaf reaches the test
        // through two reverse-import BFS hops (leaf -> mid -> test_leaf).
        let root = tmp("bfs");
        write(&root, "leaf.py", "VALUE = 1\n");
        write(&root, "mid.py", "import leaf\n");
        write(
            &root,
            "test_leaf.py",
            "import mid\ndef test_v():\n    pass\n",
        );
        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("leaf.py")],
            false,
        )
        .unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("test_leaf.py")), "{tests:?}");
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    #[test]
    fn strict_source_reaching_no_test_forces_a_full_run() {
        // Under --strict a changed source file that reaches no test via the graph
        // (dynamic-import target / unused module) forces a full run rather than a
        // silent empty selection.
        let root = tmp("strict-none");
        write(&root, "orphan.py", "x = 1\n");
        write(&root, "test_unrelated.py", "def test_u():\n    pass\n");
        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("orphan.py")],
            true,
        )
        .unwrap();
        match sel {
            Selection::FullRun(r) => assert!(r.contains("orphan.py"), "{r}"),
            Selection::Tests(tests) => panic!("expected full run, got {tests:?}"),
        }
    }

    #[test]
    fn strict_source_that_reaches_a_test_selects_normally() {
        // The strict reachability gate passes when the source provably reaches a
        // test, so selection proceeds as usual.
        let root = tmp("strict-ok");
        write(&root, "dep.py", "x = 1\n");
        write(
            &root,
            "test_dep.py",
            "import dep\ndef test_d():\n    pass\n",
        );
        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("dep.py")],
            true,
        )
        .unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.contains(&PathBuf::from("test_dep.py")), "{tests:?}");
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    #[test]
    fn a_deleted_changed_test_file_is_dropped_from_the_selection() {
        // A changed test file that no longer exists (git reports deletions) matches
        // is_test_file by name, but the exists() filter must drop it so pytest is
        // never handed a missing path.
        let root = tmp("del-changed");
        write(&root, "test_real.py", "def test_r():\n    pass\n");
        let sel = affected_tests(
            &root,
            &ProjectConfig::default(),
            &[PathBuf::from("test_ghost.py")], // never written to disk
            false,
        )
        .unwrap();
        match sel {
            Selection::Tests(tests) => {
                assert!(tests.is_empty(), "deleted test must be dropped: {tests:?}");
            }
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    fn tests_of(sel: Selection) -> Vec<PathBuf> {
        match sel {
            Selection::Tests(t) => t,
            Selection::FullRun(r) => panic!("unexpected full run: {r}"),
        }
    }

    #[test]
    fn cached_selection_matches_fresh_and_updates_on_edit() {
        // The cache is a pure speedup: cached selection must equal a fresh build,
        // both on the warm hit and after an edit changes a source's imports.
        let root = tmp("cache-eq");
        write(&root, "a.py", "VALUE = 1\n");
        write(&root, "b.py", "VALUE = 2\n");
        write(
            &root,
            "test_a.py",
            "import a\ndef test_a():\n    assert a.VALUE\n",
        );
        write(
            &root,
            "test_b.py",
            "import b\ndef test_b():\n    assert b.VALUE\n",
        );
        let proj = ProjectConfig::default();
        let mut cache = CollectionCache::new();

        // Editing a.py selects only test_a.py; cached == fresh.
        let changed = [PathBuf::from("a.py")];
        let fresh = tests_of(affected_tests(&root, &proj, &changed, false).unwrap());
        let cached =
            tests_of(affected_tests_cached(&root, &proj, &changed, false, &mut cache).unwrap());
        assert_eq!(cached, fresh);
        assert_eq!(cached, vec![PathBuf::from("test_a.py")]);

        // A second reselection reuses the cache (warm) and still agrees.
        let changed = [PathBuf::from("b.py")];
        let fresh = tests_of(affected_tests(&root, &proj, &changed, false).unwrap());
        let cached =
            tests_of(affected_tests_cached(&root, &proj, &changed, false, &mut cache).unwrap());
        assert_eq!(cached, fresh);
        assert_eq!(cached, vec![PathBuf::from("test_b.py")]);

        // Rewrite test_b.py to import a instead of b, then report THAT edit (as
        // the watcher would). The cache must re-parse test_b.py and re-edge it,
        // so a subsequent edit to a.py now reaches BOTH tests. (Sleep so the
        // mtime is observably different on coarse-resolution filesystems.)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(
            &root,
            "test_b.py",
            "import a\ndef test_b():\n    assert a.VALUE\n",
        );
        let edit = [PathBuf::from("test_b.py")];
        let _ = affected_tests_cached(&root, &proj, &edit, false, &mut cache).unwrap();

        let changed = [PathBuf::from("a.py")];
        let fresh = tests_of(affected_tests(&root, &proj, &changed, false).unwrap());
        let cached =
            tests_of(affected_tests_cached(&root, &proj, &changed, false, &mut cache).unwrap());
        assert_eq!(
            cached, fresh,
            "cached must still equal a fresh build after the edit"
        );
        assert!(
            cached.contains(&PathBuf::from("test_a.py"))
                && cached.contains(&PathBuf::from("test_b.py")),
            "the re-edged import must reach both tests: {cached:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_evicts_deleted_files() {
        // A file removed between builds must not leak stale import edges.
        let root = tmp("cache-evict");
        write(&root, "a.py", "VALUE = 1\n");
        write(
            &root,
            "test_a.py",
            "import a\ndef test_a():\n    assert a.VALUE\n",
        );
        let proj = ProjectConfig::default();
        let mut cache = CollectionCache::new();
        let _ = affected_tests_cached(&root, &proj, &[PathBuf::from("a.py")], false, &mut cache);
        std::fs::remove_file(root.join("test_a.py")).unwrap();
        // The watcher reports the deletion: a change set naming a path that no
        // longer exists is structural, forcing a full rebuild that evicts it.
        let cached = tests_of(
            affected_tests_cached(
                &root,
                &proj,
                &[PathBuf::from("test_a.py")],
                false,
                &mut cache,
            )
            .unwrap(),
        );
        assert!(
            cached.is_empty(),
            "deleted importer must not select: {cached:?}"
        );
        assert!(
            !cache.files.keys().any(|p| p.ends_with("test_a.py")),
            "cache must evict the deleted file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Before/after micro-benchmark for incremental collection under `--watch`.
    /// Ignored by default (it writes a large tree and is timing-based); run with:
    ///   cargo test -p rstest-cli --lib -- --ignored --nocapture watch_collection_bench
    #[test]
    #[ignore]
    #[allow(non_snake_case)] // FILES/TESTS/CYCLES read as consts throughout the body
    fn watch_collection_bench() {
        use std::time::Instant;
        // Source modules == test files; override with RSTEST_BENCH_FILES to sweep
        // sizes, RSTEST_BENCH_CYCLES for the edit count.
        let files: usize = std::env::var("RSTEST_BENCH_FILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(800);
        let cycles: usize = std::env::var("RSTEST_BENCH_CYCLES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20);
        let (FILES, TESTS, CYCLES) = (files, files, cycles);

        let root = tmp("bench");
        // Realistic file bodies: a block of imports + filler lines, so the
        // read+parse cost the cache removes is representative (not a 1-line stub).
        let filler: String = (0..60)
            .map(|k| format!("# line {k} of filler content here\n"))
            .collect();
        for i in 0..FILES {
            let deps: String = (0..5)
                .map(|d| format!("from pkg import mod_{}\n", (i + d + 1) % FILES))
                .collect();
            write(
                &root,
                &format!("pkg/mod_{i}.py"),
                &format!("{deps}VALUE = {i}\n{filler}"),
            );
        }
        write(&root, "pkg/__init__.py", "");
        for i in 0..TESTS {
            write(
                &root,
                &format!("tests/test_{i}.py"),
                &format!(
                    "from pkg import mod_{m}\n{filler}def test_{i}():\n    assert mod_{m}.VALUE == {m}\n",
                    m = i % FILES
                ),
            );
        }
        let proj = ProjectConfig::default();
        let changed = [PathBuf::from("pkg/mod_0.py")];
        // Simulate a real edit each cycle: rewrite the changed file (same imports,
        // different body) so its mtime moves and the cache must re-read it.
        let edit = |cycle: usize| {
            let deps: String = (0..5)
                .map(|d| format!("from pkg import mod_{}\n", (d + 1) % FILES))
                .collect();
            write(
                &root,
                "pkg/mod_0.py",
                &format!("{deps}VALUE = 0  # edit {cycle}\n{filler}"),
            );
        };

        // BEFORE: a fresh full index build every cycle (today's --watch).
        let t0 = Instant::now();
        for c in 0..CYCLES {
            edit(c);
            let _ = affected_tests(&root, &proj, &changed, false).unwrap();
        }
        let before = t0.elapsed();

        // AFTER: one warm CollectionCache reused across cycles (this feature).
        // First call warms it (full build); the rest are edit-only fast paths.
        let mut cache = CollectionCache::new();
        let t1 = Instant::now();
        for c in 0..CYCLES {
            edit(c);
            let _ = affected_tests_cached(&root, &proj, &changed, false, &mut cache).unwrap();
        }
        let after = t1.elapsed();

        let files = FILES + TESTS + 1;
        eprintln!("\n=== incremental collection under --watch ===");
        eprintln!("project: {files} .py files, {CYCLES} reselect cycles (1 file changed each)");
        eprintln!(
            "BEFORE (fresh index each change): {:>8.1?}  ({:.1?}/cycle)",
            before,
            before / CYCLES as u32
        );
        eprintln!(
            "AFTER  (warm collection cache):   {:>8.1?}  ({:.1?}/cycle)",
            after,
            after / CYCLES as u32
        );
        let speedup = before.as_secs_f64() / after.as_secs_f64().max(1e-9);
        eprintln!("speedup: {speedup:.1}x\n");
        let _ = std::fs::remove_dir_all(&root);
    }
}
