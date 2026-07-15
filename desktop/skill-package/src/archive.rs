use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use zip::ZipArchive;

use crate::{
    error, BundleMember, DependencyEvidence, InstallError, PackageFile, ValidatedArchive,
    ValidatedBundle, ValidatedPackage, CATALOG_STAMP_FILE, IMPORT_ORIGIN_FILE, MAX_ARCHIVE_BYTES,
    MAX_ARCHIVE_ENTRIES, MAX_BUNDLE_FILES, MAX_BUNDLE_TOTAL_BYTES, MAX_FILES, MAX_FILE_BYTES,
    MAX_PATH_BYTES, MAX_PATH_DEPTH, MAX_TOTAL_BYTES,
};

#[derive(Clone, Debug)]
pub(crate) struct EntryMeta {
    pub index: usize,
    pub path: String,
    pub size: u64,
    pub mode: Option<u32>,
    pub directory: bool,
}

pub(crate) fn read_archive_file(file: &mut File) -> Result<Vec<u8>, InstallError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| error("ARCHIVE_READ_FAILED", "无法定位 Skill archive", "archive"))?;
    let mut bytes = Vec::new();
    file.take((MAX_ARCHIVE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| error("ARCHIVE_READ_FAILED", "读取 Skill archive 失败", "archive"))?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(error(
            "ARCHIVE_TOO_LARGE",
            "Skill archive 超过 128 MiB 限制",
            "archive",
        ));
    }
    if !bytes.starts_with(b"PK\x03\x04")
        && !bytes.starts_with(b"PK\x05\x06")
        && !bytes.starts_with(b"PK\x07\x08")
    {
        return Err(error(
            "INVALID_ARCHIVE",
            "文件不是受支持的 ZIP/.skill archive",
            "archive",
        ));
    }
    Ok(bytes)
}

pub(crate) fn archive_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn package_from_local_archive(
    bytes: &[u8],
    archive_stem: &str,
) -> Result<ValidatedPackage, InstallError> {
    let entries = inspect_archive(bytes)?;
    let root_skill = entries
        .iter()
        .any(|entry| !entry.directory && entry.path == "SKILL.md");
    let mut directory_candidates = BTreeSet::new();
    for entry in &entries {
        if entry.directory || !entry.path.ends_with("/SKILL.md") {
            continue;
        }
        let parts = entry.path.split('/').collect::<Vec<_>>();
        if parts.len() == 2 {
            directory_candidates.insert(parts[0].to_string());
        }
    }
    let (skill_name, prefix) = match (root_skill, directory_candidates.len()) {
        (true, 0) => (validate_skill_name(archive_stem)?, String::new()),
        (false, 1) => {
            let name = directory_candidates
                .into_iter()
                .next()
                .expect("one candidate");
            (validate_skill_name(&name)?, format!("{name}/"))
        }
        _ => {
            return Err(error(
                "AMBIGUOUS_SKILL_ROOT",
                "archive 必须只包含一个顶层 SKILL.md 或一个一级 Skill 目录",
                "archive",
            ))
        }
    };
    extract_selected(bytes, &entries, &skill_name, &prefix)
}

pub(crate) fn package_or_bundle_from_local_archive(
    bytes: &[u8],
    archive_stem: &str,
) -> Result<ValidatedArchive, InstallError> {
    let entries = inspect_archive(bytes)?;
    if entries
        .iter()
        .any(|entry| !entry.directory && entry.path == "SKILL.md")
    {
        return package_from_local_archive(bytes, archive_stem).map(ValidatedArchive::Skill);
    }
    let direct_skills = direct_skill_children(&entries, "");
    if direct_skills.len() == 1 {
        let only = direct_skills.iter().next().expect("one direct skill");
        let prefix = format!("{only}/");
        let is_standalone = entries
            .iter()
            .all(|entry| entry.path == *only || entry.path.starts_with(&prefix));
        if !is_standalone {
            // Keep evaluating collection/plugin candidates below.
        } else {
            return package_from_local_archive(bytes, archive_stem).map(ValidatedArchive::Skill);
        }
    }
    let wrapper = unique_outer_directory(&entries).filter(|outer| {
        !entries.iter().any(|entry| {
            entry.path == format!("{outer}/SKILL.md")
                || entry.path.starts_with(&format!("{outer}/SKILL.md/"))
        })
    });
    let base_prefix = wrapper
        .as_ref()
        .map(|outer| format!("{outer}/"))
        .unwrap_or_default();
    recognize_archive(
        bytes,
        &entries,
        &base_prefix,
        "",
        archive_stem,
        archive_stem,
    )
}

pub(crate) fn package_or_bundle_from_github_archive(
    bytes: &[u8],
    source_path: &str,
    repo_name: &str,
) -> Result<ValidatedArchive, InstallError> {
    let entries = inspect_archive(bytes)?;
    let outer = unique_outer_directory(&entries).ok_or_else(|| {
        error(
            "INVALID_GITHUB_ARCHIVE",
            "GitHub archive 必须包含唯一外层目录",
            "archive",
        )
    })?;
    let base_prefix = format!("{outer}/");
    recognize_archive(
        bytes,
        &entries,
        &base_prefix,
        source_path.trim_matches('/'),
        repo_name,
        repo_name,
    )
}

fn recognize_archive(
    bytes: &[u8],
    entries: &[EntryMeta],
    base_prefix: &str,
    explicit_path: &str,
    single_fallback_name: &str,
    bundle_fallback_name: &str,
) -> Result<ValidatedArchive, InstallError> {
    let explicit_prefix = join_prefix(base_prefix, explicit_path);
    let skill_marker = format!("{explicit_prefix}SKILL.md");
    if entries
        .iter()
        .any(|entry| !entry.directory && entry.path == skill_marker)
    {
        let candidate_name = if explicit_path.is_empty() {
            single_fallback_name
        } else {
            explicit_path
                .rsplit('/')
                .next()
                .unwrap_or(single_fallback_name)
        };
        let skill_name = validate_skill_name(candidate_name)?;
        return extract_selected(bytes, entries, &skill_name, &explicit_prefix)
            .map(ValidatedArchive::Skill);
    }

    let logical_entries = entries
        .iter()
        .filter_map(|entry| {
            entry
                .path
                .strip_prefix(base_prefix)
                .map(|path| (entry, path))
        })
        .collect::<Vec<_>>();
    let mut candidates = BTreeSet::new();
    if collection_has_direct_skills(&logical_entries, explicit_path) {
        candidates.insert(explicit_path.trim_matches('/').to_string());
    }
    let default_skills = if explicit_path.is_empty() {
        "skills".to_string()
    } else {
        format!("{}/skills", explicit_path.trim_matches('/'))
    };
    if collection_has_direct_skills(&logical_entries, &default_skills) {
        candidates.insert(default_skills);
    }
    for candidate in plugin_skill_candidates(bytes, entries, base_prefix, explicit_path)? {
        if collection_has_direct_skills(&logical_entries, &candidate) {
            candidates.insert(candidate);
        }
    }
    if candidates.len() > 1 {
        return Err(error(
            "MULTIPLE_BUNDLE_CANDIDATES",
            format!(
                "archive 包含多个不等价 Skill 集合：{}",
                candidates.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
            "archive",
        ));
    }
    if let Some(collection_path) = candidates.into_iter().next() {
        let bundle_name = plugin_name(bytes, entries, base_prefix, explicit_path)?
            .unwrap_or_else(|| bundle_fallback_name.to_string());
        return extract_bundle(bytes, entries, base_prefix, &collection_path, &bundle_name)
            .map(ValidatedArchive::Bundle);
    }

    if explicit_path.is_empty() {
        let direct = direct_skill_children_logical(&logical_entries, "");
        if direct.len() == 1 {
            let name = direct.iter().next().expect("one direct skill");
            return extract_selected(
                bytes,
                entries,
                &validate_skill_name(name)?,
                &join_prefix(base_prefix, &format!("{name}/")),
            )
            .map(ValidatedArchive::Skill);
        }
    }
    Err(error(
        "BUNDLE_STRUCTURE_UNSUPPORTED",
        "未找到唯一的 Skill 或 Nature-like Skill 集合根",
        "archive",
    ))
}

fn extract_bundle(
    bytes: &[u8],
    entries: &[EntryMeta],
    base_prefix: &str,
    collection_path: &str,
    bundle_name: &str,
) -> Result<ValidatedBundle, InstallError> {
    let collection_prefix = join_prefix(base_prefix, collection_path);
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| error("INVALID_ARCHIVE", "无法重新打开 bundle archive", "archive"))?;
    let mut files = Vec::new();
    let mut collision_keys = BTreeMap::<String, (String, bool)>::new();
    let mut total = 0usize;
    for entry in entries {
        if entry.directory || !entry.path.starts_with(&collection_prefix) {
            continue;
        }
        let relative = &entry.path[collection_prefix.len()..];
        if relative.is_empty() || ignored_metadata(relative) {
            continue;
        }
        validate_relative_payload_path(relative)?;
        if relative.split('/').any(|component| {
            matches!(component, IMPORT_ORIGIN_FILE | CATALOG_STAMP_FILE)
                || component.starts_with(".csswitch-")
        }) {
            return Err(error(
                "RESERVED_MARKER_IN_SOURCE",
                format!("bundle 来源包含 CSSwitch 保留路径：{relative}"),
                "archive",
            ));
        }
        register_payload_path(&mut collision_keys, relative)?;
        let size = usize::try_from(entry.size).unwrap_or(usize::MAX);
        if size > MAX_FILE_BYTES {
            return Err(error(
                "FILE_TOO_LARGE",
                format!("bundle 文件超过 4 MiB：{relative}"),
                "archive",
            ));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| error("BUNDLE_LIMIT_EXCEEDED", "bundle 总大小溢出", "archive"))?;
        if total > MAX_BUNDLE_TOTAL_BYTES || files.len() >= MAX_BUNDLE_FILES {
            return Err(error(
                "BUNDLE_LIMIT_EXCEEDED",
                "bundle 超过 2000 文件或 64 MiB 限制",
                "archive",
            ));
        }
        let file = archive
            .by_index(entry.index)
            .map_err(|_| error("INVALID_ARCHIVE", "无法读取 bundle 文件", "archive"))?;
        let mut content = Vec::with_capacity(size);
        file.take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut content)
            .map_err(|_| error("ARCHIVE_READ_FAILED", "解压 bundle 文件失败", "archive"))?;
        if content.len() != size || content.len() > MAX_FILE_BYTES {
            return Err(error(
                "ARCHIVE_SIZE_MISMATCH",
                format!("bundle 文件大小与 ZIP 元数据不一致：{relative}"),
                "archive",
            ));
        }
        if std::str::from_utf8(&content).is_ok_and(|text| text.contains("${CLAUDE_PLUGIN_ROOT}")) {
            return Err(error(
                "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY",
                format!("bundle 文件依赖 CLAUDE_PLUGIN_ROOT：{relative}"),
                "dependency_scan",
            ));
        }
        files.push(PackageFile {
            path: PathBuf::from(relative),
            content,
            executable: entry.mode.is_some_and(|mode| mode & 0o111 != 0),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let member_names = files
        .iter()
        .filter_map(|file| {
            let path = file.path.to_str()?;
            let (name, tail) = path.split_once('/')?;
            (tail == "SKILL.md").then(|| name.to_string())
        })
        .collect::<BTreeSet<_>>();
    if member_names.is_empty() {
        return Err(error(
            "BUNDLE_STRUCTURE_UNSUPPORTED",
            "bundle 集合根没有直接 Skill 子目录",
            "archive",
        ));
    }
    let mut members = Vec::new();
    for name in &member_names {
        let skill_name = validate_skill_name(name)?;
        let prefix = format!("{name}/");
        let member_files = files
            .iter()
            .filter_map(|file| {
                let relative = file.path.to_str()?.strip_prefix(&prefix)?;
                Some(PackageFile {
                    path: PathBuf::from(relative),
                    content: file.content.clone(),
                    executable: file.executable,
                })
            })
            .collect::<Vec<_>>();
        members.push(BundleMember {
            skill_name,
            member_path: PathBuf::from(name),
            content_sha256: canonical_content_sha256(&member_files),
        });
    }
    let support_paths = files
        .iter()
        .filter_map(|file| file.path.components().next())
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|name| !member_names.contains(*name))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let content_sha256 = canonical_content_sha256(&files);
    Ok(ValidatedBundle {
        bundle_name: validate_bundle_name(bundle_name)?,
        collection_path: collection_path.to_string(),
        files,
        members,
        support_paths,
        content_sha256,
    })
}

fn unique_outer_directory(entries: &[EntryMeta]) -> Option<String> {
    let roots = entries
        .iter()
        .filter_map(|entry| entry.path.split('/').next())
        .collect::<BTreeSet<_>>();
    (roots.len() == 1).then(|| roots.into_iter().next().unwrap().to_string())
}

fn direct_skill_children(entries: &[EntryMeta], prefix: &str) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| {
            let relative = entry.path.strip_prefix(prefix)?;
            let (name, tail) = relative.split_once('/')?;
            (!entry.directory && tail == "SKILL.md").then(|| name.to_string())
        })
        .collect()
}

fn direct_skill_children_logical(
    entries: &[(&EntryMeta, &str)],
    collection_path: &str,
) -> BTreeSet<String> {
    let prefix = path_prefix(collection_path);
    entries
        .iter()
        .filter_map(|(entry, logical)| {
            let relative = logical.strip_prefix(&prefix)?;
            let (name, tail) = relative.split_once('/')?;
            (!entry.directory && tail == "SKILL.md").then(|| name.to_string())
        })
        .collect()
}

fn collection_has_direct_skills(entries: &[(&EntryMeta, &str)], path: &str) -> bool {
    !direct_skill_children_logical(entries, path).is_empty()
}

fn plugin_skill_candidates(
    bytes: &[u8],
    entries: &[EntryMeta],
    base_prefix: &str,
    explicit_path: &str,
) -> Result<BTreeSet<String>, InstallError> {
    let mut plugin_roots = BTreeSet::new();
    plugin_roots.insert(explicit_path.trim_matches('/').to_string());
    if explicit_path.is_empty() {
        if let Some(marketplace) = read_json_entry(
            bytes,
            entries,
            &format!("{base_prefix}.claude-plugin/marketplace.json"),
        )? {
            let local_sources = marketplace
                .get("plugins")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|plugin| plugin.get("source").and_then(Value::as_str))
                .filter_map(normalize_local_plugin_source)
                .collect::<BTreeSet<_>>();
            if local_sources.len() > 1 {
                return Err(error(
                    "MULTIPLE_BUNDLE_CANDIDATES",
                    "marketplace 包含多个本地 plugin source",
                    "archive",
                ));
            }
            plugin_roots.extend(local_sources);
        }
    }
    let mut candidates = BTreeSet::new();
    for root in plugin_roots {
        let plugin_path = if root.is_empty() {
            format!("{base_prefix}.claude-plugin/plugin.json")
        } else {
            format!("{}{}/.claude-plugin/plugin.json", base_prefix, root)
        };
        let Some(plugin) = read_json_entry(bytes, entries, &plugin_path)? else {
            continue;
        };
        if plugin.get("hooks").is_some()
            || plugin.get("mcpServers").is_some()
            || plugin.get("agents").is_some()
        {
            return Err(error(
                "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY",
                "Plugin 声明了 hooks、MCP 或 agents runtime",
                "dependency_scan",
            ));
        }
        if let Some(skills) = plugin.get("skills") {
            let paths = match skills {
                Value::String(path) => vec![path.as_str()],
                Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for path in paths {
                let relative = normalize_plugin_relative(path).ok_or_else(|| {
                    error(
                        "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY",
                        "plugin.json skills 路径不是集合根内的本地相对路径",
                        "dependency_scan",
                    )
                })?;
                candidates.insert(join_logical(&root, &relative));
            }
        }
    }
    Ok(candidates)
}

fn plugin_name(
    bytes: &[u8],
    entries: &[EntryMeta],
    base_prefix: &str,
    explicit_path: &str,
) -> Result<Option<String>, InstallError> {
    let path = if explicit_path.is_empty() {
        format!("{base_prefix}.claude-plugin/plugin.json")
    } else {
        format!(
            "{}{}/.claude-plugin/plugin.json",
            base_prefix,
            explicit_path.trim_matches('/')
        )
    };
    Ok(read_json_entry(bytes, entries, &path)?.and_then(|value| {
        value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
    }))
}

fn read_json_entry(
    bytes: &[u8],
    entries: &[EntryMeta],
    path: &str,
) -> Result<Option<Value>, InstallError> {
    let Some(entry) = entries
        .iter()
        .find(|entry| !entry.directory && entry.path == path)
    else {
        return Ok(None);
    };
    if entry.size as usize > MAX_FILE_BYTES {
        return Err(error(
            "FILE_TOO_LARGE",
            format!("Plugin manifest 超过 4 MiB：{path}"),
            "archive",
        ));
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| error("INVALID_ARCHIVE", "无法读取 Plugin manifest", "archive"))?;
    let mut body = Vec::new();
    archive
        .by_index(entry.index)
        .map_err(|_| error("INVALID_ARCHIVE", "无法读取 Plugin manifest", "archive"))?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| {
            error(
                "ARCHIVE_READ_FAILED",
                "读取 Plugin manifest 失败",
                "archive",
            )
        })?;
    serde_json::from_slice(&body).map(Some).map_err(|_| {
        error(
            "BUNDLE_STRUCTURE_UNSUPPORTED",
            "Plugin manifest JSON 非法",
            "archive",
        )
    })
}

fn normalize_local_plugin_source(value: &str) -> Option<String> {
    let value = value.strip_prefix("./").unwrap_or(value);
    normalize_plugin_relative(value)
}

fn normalize_plugin_relative(value: &str) -> Option<String> {
    let value = value.trim_matches('/');
    if value.is_empty() || value == "." {
        return Some(String::new());
    }
    validate_relative_payload_path(value).ok()?;
    Some(value.to_string())
}

fn path_prefix(path: &str) -> String {
    if path.trim_matches('/').is_empty() {
        String::new()
    } else {
        format!("{}/", path.trim_matches('/'))
    }
}

fn join_prefix(base: &str, path: &str) -> String {
    format!("{base}{}", path_prefix(path))
}

fn join_logical(left: &str, right: &str) -> String {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => String::new(),
        (true, false) => right.to_string(),
        (false, true) => left.to_string(),
        (false, false) => format!("{left}/{right}"),
    }
}

fn validate_bundle_name(name: &str) -> Result<String, InstallError> {
    let value = name.trim();
    if value.is_empty() || value.len() > 120 || value.contains(['/', '\\', '\0']) {
        return Err(error(
            "BUNDLE_STRUCTURE_UNSUPPORTED",
            "bundle 名称非法",
            "archive",
        ));
    }
    Ok(value.to_string())
}

#[cfg(test)]
pub(crate) fn package_from_github_archive(
    bytes: &[u8],
    source_path: &str,
    skill_name: &str,
) -> Result<ValidatedPackage, InstallError> {
    let entries = inspect_archive(bytes)?;
    let mut roots = BTreeSet::new();
    for entry in &entries {
        if let Some((root, _)) = entry.path.split_once('/') {
            roots.insert(root.to_string());
        }
    }
    if roots.len() != 1 {
        return Err(error(
            "INVALID_GITHUB_ARCHIVE",
            "GitHub archive 顶层目录结构非法",
            "archive",
        ));
    }
    let root = roots.into_iter().next().expect("one root");
    let prefix = format!("{root}/{}/", source_path.trim_matches('/'));
    let package = extract_selected(bytes, &entries, skill_name, &prefix)?;
    if package
        .files
        .iter()
        .all(|file| file.path != Path::new("SKILL.md"))
    {
        return Err(error(
            "SKILL_MD_MISSING",
            "GitHub 目录顶层缺少 SKILL.md",
            "archive",
        ));
    }
    Ok(package)
}

pub(crate) fn package_from_github_files(
    skill_name: &str,
    mut files: Vec<PackageFile>,
) -> Result<ValidatedPackage, InstallError> {
    files.retain(|file| {
        file.path
            .to_str()
            .is_some_and(|path| !ignored_metadata(path))
    });
    if files.len() > MAX_FILES {
        return Err(error(
            "PACKAGE_LIMIT_EXCEEDED",
            "Skill 超过 512 文件限制",
            "archive",
        ));
    }
    let mut collision_keys = BTreeMap::<String, (String, bool)>::new();
    let mut total = 0usize;
    for file in &files {
        let relative = file.path.to_str().ok_or_else(|| {
            error(
                "UNSAFE_ARCHIVE_PATH",
                "GitHub Skill 路径必须使用 UTF-8",
                "archive",
            )
        })?;
        validate_relative_payload_path(relative)?;
        let basename = relative.rsplit('/').next().unwrap_or(relative);
        if matches!(basename, IMPORT_ORIGIN_FILE | CATALOG_STAMP_FILE) {
            return Err(error(
                "RESERVED_MARKER_IN_SOURCE",
                format!("Skill 来源不得包含 {basename}"),
                "archive",
            ));
        }
        register_payload_path(&mut collision_keys, relative)?;
        if file.content.len() > MAX_FILE_BYTES {
            return Err(error(
                "FILE_TOO_LARGE",
                format!("Skill 文件超过 4 MiB：{relative}"),
                "archive",
            ));
        }
        total = total
            .checked_add(file.content.len())
            .ok_or_else(|| error("PACKAGE_TOO_LARGE", "Skill 总大小溢出", "archive"))?;
        if total > MAX_TOTAL_BYTES {
            return Err(error(
                "PACKAGE_LIMIT_EXCEEDED",
                "Skill 超过 32 MiB 限制",
                "archive",
            ));
        }
    }
    if files.is_empty() || files.iter().all(|file| file.path != Path::new("SKILL.md")) {
        return Err(error(
            "SKILL_MD_MISSING",
            "GitHub 目录顶层缺少 SKILL.md",
            "archive",
        ));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let evidence = scan_shared_dependencies(&files);
    if !evidence.is_empty() {
        return Err(error(
            "UNSUPPORTED_SHARED_DEPENDENCY",
            "Skill 引用了所选目录之外的共享文件",
            "dependency_scan",
        )
        .with_evidence(evidence));
    }
    let content_sha256 = canonical_content_sha256(&files);
    Ok(ValidatedPackage {
        skill_name: validate_skill_name(skill_name)?,
        files,
        content_sha256,
    })
}

pub(crate) fn inspect_archive(bytes: &[u8]) -> Result<Vec<EntryMeta>, InstallError> {
    validate_central_directory_paths(bytes)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| {
        error(
            "INVALID_ARCHIVE",
            "ZIP central directory 非法或不受支持",
            "archive",
        )
    })?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(error(
            "ARCHIVE_ENTRY_LIMIT",
            "archive 条目数超过 10000",
            "archive",
        ));
    }
    let mut entries = Vec::with_capacity(archive.len());
    let mut seen = BTreeSet::new();
    for index in 0..archive.len() {
        let file = archive.by_index(index).map_err(|_| {
            error(
                "INVALID_ARCHIVE",
                "无法读取 ZIP central directory 条目",
                "archive",
            )
        })?;
        let raw_name = std::str::from_utf8(file.name_raw()).map_err(|_| {
            error(
                "UNSAFE_ARCHIVE_PATH",
                "archive 路径必须使用 UTF-8",
                "archive",
            )
        })?;
        let path = validate_archive_path(raw_name)?;
        if path.is_empty() || ignored_metadata(&path) {
            continue;
        }
        let directory = file.is_dir() || raw_name.ends_with('/');
        if !seen.insert(path.clone()) {
            return Err(error(
                "DUPLICATE_ARCHIVE_PATH",
                format!("archive 包含重复路径：{path}"),
                "archive",
            ));
        }
        validate_unix_mode(file.unix_mode(), directory, &path)?;
        entries.push(EntryMeta {
            index,
            path,
            size: file.size(),
            mode: file.unix_mode(),
            directory,
        });
    }
    Ok(entries)
}

fn validate_central_directory_paths(bytes: &[u8]) -> Result<(), InstallError> {
    const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
    const CENTRAL_SIGNATURE: &[u8; 4] = b"PK\x01\x02";
    let search_start = bytes.len().saturating_sub(65_557);
    let eocd = bytes[search_start..]
        .windows(EOCD_SIGNATURE.len())
        .rposition(|window| window == EOCD_SIGNATURE)
        .map(|position| search_start + position)
        .ok_or_else(|| error("INVALID_ARCHIVE", "ZIP 缺少 central directory", "archive"))?;
    if eocd + 22 > bytes.len() {
        return Err(error(
            "INVALID_ARCHIVE",
            "ZIP central directory footer 截断",
            "archive",
        ));
    }
    let disk = read_u16(bytes, eocd + 4)?;
    let central_disk = read_u16(bytes, eocd + 6)?;
    let entries_on_disk = read_u16(bytes, eocd + 8)?;
    let entry_count = read_u16(bytes, eocd + 10)?;
    let central_size = read_u32(bytes, eocd + 12)?;
    let central_offset = read_u32(bytes, eocd + 16)?;
    let comment_len = read_u16(bytes, eocd + 20)? as usize;
    if disk != 0
        || central_disk != 0
        || entries_on_disk != entry_count
        || entry_count == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX
        || eocd + 22 + comment_len != bytes.len()
    {
        return Err(error(
            "INVALID_ARCHIVE",
            "不支持多卷、ZIP64 或尾随数据的 Skill archive",
            "archive",
        ));
    }
    let entry_count = entry_count as usize;
    if entry_count > MAX_ARCHIVE_ENTRIES {
        return Err(error(
            "ARCHIVE_ENTRY_LIMIT",
            "archive 条目数超过 10000",
            "archive",
        ));
    }
    let mut cursor = central_offset as usize;
    let central_end = cursor
        .checked_add(central_size as usize)
        .filter(|end| *end <= eocd)
        .ok_or_else(|| error("INVALID_ARCHIVE", "ZIP central directory 越界", "archive"))?;
    let mut seen = BTreeSet::new();
    for _ in 0..entry_count {
        if cursor + 46 > central_end || &bytes[cursor..cursor + 4] != CENTRAL_SIGNATURE {
            return Err(error(
                "INVALID_ARCHIVE",
                "ZIP central directory 条目截断",
                "archive",
            ));
        }
        let name_len = read_u16(bytes, cursor + 28)? as usize;
        let extra_len = read_u16(bytes, cursor + 30)? as usize;
        let comment_len = read_u16(bytes, cursor + 32)? as usize;
        let entry_end = cursor
            .checked_add(46 + name_len + extra_len + comment_len)
            .filter(|end| *end <= central_end)
            .ok_or_else(|| {
                error(
                    "INVALID_ARCHIVE",
                    "ZIP central directory 名称越界",
                    "archive",
                )
            })?;
        let raw_name =
            std::str::from_utf8(&bytes[cursor + 46..cursor + 46 + name_len]).map_err(|_| {
                error(
                    "UNSAFE_ARCHIVE_PATH",
                    "archive 路径必须使用 UTF-8",
                    "archive",
                )
            })?;
        let path = validate_archive_path(raw_name)?;
        if !path.is_empty() && !ignored_metadata(&path) && !seen.insert(path.clone()) {
            return Err(error(
                "DUPLICATE_ARCHIVE_PATH",
                format!("archive 包含重复路径：{path}"),
                "archive",
            ));
        }
        cursor = entry_end;
    }
    if cursor != central_end {
        return Err(error(
            "INVALID_ARCHIVE",
            "ZIP central directory 大小不一致",
            "archive",
        ));
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, InstallError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| error("INVALID_ARCHIVE", "ZIP 元数据截断", "archive"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, InstallError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| error("INVALID_ARCHIVE", "ZIP 元数据截断", "archive"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn validate_archive_path(raw: &str) -> Result<String, InstallError> {
    let path = raw.trim_end_matches('/');
    if path.is_empty() {
        return Ok(String::new());
    }
    if path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.len() > MAX_PATH_BYTES
    {
        return Err(error(
            "UNSAFE_ARCHIVE_PATH",
            "archive 包含绝对、反斜杠、NUL 或过长路径",
            "archive",
        ));
    }
    if path.nfc().collect::<String>() != path {
        return Err(error(
            "NON_NFC_ARCHIVE_PATH",
            "archive 路径必须是 Unicode NFC",
            "archive",
        ));
    }
    let components = path.split('/').collect::<Vec<_>>();
    if components.len() > MAX_PATH_DEPTH
        || components
            .iter()
            .any(|part| part.is_empty() || matches!(*part, "." | ".."))
    {
        return Err(error(
            "UNSAFE_ARCHIVE_PATH",
            "archive 包含越界、空组件或过深路径",
            "archive",
        ));
    }
    Ok(path.to_string())
}

fn validate_unix_mode(mode: Option<u32>, directory: bool, path: &str) -> Result<(), InstallError> {
    let Some(mode) = mode else {
        return Ok(());
    };
    let kind = mode & 0o170000;
    let valid = if directory {
        kind == 0 || kind == 0o040000
    } else {
        kind == 0 || kind == 0o100000
    };
    if !valid {
        return Err(error(
            "UNSUPPORTED_ARCHIVE_ENTRY",
            format!("archive 包含链接或特殊文件：{path}"),
            "archive",
        ));
    }
    Ok(())
}

fn ignored_metadata(path: &str) -> bool {
    path.split('/').any(|component| component == "__MACOSX")
        || path.rsplit('/').next() == Some(".DS_Store")
}

fn register_payload_path(
    collision_keys: &mut BTreeMap<String, (String, bool)>,
    relative: &str,
) -> Result<(), InstallError> {
    let components = relative.split('/').collect::<Vec<_>>();
    let mut prefix = String::new();
    for (index, component) in components.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        let is_file = index + 1 == components.len();
        let key = prefix.nfc().collect::<String>().to_lowercase();
        if let Some((previous, previous_is_file)) = collision_keys.get(&key) {
            if previous != &prefix {
                return Err(error(
                    "PATH_COLLISION",
                    format!("Skill 包含大小写或 Unicode 冲突路径：{previous} / {prefix}"),
                    "archive",
                ));
            }
            if *previous_is_file != is_file {
                return Err(error(
                    "PATH_TYPE_CONFLICT",
                    format!("Skill 路径同时作为文件和目录：{prefix}"),
                    "archive",
                ));
            }
        } else {
            collision_keys.insert(key, (prefix.clone(), is_file));
        }
    }
    Ok(())
}

fn extract_selected(
    bytes: &[u8],
    entries: &[EntryMeta],
    skill_name: &str,
    prefix: &str,
) -> Result<ValidatedPackage, InstallError> {
    let mut selected = Vec::new();
    let mut collision_keys = BTreeMap::<String, (String, bool)>::new();
    let mut total = 0usize;
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| error("INVALID_ARCHIVE", "无法重新打开 ZIP archive", "archive"))?;
    for entry in entries {
        if entry.directory || !entry.path.starts_with(prefix) {
            continue;
        }
        let relative = &entry.path[prefix.len()..];
        if relative.is_empty() || ignored_metadata(relative) {
            continue;
        }
        validate_relative_payload_path(relative)?;
        let basename = relative.rsplit('/').next().unwrap_or(relative);
        if matches!(basename, IMPORT_ORIGIN_FILE | CATALOG_STAMP_FILE) {
            return Err(error(
                "RESERVED_MARKER_IN_SOURCE",
                format!("Skill 来源不得包含 {basename}"),
                "archive",
            ));
        }
        register_payload_path(&mut collision_keys, relative)?;
        let size = usize::try_from(entry.size).unwrap_or(usize::MAX);
        if size > MAX_FILE_BYTES {
            return Err(error(
                "FILE_TOO_LARGE",
                format!("Skill 文件超过 4 MiB：{relative}"),
                "archive",
            ));
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| error("PACKAGE_TOO_LARGE", "Skill 总大小溢出", "archive"))?;
        if total > MAX_TOTAL_BYTES || selected.len() >= MAX_FILES {
            return Err(error(
                "PACKAGE_LIMIT_EXCEEDED",
                "Skill 超过 512 文件或 32 MiB 限制",
                "archive",
            ));
        }
        let file = archive
            .by_index(entry.index)
            .map_err(|_| error("INVALID_ARCHIVE", "无法读取 Skill 文件", "archive"))?;
        let mut content = Vec::with_capacity(size);
        file.take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut content)
            .map_err(|_| error("ARCHIVE_READ_FAILED", "解压 Skill 文件失败", "archive"))?;
        if content.len() != size || content.len() > MAX_FILE_BYTES {
            return Err(error(
                "ARCHIVE_SIZE_MISMATCH",
                format!("Skill 文件大小与 ZIP 元数据不一致：{relative}"),
                "archive",
            ));
        }
        let path = PathBuf::from(relative);
        selected.push(PackageFile {
            path,
            content,
            executable: entry.mode.is_some_and(|mode| mode & 0o111 != 0),
        });
    }
    if selected.is_empty()
        || selected
            .iter()
            .all(|file| file.path != Path::new("SKILL.md"))
    {
        return Err(error(
            "SKILL_MD_MISSING",
            "Skill 顶层缺少 SKILL.md",
            "archive",
        ));
    }
    selected.sort_by(|left, right| left.path.cmp(&right.path));
    let evidence = scan_shared_dependencies(&selected);
    if !evidence.is_empty() {
        return Err(error(
            "UNSUPPORTED_SHARED_DEPENDENCY",
            "Skill 引用了所选目录之外的共享文件",
            "dependency_scan",
        )
        .with_evidence(evidence));
    }
    let content_sha256 = canonical_content_sha256(&selected);
    Ok(ValidatedPackage {
        skill_name: validate_skill_name(skill_name)?,
        files: selected,
        content_sha256,
    })
}

fn validate_relative_payload_path(path: &str) -> Result<(), InstallError> {
    if path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.nfc().collect::<String>() != path
        || path.len() > MAX_PATH_BYTES
        || path.split('/').count() > MAX_PATH_DEPTH
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(error(
            "UNSAFE_ARCHIVE_PATH",
            "Skill payload 路径非法",
            "archive",
        ));
    }
    Ok(())
}

pub(crate) fn validate_skill_name(name: &str) -> Result<String, InstallError> {
    let valid = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$").expect("static regex");
    if !valid.is_match(name) || matches!(name, "." | "..") {
        return Err(error(
            "INVALID_SKILL_NAME",
            "Skill 名称必须是 1-80 位 ASCII 字母、数字、点、下划线或连字符",
            "archive",
        ));
    }
    Ok(name.to_string())
}

pub(crate) fn canonical_content_sha256(files: &[PackageFile]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"csswitch-skill-content-v1\0");
    let mut sorted = files.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| left.path.cmp(&right.path));
    for file in sorted {
        let path = file.path.to_string_lossy();
        hash.update((path.len() as u64).to_be_bytes());
        hash.update(path.as_bytes());
        hash.update([u8::from(file.executable)]);
        hash.update((file.content.len() as u64).to_be_bytes());
        hash.update(&file.content);
    }
    format!("{:x}", hash.finalize())
}

fn scan_shared_dependencies(files: &[PackageFile]) -> Vec<DependencyEvidence> {
    let markdown_link = Regex::new(r"\]\(([^)\s]+)").expect("static regex");
    let quoted_path =
        Regex::new(r#"[\"']([^\"']*(?:\.\./|_shared/)[^\"']*)[\"']"#).expect("static regex");
    let yaml_path = Regex::new(r":\s*(\.\./[^\s#]+)").expect("static regex");
    let mut evidence = Vec::new();
    for file in files {
        let relative = file.path.to_string_lossy();
        let is_skill = relative == "SKILL.md";
        let is_script = file
            .path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| {
                matches!(
                    extension,
                    "py" | "sh" | "bash" | "zsh" | "js" | "mjs" | "cjs" | "ts" | "r" | "R" | "jl"
                )
            });
        if !is_skill && !is_script {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&file.content) else {
            continue;
        };
        let lines = text.lines().collect::<Vec<_>>();
        let frontmatter_end = (lines.first().is_some_and(|line| line.trim() == "---"))
            .then(|| {
                lines
                    .iter()
                    .enumerate()
                    .skip(1)
                    .find_map(|(index, line)| (line.trim() == "---").then_some(index))
            })
            .flatten();
        for (line_index, line) in lines.into_iter().enumerate() {
            let mut candidates = Vec::new();
            if is_skill {
                candidates.extend(
                    markdown_link
                        .captures_iter(line)
                        .filter_map(|capture| capture.get(1)),
                );
                if line_index > 0 && frontmatter_end.is_some_and(|end| line_index < end) {
                    candidates.extend(
                        yaml_path
                            .captures_iter(line)
                            .filter_map(|capture| capture.get(1)),
                    );
                    candidates.extend(
                        quoted_path
                            .captures_iter(line)
                            .filter_map(|capture| capture.get(1)),
                    );
                }
            } else if is_script {
                candidates.extend(
                    quoted_path
                        .captures_iter(line)
                        .filter_map(|capture| capture.get(1)),
                );
            }
            for candidate in candidates {
                let value = candidate
                    .as_str()
                    .trim_matches(['<', '>'])
                    .split(['#', '?'])
                    .next()
                    .unwrap_or("");
                if reference_escapes_skill(file.path.parent().unwrap_or(Path::new("")), value) {
                    evidence.push(DependencyEvidence {
                        file: relative.to_string(),
                        line: line_index + 1,
                        column: candidate.start() + 1,
                        pattern: "relative-path-outside-skill".to_string(),
                    });
                }
            }
        }
    }
    evidence
}

fn reference_escapes_skill(base: &Path, reference: &str) -> bool {
    if reference.starts_with('/')
        || reference.starts_with("http://")
        || reference.starts_with("https://")
        || reference.starts_with('#')
    {
        return false;
    }
    let mut depth = base.components().count() as isize;
    for component in reference.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            _ => depth += 1,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn zip_bytes(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        for (name, content, mode) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default().unix_permissions(*mode))
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn error_code(entries: &[(&str, &[u8], u32)]) -> String {
        package_from_local_archive(&zip_bytes(entries), "demo")
            .unwrap_err()
            .code
    }

    fn replace_all_same_length(bytes: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        let mut offset = 0;
        while let Some(found) = bytes[offset..]
            .windows(from.len())
            .position(|window| window == from)
        {
            let start = offset + found;
            bytes[start..start + to.len()].copy_from_slice(to);
            offset = start + to.len();
        }
    }

    #[test]
    fn canonical_hash_binds_path_mode_and_content() {
        let base = vec![PackageFile {
            path: PathBuf::from("SKILL.md"),
            content: b"hello".to_vec(),
            executable: false,
        }];
        let mut changed = base.clone();
        changed[0].executable = true;
        assert_ne!(
            canonical_content_sha256(&base),
            canonical_content_sha256(&changed)
        );
        let mut reversed = vec![
            PackageFile {
                path: PathBuf::from("z.txt"),
                content: b"z".to_vec(),
                executable: false,
            },
            base[0].clone(),
        ];
        let first = canonical_content_sha256(&reversed);
        reversed.reverse();
        assert_eq!(first, canonical_content_sha256(&reversed));
        changed[0].executable = false;
        changed[0].content.push(b'!');
        assert_ne!(
            canonical_content_sha256(&base),
            canonical_content_sha256(&changed)
        );
    }

    #[test]
    fn escaping_reference_detection_is_conservative() {
        assert!(reference_escapes_skill(Path::new(""), "../_shared/x.py"));
        assert!(!reference_escapes_skill(
            Path::new("scripts"),
            "../data/x.json"
        ));
        assert!(!reference_escapes_skill(
            Path::new(""),
            "https://example.com/x"
        ));
    }

    #[test]
    fn local_archive_recognizes_root_and_one_directory_layouts() {
        let root = zip_bytes(&[
            ("SKILL.md", b"# Demo", 0o644),
            ("scripts/run.sh", b"echo ok", 0o755),
            ("__MACOSX/ignored", b"x", 0o644),
            (".DS_Store", b"x", 0o644),
        ]);
        let package = package_from_local_archive(&root, "demo").unwrap();
        assert_eq!(package.skill_name, "demo");
        assert_eq!(package.files.len(), 2);
        assert!(
            package
                .files
                .iter()
                .find(|file| file.path == Path::new("scripts/run.sh"))
                .unwrap()
                .executable
        );

        let nested = zip_bytes(&[("nested/SKILL.md", b"# Demo", 0o644)]);
        assert_eq!(
            package_from_local_archive(&nested, "ignored")
                .unwrap()
                .skill_name,
            "nested"
        );
    }

    #[test]
    fn archive_rejects_ambiguous_roots_reserved_markers_and_collisions() {
        assert_eq!(
            error_code(&[
                ("one/SKILL.md", b"one", 0o644),
                ("two/SKILL.md", b"two", 0o644),
            ]),
            "AMBIGUOUS_SKILL_ROOT"
        );
        assert_eq!(
            error_code(&[
                ("SKILL.md", b"demo", 0o644),
                (".import-origin", b"{}", 0o644),
            ]),
            "RESERVED_MARKER_IN_SOURCE"
        );
        assert_eq!(
            error_code(&[
                ("SKILL.md", b"demo", 0o644),
                ("A.txt", b"a", 0o644),
                ("a.txt", b"b", 0o644),
            ]),
            "PATH_COLLISION"
        );
        assert_eq!(
            error_code(&[
                ("SKILL.md", b"demo", 0o644),
                ("A/one.txt", b"a", 0o644),
                ("a/two.txt", b"b", 0o644),
            ]),
            "PATH_COLLISION"
        );
        assert_eq!(
            error_code(&[
                ("SKILL.md", b"demo", 0o644),
                ("scripts/run/file", b"child", 0o644),
                ("scripts/run", b"parent", 0o644),
            ]),
            "PATH_TYPE_CONFLICT"
        );
    }

    #[test]
    fn archive_rejects_duplicate_non_nfc_traversal_and_symlink_entries() {
        let mut duplicate = zip_bytes(&[("SKILL.md", b"one", 0o644), ("OTHER.md", b"two", 0o644)]);
        replace_all_same_length(&mut duplicate, b"OTHER.md", b"SKILL.md");
        assert_eq!(
            package_from_local_archive(&duplicate, "demo")
                .unwrap_err()
                .code,
            "DUPLICATE_ARCHIVE_PATH"
        );
        assert_eq!(
            validate_archive_path("../escape").unwrap_err().code,
            "UNSAFE_ARCHIVE_PATH"
        );
        assert_eq!(
            validate_archive_path("cafe\u{301}.txt").unwrap_err().code,
            "NON_NFC_ARCHIVE_PATH"
        );

        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        writer
            .start_file("SKILL.md", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"demo").unwrap();
        writer
            .add_symlink(
                "scripts/current",
                "../outside",
                SimpleFileOptions::default(),
            )
            .unwrap();
        let bytes = writer.finish().unwrap().into_inner();
        assert_eq!(
            package_from_local_archive(&bytes, "demo").unwrap_err().code,
            "UNSUPPORTED_ARCHIVE_ENTRY"
        );
    }

    #[test]
    fn package_limits_reject_excess_files_large_files_and_deep_paths() {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        writer
            .start_file("SKILL.md", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"demo").unwrap();
        for index in 0..MAX_FILES {
            writer
                .start_file(format!("files/{index}.txt"), SimpleFileOptions::default())
                .unwrap();
        }
        let too_many = writer.finish().unwrap().into_inner();
        assert_eq!(
            package_from_local_archive(&too_many, "demo")
                .unwrap_err()
                .code,
            "PACKAGE_LIMIT_EXCEEDED"
        );

        let large = vec![0_u8; MAX_FILE_BYTES + 1];
        assert_eq!(
            error_code(&[("SKILL.md", b"demo", 0o644), ("large.bin", &large, 0o644)]),
            "FILE_TOO_LARGE"
        );

        let deep = (0..=MAX_PATH_DEPTH)
            .map(|_| "a")
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(
            validate_archive_path(&deep).unwrap_err().code,
            "UNSAFE_ARCHIVE_PATH"
        );
    }

    #[test]
    fn dependency_scan_is_best_effort_and_ignores_readme_prose() {
        let safe = zip_bytes(&[
            (
                "SKILL.md",
                b"---\nname: demo\n---\n# Demo\nExample config: '../_shared/not-runtime'",
                0o644,
            ),
            ("README.md", b"example ../_shared/not-a-runtime-path", 0o644),
        ]);
        assert!(package_from_local_archive(&safe, "demo").is_ok());

        let unsafe_package = zip_bytes(&[("SKILL.md", b"[shared](../_shared/helper.py)", 0o644)]);
        let error = package_from_local_archive(&unsafe_package, "demo").unwrap_err();
        assert_eq!(error.code, "UNSUPPORTED_SHARED_DEPENDENCY");
        assert_eq!(error.evidence[0].file, "SKILL.md");
        assert_eq!(error.evidence[0].line, 1);

        let unsafe_frontmatter = zip_bytes(&[(
            "SKILL.md",
            b"---\nscript: '../_shared/helper.py'\n---\n# Demo",
            0o644,
        )]);
        let error = package_from_local_archive(&unsafe_frontmatter, "demo").unwrap_err();
        assert_eq!(error.code, "UNSUPPORTED_SHARED_DEPENDENCY");
        assert_eq!(error.evidence[0].line, 2);
    }

    #[test]
    fn github_archive_extracts_only_selected_subtree() {
        let bytes = zip_bytes(&[
            ("repo-sha/skills/demo/SKILL.md", b"demo", 0o644),
            ("repo-sha/skills/demo/run.sh", b"run", 0o755),
            ("repo-sha/skills/other/SKILL.md", b"other", 0o644),
        ]);
        let package = package_from_github_archive(&bytes, "skills/demo", "demo").unwrap();
        assert_eq!(package.files.len(), 2);
        assert!(package
            .files
            .iter()
            .all(|file| !file.path.starts_with("other")));
        assert!(
            package
                .files
                .iter()
                .find(|file| file.path == Path::new("run.sh"))
                .unwrap()
                .executable
        );
    }

    #[test]
    fn nature_like_bundle_keeps_members_and_shared_support() {
        let bytes = zip_bytes(&[
            (
                "nature-sha/.claude-plugin/plugin.json",
                br#"{"name":"nature-skills"}"#,
                0o644,
            ),
            (
                "nature-sha/skills/nature-reader/SKILL.md",
                b"# Reader",
                0o644,
            ),
            (
                "nature-sha/skills/nature-reader/scripts/run.py",
                b"from pathlib import Path",
                0o755,
            ),
            (
                "nature-sha/skills/nature-writer/SKILL.md",
                b"# Writer",
                0o644,
            ),
            (
                "nature-sha/skills/_shared/client.py",
                b"print('shared')",
                0o644,
            ),
        ]);
        let ValidatedArchive::Bundle(bundle) =
            package_or_bundle_from_github_archive(&bytes, "", "nature-skills").unwrap()
        else {
            panic!("expected bundle");
        };
        assert_eq!(bundle.bundle_name, "nature-skills");
        assert_eq!(bundle.collection_path, "skills");
        assert_eq!(
            bundle
                .members
                .iter()
                .map(|member| member.skill_name.as_str())
                .collect::<Vec<_>>(),
            vec!["nature-reader", "nature-writer"]
        );
        assert_eq!(bundle.support_paths, vec!["_shared"]);
        assert!(bundle
            .files
            .iter()
            .any(|file| file.path == Path::new("_shared/client.py")));
        assert!(
            bundle
                .files
                .iter()
                .find(|file| file.path == Path::new("nature-reader/scripts/run.py"))
                .unwrap()
                .executable
        );
    }

    #[test]
    fn bundle_candidate_ambiguity_and_runtime_dependency_are_rejected() {
        let ambiguous = zip_bytes(&[
            ("one/SKILL.md", b"one", 0o644),
            ("skills/two/SKILL.md", b"two", 0o644),
        ]);
        assert_eq!(
            package_or_bundle_from_local_archive(&ambiguous, "bundle")
                .unwrap_err()
                .code,
            "MULTIPLE_BUNDLE_CANDIDATES"
        );

        let runtime = zip_bytes(&[
            ("skills/one/SKILL.md", b"one", 0o644),
            ("skills/two/SKILL.md", b"two", 0o644),
            (
                "skills/_shared/run.sh",
                b"source ${CLAUDE_PLUGIN_ROOT}/env.sh",
                0o755,
            ),
        ]);
        assert_eq!(
            package_or_bundle_from_local_archive(&runtime, "bundle")
                .unwrap_err()
                .code,
            "UNSUPPORTED_PLUGIN_RUNTIME_DEPENDENCY"
        );
    }
}
