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

fn declaration_segments(text: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        current.push(c);
        if c == ';' || c == '{' || c == '}' {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

fn is_incomplete_declaration(segment: &str) -> bool {
    let segment = segment.trim();
    if segment.is_empty() || segment.contains([';', '{', '}']) {
        return false;
    }
    if segment == "mod" || segment == "pub" || segment == "impl" {
        return true;
    }
    if let Some(after_pub) = segment.strip_prefix("pub") {
        if after_pub.starts_with(|c: char| c.is_whitespace() || c == '(') {
            let rest = after_pub.trim_start();
            if rest.starts_with('(') && !rest.contains(')') {
                return true;
            }
            let rest = match rest.strip_prefix('(') {
                Some(inner) => match inner.split(')').nth(1) {
                    Some(rest) => rest.trim_start(),
                    None => return true,
                },
                None => rest,
            };
            return rest.is_empty() || rest == "struct" || rest == "mod";
        }
    }
    if let Some(after_impl) = segment.strip_prefix("impl") {
        if after_impl.starts_with(|c: char| c.is_whitespace() || c == '<') {
            if !segment.contains(" for ") {
                return true;
            }
            return segment
                .rsplit(" for ")
                .next()
                .is_some_and(|rest| rest.trim().is_empty());
        }
    }
    false
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

fn find_attr_close(text: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if in_string {
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
            '"' => in_string = true,
            ']' => return Some(index),
            _ => {}
        }
    }
    None
}

fn normalize_attr(attr: &str) -> String {
    attr.chars().filter(|c| !c.is_whitespace()).collect()
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

fn strip_segment_attrs(segment: &str) -> (String, bool, Option<String>) {
    let mut rest = segment.trim();
    let mut cfg_test = false;
    loop {
        let Some(inner) = rest.strip_prefix("#[") else {
            break;
        };
        let Some(end) = find_attr_close(inner) else {
            return (String::new(), cfg_test, Some(inner.to_string()));
        };
        if cfg_attr_is_test_only(&normalize_attr(&inner[..end])) {
            cfg_test = true;
        }
        rest = inner[end + 1..].trim_start();
    }
    (rest.to_string(), cfg_test, None)
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
    let mut pending_decl: Option<String> = None;
    let mut skeleton_state = SkeletonState::default();
    let mut attr_buf: Option<String> = None;
    let mut attr_saved_test = false;
    for raw_line in source.lines() {
        let line = code_skeleton(raw_line, &mut skeleton_state);
        let mut line_cfg_test = false;
        let mut rest_owned: String;
        let mut rest: &str;
        if let Some(mut buf) = attr_buf.take() {
            let Some(end) = find_attr_close(&line) else {
                buf.push_str(&line);
                buf.push(' ');
                attr_buf = Some(buf);
                continue;
            };
            buf.push_str(&line[..end]);
            if cfg_attr_is_test_only(&normalize_attr(&buf)) {
                line_cfg_test = true;
            }
            if attr_saved_test {
                line_cfg_test = true;
                attr_saved_test = false;
            }
            rest_owned = line[end + 1..].trim_start().to_string();
            rest = &rest_owned;
        } else {
            rest = line.trim();
        }
        let mut incomplete = false;
        while let Some(inner) = rest.strip_prefix("#[") {
            let Some(end) = find_attr_close(inner) else {
                attr_buf = Some(inner.to_string());
                attr_saved_test = line_cfg_test;
                incomplete = true;
                break;
            };
            let attr = normalize_attr(&inner[..end]);
            if cfg_attr_is_test_only(&attr) {
                line_cfg_test = true;
            }
            rest_owned = inner[end + 1..].trim_start().to_string();
            rest = &rest_owned;
        }
        if incomplete {
            continue;
        }
        if rest.is_empty() {
            if line_cfg_test && test_block_at.is_none() {
                pending_test = true;
            }
            continue;
        }
        let mut gated = pending_test || line_cfg_test;
        let mut segments: std::collections::VecDeque<(String, bool)> =
            declaration_segments(rest)
                .into_iter()
                .map(|segment| (segment, false))
                .collect();
        while let Some((segment, entry_cfg)) = segments.pop_front() {
            let opens = segment.matches('{').count() as i32;
            let closes = segment.matches('}').count() as i32;
            if let Some(target) = test_block_at {
                pending_decl = None;
                depth += opens - closes;
                if depth <= target {
                    test_block_at = None;
                    pending_test = false;
                    gated = false;
                }
                continue;
            }
            let (clean, seg_cfg_test, unterminated) = strip_segment_attrs(&segment);
            if unterminated.is_some() {
                let mut joined = segment.trim().to_string();
                for (queued, _) in &segments {
                    joined.push_str(queued);
                }
                let (rest_clean, rest_cfg, rest_unterminated) = strip_segment_attrs(&joined);
                let total_cfg = entry_cfg || seg_cfg_test || rest_cfg;
                if let Some(joined_inner) = rest_unterminated {
                    attr_buf = Some(joined_inner);
                    attr_saved_test = gated || total_cfg;
                    break;
                }
                segments.clear();
                if rest_clean.trim().is_empty() {
                    gated = gated || total_cfg;
                    if gated {
                        pending_test = true;
                    }
                    continue;
                }
                let mut first = true;
                for piece in declaration_segments(&rest_clean) {
                    segments.push_back((piece, first && total_cfg));
                    first = false;
                }
                continue;
            }
            let seg = clean.trim();
            let seg_opens = seg.matches('{').count() as i32;
            let seg_closes = seg.matches('}').count() as i32;
            gated = gated || entry_cfg || seg_cfg_test;
            if seg.is_empty() {
                if gated {
                    pending_test = true;
                }
                continue;
            }
            if gated {
                gated = false;
                pending_decl = None;
                if seg_opens > seg_closes {
                    test_block_at = Some(depth);
                    pending_test = false;
                } else if seg.contains([';', '}']) {
                    pending_test = false;
                } else {
                    pending_test = true;
                }
                depth += seg_opens - seg_closes;
                continue;
            }
            let own_struct = struct_name(seg);
            let own_impl = manager_impl_name(seg);
            if own_struct.is_some() || own_impl.is_some() {
                if let Some(name) = own_struct {
                    structs.push(name);
                }
                if let Some(name) = own_impl {
                    impls.push(name);
                }
                pending_decl = None;
            } else if let Some(pending) = pending_decl.take() {
                let combined = format!("{pending} {seg}");
                let combined_struct = struct_name(&combined);
                let combined_impl = manager_impl_name(&combined);
                if combined_struct.is_some() || combined_impl.is_some() {
                    if let Some(name) = combined_struct {
                        structs.push(name);
                    }
                    if let Some(name) = combined_impl {
                        impls.push(name);
                    }
                } else if is_incomplete_declaration(&combined) {
                    pending_decl = Some(combined);
                }
            } else if is_incomplete_declaration(seg) {
                pending_decl = Some(seg.to_string());
            }
            depth += seg_opens - seg_closes;
        }
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

fn declared_modules_inner(source: &str) -> Vec<(String, bool)> {
    let mut modules = Vec::new();
    let mut awaiting_terminator: Option<String> = None;
    let mut cfg_test = false;
    let mut pending_mod: Option<String> = None;
    let mut attr_buf: Option<String> = None;
    let mut attr_skel = SkeletonState::default();
    let mut depth: i32 = 0;
    let mut test_block_at: Option<i32> = None;
    let mut block_skel = SkeletonState::default();
    for line in source.lines() {
        let mut rest_owned: String;
        let mut rest: &str;
        let mut line_end_state = SkeletonState::default();
        let mut end_state_set = false;
        let mut line_preskel = false;
        if test_block_at.is_some() {
            line_preskel = true;
            rest_owned = code_skeleton(line, &mut block_skel);
            rest = &rest_owned;
        } else if let Some(mut buf) = attr_buf.take() {
            let skel_line = code_skeleton(line, &mut attr_skel);
            line_end_state = attr_skel;
            end_state_set = true;
            let Some(end) = find_attr_close(&skel_line) else {
                buf.push_str(&skel_line);
                buf.push(' ');
                attr_buf = Some(buf);
                continue;
            };
            buf.push_str(&skel_line[..end]);
            if cfg_attr_is_test_only(&normalize_attr(&buf)) {
                cfg_test = true;
            }
            attr_skel = SkeletonState::default();
            rest_owned = skel_line[end + 1..].trim_start().to_string();
            rest = &rest_owned;
        } else {
            rest = line.trim();
        }
        let mut incomplete = false;
        while !line_preskel {
            let Some(inner) = rest.strip_prefix("#[") else {
                break;
            };
            let mut frag_state = SkeletonState::default();
            let skel_inner = code_skeleton(inner, &mut frag_state);
            if !end_state_set {
                line_end_state = frag_state;
                end_state_set = true;
            }
            let Some(end) = find_attr_close(&skel_inner) else {
                attr_skel = line_end_state;
                attr_buf = Some(skel_inner);
                incomplete = true;
                break;
            };
            let attr = normalize_attr(&skel_inner[..end]);
            if cfg_attr_is_test_only(&attr) {
                cfg_test = true;
            }
            rest_owned = skel_inner[end + 1..].trim_start().to_string();
            rest = &rest_owned;
        }
        if incomplete {
            continue;
        }
        if rest.is_empty() || rest.starts_with("//") {
            continue;
        }
        let mut segments: std::collections::VecDeque<(String, bool, bool)> =
            declaration_segments(rest)
                .into_iter()
                .map(|segment| (segment, false, line_preskel))
                .collect();
        while let Some((segment, entry_cfg, preskel)) = segments.pop_front() {
            let seg_raw = segment.trim();
            if seg_raw.is_empty() {
                continue;
            }
            if let Some(target) = test_block_at {
                pending_mod = None;
                let (opens, closes) = if preskel {
                    (
                        segment.matches('{').count() as i32,
                        segment.matches('}').count() as i32,
                    )
                } else {
                    let skel_seg = code_skeleton(seg_raw, &mut block_skel);
                    (
                        skel_seg.matches('{').count() as i32,
                        skel_seg.matches('}').count() as i32,
                    )
                };
                depth += opens - closes;
                if depth <= target {
                    test_block_at = None;
                    cfg_test = false;
                    block_skel = SkeletonState::default();
                }
                continue;
            }
            let mut seg_state = SkeletonState::default();
            let skel_seg = code_skeleton(seg_raw, &mut seg_state);
            let (clean, seg_cfg_test, unterminated) = strip_segment_attrs(&skel_seg);
            if unterminated.is_some() {
                let mut joined = seg_raw.to_string();
                for (queued, _, _) in &segments {
                    joined.push_str(queued);
                }
                let mut joined_state = SkeletonState::default();
                let joined_skel = code_skeleton(&joined, &mut joined_state);
                let (rest_clean, rest_cfg, rest_unterminated) =
                    strip_segment_attrs(&joined_skel);
                let total_cfg = entry_cfg || seg_cfg_test || rest_cfg;
                if let Some(joined_inner) = rest_unterminated {
                    cfg_test = cfg_test || total_cfg;
                    attr_buf = Some(joined_inner);
                    attr_skel = joined_state;
                    break;
                }
                segments.clear();
                if rest_clean.trim().is_empty() {
                    if cfg_test || total_cfg {
                        cfg_test = true;
                    }
                    continue;
                }
                let mut first = true;
                for piece in declaration_segments(&rest_clean) {
                    segments.push_back((piece, first && total_cfg, false));
                    first = false;
                }
                continue;
            }
            let seg = clean.trim();
            let seg_opens = seg.matches('{').count() as i32;
            let seg_closes = seg.matches('}').count() as i32;
            let gated = cfg_test || entry_cfg || seg_cfg_test;
            if seg.is_empty() {
                if gated {
                    cfg_test = true;
                }
                continue;
            }
            if let Some(awaited) = awaiting_terminator.take() {
                if seg == ";" {
                    modules.push((awaited, true));
                    continue;
                }
                modules.push((awaited, false));
            }
            if gated {
                pending_mod = None;
                if seg_opens > seg_closes {
                    test_block_at = Some(depth);
                    block_skel = seg_state;
                    cfg_test = false;
                    let mut joined = String::new();
                    for (queued, _, _) in &segments {
                        joined.push_str(queued);
                    }
                    segments.clear();
                    if !joined.is_empty() {
                        let skel_joined = code_skeleton(&joined, &mut block_skel);
                        for piece in declaration_segments(&skel_joined) {
                            segments.push_back((piece, false, true));
                        }
                    }
                } else if seg.contains([';', '}']) {
                    cfg_test = false;
                } else {
                    cfg_test = true;
                }
                depth += seg_opens - seg_closes;
                continue;
            }
            if let Some(name) = mod_declaration_name(seg) {
                if seg.ends_with(';') {
                    modules.push((name, true));
                } else if seg.contains(['{', '}']) {
                    modules.push((name, false));
                } else {
                    awaiting_terminator = Some(name);
                }
                pending_mod = None;
                depth += seg_opens - seg_closes;
                continue;
            }
            if let Some(pending) = pending_mod.take() {
                let combined = format!("{pending} {seg}");
                if let Some(name) = mod_declaration_name(&combined) {
                    if seg.ends_with(';') {
                        modules.push((name, true));
                    } else if seg.contains(['{', '}']) {
                        modules.push((name, false));
                    } else {
                        awaiting_terminator = Some(name);
                    }
                    depth += seg_opens - seg_closes;
                    continue;
                }
                if is_incomplete_declaration(&combined) {
                    pending_mod = Some(combined);
                    depth += seg_opens - seg_closes;
                    continue;
                }
                depth += seg_opens - seg_closes;
                continue;
            }
            if is_incomplete_declaration(seg) {
                pending_mod = Some(seg.to_string());
                depth += seg_opens - seg_closes;
                continue;
            }
            depth += seg_opens - seg_closes;
        }
    }
    if let Some(awaited) = awaiting_terminator.take() {
        modules.push((awaited, false));
    }
    modules.sort();
    modules
}

fn declared_shipped_modules_from(source: &str) -> Vec<String> {
    declared_modules_inner(source)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn declared_file_submodules_from(source: &str) -> Vec<String> {
    let mut skeleton_state = SkeletonState::default();
    let skeleton = source
        .lines()
        .map(|line| code_skeleton(line, &mut skeleton_state))
        .collect::<Vec<String>>()
        .join("\n");
    declared_modules_inner(&skeleton)
        .into_iter()
        .filter(|(_, file_backed)| *file_backed)
        .map(|(name, _)| name)
        .collect()
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
fn file_submodule_scan_catches_file_backed_declarations() {
    let caught = [
        "pub mod orphan;\n",
        "mod orphan;\n",
        "pub(crate) mod orphan;\n",
        "pub struct BunManager; pub mod orphan;\n",
        "pub\nmod orphan;\n",
        "pub mod\norphan;\n",
        "pub mod orphan\n;\n",
        "#[cfg(feature = \"x\")] pub mod orphan;\n",
        "#[allow(dead_code)]\npub mod orphan;\n",
    ];
    for source in caught {
        assert_eq!(
            declared_file_submodules_from(source),
            vec!["orphan".to_string()],
            "scan missed a file-backed submodule: {source:?}"
        );
        assert_eq!(
            declared_shipped_modules_from(source),
            vec!["orphan".to_string()],
            "module scan lost a file-backed submodule: {source:?}"
        );
    }
}

#[test]
fn file_submodule_scan_ignores_comment_contents_and_gaps() {
    let caught = [
        "pub mod orphan /* ; */ ;\n",
        "pub mod orphan /* { */ ;\n",
        "pub mod orphan /* outer /* ; */ inner */ ;\n",
        "pub mod orphan /* comment\nmore */ ;\n",
        "pub mod orphan\n/* comment\nmore */\n;\n",
        "pub mod /* ; */ orphan;\n",
        "pub /* comment\nmore */ mod orphan;\n",
        "pub mod orphan // ;\n;\n",
    ];
    for source in caught {
        assert_eq!(
            declared_file_submodules_from(source),
            vec!["orphan".to_string()],
            "scan missed a file-backed submodule: {source:?}"
        );
    }
    let skipped = [
        "pub mod orphan /* ; */\n{ }\n",
        "pub mod orphan {\n /* ; } */\n}\n",
        "#[cfg(test)] pub mod orphan /* ; */ ;\n",
    ];
    for source in skipped {
        assert_eq!(
            declared_file_submodules_from(source),
            Vec::<String>::new(),
            "scan counted an inline or test-only module as file-backed: {source:?}"
        );
    }
}

#[test]
fn file_submodule_scan_skips_inline_and_test_only_modules() {
    let skipped = [
        "pub mod orphan {}\n",
        "pub mod orphan {\n pub struct Hidden;\n}\n",
        "pub mod orphan\n{\n pub struct Hidden;\n}\n",
        "#[cfg(test)] pub mod orphan;\n",
        "#[cfg(test)]\npub mod orphan;\n",
        "#[cfg(all(test, feature = \"x\"))] pub mod orphan;\n",
        "#[cfg(test)]\nmod tests {\n pub mod orphan;\n}\n",
        "pub struct OrphanManager;\n",
    ];
    for source in skipped {
        assert_eq!(
            declared_file_submodules_from(source),
            Vec::<String>::new(),
            "scan counted an inline or test-only module as file-backed: {source:?}"
        );
    }
    let mixed = "pub mod inline {} pub mod orphan; #[cfg(test)] pub mod tests;\n";
    assert_eq!(
        declared_file_submodules_from(mixed),
        vec!["orphan".to_string()]
    );
    assert_eq!(
        declared_shipped_modules_from(mixed),
        vec!["inline".to_string(), "orphan".to_string()]
    );
}

#[test]
fn no_shipped_manager_source_declares_a_file_submodule() {
    for kind in registered_kinds() {
        assert_eq!(
            declared_file_submodules_from(case_for_kind(kind).source),
            Vec::<String>::new(),
            "manager source for {kind:?} declares a file-backed submodule, whose types the conformance scans cannot see"
        );
    }
    assert_eq!(
        declared_file_submodules_from(MANAGER_RS),
        Vec::<String>::new(),
        "manager.rs declares a file-backed submodule, whose types the conformance scans cannot see"
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
fn manager_type_scan_handles_multiline_cfg_attributes() {
    let skipped = [
        "#[cfg(\n test\n)]\npub struct TestOnlyManager;\n",
        "#[cfg(\n test\n)] pub struct TestOnlyManager;\n",
        "#[cfg(all(\n feature = \"x\",\n test\n))] pub struct TestOnlyManager;\n",
        "#[cfg(\n any(test, test)\n)]\nimpl Manager for TestOnlyManager {}\n",
        "#[derive(Debug)]\n#[cfg(\n test\n)]\npub struct TestOnlyManager;\n",
        "#[cfg(\n test\n)]\nmod tests {\n pub struct Hidden;\n}\npub struct After;\n",
    ];
    for source in skipped {
        let scan = scan_manager_types(source);
        assert_eq!(
            scan.structs.iter().filter(|name| name.as_str() != "After").count(),
            0,
            "scan counted a test-only struct: {source:?}"
        );
        assert_eq!(scan.impls, Vec::<String>::new(), "scan counted a test-only impl: {source:?}");
        if source.contains("After") {
            assert_eq!(scan.structs, vec!["After".to_string()]);
        }
    }
    let in_block = "#[cfg(test)]\nmod tests {\n #[cfg(\n test\n)]\n pub struct Hidden;\n}\npub struct After;\nimpl Manager for After {}\n";
    let scan = scan_manager_types(in_block);
    assert_eq!(scan.structs, vec!["After".to_string()]);
    assert_eq!(scan.impls, vec!["After".to_string()]);
    let in_block_single_line = "#[cfg(test)]\nmod tests {\n #[cfg(test)]\n pub struct Hidden;\n}\npub struct After;\nimpl Manager for After {}\n";
    let scan = scan_manager_types(in_block_single_line);
    assert_eq!(scan.structs, vec!["After".to_string()]);
    assert_eq!(scan.impls, vec!["After".to_string()]);
    let counted = [
        "#[cfg(any(\n test,\n feature = \"x\"\n))] pub struct ShippedManager;\n",
        "#[cfg(\n not(test)\n)] pub struct ShippedManager;\n",
        "#[cfg(any(\n test,\n feature = \"x\"\n))]\nimpl Manager for ShippedManager {}\n",
    ];
    for source in counted {
        let scan = scan_manager_types(source);
        assert_eq!(
            scan.structs.len() + scan.impls.len(),
            1,
            "scan skipped a shipped declaration: {source:?}"
        );
    }
}

#[test]
fn manager_type_scan_catches_split_and_merged_declarations() {
    let split = "pub struct\nOrphanManager;\nimpl Manager\nfor OrphanManager {}\n";
    let scan = scan_manager_types(split);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let split_keywords = "pub\nstruct OrphanManager;\nimpl\nManager for OrphanManager {}\n";
    let scan = scan_manager_types(split_keywords);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let split_impl_generic = "pub(crate)\nstruct OrphanManager;\nimpl<T>\nManager for OrphanManager<T> {}\n";
    let scan = scan_manager_types(split_impl_generic);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let split_body = "pub struct OrphanManager\n{}\nimpl Manager for OrphanManager\n{}\n";
    let scan = scan_manager_types(split_body);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let merged = "pub struct AManager; pub struct BManager;\nimpl Manager for AManager {} impl Manager for BManager {}\n";
    let scan = scan_manager_types(merged);
    assert_eq!(
        scan.structs,
        vec!["AManager".to_string(), "BManager".to_string()]
    );
    assert_eq!(scan.impls, scan.structs);
    let merged_braced = "pub struct AManager {} pub struct BManager {}\n";
    assert_eq!(
        scan_manager_types(merged_braced).structs,
        vec!["AManager".to_string(), "BManager".to_string()]
    );
    let after_statement = "let x = 1; pub struct OrphanManager;\nlet y = 2; impl Manager for OrphanManager {}\n";
    let scan = scan_manager_types(after_statement);
    assert_eq!(scan.structs, vec!["OrphanManager".to_string()]);
    assert_eq!(scan.impls, vec!["OrphanManager".to_string()]);
    let after_string_statement = "let s = \"//\"; pub struct OrphanManager;\n";
    assert_eq!(
        scan_manager_types(after_string_statement).structs,
        vec!["OrphanManager".to_string()]
    );
    let dangling = "pub struct\n";
    assert_eq!(
        scan_manager_types(dangling).structs,
        Vec::<String>::new()
    );
}

#[test]
fn manager_type_scan_split_and_merged_declarations_respect_cfg_test() {
    let split_test_only = "#[cfg(test)]\npub struct\nTestOnlyManager;\n#[cfg(test)]\nimpl Manager\nfor TestOnlyManager {}\npub struct After;\nimpl Manager for After {}\n";
    let scan = scan_manager_types(split_test_only);
    assert_eq!(scan.structs, vec!["After".to_string()]);
    assert_eq!(scan.impls, vec!["After".to_string()]);
    let merged_gated_first = "#[cfg(test)] pub struct TestOnlyManager; pub struct ShippedManager;\n#[cfg(test)] impl Manager for TestOnlyManager {} impl Manager for ShippedManager {}\n";
    let scan = scan_manager_types(merged_gated_first);
    assert_eq!(scan.structs, vec!["ShippedManager".to_string()]);
    assert_eq!(scan.impls, vec!["ShippedManager".to_string()]);
    let split_test_block = "#[cfg(test)]\nmod tests\n{\n pub struct Hidden;\n}\npub struct After;\n";
    assert_eq!(
        scan_manager_types(split_test_block).structs,
        vec!["After".to_string()]
    );
}

#[test]
fn manager_type_scan_reviewer_regression_shapes() {
    let after_gated_brace = "#[cfg(test)]\nimpl Manager\nfor T {} pub struct ShippedManager;\n";
    assert_eq!(
        scan_manager_types(after_gated_brace).structs,
        vec!["ShippedManager".to_string()]
    );
    let after_gated_brace_one_line = "#[cfg(test)] impl Manager for T {} pub struct ShippedManager;\n";
    assert_eq!(
        scan_manager_types(after_gated_brace_one_line).structs,
        vec!["ShippedManager".to_string()]
    );
    let split_visibility_parens = "pub(\ncrate) struct OrphanManager;\n";
    assert_eq!(
        scan_manager_types(split_visibility_parens).structs,
        vec!["OrphanManager".to_string()]
    );
    let split_test_only_semicolon_line = "#[cfg(test)]\npub struct\nTestOnlyManager\n;\npub struct After;\n";
    assert_eq!(
        scan_manager_types(split_test_only_semicolon_line).structs,
        vec!["After".to_string()]
    );
}

#[test]
fn manager_type_scan_per_segment_attributes() {
    let merged_attr = "pub struct AManager; #[derive(Debug)] pub struct BManager;\n#[allow(dead_code)] impl Manager for AManager {} #[allow(dead_code)] impl Manager for BManager {}\n";
    let scan = scan_manager_types(merged_attr);
    assert_eq!(
        scan.structs,
        vec!["AManager".to_string(), "BManager".to_string()]
    );
    assert_eq!(scan.impls, scan.structs);
    let merged_cfg = "pub struct AManager; #[cfg(test)] pub struct TestOnlyManager; pub struct BManager;\nimpl Manager for AManager {} #[cfg(test)] impl Manager for TestOnlyManager {} impl Manager for BManager {}\n";
    let scan = scan_manager_types(merged_cfg);
    assert_eq!(
        scan.structs,
        vec!["AManager".to_string(), "BManager".to_string()]
    );
    assert_eq!(scan.impls, scan.structs);
    let mid_line_block = "fn f() {} #[cfg(test)] mod t { pub struct Hidden; impl Manager for Hidden {} } pub struct After;\nimpl Manager for After {}\n";
    let scan = scan_manager_types(mid_line_block);
    assert_eq!(scan.structs, vec!["After".to_string()]);
    assert_eq!(scan.impls, vec!["After".to_string()]);
    let split_mid_line_cfg = "pub struct AManager; #[cfg(\n test\n)] pub struct TestOnlyManager; pub struct BManager;\n";
    let scan = scan_manager_types(split_mid_line_cfg);
    assert_eq!(
        scan.structs,
        vec!["AManager".to_string(), "BManager".to_string()]
    );
    let attr_with_semicolon = "pub struct AManager; #[foo(x; y)] pub struct BManager;\nimpl Manager for BManager {}\npub struct CManager;\n";
    let scan = scan_manager_types(attr_with_semicolon);
    assert_eq!(
        scan.structs,
        vec![
            "AManager".to_string(),
            "BManager".to_string(),
            "CManager".to_string()
        ]
    );
    assert_eq!(scan.impls, vec!["BManager".to_string()]);
    let attr_with_braces = "pub struct AManager; #[foo({})] pub struct BManager;\nimpl Manager for BManager {}\npub struct CManager;\n";
    let scan = scan_manager_types(attr_with_braces);
    assert_eq!(
        scan.structs,
        vec![
            "AManager".to_string(),
            "BManager".to_string(),
            "CManager".to_string()
        ]
    );
    assert_eq!(scan.impls, vec!["BManager".to_string()]);
    let attr_same_line_after = "pub struct AManager; #[foo(x; y)] pub struct BManager; pub struct CManager;\nimpl Manager for BManager {} impl Manager for CManager {}\n";
    let scan = scan_manager_types(attr_same_line_after);
    assert_eq!(
        scan.structs,
        vec![
            "AManager".to_string(),
            "BManager".to_string(),
            "CManager".to_string()
        ]
    );
    assert_eq!(
        scan.impls,
        vec!["BManager".to_string(), "CManager".to_string()]
    );
}

#[test]
fn shipped_module_scan_per_segment_attributes_and_test_blocks() {
    let merged_attr = "pub mod bun; #[allow(dead_code)] pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(merged_attr),
        vec!["bun".to_string(), "npm".to_string()]
    );
    let merged_cfg = "pub mod bun; #[cfg(test)] pub mod template; pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(merged_cfg),
        vec!["bun".to_string(), "npm".to_string()]
    );
    let one_line_block = "#[cfg(test)] mod tests { pub mod hidden; } pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(one_line_block),
        vec!["npm".to_string()]
    );
    let multiline_block = "#[cfg(test)]\nmod tests {\n pub mod hidden;\n}\npub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(multiline_block),
        vec!["npm".to_string()]
    );
    let split_block = "#[cfg(test)]\nmod tests\n{\n pub mod hidden;\n}\npub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(split_block),
        vec!["npm".to_string()]
    );
    let mid_line_block = "fn f() {} #[cfg(test)] mod t { pub mod hidden; } pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(mid_line_block),
        vec!["npm".to_string()]
    );
    let split_mid_line_cfg = "pub mod bun; #[cfg(\n test\n)] pub mod template; pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(split_mid_line_cfg),
        vec!["bun".to_string(), "npm".to_string()]
    );
    let brace_literals_in_block = [
        "#[cfg(test)]\nmod tests {\n let x = '}';\n pub mod hidden;\n}\npub mod npm;\n",
        "#[cfg(test)]\nmod tests {\n let s = \"}\";\n pub mod hidden;\n}\npub mod npm;\n",
        "#[cfg(test)]\nmod tests {\n /* } */\n pub mod hidden;\n}\npub mod npm;\n",
        "#[cfg(test)]\nmod tests {\n pub mod hidden; // }\n pub mod hidden2;\n}\npub mod npm;\n",
        "#[cfg(test)]\nmod tests {\n let x = '{';\n pub mod hidden;\n}\npub mod npm;\n",
        "#[cfg(test)] mod tests { let x = '}'; pub mod hidden; } pub mod npm;\n",
    ];
    for source in brace_literals_in_block {
        assert_eq!(
            declared_shipped_modules_from(source),
            vec!["npm".to_string()],
            "scan mishandled brace literal in a test block: {source:?}"
        );
    }
    let attr_with_semicolon = "pub mod bun; #[cfg(feature = \"a;b\")] pub mod npm;\npub mod rust;\n";
    assert_eq!(
        declared_shipped_modules_from(attr_with_semicolon),
        vec!["bun".to_string(), "npm".to_string(), "rust".to_string()]
    );
    let attr_same_line_after = "pub mod bun; #[cfg(feature = \"a;b\")] pub mod npm; pub mod rust;\n";
    assert_eq!(
        declared_shipped_modules_from(attr_same_line_after),
        vec!["bun".to_string(), "npm".to_string(), "rust".to_string()]
    );
    let attr_in_block_at_column_zero = "#[cfg(test)]\nmod tests {\n#[cfg(\ntest\n)]\npub mod hidden;\n}\npub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(attr_in_block_at_column_zero),
        vec!["npm".to_string()]
    );
    let comment_bracket_in_split_cfg = [
        "pub mod bun; #[cfg( // gate ]\n not(test)\n)] pub mod npm;\n",
        "pub mod bun; #[cfg(/* gate ] */\n not(test)\n)] pub mod npm;\n",
    ];
    for source in comment_bracket_in_split_cfg {
        assert_eq!(
            declared_shipped_modules_from(source),
            vec!["bun".to_string(), "npm".to_string()],
            "scan mishandled a comment bracket in a split cfg: {source:?}"
        );
    }
}

#[test]
fn shipped_module_scan_reviewer_regression_shapes() {
    let merged_after_braces = "pub mod a {}\npub mod b {} pub mod c;\n";
    assert_eq!(
        declared_shipped_modules_from(merged_after_braces),
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
    let split_after_sibling = "pub mod a; pub\nmod b;\n";
    assert_eq!(
        declared_shipped_modules_from(split_after_sibling),
        vec!["a".to_string(), "b".to_string()]
    );
}

#[test]
fn shipped_module_scan_catches_split_and_merged_declarations() {
    let split = "pub mod\nnpm;\n";
    assert_eq!(
        declared_shipped_modules_from(split),
        vec!["npm".to_string()]
    );
    let split_keyword = "pub\nmod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(split_keyword),
        vec!["npm".to_string()]
    );
    let split_visibility = "pub(crate)\nmod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(split_visibility),
        vec!["npm".to_string()]
    );
    let merged = "pub mod bun; pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(merged),
        vec!["bun".to_string(), "npm".to_string()]
    );
    let merged_braced = "pub mod bun {} pub mod npm {}\n";
    assert_eq!(
        declared_shipped_modules_from(merged_braced),
        vec!["bun".to_string(), "npm".to_string()]
    );
    let gated_first = "#[cfg(test)] pub mod template; pub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(gated_first),
        vec!["npm".to_string()]
    );
    let split_test_only = "#[cfg(test)]\npub mod\ntemplate;\npub mod npm;\n";
    assert_eq!(
        declared_shipped_modules_from(split_test_only),
        vec!["npm".to_string()]
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
fn shipped_module_scan_handles_multiline_cfg_attributes() {
    let skipped = [
        "#[cfg(\n test\n)]\npub mod template;\n",
        "#[cfg(\n test\n)] pub mod template;\n",
        "#[cfg(all(\n feature = \"x\",\n test\n))] pub mod template;\n",
        "#[cfg(any(\n test,\n test\n))]\npub mod template;\n",
        "#[cfg(\n // gate\n test\n)]\npub mod template;\n",
        "#[cfg(\n test // gate\n)]\npub mod template;\n",
        "#[cfg(\n /* gate */\n test\n)]\npub mod template;\n",
        "#[cfg( // gate\n test\n)]\npub mod template;\n",
        "#[cfg( // gate ]\n test\n)]\npub mod template;\n",
        "#[cfg(/* gate ] */\n test\n)]\npub mod template;\n",
        "#[derive(Debug)] #[cfg(/* gate\n still gate */ test)]\npub mod template;\n",
    ];
    for source in skipped {
        assert_eq!(
            declared_shipped_modules_from(source),
            Vec::<String>::new(),
            "scan counted a test-only module: {source:?}"
        );
    }
    let counted = [
        "#[cfg(any(\n test,\n feature = \"x\"\n))] pub mod npm;\n",
        "#[cfg(\n not(test)\n)]\npub mod npm;\n",
        "#[cfg(any(\n test,\n feature = \"a]b\"\n))] pub mod npm;\n",
        "#[cfg( // gate ]\n not(test)\n)] pub mod npm;\n",
        "#[cfg(/* ] */ not(test))] pub mod npm;\n",
    ];
    for source in counted {
        assert_eq!(
            declared_shipped_modules_from(source),
            vec!["npm".to_string()],
            "scan skipped a shipped module: {source:?}"
        );
    }
    let mixed = "pub mod bun;\n#[cfg(\n test\n)]\npub mod template;\npub mod rust;\n";
    assert_eq!(
        declared_shipped_modules_from(mixed),
        vec!["bun".to_string(), "rust".to_string()]
    );
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


