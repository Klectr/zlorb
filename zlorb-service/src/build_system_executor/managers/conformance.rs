use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use zlorb_lib::{
    config::{BuildStrategyKind, RepositoryConfiguration},
    error::ZlorbError,
};

use crate::build_system_executor::{
    managers::{
        bun::BunManager, manager::BuildStrategy, rust::RustManager, template::TemplateManager,
    },
    select_strategy,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Step {
    script: &'static str,
    label: &'static str,
    fail_fragment: &'static str,
}

struct ConformanceCase {
    name: &'static str,
    runtime_name: &'static str,
    source: &'static str,
    steps: &'static [Step],
    run: fn(&RepositoryConfiguration, &Path) -> Result<(), ZlorbError>,
}

fn run_bun(config: &RepositoryConfiguration, runtime: &Path) -> Result<(), ZlorbError> {
    BunManager.exec_with_runtime_path(config, runtime)
}

fn run_rust(config: &RepositoryConfiguration, runtime: &Path) -> Result<(), ZlorbError> {
    RustManager.exec_with_runtime_path(config, runtime)
}

fn run_template(config: &RepositoryConfiguration, runtime: &Path) -> Result<(), ZlorbError> {
    let search_path = runtime.parent().map(|dir| dir.to_path_buf().into_os_string());
    TemplateManager.exec_with_search_path(config, search_path)
}

fn bun_case() -> ConformanceCase {
    ConformanceCase {
        name: "bun",
        runtime_name: "bun",
        source: include_str!("bun.rs"),
        steps: &[
            Step {
                script: "install",
                label: "install",
                fail_fragment: "Failed to install dependencies",
            },
            Step {
                script: "run",
                label: "run build",
                fail_fragment: "Failed to build",
            },
        ],
        run: run_bun,
    }
}

fn rust_case() -> ConformanceCase {
    ConformanceCase {
        name: "rust",
        runtime_name: "cargo",
        source: include_str!("rust.rs"),
        steps: &[Step {
            script: "build",
            label: "build",
            fail_fragment: "Failed to build",
        }],
        run: run_rust,
    }
}

fn template_case() -> ConformanceCase {
    ConformanceCase {
        name: "template",
        runtime_name: "template-runtime",
        source: include_str!("template.rs"),
        steps: &[
            Step {
                script: "install",
                label: "install",
                fail_fragment: "Failed to install dependencies",
            },
            Step {
                script: "run",
                label: "run build",
                fail_fragment: "Failed to build",
            },
        ],
        run: run_template,
    }
}

fn case_for_kind(kind: BuildStrategyKind) -> ConformanceCase {
    match kind {
        BuildStrategyKind::Bun => bun_case(),
        BuildStrategyKind::Rust => rust_case(),
    }
}

fn case_for_strategy(strategy: &BuildStrategy) -> ConformanceCase {
    match strategy {
        BuildStrategy::Bun(_) => bun_case(),
        BuildStrategy::Rust(_) => rust_case(),
    }
}

const MANAGERS_MOD_RS: &str = include_str!("mod.rs");

const MANAGER_RS: &str = include_str!("manager.rs");

#[derive(Clone, Copy, Default)]
enum SkeletonState {
    #[default]
    Code,
    BlockComment(u32),
    Str,
    RawStr(usize),
}

fn raw_string_open(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let mut i = start;
    if chars[i] == 'b' {
        if chars.get(i + 1) != Some(&'r') {
            return None;
        }
        i += 1;
    }
    if chars[i] != 'r' {
        return None;
    }
    i += 1;
    let mut hashes = 0;
    while chars.get(i) == Some(&'#') {
        hashes += 1;
        i += 1;
    }
    if chars.get(i) == Some(&'"') {
        Some((hashes, i + 1 - start))
    } else {
        None
    }
}

fn char_literal_len(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start + 1;
    match chars.get(i) {
        Some('\\') => {
            i += 1;
            if chars.get(i) == Some(&'u') && chars.get(i + 1) == Some(&'{') {
                i += 2;
                while chars.get(i).is_some_and(|c| *c != '}') {
                    i += 1;
                }
                i += 1;
            } else if chars.get(i) == Some(&'x') {
                i += 1;
                for _ in 0..2 {
                    if chars.get(i).is_some_and(|c| c.is_ascii_hexdigit()) {
                        i += 1;
                    } else {
                        break;
                    }
                }
            } else {
                i += 1;
            }
        }
        Some(_) => i += 1,
        None => return None,
    }
    if chars.get(i) == Some(&'\'') {
        Some(i + 1 - start)
    } else {
        None
    }
}

fn code_skeleton(line: &str, state: &mut SkeletonState) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match *state {
            SkeletonState::BlockComment(depth) => {
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    *state = SkeletonState::BlockComment(depth + 1);
                    i += 2;
                } else if c == '*' && chars.get(i + 1) == Some(&'/') {
                    *state = if depth <= 1 {
                        SkeletonState::Code
                    } else {
                        SkeletonState::BlockComment(depth - 1)
                    };
                    i += 2;
                } else {
                    i += 1;
                }
            }
            SkeletonState::Str => {
                if c == '\\' {
                    i += 2;
                } else {
                    if c == '"' {
                        *state = SkeletonState::Code;
                    }
                    i += 1;
                }
            }
            SkeletonState::RawStr(hashes) => {
                if c == '"'
                    && (0..hashes).all(|offset| chars.get(i + 1 + offset) == Some(&'#'))
                {
                    *state = SkeletonState::Code;
                    i += 1 + hashes;
                } else {
                    i += 1;
                }
            }
            SkeletonState::Code => {
                if c == '/' && chars.get(i + 1) == Some(&'/') {
                    break;
                }
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    *state = SkeletonState::BlockComment(1);
                    i += 2;
                    continue;
                }
                if c == '"' {
                    *state = SkeletonState::Str;
                    i += 1;
                    continue;
                }
                let at_token_boundary =
                    i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
                if at_token_boundary && (c == 'r' || c == 'b') {
                    if let Some((hashes, len)) = raw_string_open(&chars, i) {
                        *state = SkeletonState::RawStr(hashes);
                        i += len;
                        continue;
                    }
                }
                if at_token_boundary && c == 'b' && chars.get(i + 1) == Some(&'"') {
                    *state = SkeletonState::Str;
                    i += 2;
                    continue;
                }
                if c == '\'' || (c == 'b' && chars.get(i + 1) == Some(&'\'')) {
                    let quote_at = if c == 'b' { i + 1 } else { i };
                    match char_literal_len(&chars, quote_at) {
                        Some(len) => {
                            i = quote_at + len;
                            continue;
                        }
                        None => {
                            out.push(c);
                            i += 1;
                            continue;
                        }
                    }
                }
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn struct_name(declaration: &str) -> Option<String> {
    let after_pub = declaration.strip_prefix("pub")?;
    if !after_pub.starts_with(|c: char| c.is_whitespace() || c == '(') {
        return None;
    }
    let mut rest = after_pub.trim_start();
    if let Some(inner) = rest.strip_prefix('(') {
        rest = inner.split(')').nth(1)?.trim_start();
    }
    let after_struct = rest.strip_prefix("struct")?;
    if !after_struct.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let name: String = after_struct
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn manager_impl_name(declaration: &str) -> Option<String> {
    let after_impl = declaration.strip_prefix("impl")?;
    if !after_impl.starts_with(|c: char| c.is_whitespace() || c == '<') {
        return None;
    }
    let (trait_part, rest) = after_impl.split_once(" for ")?;
    let trait_name = trait_part.split_whitespace().last()?.rsplit("::").next()?;
    if trait_name != "Manager" && !trait_name.starts_with("Manager<") {
        return None;
    }
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn split_cfg_args(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut current = String::new();
    for c in inner.chars() {
        if in_string {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                current.push(c);
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                args.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

fn cfg_expr_is_test_only(expr: &str) -> bool {
    let expr = expr.trim();
    if expr == "test" {
        return true;
    }
    if let Some(inner) = expr
        .strip_prefix("all(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return split_cfg_args(inner)
            .iter()
            .any(|arg| cfg_expr_is_test_only(arg));
    }
    if let Some(inner) = expr
        .strip_prefix("any(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let args = split_cfg_args(inner);
        return !args.is_empty()
            && args.iter().all(|arg| cfg_expr_is_test_only(arg));
    }
    false
}

fn cfg_attr_is_test_only(attr: &str) -> bool {
    let Some(inner) = attr
        .strip_prefix("cfg(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    cfg_expr_is_test_only(inner)
}

struct ManagerTypeScan {
    structs: Vec<String>,
    impls: Vec<String>,
}

fn scan_manager_types(source: &str) -> ManagerTypeScan {
    let mut structs = Vec::new();
    let mut impls = Vec::new();
    let mut depth: i32 = 0;
    let mut test_block_at: Option<i32> = None;
    let mut pending_test = false;
    let mut skeleton_state = SkeletonState::default();
    for raw_line in source.lines() {
        let line = code_skeleton(raw_line, &mut skeleton_state);
        let mut rest = line.trim();
        let mut line_cfg_test = false;
        while let Some(inner) = rest.strip_prefix("#[") {
            let Some(end) = inner.find(']') else { break };
            let attr = inner[..end].replace(' ', "");
            if cfg_attr_is_test_only(&attr) {
                line_cfg_test = true;
            }
            rest = inner[end + 1..].trim_start();
        }
        if rest.is_empty() {
            if line_cfg_test {
                pending_test = true;
            }
            continue;
        }
        let opens = rest.matches('{').count() as i32;
        let closes = rest.matches('}').count() as i32;
        if let Some(target) = test_block_at {
            depth += opens - closes;
            if depth <= target {
                test_block_at = None;
            }
            continue;
        }
        if pending_test || line_cfg_test {
            pending_test = false;
            if opens > closes {
                test_block_at = Some(depth);
            }
            depth += opens - closes;
            continue;
        }
        if let Some(name) = struct_name(rest) {
            structs.push(name);
        }
        if let Some(name) = manager_impl_name(rest) {
            impls.push(name);
        }
        depth += opens - closes;
    }
    structs.sort();
    impls.sort();
    ManagerTypeScan { structs, impls }
}

fn registered_manager_names() -> Vec<String> {
    let mut names: Vec<String> = BuildStrategy::all()
        .iter()
        .map(|strategy| strategy.manager_name().to_string())
        .collect();
    names.sort();
    names
}

fn shipped_manager_types() -> ManagerTypeScan {
    let mut structs = Vec::new();
    let mut impls = Vec::new();
    for kind in registered_kinds() {
        let scan = scan_manager_types(case_for_kind(kind).source);
        structs.extend(scan.structs);
        impls.extend(scan.impls);
    }
    for source in [MANAGERS_MOD_RS, MANAGER_RS] {
        let scan = scan_manager_types(source);
        structs.extend(scan.structs);
        impls.extend(scan.impls);
    }
    structs.sort();
    impls.sort();
    ManagerTypeScan { structs, impls }
}

fn mod_declaration_name(declaration: &str) -> Option<String> {
    let after_pub = match declaration.strip_prefix("pub") {
        Some(rest) if rest.starts_with(|c: char| c.is_whitespace() || c == '(') => {
            let rest = rest.trim_start();
            match rest.strip_prefix('(') {
                Some(inner) => inner.split(')').nth(1)?.trim_start(),
                None => rest,
            }
        }
        _ => declaration,
    };
    let after_mod = after_pub.strip_prefix("mod")?;
    if !after_mod.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let name: String = after_mod
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn declared_shipped_modules_from(source: &str) -> Vec<String> {
    let mut modules = Vec::new();
    let mut cfg_test = false;
    for line in source.lines() {
        let mut rest = line.trim();
        while let Some(inner) = rest.strip_prefix("#[") {
            let Some(end) = inner.find(']') else { break };
            let attr = inner[..end].replace(' ', "");
            if cfg_attr_is_test_only(&attr) {
                cfg_test = true;
            }
            rest = inner[end + 1..].trim_start();
        }
        if rest.is_empty() || rest.starts_with("//") {
            continue;
        }
        match mod_declaration_name(rest) {
            Some(name) => {
                if !cfg_test {
                    modules.push(name);
                }
                cfg_test = false;
            }
            None => {
                cfg_test = false;
            }
        }
    }
    modules.sort();
    modules
}

fn declared_shipped_modules() -> Vec<String> {
    declared_shipped_modules_from(MANAGERS_MOD_RS)
}

fn kind_module_name(kind: BuildStrategyKind) -> String {
    kind.name().to_string()
}

fn registered_kinds() -> Vec<BuildStrategyKind> {
    BuildStrategyKind::ALL.to_vec()
}

fn registered_cases() -> Vec<ConformanceCase> {
    let mut cases: Vec<ConformanceCase> = registered_kinds()
        .iter()
        .map(|kind| case_for_kind(*kind))
        .collect();
    cases.push(template_case());
    cases
}

struct Fixture {
    bin_dir: PathBuf,
    repo_dir: PathBuf,
}

fn fixture(case_name: &str, label: &str) -> Fixture {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "zlorb-conformance-{}-{}-{}-{}",
        std::process::id(),
        case_name,
        label,
        n
    ));
    let bin_dir = root.join("bin");
    let repo_dir = root.join("repo");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo_dir).unwrap();
    Fixture { bin_dir, repo_dir }
}

impl Fixture {
    fn root(&self) -> PathBuf {
        self.bin_dir.parent().unwrap().to_path_buf()
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(self.root());
    }
}

fn write_stand_in(fixture: &Fixture, case: &ConformanceCase, fail_at: Option<usize>) -> PathBuf {
    std::os::unix::fs::symlink("/bin/sh", fixture.bin_dir.join(case.runtime_name)).unwrap();
    for (index, step) in case.steps.iter().enumerate() {
        let mut body = format!(
            "echo \"call:{}\" >> invocations.log\npwd >> invocations.log\necho conformance-warning >&2\n",
            step.label
        );
        if fail_at == Some(index) {
            body.push_str("echo conformance-boom >&2\nexit 3\n");
        }
        std::fs::write(fixture.repo_dir.join(step.script), body).unwrap();
    }
    fixture.bin_dir.join(case.runtime_name)
}

fn repo_config(repo_dir: &Path) -> RepositoryConfiguration {
    RepositoryConfiguration {
        name: "demo".to_string(),
        path: repo_dir.to_string_lossy().into_owned(),
        remote: "origin".to_string(),
        branch: "master".to_string(),
        build_strategy: None,
        build_command: "".to_string(),
        restart_command: None,
        out_dir: None,
    }
}

#[test]
fn registered_kinds_are_exactly_all_kinds_once_each() {
    assert_eq!(registered_kinds(), BuildStrategyKind::ALL);
    for kind in BuildStrategyKind::ALL {
        assert_eq!(
            registered_kinds().iter().filter(|k| *k == kind).count(),
            1,
            "registered kinds must contain {kind:?} exactly once"
        );
    }
    let cases = registered_cases();
    assert_eq!(cases.len(), BuildStrategyKind::ALL.len() + 1);
    for (index, kind) in BuildStrategyKind::ALL.iter().enumerate() {
        assert_eq!(cases[index].name, kind.name());
    }
    assert_eq!(cases[cases.len() - 1].name, template_case().name);
    for (index, case) in cases.iter().enumerate() {
        for other in &cases[index + 1..] {
            assert_ne!(case.name, other.name, "duplicate conformance case name");
            assert_ne!(
                case.runtime_name, other.runtime_name,
                "duplicate conformance runtime name"
            );
        }
    }
}

#[test]
fn every_shipped_manager_module_is_reachable_from_a_build_strategy() {
    let mut expected: Vec<String> = registered_kinds()
        .iter()
        .map(|kind| kind_module_name(*kind))
        .collect();
    expected.push("manager".to_string());
    expected.sort();
    assert_eq!(
        declared_shipped_modules(),
        expected,
        "managers/mod.rs must declare exactly one shipped module per registered build strategy, plus the manager module"
    );
}

#[test]
fn every_shipped_manager_type_is_registered() {
    let scan = shipped_manager_types();
    let expected = registered_manager_names();
    assert_eq!(
        scan.structs, expected,
        "shipped manager modules must declare exactly the manager structs registered in the build_strategies! list"
    );
    assert_eq!(
        scan.impls, expected,
        "shipped manager modules must implement Manager exactly for the manager structs registered in the build_strategies! list"
    );
}

#[test]
fn template_manager_is_the_only_manager_type_in_the_template_source() {
    let scan = scan_manager_types(template_case().source);
    assert_eq!(scan.structs, vec!["TemplateManager".to_string()]);
    assert_eq!(scan.impls, vec!["TemplateManager".to_string()]);
}

#[test]
fn manager_type_scan_catches_an_orphan_inside_an_existing_module() {
    let both = scan_manager_types(
        "pub struct BunManager;\nimpl Manager for BunManager {}\npub struct OrphanManager;\nimpl Manager for OrphanManager {}\n",
    );
    assert_eq!(
        both.structs,
        vec!["BunManager".to_string(), "OrphanManager".to_string()]
    );
    assert_eq!(both.impls, both.structs);
    let struct_only = scan_manager_types(
        "pub struct BunManager;\nimpl Manager for BunManager {}\npub struct OrphanManager;\n",
    );
    assert_eq!(
        struct_only.structs,
        vec!["BunManager".to_string(), "OrphanManager".to_string()]
    );
    assert_eq!(struct_only.impls, vec!["BunManager".to_string()]);
    let impl_only = scan_manager_types(
        "pub struct BunManager;\nimpl Manager for BunManager {}\nimpl Manager for OrphanManager {}\n",
    );
    assert_eq!(impl_only.structs, vec!["BunManager".to_string()]);
    assert_eq!(
        impl_only.impls,
        vec!["BunManager".to_string(), "OrphanManager".to_string()]
    );
}

#[test]
fn manager_type_scan_catches_common_declaration_shapes() {
    let struct_shapes = [
        "pub(crate) struct OrphanManager;\n",
        "pub struct OrphanManager {}\n",
        "pub struct OrphanManager(pub u8);\n",
        "pub struct OrphanManager<T>;\n",
        "#[derive(Debug)]\npub struct OrphanManager;\n",
        "#[derive(Debug)] pub struct OrphanManager;\n",
    ];
    for source in struct_shapes {
        assert_eq!(
            scan_manager_types(source).structs,
            vec!["OrphanManager".to_string()],
            "scan missed struct shape: {source:?}"
        );
    }
    let impl_shapes = [
        "impl Manager for OrphanManager {}\n",
        "impl crate::build_system_executor::managers::manager::Manager for OrphanManager {}\n",
        "impl<T> Manager for OrphanManager<T> {}\n",
    ];
    for source in impl_shapes {
        assert_eq!(
            scan_manager_types(source).impls,
            vec!["OrphanManager".to_string()],
            "scan missed impl shape: {source:?}"
        );
    }
}

#[test]
fn manager_type_scan_skips_test_only_types() {
    let shapes = [
        "#[cfg(test)]\npub struct TestOnlyManager;\n",
        "#[cfg(test)] pub struct TestOnlyManager;\n",
        "#[cfg(test)]\nimpl Manager for TestOnlyManager {}\n",
        "#[cfg(test)] // tests only\npub struct TestOnlyManager;\n",
        "#[cfg(test)]\n// comment\npub struct TestOnlyManager;\n",
        "#[cfg(test)]\n/// doc\npub struct TestOnlyManager;\n",
        "#[cfg(all(test, feature = \"x\"))] pub struct TestOnlyManager;\n",
    ];
    for source in shapes {
        let scan = scan_manager_types(source);
        assert_eq!(scan.structs, Vec::<String>::new(), "scan counted a test-only struct: {source:?}");
        assert_eq!(scan.impls, Vec::<String>::new(), "scan counted a test-only impl: {source:?}");
    }
    let mixed = "#[cfg(test)]\nmod tests {\n pub struct InTests;\n impl Manager for InTests {}\n}\npub struct After;\nimpl Manager for After {}\n";
    let scan = scan_manager_types(mixed);
    assert_eq!(scan.structs, vec!["After".to_string()]);
    assert_eq!(scan.impls, vec!["After".to_string()]);
    let braced_item = "#[cfg(test)]\nfn helper() {\n let x = 1;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(braced_item).structs,
        vec!["After".to_string()]
    );
}

#[test]
fn manager_type_scan_cfg_predicate_matches_test_only_semantics() {
    let skipped = [
        "#[cfg(all(feature = \"x\", test))] pub struct TestOnlyManager;\n",
        "#[cfg(any(test))] pub struct TestOnlyManager;\n",
        "#[cfg(any(all(test, feature = \"x\"), test))] pub struct TestOnlyManager;\n",
        "#[cfg(all(any(test, test), feature = \"x\"))] pub struct TestOnlyManager;\n",
    ];
    for source in skipped {
        let scan = scan_manager_types(&source);
        assert_eq!(
            scan.structs,
            Vec::<String>::new(),
            "scan counted a test-only struct: {source:?}"
        );
    }
    let counted = [
        "#[cfg(any(test, feature = \"x\"))] pub struct ShippedManager;\n",
        "#[cfg(any(feature = \"x\", test))] pub struct ShippedManager;\n",
        "#[cfg(any(test, feature = \"x\"))]\nimpl Manager for ShippedManager {}\n",
        "#[cfg(not(test))] pub struct ShippedManager;\n",
        "#[cfg(all(feature = \"x\", any(test, debug_assertions)))] pub struct ShippedManager;\n",
    ];
    for source in counted {
        let scan = scan_manager_types(source);
        assert_eq!(
            scan.structs.len() + scan.impls.len(),
            1,
            "scan skipped a shipped declaration: {source:?}"
        );
    }
    let mixed = "#[cfg(any(test, feature = \"x\"))]\nmod gated {\n pub struct InGated;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(mixed).structs,
        vec!["After".to_string(), "InGated".to_string()]
    );
}

#[test]
fn manager_type_scan_ignores_comments_and_strings() {
    let commented = "// pub struct Commented;\n// impl Manager for Commented {}\n/* pub struct Blocked; */\npub struct Real;\n";
    let scan = scan_manager_types(commented);
    assert_eq!(scan.structs, vec!["Real".to_string()]);
    assert_eq!(scan.impls, Vec::<String>::new());
    let string_brace = "let label = \"{\";\n#[cfg(test)]\nmod tests {\n pub struct Hidden;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(string_brace).structs,
        vec!["After".to_string()]
    );
}

#[test]
fn manager_type_scan_ignores_char_literals_raw_strings_and_nested_comments() {
    let quote_char = "let q = '\"';\npub struct Real;\nimpl Manager for Real {}\n";
    let scan = scan_manager_types(quote_char);
    assert_eq!(scan.structs, vec!["Real".to_string()]);
    assert_eq!(scan.impls, vec!["Real".to_string()]);
    let brace_char_in_tests = "#[cfg(test)]\nmod tests {\n let open = '{';\n pub struct Hidden;\n}\npub struct After;\nimpl Manager for After {}\n";
    let scan = scan_manager_types(brace_char_in_tests);
    assert_eq!(scan.structs, vec!["After".to_string()]);
    assert_eq!(scan.impls, vec!["After".to_string()]);
    let quote_char_in_tests = "#[cfg(test)]\nmod tests {\n let q = '\"';\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(quote_char_in_tests).structs,
        vec!["After".to_string()]
    );
    let raw_inner_quote = "let s = r#\"a \" b\"#;\npub struct Real;\n";
    assert_eq!(
        scan_manager_types(raw_inner_quote).structs,
        vec!["Real".to_string()]
    );
    let raw_multiline = "let s = r#\"a \"\npub struct Hidden;\n\"#;\npub struct Real;\n";
    assert_eq!(
        scan_manager_types(raw_multiline).structs,
        vec!["Real".to_string()]
    );
    let nested_comment = "/* outer /* inner */ pub struct Hidden; */\npub struct Real;\n";
    assert_eq!(
        scan_manager_types(nested_comment).structs,
        vec!["Real".to_string()]
    );
    let multiline_string = "let s = \"start\npub struct Hidden;\nend\";\npub struct Real;\n";
    assert_eq!(
        scan_manager_types(multiline_string).structs,
        vec!["Real".to_string()]
    );
    let unicode_escape_before_tests = "let smile = '\\u{1F600}';\n#[cfg(test)]\nmod tests {\n pub struct Hidden;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(unicode_escape_before_tests).structs,
        vec!["After".to_string()]
    );
    let unicode_escape_inside_tests = "#[cfg(test)]\nmod tests {\n let smile = '\\u{1F600}';\n pub struct Hidden;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(unicode_escape_inside_tests).structs,
        vec!["After".to_string()]
    );
    let lifetimes_and_escaped_char =
        "fn f<'a>(x: &'a str) -> &'a str { x }\nlet q = '\\'';\nlet brace = '\\u{7d}';\npub struct Real;\n";
    assert_eq!(
        scan_manager_types(lifetimes_and_escaped_char).structs,
        vec!["Real".to_string()]
    );
}

#[test]
fn shipped_module_scan_catches_every_declaration_shape() {
    let shapes = [
        "pub mod npm;\n",
        "pub mod npm {\n}\n",
        "pub(crate) mod npm;\n",
        "mod npm;\n",
        "#[cfg(feature = \"npm\")] pub mod npm;\n",
        "#[cfg(feature = \"npm\")]\npub mod npm;\n",
        "pub mod npm; // npm manager\n",
        "#[allow(dead_code)] pub mod npm;\n",
    ];
    for source in shapes {
        assert_eq!(
            declared_shipped_modules_from(source),
            vec!["npm".to_string()],
            "scan missed declaration shape: {source:?}"
        );
    }
}

#[test]
fn shipped_module_scan_skips_test_only_modules_in_every_shape() {
    let shapes = [
        "#[cfg(test)]\npub mod template;\n",
        "#[cfg(test)] pub mod template;\n",
        "#[cfg(all(test, feature = \"x\"))] pub mod template;\n",
        "#[cfg(test)] // tests only\npub mod template;\n",
        "#[cfg(test)]\n// comment\npub mod template;\n",
        "#[cfg(test)]\n/// doc\npub mod template;\n",
    ];
    for source in shapes {
        assert_eq!(
            declared_shipped_modules_from(source),
            Vec::<String>::new(),
            "scan counted a test-only module: {source:?}"
        );
    }
    let mixed = "pub mod bun;\n#[cfg(test)]\npub mod template;\npub mod rust;\n";
    assert_eq!(
        declared_shipped_modules_from(mixed),
        vec!["bun".to_string(), "rust".to_string()]
    );
}

#[test]
fn shipped_module_scan_cfg_predicate_matches_test_only_semantics() {
    let skipped = [
        "#[cfg(all(feature = \"x\", test))] pub mod template;\n",
        "#[cfg(any(test))] pub mod template;\n",
        "#[cfg(any(all(test, feature = \"x\"), test))]\npub mod template;\n",
        "#[cfg(all(feature = \"a,b\", test))] pub mod template;\n",
        "#[cfg(all(test,))] pub mod template;\n",
    ];
    for source in skipped {
        assert_eq!(
            declared_shipped_modules_from(source),
            Vec::<String>::new(),
            "scan counted a test-only module: {source:?}"
        );
    }
    let counted = [
        "#[cfg(any(test, feature = \"x\"))] pub mod npm;\n",
        "#[cfg(any(test, feature = \"x\"))]\npub mod npm;\n",
        "#[cfg(not(test))] pub mod npm;\n",
        "#[cfg(any(test, feature = \"a)b\"))] pub mod npm;\n",
        "#[cfg(any(test, feature = \"a\\\"b\"))] pub mod npm;\n",
    ];
    for source in counted {
        assert_eq!(
            declared_shipped_modules_from(source),
            vec!["npm".to_string()],
            "scan skipped a shipped module: {source:?}"
        );
    }
}

#[test]
fn registered_strategies_resolve_to_their_conformance_cases() {
    for kind in registered_kinds() {
        let mut config = repo_config(Path::new("/tmp/demo"));
        config.build_strategy = Some(kind.name().to_string());
        let strategy = select_strategy(&config).unwrap();
        assert_eq!(strategy.kind(), kind);
        assert_eq!(case_for_strategy(&strategy).name, case_for_kind(kind).name);
    }
}

#[test]
fn every_build_strategy_variant_maps_to_a_kind_once_each() {
    let strategies = BuildStrategy::all();
    assert_eq!(strategies.len(), BuildStrategyKind::ALL.len());
    for kind in BuildStrategyKind::ALL {
        assert_eq!(
            strategies.iter().filter(|s| s.kind() == *kind).count(),
            1,
            "build strategies must map to {kind:?} exactly once"
        );
    }
    for strategy in &strategies {
        let case = case_for_strategy(strategy);
        let expected = case_for_kind(strategy.kind());
        assert_eq!(case.name, strategy.kind().name());
        assert_eq!(case.name, expected.name);
        assert_eq!(case.runtime_name, expected.runtime_name);
        assert_eq!(case.steps.len(), expected.steps.len());
        for (step, expected_step) in case.steps.iter().zip(expected.steps.iter()) {
            assert_eq!(step.script, expected_step.script);
            assert_eq!(step.label, expected_step.label);
            assert_eq!(step.fail_fragment, expected_step.fail_fragment);
        }
    }
}

fn assert_invocation_log(fixture: &Fixture, case: &ConformanceCase) {
    let log = std::fs::read_to_string(fixture.repo_dir.join("invocations.log")).unwrap();
    let repo_canon = fixture.repo_dir.canonicalize().unwrap();
    let repo_canon = repo_canon.to_string_lossy();
    let mut expected: Vec<&str> = Vec::new();
    for step in case.steps {
        expected.push(step.label);
        expected.push(repo_canon.as_ref());
    }
    let lines: Vec<String> = log
        .lines()
        .map(|line| {
            line.strip_prefix("call:")
                .map(|label| label.to_string())
                .unwrap_or_else(|| line.to_string())
        })
        .collect();
    let expected: Vec<String> = expected.iter().map(|line| line.to_string()).collect();
    assert_eq!(lines, expected, "{}: invocation log mismatch", case.name);
}

#[test]
fn conformance_success_runs_steps_in_order_in_the_repo_dir() {
    for case in registered_cases() {
        let fixture = fixture(case.name, "ok");
        let runtime = write_stand_in(&fixture, &case, None);
        let result = (case.run)(&repo_config(&fixture.repo_dir), &runtime);
        assert!(result.is_ok(), "{}: exec failed: {:?}", case.name, result);
        assert_invocation_log(&fixture, &case);
        fixture.cleanup();
    }
}

#[test]
fn selected_strategy_payload_runs_its_kinds_steps() {
    for kind in registered_kinds() {
        let case = case_for_kind(kind);
        let fixture = fixture(case.name, "payload");
        let runtime = write_stand_in(&fixture, &case, None);
        let mut config = repo_config(&fixture.repo_dir);
        config.build_strategy = Some(kind.name().to_string());
        let strategy = select_strategy(&config).unwrap();
        assert_eq!(strategy.kind(), kind);
        let result = strategy.exec_with_runtime_path(&config, &runtime);
        assert!(
            result.is_ok(),
            "{}: selected payload exec failed: {:?}",
            case.name,
            result
        );
        assert_invocation_log(&fixture, &case);
        fixture.cleanup();
    }
}

#[test]
fn selected_strategy_payload_failure_names_its_kinds_step() {
    for kind in registered_kinds() {
        let case = case_for_kind(kind);
        let fixture = fixture(case.name, "payload-fail");
        let runtime = write_stand_in(&fixture, &case, Some(0));
        let mut config = repo_config(&fixture.repo_dir);
        config.build_strategy = Some(kind.name().to_string());
        let strategy = select_strategy(&config).unwrap();
        let err = strategy
            .exec_with_runtime_path(&config, &runtime)
            .unwrap_err();
        let debug = format!("{err:?}");
        assert!(
            debug.contains(case.steps[0].fail_fragment),
            "{}: payload error does not name its kind's first step: {}",
            case.name,
            debug
        );
        assert!(
            debug.contains("conformance-boom"),
            "{}: payload error does not carry the command stderr: {}",
            case.name,
            debug
        );
        fixture.cleanup();
    }
}

#[test]
fn conformance_step_failure_carries_step_name_and_stderr() {
    for case in registered_cases() {
        for fail_index in 0..case.steps.len() {
            let label = format!("fail{fail_index}");
            let fixture = fixture(case.name, &label);
            let runtime = write_stand_in(&fixture, &case, Some(fail_index));
            let err = (case.run)(&repo_config(&fixture.repo_dir), &runtime).unwrap_err();
            let debug = format!("{err:?}");
            let step = &case.steps[fail_index];
            assert!(
                debug.contains(step.fail_fragment),
                "{}: error does not name the failing step {}: {}",
                case.name,
                step.label,
                debug
            );
            assert!(
                debug.contains("conformance-boom"),
                "{}: error does not carry the command stderr: {}",
                case.name,
                debug
            );
            fixture.cleanup();
        }
    }
}

#[test]
fn conformance_missing_runtime_errors_instead_of_panicking() {
    for case in registered_cases() {
        let fixture = fixture(case.name, "missing");
        let missing = fixture.bin_dir.join("no-such-runtime");
        let result = (case.run)(&repo_config(&fixture.repo_dir), &missing);
        assert!(result.is_err(), "{}: missing runtime was not an error", case.name);
        fixture.cleanup();
    }
}

#[test]
fn manager_type_scan_handles_hex_escapes_and_raw_string_boundaries() {
    let hex_open = "#[cfg(test)]\nmod tests {\n const X: bool = '\\x41'<'{';\n}\npub struct OrphanManager;\nimpl Manager for OrphanManager {}\n";
    let scan = scan_manager_types(hex_open);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let hex_close = "#[cfg(test)]\nmod tests {\n const X: bool = '\\x41'>'}';\n pub struct TestOnly;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(hex_close).structs,
        vec!["After".to_string()]
    );
    let hex_fn_body = "#[cfg(test)]\nmod tests {\n fn t() { let y = '\\x7d'<'{'; }\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(hex_fn_body).structs,
        vec!["After".to_string()]
    );
    let hex_byte = "#[cfg(test)]\nmod tests {\n const X: bool = b'\\x41'<b'{';\n}\npub struct OrphanManager;\nimpl Manager for OrphanManager {}\n";
    let scan = scan_manager_types(hex_byte);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let hex_spaced = "#[cfg(test)]\nmod tests {\n const X: bool = '\\x41' < '{';\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(hex_spaced).structs,
        vec!["After".to_string()]
    );
    let macro_raw = "some_macro! {\n ar#\"a\"\n pub struct OrphanManager;\n impl Manager for OrphanManager {}\n \"#\"\n}\n";
    let scan = scan_manager_types(macro_raw);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
}

#[test]
fn manager_type_scan_catches_generic_manager_shapes() {
    let cases: [(&str, &str); 10] = [
        ("generic lifetime tuple", "pub struct OrphanManager<'a>(&'a str);\nimpl<'a> Manager for OrphanManager<'a> {}\n"),
        ("phantom lifetime", "pub struct OrphanManager<'a> { pub marker: std::marker::PhantomData<&'a ()> }\nimpl Manager for OrphanManager<'_> {}\n"),
        ("char const default", "pub struct OrphanManager<const C: char = '{'>;\nimpl Manager for OrphanManager<'{'> {}\n"),
        ("static ref default", "pub struct OrphanManager<T = &'static str>(pub T);\nimpl<T> Manager for OrphanManager<T> {}\n"),
        ("apostrophe comments", "pub struct OrphanManager; // don't\nimpl Manager for OrphanManager {} // it's fine\n"),
        ("slash star chars", "let a = '/';\nlet b = '*';\npub struct OrphanManager;\nimpl Manager for OrphanManager {}\n"),
        ("byte string", "let s = b\"abc\";\npub struct OrphanManager;\nimpl Manager for OrphanManager {}\n"),
        ("comment with quote", "/* it's a \"test\" */ pub struct OrphanManager;\nimpl Manager for OrphanManager {}\n"),
        ("string with comment opener", "let s = \"/*\";\npub struct OrphanManager;\nimpl Manager for OrphanManager {}\n"),
        ("raw identifier field", "pub struct OrphanManager { pub r#type: u8 }\nimpl Manager for OrphanManager {}\n"),
    ];
    for (label, source) in cases {
        let scan = scan_manager_types(source);
        assert_eq!(scan.structs, vec!["OrphanManager".to_string()], "structs missed: {label}");
        assert_eq!(scan.impls, vec!["OrphanManager".to_string()], "impls missed: {label}");
    }
}
