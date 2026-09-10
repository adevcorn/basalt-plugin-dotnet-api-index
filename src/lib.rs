//! Basalt API endpoint index for .NET projects.
//!
//! First pass is heuristic-only. It depends on `project-model:dotnet` for
//! routing and declares an optional future semantic dependency on a
//! `semantic-query:dotnet` provider, which a dotnet LSP plugin can satisfy
//! later.

use basalt_plugin_sdk::prelude::*;
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

basalt_plugin_meta! {
    name:              "dotnet-api-index",
    version:           "0.1.0",
    hook_flags:        CAP_API_INDEX,
    provides:          "api-index:dotnet",
    requires:          "project-model:dotnet",
    optional_requires: "",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

#[cfg(not(test))]
extern "C" {
    fn basalt_read_file(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn basalt_list_files(root_ptr: i32, root_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}


const FILE_BUF_SIZE: usize = 4 * 1024 * 1024;
const LIST_BUF_SIZE: usize = 8 * 1024 * 1024;

static mut FILE_BUF: [u8; FILE_BUF_SIZE] = [0u8; FILE_BUF_SIZE];
static mut LIST_BUF: [u8; LIST_BUF_SIZE] = [0u8; LIST_BUF_SIZE];

#[derive(Serialize)]
struct ApiIndexDoc {
    schema_version: u32,
    ecosystem: String,
    root: String,
    display_name: String,
    projects: Vec<ProjectRecord>,
}

#[derive(Serialize)]
struct ProjectRecord {
    id: String,
    name: String,
    root_path: String,
    manifest_path: String,
    capabilities: Vec<String>,
    endpoints: Vec<EndpointRecord>,
}

#[derive(Serialize)]
struct EndpointRecord {
    id: String,
    project_id: String,
    method: String,
    route: String,
    source_path: String,
    line: u32,
    style: String,
    symbol: Option<String>,
    requires_auth: bool,
    allows_anonymous: bool,
    auth_schemes: Vec<String>,
}

struct ControllerContext {
    name: String,
    token: String,
    area: Option<String>,
    base_route: String,
    auth: AuthMetadata,
}

#[derive(Clone, Default)]
struct AuthMetadata {
    requires_auth: bool,
    allows_anonymous: bool,
    schemes: Vec<String>,
}

#[basalt_plugin]
fn api_index(root: &str) -> Vec<u8> {
    let doc = if is_solution_path(root) {
        build_solution_index(root)
    } else if is_project_path(root) {
        build_project_index(root)
    } else {
        // Core passes the workspace root directory: discover the manifest.
        // Prefer a solution, fall back to the first project file found.
        discover_manifest(root).and_then(|m| {
            if is_solution_path(&m) {
                build_solution_index(&m)
            } else {
                build_project_index(&m)
            }
        })
    };

    doc.and_then(|doc| serde_json::to_vec(&doc).ok()).unwrap_or_default()
}

/// Find the best build manifest under a workspace root directory.
/// Returns the host path (root joined with the relative entry).
fn discover_manifest(root: &str) -> Option<String> {
    let mut best_sln: Option<String> = None;
    let mut best_proj: Option<String> = None;
    for rel in list_host_files(root) {
        let norm = normalize_rel_path(&rel);
        if best_sln.is_none() && is_solution_path(&norm) {
            best_sln = Some(join_path(root, &norm));
        } else if best_proj.is_none() && is_project_path(&norm) {
            best_proj = Some(join_path(root, &norm));
        }
        if best_sln.is_some() && best_proj.is_some() {
            break;
        }
    }
    best_sln.or(best_proj)
}

fn build_solution_index(solution_path: &str) -> Option<ApiIndexDoc> {
    let solution = read_host_file(solution_path)?;
    if solution.is_empty() {
        return None;
    }

    let solution_dir = parent_dir(solution_path);
    let display_name = file_stem(solution_path).to_string();
    let mut projects = Vec::new();

    for (name, rel_path) in parse_solution_projects(&solution) {
        let normalized_rel = normalize_rel_path(&rel_path);
        if !is_project_path(&normalized_rel) {
            continue;
        }
        let manifest_path = join_path(&solution_dir, &normalized_rel);
        let root_path = normalize_rel_path(
            &parent_dir_rel(&normalized_rel)
                .unwrap_or_default()
        );
        if let Some(project) = build_project_record(&manifest_path, Some(name), &root_path) {
            projects.push(project);
        }
    }

    if projects.is_empty() {
        return None;
    }

    Some(ApiIndexDoc {
        schema_version: 1,
        ecosystem: "dotnet".to_string(),
        root: solution_dir,
        display_name,
        projects,
    })
}

fn build_project_index(project_path: &str) -> Option<ApiIndexDoc> {
    let display_name = file_stem(project_path).to_string();
    let project = build_project_record(project_path, Some(display_name.clone()), "")?;
    let root = parent_dir(project_path);
    Some(ApiIndexDoc {
        schema_version: 1,
        ecosystem: "dotnet".to_string(),
        root,
        display_name,
        projects: vec![project],
    })
}

fn build_project_record(
    manifest_path: &str,
    fallback_name: Option<String>,
    root_path: &str,
) -> Option<ProjectRecord> {
    let manifest = read_host_file(manifest_path)?;
    if manifest.is_empty() {
        return None;
    }

    let project_name = parse_xml_text(&manifest, "AssemblyName")
        .or_else(|| parse_xml_text(&manifest, "RootNamespace"))
        .or(fallback_name)
        .unwrap_or_else(|| file_stem(manifest_path).to_string());

    if !project_may_expose_api(&manifest) {
        return None;
    }

    let project_dir = parent_dir(manifest_path);
    let files = list_host_files(&project_dir);
    if files.is_empty() {
        return None;
    }

    let project_id = sanitize_id(&project_name);
    let mut endpoint_map: BTreeMap<(String, String), EndpointRecord> = BTreeMap::new();
    let mut found_oas = false;

    for endpoint in discover_oas_endpoints(&project_id, root_path, &project_dir, &files) {
        found_oas = true;
        merge_endpoint(&mut endpoint_map, endpoint);
    }

    if let Some(oas_output_dir) = parse_xml_text(&manifest, "OpenApiDocumentsDirectory")
        .map(|value| join_path(&project_dir, value.trim()))
        .map(|value| normalize_path(&value))
        .filter(|value| !value.is_empty() && *value != normalize_path(&project_dir))
    {
        let output_files = list_host_files(&oas_output_dir);
        for endpoint in discover_oas_endpoints(&project_id, root_path, &oas_output_dir, &output_files) {
            found_oas = true;
            merge_endpoint(&mut endpoint_map, endpoint);
        }
    }

    if found_oas {
        let mut endpoints: Vec<EndpointRecord> = endpoint_map.into_values().collect();
        endpoints.sort_by(|a, b| {
            a.source_path
                .cmp(&b.source_path)
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.method.cmp(&b.method))
                .then_with(|| a.route.cmp(&b.route))
        });

        return Some(ProjectRecord {
            id: project_id,
            name: project_name,
            root_path: root_path.to_string(),
            manifest_path: manifest_path.to_string(),
            capabilities: vec!["exposes-api".to_string()],
            endpoints,
        });
    }

    for rel_path in &files {
        if !rel_path.ends_with(".cs") || is_ignored_source_path(&rel_path) {
            continue;
        }
        let abs_path = join_path(&project_dir, &rel_path);
        let Some(source) = read_host_file(&abs_path) else { continue };
        if source.is_empty() {
            continue;
        }

        let source_path = if root_path.is_empty() {
            rel_path.clone()
        } else {
            format!("{}/{}", trim_trailing_slash(root_path), rel_path)
        };
        for endpoint in parse_minimal_api_endpoints(&project_id, &source_path, &source) {
            merge_endpoint(&mut endpoint_map, endpoint);
        }
        for endpoint in parse_controller_endpoints(&project_id, &source_path, &source) {
            merge_endpoint(&mut endpoint_map, endpoint);
        }
    }

    let mut endpoints: Vec<EndpointRecord> = endpoint_map.into_values().collect();

    if endpoints.is_empty() {
        return None;
    }

    endpoints.sort_by(|a, b| {
        a.source_path
            .cmp(&b.source_path)
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.method.cmp(&b.method))
            .then_with(|| a.route.cmp(&b.route))
    });

    Some(ProjectRecord {
        id: project_id,
        name: project_name,
        root_path: root_path.to_string(),
        manifest_path: manifest_path.to_string(),
        capabilities: vec!["exposes-api".to_string()],
        endpoints,
    })
}

fn merge_endpoint(
    endpoint_map: &mut BTreeMap<(String, String), EndpointRecord>,
    endpoint: EndpointRecord,
) {
    let key = endpoint_merge_key(&endpoint.method, &endpoint.route);
    if let Some(existing) = endpoint_map.get_mut(&key) {
        if existing.style == "oas" && endpoint.style != "oas" {
            enrich_endpoint(existing, &endpoint);
            return;
        }
        if existing.style != "oas" && endpoint.style == "oas" {
            let mut preferred = endpoint;
            enrich_endpoint(&mut preferred, existing);
            *existing = preferred;
            return;
        }
        enrich_endpoint(existing, &endpoint);
        return;
    }
    endpoint_map.insert(key, endpoint);
}

fn enrich_endpoint(target: &mut EndpointRecord, other: &EndpointRecord) {
    if target.symbol.is_none() {
        target.symbol = other.symbol.clone();
    }
    if !other.auth_schemes.is_empty() {
        for scheme in &other.auth_schemes {
            if !target.auth_schemes.iter().any(|existing| existing == scheme) {
                target.auth_schemes.push(scheme.clone());
            }
        }
    }
    target.requires_auth = target.requires_auth || other.requires_auth;
    target.allows_anonymous = target.allows_anonymous || other.allows_anonymous;
    if target.source_path.is_empty() {
        target.source_path = other.source_path.clone();
    }
    if target.line == 0 {
        target.line = other.line;
    }
}

fn discover_oas_endpoints(
    project_id: &str,
    root_path: &str,
    project_dir: &str,
    files: &[String],
) -> Vec<EndpointRecord> {
    let mut endpoints = Vec::new();
    for rel_path in files {
        if !looks_like_oas_path(rel_path) {
            continue;
        }
        let abs_path = join_path(project_dir, rel_path);
        let Some(source) = read_host_file(&abs_path) else { continue };
        if source.is_empty() {
            continue;
        }
        let source_path = if root_path.is_empty() {
            rel_path.clone()
        } else {
            format!("{}/{}", trim_trailing_slash(root_path), rel_path)
        };
        endpoints.extend(parse_oas_endpoints(project_id, &source_path, &source));
    }
    endpoints
}

fn looks_like_oas_path(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase();
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);
    let likely_name = matches!(
        file_name,
        "openapi.yaml" | "openapi.yml" | "openapi.json" |
        "swagger.yaml" | "swagger.yml" | "swagger.json"
    );
    likely_name
        || ((normalized.contains("/openapi/") || normalized.contains("/swagger/") || normalized.contains("/docs/"))
            && (normalized.ends_with(".json") || normalized.ends_with(".yaml") || normalized.ends_with(".yml")))
}

fn parse_oas_endpoints(project_id: &str, source_path: &str, source: &str) -> Vec<EndpointRecord> {
    let value = if source_path.ends_with(".json") {
        serde_json::from_str::<serde_json::Value>(source).ok()
    } else {
        serde_yaml::from_str::<serde_json::Value>(source).ok()
    };
    let Some(doc) = value else { return Vec::new() };
    let Some(paths) = doc.get("paths").and_then(|v| v.as_object()) else { return Vec::new() };
    let top_level_security = doc.get("security");
    let mut endpoints = Vec::new();

    for (route, path_item) in paths {
        let Some(path_obj) = path_item.as_object() else { continue };
        for verb in ["get", "post", "put", "delete", "patch", "head", "options"] {
            let Some(op) = path_obj.get(verb).and_then(|v| v.as_object()) else { continue };
            let security = op.get("security").or(top_level_security);
            let (requires_auth, allows_anonymous, auth_schemes) = security_metadata(security);
            let symbol = op
                .get("operationId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| op.get("summary").and_then(|v| v.as_str()).map(|s| s.to_string()));
            endpoints.push(EndpointRecord {
                id: format!("{project_id}:{source_path}:oas:{}:{}", verb.to_uppercase(), route),
                project_id: project_id.to_string(),
                method: verb.to_uppercase(),
                route: route.to_string(),
                source_path: source_path.to_string(),
                line: 1,
                style: "oas".to_string(),
                symbol,
                requires_auth,
                allows_anonymous,
                auth_schemes,
            });
        }
    }

    endpoints
}

fn security_metadata(value: Option<&serde_json::Value>) -> (bool, bool, Vec<String>) {
    let Some(value) = value else { return (false, false, Vec::new()) };
    let Some(entries) = value.as_array() else { return (false, false, Vec::new()) };
    if entries.is_empty() {
        return (false, true, Vec::new());
    }

    let mut schemes = Vec::new();
    for entry in entries {
        let Some(obj) = entry.as_object() else { continue };
        for key in obj.keys() {
            if !schemes.iter().any(|existing| existing == key) {
                schemes.push(key.clone());
            }
        }
    }
    (true, false, schemes)
}

fn parse_minimal_api_endpoints(project_id: &str, source_path: &str, source: &str) -> Vec<EndpointRecord> {
    let group_assign_re = Regex::new(
        r#"(?:(?:var|let)\s+)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*=\s*(?P<expr>.+)$"#,
    )
    .unwrap();

    let mut endpoints = Vec::new();
    let mut groups: HashMap<String, String> = HashMap::new();
    let mut byte_offset = 0usize;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            byte_offset += line.len() + 1;
            continue;
        }

        if trimmed.contains(".MapGroup(") {
            if let Some(caps) = group_assign_re.captures(trimmed) {
                let name = caps.name("name").map(|m| m.as_str()).unwrap_or_default();
                let expr = caps.name("expr").map(|m| m.as_str()).unwrap_or_default();
                if !name.is_empty() {
                    let prefix = resolve_group_prefix_expr(expr, &groups);
                    if !prefix.is_empty() {
                        groups.insert(name.to_string(), prefix);
                    }
                }
            }
        }

        if let Some((prefix_expr, method, route)) = parse_minimal_endpoint_call(trimmed) {
            let group_prefix = resolve_group_prefix_expr(&prefix_expr, &groups);
            let full_route = combine_routes(&group_prefix, &route, None, None, None);
            let line_no = line_number_for_offset(source, byte_offset);
            endpoints.push(EndpointRecord {
                id: format!("{project_id}:{}:{line_no}:{method}:minimal", source_path),
                project_id: project_id.to_string(),
                method: method.to_uppercase(),
                route: full_route,
                source_path: source_path.to_string(),
                line: line_no,
                style: "minimal-api".to_string(),
                symbol: None,
                requires_auth: line_requires_authorization(trimmed),
                allows_anonymous: line_allows_anonymous(trimmed),
                auth_schemes: authorization_schemes_from_line(trimmed),
            });
        }

        byte_offset += line.len() + 1;
    }

    endpoints
}

fn parse_controller_endpoints(project_id: &str, source_path: &str, source: &str) -> Vec<EndpointRecord> {
    let class_re = Regex::new(r#"\bclass\s+([A-Za-z_][A-Za-z0-9_]*)"#).unwrap();
    let method_re = Regex::new(
        r#"^\s*(?:public|internal|protected)\s+(?:async\s+)?(?:[A-Za-z_][A-Za-z0-9_<>,\.\?\[\]]*\s+)+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\("#,
    )
    .unwrap();

    let mut endpoints = Vec::new();
    let mut pending_attrs: Vec<String> = Vec::new();
    let mut current_controller: Option<ControllerContext> = None;
    let mut current_class_indent: Option<usize> = None;
    let mut byte_offset = 0usize;

    for line in source.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('[') {
            pending_attrs.push(trimmed.to_string());
            byte_offset += line.len() + 1;
            continue;
        }

        if let Some(caps) = class_re.captures(line) {
            let class_name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let controller_token = class_name.strip_suffix("Controller").unwrap_or(class_name);
            let is_controller = class_name.ends_with("Controller")
                || pending_attrs.iter().any(|attr| attr.contains("ApiController"))
                || pending_attrs.iter().any(|attr| attr.contains("[Route("));
            if is_controller {
                current_controller = Some(ControllerContext {
                    name: class_name.to_string(),
                    token: controller_token.to_string(),
                    area: parse_area_attribute(&pending_attrs),
                    base_route: parse_route_attribute(&pending_attrs).unwrap_or_default(),
                    auth: parse_auth_metadata(&pending_attrs),
                });
                current_class_indent = Some(line.chars().take_while(|c| c.is_whitespace()).count());
            } else {
                current_controller = None;
                current_class_indent = None;
            }
            pending_attrs.clear();
            byte_offset += line.len() + 1;
            continue;
        }

        if let Some(indent) = current_class_indent {
            let current_indent = line.chars().take_while(|c| c.is_whitespace()).count();
            if !trimmed.is_empty() && current_indent <= indent && !trimmed.starts_with('[') {
                current_controller = None;
                current_class_indent = None;
            }
        }

        if let Some(controller) = &current_controller {
            if let Some(caps) = method_re.captures(line) {
                let method_name = caps.name("name").map(|m| m.as_str()).unwrap_or_default();
                let http_methods = parse_http_method_attributes(&pending_attrs);
                if !http_methods.is_empty() {
                    let method_route = parse_route_attribute(&pending_attrs);
                    let effective_auth = merge_auth_metadata(&controller.auth, &pending_attrs);
                    let line_no = line_number_for_offset(source, byte_offset);
                    for (verb, attr_route) in http_methods {
                        let full_route = combine_routes(
                            &controller.base_route,
                            attr_route.as_deref().or(method_route.as_deref()).unwrap_or_default(),
                            Some(&controller.token),
                            Some(method_name),
                            controller.area.as_deref(),
                        );
                        endpoints.push(EndpointRecord {
                            id: format!("{project_id}:{}:{line_no}:{verb}:{method_name}", source_path),
                            project_id: project_id.to_string(),
                            method: verb,
                            route: full_route,
                            source_path: source_path.to_string(),
                            line: line_no,
                            style: "controller".to_string(),
                            symbol: Some(format!("{}.{}", controller.name, method_name)),
                            requires_auth: effective_auth.requires_auth,
                            allows_anonymous: effective_auth.allows_anonymous,
                            auth_schemes: effective_auth.schemes.clone(),
                        });
                    }
                }
            }
        }

        pending_attrs.clear();
        byte_offset += line.len() + 1;
    }

    endpoints
}

fn parse_http_method_attributes(attrs: &[String]) -> Vec<(String, Option<String>)> {
    let mut methods = Vec::new();
    for attr in attrs {
        let normalized = attr.trim();
        for name in ["HttpGet", "HttpPost", "HttpPut", "HttpDelete", "HttpPatch"] {
            let marker = format!("[{name}");
            if normalized.contains(&marker) {
                methods.push((name.trim_start_matches("Http").to_uppercase(), parse_first_string_literal(normalized)));
                break;
            }
        }
    }
    methods
}

fn parse_route_attribute(attrs: &[String]) -> Option<String> {
    attrs.iter()
        .find(|attr| attr.contains("[Route("))
        .and_then(|attr| parse_first_string_literal(attr))
}

fn parse_area_attribute(attrs: &[String]) -> Option<String> {
    attrs.iter()
        .find(|attr| attr.contains("[Area("))
        .and_then(|attr| parse_first_string_literal(attr))
}

fn parse_auth_metadata(attrs: &[String]) -> AuthMetadata {
    let mut auth = AuthMetadata::default();
    for attr in attrs {
        let normalized = attr.trim();
        if normalized.contains("[AllowAnonymous") {
            auth.allows_anonymous = true;
            auth.requires_auth = false;
            auth.schemes.clear();
            continue;
        }
        if normalized.contains("[Authorize") {
            auth.requires_auth = true;
            for scheme in parse_named_string_list(normalized, "AuthenticationSchemes") {
                if !auth.schemes.iter().any(|existing| existing == &scheme) {
                    auth.schemes.push(scheme);
                }
            }
        }
    }
    auth
}

fn merge_auth_metadata(inherited: &AuthMetadata, attrs: &[String]) -> AuthMetadata {
    let local = parse_auth_metadata(attrs);
    if local.allows_anonymous {
        return local;
    }
    let mut merged = inherited.clone();
    if local.requires_auth {
        merged.requires_auth = true;
    }
    for scheme in local.schemes {
        if !merged.schemes.iter().any(|existing| existing == &scheme) {
            merged.schemes.push(scheme);
        }
    }
    merged
}

fn parse_first_string_literal(input: &str) -> Option<String> {
    let start = input.find('"')?;
    let rest = &input[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn parse_named_string_list(input: &str, key: &str) -> Vec<String> {
    let marker = format!("{key} = ");
    let Some(start) = input.find(&marker) else { return Vec::new() };
    let value = &input[start + marker.len()..];
    let Some(literal) = parse_first_string_literal(value) else { return Vec::new() };
    literal
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

fn line_requires_authorization(line: &str) -> bool {
    line.contains(".RequireAuthorization(")
        || line.ends_with(".RequireAuthorization()")
        || line.contains(".RequireAuthorization()")
}

fn line_allows_anonymous(line: &str) -> bool {
    line.contains(".AllowAnonymous(") || line.ends_with(".AllowAnonymous()") || line.contains(".AllowAnonymous()")
}

fn authorization_schemes_from_line(line: &str) -> Vec<String> {
    let mut schemes = Vec::new();
    if let Some(idx) = line.find(".RequireAuthorization(") {
        let after = &line[idx + ".RequireAuthorization(".len()..];
        if let Some(first) = parse_first_string_literal(after) {
            schemes.push(first);
        }
    }
    schemes
}

fn combine_routes(
    prefix: &str,
    suffix: &str,
    controller_token: Option<&str>,
    action_token: Option<&str>,
    area_token: Option<&str>,
) -> String {
    let replace_tokens = |route: &str| {
        let mut output = route.to_string();
        if let Some(token) = controller_token {
            output = output.replace("[controller]", token);
        }
        if let Some(token) = action_token {
            output = output.replace("[action]", token);
        }
        if let Some(token) = area_token {
            output = output.replace("[area]", token);
        }
        output
    };

    let prefix = trim_route_slashes(&replace_tokens(prefix));
    let suffix = trim_route_slashes(&replace_tokens(suffix));
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => "/".to_string(),
        (false, true) => format!("/{}", prefix),
        (true, false) => format!("/{}", suffix),
        (false, false) => format!("/{}/{}", prefix, suffix),
    }
}

fn endpoint_merge_key(method: &str, route: &str) -> (String, String) {
    (method.to_ascii_uppercase(), canonicalize_route_for_merge(route))
}

fn canonicalize_route_for_merge(route: &str) -> String {
    let mut out = String::with_capacity(route.len());
    let chars: Vec<char> = route.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '{' {
            let start = i;
            i += 1;
            let mut segment = String::new();
            while i < chars.len() && chars[i] != '}' {
                segment.push(chars[i]);
                i += 1;
            }
            let normalized = normalize_route_parameter(&segment);
            out.push('{');
            out.push_str(&normalized);
            if i < chars.len() && chars[i] == '}' {
                out.push('}');
            }
            if i == start {
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }

    let trimmed = trim_route_slashes(&out);
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", trimmed)
    }
}

fn normalize_route_parameter(segment: &str) -> String {
    let mut value = segment.trim();
    if let Some((name, _constraint)) = value.split_once(':') {
        value = name;
    }
    if let Some((name, _rest)) = value.split_once('=') {
        value = name;
    }
    value.trim_end_matches('?').trim().to_string()
}

fn resolve_group_prefix_expr(expr: &str, groups: &HashMap<String, String>) -> String {
    let mut prefix = String::new();
    let mut remaining = expr.trim();

    if let Some(first_segment) = remaining
        .split('.')
        .next()
        .map(str::trim)
        .filter(|segment| !segment.contains('('))
    {
        if let Some(existing) = groups.get(first_segment) {
            prefix = existing.clone();
        }
    }

    while let Some(idx) = remaining.find(".MapGroup(") {
        let after = &remaining[idx + ".MapGroup(".len()..];
        let Some(route) = parse_first_string_literal(after) else { break };
        prefix = combine_routes(&prefix, &route, None, None, None);
        remaining = after;
    }

    prefix
}

fn parse_minimal_endpoint_call(line: &str) -> Option<(String, String, String)> {
    for method in ["Get", "Post", "Put", "Delete", "Patch"] {
        let needle = format!(".Map{method}(");
        let Some(idx) = line.find(&needle) else { continue };
        let prefix = line[..idx].trim().to_string();
        let after = &line[idx + needle.len()..];
        let route = parse_first_string_literal(after)?;
        return Some((prefix, method.to_string(), route));
    }
    None
}

fn project_may_expose_api(manifest: &str) -> bool {
    let sdk = parse_project_sdk(manifest).unwrap_or_default();
    let uses_aspnet = sdk.contains("Microsoft.NET.Sdk.Web")
        || manifest.contains("Microsoft.AspNetCore.App")
        || manifest.contains("Microsoft.AspNetCore.");
    let exposes_openapi = manifest.contains("Swashbuckle")
        || manifest.contains("Microsoft.AspNetCore.OpenApi")
        || manifest.contains("OpenApi");
    uses_aspnet || exposes_openapi
}

fn trim_route_slashes(value: &str) -> String {
    value.trim().trim_matches('/').to_string()
}

fn line_number_for_offset(source: &str, offset: usize) -> u32 {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|b| *b == b'\n')
        .count() as u32
        + 1
}

fn parse_solution_projects(solution: &str) -> Vec<(String, String)> {
    let mut projects = Vec::new();
    let re = Regex::new(
        r#"Project\([^)]*\)\s*=\s*"(?P<name>[^"]+)",\s*"(?P<path>[^"]+)""#,
    )
    .unwrap();
    for caps in re.captures_iter(solution) {
        let name = caps.name("name").map(|m| m.as_str()).unwrap_or_default();
        let path = caps.name("path").map(|m| m.as_str()).unwrap_or_default();
        if !name.is_empty() && !path.is_empty() {
            projects.push((name.to_string(), path.replace('\\', "/")));
        }
    }
    projects
}

fn parse_xml_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].trim().to_string())
}

fn parse_project_sdk(xml: &str) -> Option<String> {
    let project_idx = xml.find("<Project")?;
    let slice = &xml[project_idx..];
    let sdk_idx = slice.find("Sdk=\"")? + "Sdk=\"".len();
    let rest = &slice[sdk_idx..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn read_host_file(path: &str) -> Option<String> {
    #[cfg(test)]
    {
        return std::fs::read_to_string(path).ok();
    }
    #[cfg(not(test))]
    unsafe {
        let path_bytes = path.as_bytes();
        let written = basalt_read_file(
            path_bytes.as_ptr() as i32,
            path_bytes.len() as i32,
            core::ptr::addr_of_mut!(FILE_BUF) as *mut u8 as i32,
            FILE_BUF_SIZE as i32,
        );
        if written <= 0 {
            return None;
        }
        String::from_utf8(FILE_BUF[..written as usize].to_vec()).ok()
    }
}

fn list_host_files(root: &str) -> Vec<String> {
    #[cfg(test)]
    {
        fn visit(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) -> std::io::Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let ty = entry.file_type()?;
                if ty.is_dir() {
                    visit(base, &path, out)?;
                } else if ty.is_file() {
                    if let Ok(rel) = path.strip_prefix(base) {
                        out.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
            Ok(())
        }

        let root_path = std::path::Path::new(root);
        let mut files = Vec::new();
        let _ = visit(root_path, root_path, &mut files);
        files.sort();
        return files;
    }
    #[cfg(not(test))]
    unsafe {
        let root_bytes = root.as_bytes();
        let written = basalt_list_files(
            root_bytes.as_ptr() as i32,
            root_bytes.len() as i32,
            core::ptr::addr_of_mut!(LIST_BUF) as *mut u8 as i32,
            LIST_BUF_SIZE as i32,
        );
        if written <= 0 {
            return Vec::new();
        }
        let text = String::from_utf8(LIST_BUF[..written as usize].to_vec()).unwrap_or_default();
        text.lines().map(|line| line.trim().to_string()).filter(|line| !line.is_empty()).collect()
    }
}

fn is_solution_path(path: &str) -> bool {
    path.ends_with(".sln")
}

fn is_project_path(path: &str) -> bool {
    path.ends_with(".csproj") || path.ends_with(".fsproj") || path.ends_with(".vbproj")
}

fn is_ignored_source_path(path: &str) -> bool {
    path.starts_with("obj/")
        || path.starts_with("bin/")
        || path.contains("/obj/")
        || path.contains("/bin/")
        || path.contains("/.git/")
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) if idx > 0 => path[..idx].to_string(),
        Some(_) => "/".to_string(),
        None => ".".to_string(),
    }
}

fn parent_dir_rel(path: &str) -> Option<String> {
    match path.rfind('/') {
        Some(idx) => Some(path[..idx].to_string()),
        None => None,
    }
}

fn file_stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    match file.rfind('.') {
        Some(idx) => &file[..idx],
        None => file,
    }
}

fn join_path(base: &str, rel: &str) -> String {
    if base.is_empty() || base == "." {
        rel.to_string()
    } else if base.ends_with('/') {
        format!("{base}{rel}")
    } else {
        format!("{base}/{rel}")
    }
}

fn normalize_path(path: &str) -> String {
    use std::path::{Component, Path};

    let mut out: Vec<String> = Vec::new();
    let absolute = path.starts_with('/');
    for component in Path::new(path).components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.is_empty() {
                    out.pop();
                }
            }
            Component::Normal(part) => out.push(part.to_string_lossy().into_owned()),
            Component::Prefix(prefix) => out.push(prefix.as_os_str().to_string_lossy().into_owned()),
        }
    }

    let joined = out.join("/");
    if absolute {
        format!("/{}", joined)
    } else {
        joined
    }
}

fn normalize_rel_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

fn trim_trailing_slash(path: &str) -> &str {
    path.trim_end_matches('/')
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressfinder_api_emits_endpoints() {
        let path = "/Users/aavu/repositories/github.com/github-work.com/addressfinder/AddressFinder.Api/AddressFinder.Api.csproj";
        let project = build_project_record(path, Some("AddressFinder.Api".to_string()), "AddressFinder.Api")
            .expect("expected AddressFinder.Api to produce endpoint data");
        assert!(!project.endpoints.is_empty(), "expected at least one endpoint");
        assert!(project.endpoints.iter().any(|ep| ep.route.contains("addresses")));
    }

    #[test]
    fn addressfinder_solution_maps_api_project_root() {
        let path = "/Users/aavu/repositories/github.com/github-work.com/addressfinder/AddressFinder.sln";
        let doc = build_solution_index(path).expect("expected solution index");
        let api_project = doc
            .projects
            .iter()
            .find(|project| project.root_path == "AddressFinder.Api")
            .expect("expected AddressFinder.Api solution child");
        assert!(!api_project.endpoints.is_empty(), "expected endpoints under AddressFinder.Api");
    }

    #[test]
    fn merge_key_normalizes_route_constraints_and_prefers_oas() {
        let mut map = BTreeMap::new();

        merge_endpoint(
            &mut map,
            EndpointRecord {
                id: "code".to_string(),
                project_id: "p".to_string(),
                method: "GET".to_string(),
                route: "/v1/draws/{drawNumber:long}".to_string(),
                source_path: "Endpoints/DrawsEndpoints.cs".to_string(),
                line: 10,
                style: "minimal-api".to_string(),
                symbol: None,
                requires_auth: true,
                allows_anonymous: false,
                auth_schemes: vec!["workload".to_string()],
            },
        );

        merge_endpoint(
            &mut map,
            EndpointRecord {
                id: "oas".to_string(),
                project_id: "p".to_string(),
                method: "GET".to_string(),
                route: "/v1/draws/{drawNumber}".to_string(),
                source_path: "openapi/openapi.json".to_string(),
                line: 1,
                style: "oas".to_string(),
                symbol: Some("GetDraw".to_string()),
                requires_auth: false,
                allows_anonymous: false,
                auth_schemes: Vec::new(),
            },
        );

        assert_eq!(map.len(), 1);
        let endpoint = map.values().next().unwrap();
        assert_eq!(endpoint.style, "oas");
        assert_eq!(endpoint.route, "/v1/draws/{drawNumber}");
        assert!(endpoint.requires_auth);
        assert!(endpoint.auth_schemes.iter().any(|scheme| scheme == "workload"));
        assert_eq!(endpoint.symbol.as_deref(), Some("GetDraw"));
    }

    #[test]
    fn elza_restfulapi_reads_openapi_output_directory_when_present() {
        let path = "/Users/aavu/repositories/github.com/github-work.com/elza-reference-net/src/ElzaReferenceNet.RestfulApi/ElzaReferenceNet.RestfulApi.csproj";
        if !std::path::Path::new(path).exists() {
            return;
        }

        let project = build_project_record(
            path,
            Some("ElzaReferenceNet.RestfulApi".to_string()),
            "src/ElzaReferenceNet.RestfulApi",
        )
        .expect("expected project record");

        assert!(
            project.endpoints.iter().any(|endpoint| endpoint.style == "oas"),
            "expected at least one OAS-backed endpoint"
        );
        assert!(
            project.endpoints.iter().all(|endpoint| endpoint.style == "oas"),
            "expected OAS to suppress regex-discovered duplicates"
        );
    }
}
