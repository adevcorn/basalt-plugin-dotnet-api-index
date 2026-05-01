//! Basalt API endpoint index for .NET projects.
//!
//! First pass is heuristic-only. It depends on `project-model:dotnet` for
//! routing and declares an optional future semantic dependency on a
//! `semantic-query:dotnet` provider, which a dotnet LSP plugin can satisfy
//! later.

use basalt_plugin_sdk::prelude::*;
use regex::Regex;
use serde::Serialize;
use std::collections::HashMap;

basalt_plugin_meta! {
    name:              "dotnet-api-index",
    version:           "0.1.0",
    hook_flags:        CAP_API_INDEX,
    provides:          "api-index:dotnet",
    requires:          "project-model:dotnet",
    optional_requires: "semantic-query:dotnet",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

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
}

struct ControllerContext {
    name: String,
    token: String,
    base_route: String,
}

#[basalt_plugin]
fn api_index(root: &str) -> Vec<u8> {
    let doc = if is_solution_path(root) {
        build_solution_index(root)
    } else if is_project_path(root) {
        build_project_index(root)
    } else {
        None
    };

    doc.and_then(|doc| serde_json::to_vec(&doc).ok()).unwrap_or_default()
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

    let project_dir = parent_dir(manifest_path);
    let files = list_host_files(&project_dir);
    if files.is_empty() {
        return None;
    }

    let mut endpoints = Vec::new();
    let project_id = sanitize_id(&project_name);

    for rel_path in files {
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
        endpoints.extend(parse_minimal_api_endpoints(&project_id, &source_path, &source));
        endpoints.extend(parse_controller_endpoints(&project_id, &source_path, &source));
    }

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

fn parse_minimal_api_endpoints(project_id: &str, source_path: &str, source: &str) -> Vec<EndpointRecord> {
    let group_re = Regex::new(
        r#"(?m)\b(?:(?:var|let)\s+)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*=\s*.*?\.MapGroup\(\s*"(?P<route>[^"]+)""#,
    )
    .unwrap();
    let endpoint_re = Regex::new(
        r#"(?m)(?P<prefix>[A-Za-z_][A-Za-z0-9_\.]*)\.Map(?P<method>Get|Post|Put|Delete|Patch)\(\s*"(?P<route>[^"]*)""#,
    )
    .unwrap();

    let mut groups: HashMap<String, String> = HashMap::new();
    for caps in group_re.captures_iter(source) {
        let name = caps.name("name").map(|m| m.as_str()).unwrap_or_default();
        let route = caps.name("route").map(|m| m.as_str()).unwrap_or_default();
        if !name.is_empty() && !route.is_empty() {
            groups.insert(name.to_string(), route.to_string());
        }
    }

    let mut endpoints = Vec::new();
    for caps in endpoint_re.captures_iter(source) {
        let method = caps.name("method").map(|m| m.as_str()).unwrap_or_default();
        let route = caps.name("route").map(|m| m.as_str()).unwrap_or_default();
        let prefix_name = caps
            .name("prefix")
            .map(|m| m.as_str())
            .unwrap_or_default()
            .split('.')
            .last()
            .unwrap_or_default();
        let group_prefix = groups.get(prefix_name).map(String::as_str).unwrap_or_default();
        let full_route = combine_routes(group_prefix, route, None);
        let byte_offset = caps.get(0).map(|m| m.start()).unwrap_or(0);
        let line = line_number_for_offset(source, byte_offset);
        endpoints.push(EndpointRecord {
            id: format!("{project_id}:{}:{line}:{method}:minimal", source_path),
            project_id: project_id.to_string(),
            method: method.to_uppercase(),
            route: full_route,
            source_path: source_path.to_string(),
            line,
            style: "minimal-api".to_string(),
            symbol: None,
        });
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
                    base_route: parse_route_attribute(&pending_attrs).unwrap_or_default(),
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
                    let line_no = line_number_for_offset(source, byte_offset);
                    for (verb, attr_route) in http_methods {
                        let full_route = combine_routes(
                            &controller.base_route,
                            attr_route.as_deref().or(method_route.as_deref()).unwrap_or_default(),
                            Some(&controller.token),
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

fn parse_first_string_literal(input: &str) -> Option<String> {
    let start = input.find('"')?;
    let rest = &input[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn combine_routes(prefix: &str, suffix: &str, controller_token: Option<&str>) -> String {
    let replace_controller = |route: &str| match controller_token {
        Some(token) => route.replace("[controller]", token),
        None => route.to_string(),
    };

    let prefix = trim_route_slashes(&replace_controller(prefix));
    let suffix = trim_route_slashes(&replace_controller(suffix));
    match (prefix.is_empty(), suffix.is_empty()) {
        (true, true) => "/".to_string(),
        (false, true) => format!("/{}", prefix),
        (true, false) => format!("/{}", suffix),
        (false, false) => format!("/{}/{}", prefix, suffix),
    }
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

fn read_host_file(path: &str) -> Option<String> {
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
