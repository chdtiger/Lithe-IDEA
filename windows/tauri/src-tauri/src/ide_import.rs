//! Imports another editor's recent-project list.
//!
//! Reads what each installed editor records for itself, without modifying any
//! of it: the VS Code family keeps `history.recentlyOpenedPathsList` in
//! `state.vscdb` (SQLite), Zed keeps its workspaces table in its own SQLite
//! database, and JetBrains keeps `recentProjects.xml`. Every source is
//! best-effort -- an absent, locked, or re-shaped store contributes nothing --
//! because the dialog is a list of candidates, not an error reporter.

use quick_xml::{events::Event, Reader};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One project another editor has listed, ready to be opened as a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportableIdeProject {
    pub name: String,
    pub path: String,
    pub source_id: String,
    pub source_name: String,
}

/// Upper bound per source: this feeds a chooser dialog, not an archive.
const MAX_PROJECTS_PER_SOURCE: usize = 100;

/// VS Code family editors, by their `%APPDATA%` directory and the source id
/// and display name the import dialog groups them under.
const VS_CODE_EDITORS: &[(&str, &str, &str)] = &[
    ("Code", "vscode", "VS Code"),
    ("Code - Insiders", "vscode-insiders", "VS Code"),
    ("VSCodium", "vscodium", "VS Code"),
    ("Cursor", "cursor", "Cursor"),
    ("Windsurf", "windsurf", "Windsurf"),
];

/// Zed release channels, by their database directory and the source id.
const ZED_CHANNELS: &[(&str, &str)] = &[
    ("0-stable", "zed"),
    ("0-preview", "zed-preview"),
    ("0-dev", "zed-dev"),
];

/// Lists the recent projects of every installed editor.
#[tauri::command]
pub fn get_importable_ide_projects() -> Vec<ImportableIdeProject> {
    let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) else {
        return Vec::new();
    };
    let local_appdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let user_profile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    scan_sources(&appdata, local_appdata.as_deref(), user_profile.as_deref())
}

/// Collects projects from every installed editor. Split from the environment
/// lookup so tests can point it at a prepared `%APPDATA%`-shaped directory.
fn scan_sources(
    appdata: &Path,
    local_appdata: Option<&Path>,
    user_profile: Option<&Path>,
) -> Vec<ImportableIdeProject> {
    let mut projects = Vec::new();
    let mut seen = HashSet::new();

    for (directory, source_id, source_name) in VS_CODE_EDITORS {
        push_projects(
            &mut projects,
            &mut seen,
            vs_code_recent_projects(&appdata.join(directory)),
            source_id,
            source_name,
        );
    }
    if let Some(local_appdata) = local_appdata {
        for (channel, source_id) in ZED_CHANNELS {
            push_projects(
                &mut projects,
                &mut seen,
                zed_recent_projects(
                    &local_appdata
                        .join("Zed")
                        .join("db")
                        .join(channel)
                        .join("db.sqlite"),
                ),
                source_id,
                "Zed",
            );
        }
    }
    push_projects(
        &mut projects,
        &mut seen,
        jetbrains_recent_projects(&appdata.join("JetBrains"), user_profile),
        "jetbrains",
        "JetBrains",
    );

    projects
}

/// Keeps first-seen order, drops paths an earlier source already listed and
/// directories that no longer exist, and stops at the per-source bound.
fn push_projects(
    projects: &mut Vec<ImportableIdeProject>,
    seen: &mut HashSet<String>,
    paths: Vec<String>,
    source_id: &str,
    source_name: &str,
) {
    for path in paths.into_iter().take(MAX_PROJECTS_PER_SOURCE) {
        let path = path.trim_end_matches(['\\', '/']).to_string();
        if path.is_empty() || !Path::new(&path).is_dir() {
            continue;
        }
        if !seen.insert(path.replace('/', "\\").to_lowercase()) {
            continue;
        }
        let Some(name) = Path::new(&path)
            .file_name()
            .and_then(|value| value.to_str())
            .map(str::to_string)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        projects.push(ImportableIdeProject {
            name,
            path,
            source_id: source_id.to_string(),
            source_name: source_name.to_string(),
        });
    }
}

/// Recent folders from one VS Code family editor, most recent first.
///
/// Modern versions keep the list inside `state.vscdb`, and each profile
/// carries its own store. Older versions kept the same document in
/// `storage.json`, which is read as a fallback.
fn vs_code_recent_projects(user_directory: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    let mut stores = vec![user_directory.join("globalStorage").join("state.vscdb")];
    if let Ok(profiles) = std::fs::read_dir(user_directory.join("profiles")) {
        let mut directories = profiles
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        directories.sort();
        for directory in directories {
            stores.push(directory.join("globalStorage").join("state.vscdb"));
        }
    }
    for store in stores {
        paths.extend(vs_code_recent_from_vscdb(&store));
    }
    paths.extend(vs_code_recent_from_storage_json(
        &user_directory.join("globalStorage").join("storage.json"),
    ));
    paths
}

fn vs_code_recent_from_vscdb(path: &Path) -> Vec<String> {
    if !path.is_file() {
        return Vec::new();
    }
    // Read-only: the editor may hold the database open right now.
    let Ok(connection) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection
        .prepare("SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList'")
    else {
        return Vec::new();
    };
    // VS Code stores the JSON as a BLOB; read bytes so either spelling works.
    let Ok(bytes) = statement.query_row([], |row| row.get::<_, Vec<u8>>(0)) else {
        return Vec::new();
    };
    let Ok(value) = String::from_utf8(bytes) else {
        return Vec::new();
    };
    vs_code_recent_entries(&value)
}

fn vs_code_recent_from_storage_json(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(list) = document
        .get("history")
        .and_then(|history| history.get("recentlyOpenedPathsList"))
    else {
        return Vec::new();
    };
    vs_code_paths_from_list(list)
}

fn vs_code_recent_entries(document: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(document) else {
        return Vec::new();
    };
    vs_code_paths_from_list(&value)
}

/// Folder and workspace entries only: a `fileUri` names one file, and a
/// remote URI (`vscode-remote://`, `file://wsl.localhost/...`) has no local
/// folder to open.
fn vs_code_paths_from_list(list: &serde_json::Value) -> Vec<String> {
    let Some(entries) = list.get("entries").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries {
        let uri = entry
            .get("folderUri")
            .or_else(|| {
                entry
                    .get("workspace")
                    .and_then(|workspace| workspace.get("configPath"))
            })
            .and_then(|value| value.as_str());
        let Some(uri) = uri else {
            continue;
        };
        if let Some(path) = local_path_from_file_uri(uri) {
            paths.push(path);
        }
    }
    paths
}

/// Converts a `file://` URI to a local path. Remote authorities such as
/// `wsl.localhost` describe another machine and are not importable here.
fn local_path_from_file_uri(uri: &str) -> Option<String> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    if url.host_str().is_some_and(|host| !host.is_empty()) {
        return None;
    }
    let path = url.to_file_path().ok()?;
    let text = path.to_string_lossy().into_owned();
    if text.trim().is_empty() {
        return None;
    }
    // Normalize the drive letter: `file:///d%3A` and `D:` name the same root,
    // and one spelling keeps de-duplication and display predictable.
    Some(match text.chars().next() {
        Some(drive) if drive.is_ascii_lowercase() && text.as_bytes().get(1) == Some(&b':') => {
            format!("{}{}", drive.to_ascii_uppercase(), &text[1..])
        }
        _ => text,
    })
}

/// Recent local workspaces from one Zed channel database, most recent first.
fn zed_recent_projects(database: &Path) -> Vec<String> {
    if !database.is_file() {
        return Vec::new();
    }
    let Ok(connection) = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT paths FROM workspaces WHERE remote_connection_id IS NULL ORDER BY timestamp DESC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for row in rows.flatten() {
        paths.extend(zed_paths_from_column(&row));
    }
    paths
}

/// Zed stores the workspace roots as one column; a single-root workspace is a
/// bare path, and a multi-root workspace has been seen as a JSON array.
fn zed_paths_from_column(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.starts_with('[') {
        if let Ok(paths) = serde_json::from_str::<Vec<String>>(trimmed) {
            return paths;
        }
    }
    vec![trimmed.to_string()]
}

/// Recent projects from every installed JetBrains product. The store is
/// `options/recentProjects.xml` under each product directory.
fn jetbrains_recent_projects(directory: &Path, user_profile: Option<&Path>) -> Vec<String> {
    let Ok(products) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut directories = products
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    directories.sort();
    let home = user_profile.map(|path| path.to_string_lossy().replace('\\', "/"));
    let mut paths = Vec::new();
    for product in directories {
        paths.extend(jetbrains_recent_from_xml(
            &product.join("options").join("recentProjects.xml"),
            home.as_deref(),
        ));
    }
    paths
}

fn jetbrains_recent_from_xml(path: &Path, user_home: Option<&str>) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    jetbrains_entries_from_document(&text, user_home)
}

/// Reads `RecentProjectsManager`'s `additionalInfo` map keys: they are the
/// project paths, with `$USER_HOME$` standing in for the user profile. A key
/// that still contains an unresolved macro is skipped, not guessed at.
fn jetbrains_entries_from_document(document: &str, user_home: Option<&str>) -> Vec<String> {
    let mut reader = Reader::from_str(document);
    reader.config_mut().trim_text(true);
    let mut inside_manager = false;
    let mut paths = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if element.local_name().as_ref() == b"component" {
                    inside_manager = attribute_value(&element, b"name").as_deref()
                        == Some("RecentProjectsManager");
                }
            }
            Ok(Event::End(element)) => {
                if element.local_name().as_ref() == b"component" {
                    inside_manager = false;
                }
            }
            Ok(Event::Empty(element)) => {
                if !inside_manager || element.local_name().as_ref() != b"entry" {
                    continue;
                }
                let Some(key) = attribute_value(&element, b"key") else {
                    continue;
                };
                let resolved = match key.strip_prefix("$USER_HOME$") {
                    Some(rest) => match user_home {
                        Some(home) => format!("{home}{rest}"),
                        None => continue,
                    },
                    None => key,
                };
                if resolved.contains('$') {
                    continue;
                }
                paths.push(resolved.replace('/', "\\"));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    paths
}

fn attribute_value(element: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.local_name().as_ref() == name)
        .and_then(|attribute| attribute.unescape_value().ok())
        .map(|value| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris_map_to_local_paths_and_skip_remote_authorities() {
        assert_eq!(
            local_path_from_file_uri("file:///d%3A/work/%E6%89%8B%E6%8C%81%E6%9C%BA"),
            Some(r"D:\work\手持机".to_string())
        );
        assert_eq!(
            local_path_from_file_uri("file:///c%3A/x"),
            Some(r"C:\x".to_string())
        );
        assert_eq!(
            local_path_from_file_uri("file://wsl.localhost/Ubuntu-24.04/home"),
            None
        );
        assert_eq!(
            local_path_from_file_uri("vscode-remote://ssh-remote+box/root"),
            None
        );
        assert_eq!(local_path_from_file_uri("https://example.invalid"), None);
    }

    #[test]
    fn vs_code_entries_keep_folders_and_workspaces_only() {
        let document = r#"{"entries":[
            {"folderUri":"file:///d%3A/work/app"},
            {"fileUri":"file:///d%3A/work/app/README.md"},
            {"workspace":{"configPath":"file:///d%3A/work/team.code-workspace"}},
            {"folderUri":"vscode-remote://ssh-remote+box/root"}
        ]}"#;
        assert_eq!(
            vs_code_recent_entries(document),
            vec![
                r"D:\work\app".to_string(),
                r"D:\work\team.code-workspace".to_string()
            ]
        );
    }

    #[test]
    fn zed_paths_column_reads_single_and_multi_root_values() {
        assert_eq!(
            zed_paths_from_column(r"D:\work\app"),
            vec![r"D:\work\app".to_string()]
        );
        assert_eq!(
            zed_paths_from_column(r#"["D:\\work\\a","D:\\work\\b"]"#),
            vec![r"D:\work\a".to_string(), r"D:\work\b".to_string()]
        );
        assert!(zed_paths_from_column("   ").is_empty());
    }

    #[test]
    fn jetbrains_entries_resolve_the_home_macro_and_skip_unknown_ones() {
        let document = r#"<application>
            <component name="RecentProjectsManager">
                <option name="additionalInfo">
                    <map>
                        <entry key="$USER_HOME$/IdeaProjects/alpha"/>
                        <entry key="D:/work/beta"/>
                        <entry key="$APPLICATION_HOME_DIR$/samples"/>
                    </map>
                </option>
            </component>
            <component name="Unrelated">
                <entry key="D:/ignored"/>
            </component>
        </application>"#;
        assert_eq!(
            jetbrains_entries_from_document(document, Some("C:/Users/dev")),
            vec![
                r"C:\Users\dev\IdeaProjects\alpha".to_string(),
                r"D:\work\beta".to_string()
            ]
        );
    }

    #[test]
    fn vscdb_recent_list_is_read_from_a_sqlite_store() {
        let directory = std::env::temp_dir().join(format!(
            "lithe-ide-import-{}-{}",
            std::process::id(),
            "vscdb"
        ));
        std::fs::create_dir_all(&directory).expect("scratch directory");
        let database = directory.join("state.vscdb");
        {
            let connection = Connection::open(&database).expect("scratch store");
            connection
                .execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);")
                .expect("schema");
            connection
                .execute(
                    "INSERT INTO ItemTable (key, value) VALUES ('history.recentlyOpenedPathsList', ?1)",
                    rusqlite::params![
                        br#"{"entries":[{"folderUri":"file:///d%3A/work/app"}]}"#.to_vec()
                    ],
                )
                .expect("row");
        }
        assert_eq!(
            vs_code_recent_from_vscdb(&database),
            vec![r"D:\work\app".to_string()]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_and_unreadable_stores_contribute_nothing() {
        let missing = std::env::temp_dir()
            .join("lithe-ide-import-missing-store")
            .join("state.vscdb");
        assert!(vs_code_recent_from_vscdb(&missing).is_empty());
        assert!(zed_recent_projects(&missing).is_empty());
        assert!(vs_code_recent_from_storage_json(&missing).is_empty());
    }
}
