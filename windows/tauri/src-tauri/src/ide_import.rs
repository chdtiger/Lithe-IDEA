//! Imports another editor's recent-project list.
//!
//! Reads what each installed editor records for itself, without modifying any
//! of it: the VS Code family keeps `history.recentlyOpenedPathsList` in
//! `state.vscdb` (SQLite) under each product's `User` directory, Zed keeps its
//! workspaces table in its own SQLite database, and JetBrains keeps
//! `recentProjects.xml`. Every source is best-effort -- an absent, locked, or
//! re-shaped store contributes nothing -- because the dialog is a list of
//! candidates, not an error reporter.
//!
//! Stores open read-only with a short busy timeout: the editor may hold its
//! database right now, and a missing list is an acceptable outcome.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// How long a read-only open waits for a locked database before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_millis(300);

/// VS Code family editors, by their `%APPDATA%` directory and the source id
/// and display name the import dialog groups them under. The `User` layer is
/// appended when scanning.
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
///
/// Each source keeps its own list: a path two editors both remember stays
/// listed under both, because the dialog imports only the source the user
/// picked, and dropping the later one would empty that pick.
fn scan_sources(
    appdata: &Path,
    local_appdata: Option<&Path>,
    user_profile: Option<&Path>,
) -> Vec<ImportableIdeProject> {
    let mut projects = Vec::new();

    for (directory, source_id, source_name) in VS_CODE_EDITORS {
        let mut seen = HashSet::new();
        push_projects(
            &mut projects,
            &mut seen,
            vs_code_recent_projects(&appdata.join(directory).join("User")),
            source_id,
            source_name,
        );
    }
    if let Some(local_appdata) = local_appdata {
        for (channel, source_id) in ZED_CHANNELS {
            let mut seen = HashSet::new();
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
    let mut seen = HashSet::new();
    push_projects(
        &mut projects,
        &mut seen,
        jetbrains_recent_projects(&appdata.join("JetBrains"), user_profile),
        "jetbrains",
        "JetBrains",
    );

    projects
}

/// Keeps first-seen order, drops paths this source already listed and
/// entries that are not local directories, and stops at the per-source
/// bound -- after filtering, so repeated or dead entries never consume the
/// budget of live ones.
///
/// Relative paths are refused: they would resolve against whatever the
/// process happens to have as its working directory. UNC and WSL paths are
/// refused for the same reason the VS Code family's remote authorities are:
/// they name another machine, not a folder this window can open.
fn push_projects(
    projects: &mut Vec<ImportableIdeProject>,
    seen: &mut HashSet<String>,
    paths: Vec<String>,
    source_id: &str,
    source_name: &str,
) {
    let mut added = 0;
    for path in paths {
        if added >= MAX_PROJECTS_PER_SOURCE {
            break;
        }
        let path = path.trim_end_matches(['\\', '/']).to_string();
        if path.is_empty() || !Path::new(&path).is_absolute() || path.starts_with("\\\\") {
            continue;
        }
        if !Path::new(&path).is_dir() {
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
        added += 1;
    }
}

/// Recent folders from one VS Code family editor's `User` directory, most
/// recent first.
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
    let _ = connection.busy_timeout(BUSY_TIMEOUT);
    let Ok(mut statement) = connection
        .prepare("SELECT value FROM ItemTable WHERE key = 'history.recentlyOpenedPathsList'")
    else {
        return Vec::new();
    };
    let Ok(value) = statement.query_row([], |row| sqlite_text(row.get_ref(0)?)) else {
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
/// remote URI (`vscode-remote://`, `file://wsl.localhost/...`) names another
/// machine.
///
/// A history entry for a multi-root window records the `.code-workspace`
/// file; its roots are what can be opened as folders.
fn vs_code_paths_from_list(list: &serde_json::Value) -> Vec<String> {
    let Some(entries) = list.get("entries").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries {
        if let Some(uri) = entry.get("folderUri").and_then(|value| value.as_str()) {
            if let Some(path) = local_path_from_file_uri(uri) {
                paths.push(path);
            }
            continue;
        }
        let config_uri = entry
            .get("workspace")
            .and_then(|workspace| workspace.get("configPath"))
            .and_then(|value| value.as_str());
        if let Some(config_path) = config_uri.and_then(local_path_from_file_uri) {
            paths.extend(workspace_folder_paths(&config_path));
        }
    }
    paths
}

/// The roots a `.code-workspace` file lists. Relative entries resolve
/// against the workspace file's own directory, the way VS Code resolves them.
fn workspace_folder_paths(config_path: &str) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(folders) = document.get("folders").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let base = Path::new(config_path).parent();
    folders
        .iter()
        .filter_map(|folder| folder.get("path").and_then(|value| value.as_str()))
        .filter_map(|path| {
            let path = Path::new(path);
            let resolved = if path.is_absolute() {
                Some(path.to_path_buf())
            } else {
                base.map(|base| base.join(path))
            };
            resolved.map(|path| path.to_string_lossy().replace('/', "\\"))
        })
        .collect()
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
    let _ = connection.busy_timeout(BUSY_TIMEOUT);
    let Ok(mut statement) = connection.prepare(
        "SELECT paths FROM workspaces WHERE remote_connection_id IS NULL ORDER BY timestamp DESC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| sqlite_text(row.get_ref(0)?)) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for row in rows.flatten() {
        paths.extend(zed_paths_from_column(&row));
    }
    paths
}

/// Zed's `PathList` serializes the workspace roots joined by newlines; a
/// single-root workspace is simply one line.
fn zed_paths_from_column(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Reads a SQLite cell that may be stored as either TEXT or BLOB. The column
/// declaration does not decide the storage class of each cell, and the
/// editors have written both spellings.
fn sqlite_text(value: ValueRef<'_>) -> rusqlite::Result<String> {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Ok(String::from_utf8_lossy(bytes).into_owned())
        }
        other => Err(rusqlite::Error::InvalidColumnType(
            0,
            "value".to_string(),
            other.data_type(),
        )),
    }
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
///
/// Real records wrap each key in `<entry><value><RecentProjectMetaInfo/></value>
/// </entry>`, so both the open and the self-closing form are read.
fn jetbrains_entries_from_document(document: &str, user_home: Option<&str>) -> Vec<String> {
    let mut reader = Reader::from_str(document);
    reader.config_mut().trim_text(true);
    let mut in_manager = false;
    let mut in_additional_info = false;
    let mut in_map = false;
    let mut paths = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"component" => {
                    in_manager = attribute_value(&element, b"name").as_deref()
                        == Some("RecentProjectsManager");
                }
                b"option" if in_manager && !in_map => {
                    in_additional_info =
                        attribute_value(&element, b"name").as_deref() == Some("additionalInfo");
                }
                b"map" if in_additional_info => in_map = true,
                b"entry" if in_map => {
                    collect_jetbrains_entry(&element, user_home, &mut paths);
                }
                _ => {}
            },
            Ok(Event::Empty(element)) => {
                if in_map && element.local_name().as_ref() == b"entry" {
                    collect_jetbrains_entry(&element, user_home, &mut paths);
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"component" => {
                    in_manager = false;
                    in_additional_info = false;
                    in_map = false;
                }
                b"map" => in_map = false,
                b"option" if !in_map => in_additional_info = false,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    paths
}

fn collect_jetbrains_entry(
    element: &BytesStart<'_>,
    user_home: Option<&str>,
    paths: &mut Vec<String>,
) {
    let Some(key) = attribute_value(element, b"key") else {
        return;
    };
    let resolved = match key.strip_prefix("$USER_HOME$") {
        Some(rest) => match user_home {
            Some(home) => format!("{home}{rest}"),
            None => return,
        },
        None => key,
    };
    if resolved.contains('$') {
        return;
    }
    paths.push(resolved.replace('/', "\\"));
}

fn attribute_value(element: &BytesStart<'_>, name: &[u8]) -> Option<String> {
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

    fn scratch_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("lithe-ide-import-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    fn create_project(root: &Path, name: &str) -> PathBuf {
        let project = root.join("projects").join(name);
        std::fs::create_dir_all(&project).expect("project directory");
        project
    }

    fn file_uri(path: &Path) -> String {
        url::Url::from_file_path(path)
            .expect("file path is absolute")
            .to_string()
    }

    fn write_vscdb(path: &Path, value: &str, as_blob: bool) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("store directory");
        let connection = Connection::open(path).expect("scratch store");
        connection
            .execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB);")
            .expect("schema");
        if as_blob {
            connection
                .execute(
                    "INSERT INTO ItemTable (key, value) VALUES ('history.recentlyOpenedPathsList', ?1)",
                    rusqlite::params![value.as_bytes().to_vec()],
                )
                .expect("row");
        } else {
            connection
                .execute(
                    "INSERT INTO ItemTable (key, value) VALUES ('history.recentlyOpenedPathsList', ?1)",
                    rusqlite::params![value],
                )
                .expect("row");
        }
    }

    fn write_zed_db(path: &Path, values: &[&str]) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("store directory");
        let connection = Connection::open(path).expect("scratch store");
        connection
            .execute_batch(
                "CREATE TABLE workspaces (paths TEXT, remote_connection_id INTEGER, timestamp TEXT);",
            )
            .expect("schema");
        for (index, value) in values.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO workspaces (paths, remote_connection_id, timestamp) VALUES (?1, NULL, ?2)",
                    rusqlite::params![value, format!("2026-10-0{index}")],
                )
                .expect("row");
        }
    }

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
    fn vs_code_entries_keep_folders_and_skip_files_and_remote_folders() {
        let document = r#"{"entries":[
            {"folderUri":"file:///d%3A/work/app"},
            {"fileUri":"file:///d%3A/work/app/README.md"},
            {"folderUri":"vscode-remote://ssh-remote+box/root"}
        ]}"#;
        assert_eq!(
            vs_code_recent_entries(document),
            vec![r"D:\work\app".to_string()]
        );
    }

    #[test]
    fn a_workspace_entry_resolves_to_the_folders_it_lists() {
        let root = scratch_root("workspace");
        let alpha = create_project(&root, "alpha");
        let beta = create_project(&root, "beta");
        let config = root.join("team.code-workspace");
        std::fs::write(
            &config,
            format!(
                r#"{{"folders":[{{"path":"projects/alpha"}},{{"path":"{}"}}]}}"#,
                beta.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .expect("workspace file");
        let document = format!(
            r#"{{"entries":[{{"workspace":{{"configPath":"{}"}}}}]}}"#,
            file_uri(&config)
        );
        assert_eq!(
            vs_code_recent_entries(&document),
            vec![
                alpha.to_string_lossy().into_owned(),
                beta.to_string_lossy().into_owned()
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn zed_paths_column_reads_newline_separated_roots() {
        assert_eq!(
            zed_paths_from_column("D:\\work\\a\nD:\\work\\b"),
            vec![r"D:\work\a".to_string(), r"D:\work\b".to_string()]
        );
        assert_eq!(
            zed_paths_from_column(r"D:\work\app"),
            vec![r"D:\work\app".to_string()]
        );
        assert!(zed_paths_from_column("  \n \n").is_empty());
    }

    #[test]
    fn jetbrains_entries_read_wrapped_and_self_closing_forms_inside_additional_info() {
        let document = r#"<application>
            <component name="RecentProjectsManager">
                <option name="additionalInfo">
                    <map>
                        <entry key="$USER_HOME$/projects/alpha">
                            <value>
                                <RecentProjectMetaInfo>
                                    <option name="productionCode" value="IU" />
                                </RecentProjectMetaInfo>
                            </value>
                        </entry>
                        <entry key="D:/work/beta" />
                        <entry key="$APPLICATION_HOME_DIR$/samples" />
                    </map>
                </option>
            </component>
            <component name="OtherComponent">
                <option name="additionalInfo">
                    <map>
                        <entry key="D:/ignored" />
                    </map>
                </option>
            </component>
        </application>"#;
        assert_eq!(
            jetbrains_entries_from_document(document, Some("C:/Users/dev")),
            vec![
                r"C:\Users\dev\projects\alpha".to_string(),
                r"D:\work\beta".to_string()
            ]
        );
    }

    #[test]
    fn vscdb_blob_and_text_values_are_both_read() {
        let root = scratch_root("vscdb-bindings");
        let project = create_project(&root, "app");
        let value = format!(
            r#"{{"entries":[{{"folderUri":"{}"}}]}}"#,
            file_uri(&project)
        );
        let expected = vec![project.to_string_lossy().into_owned()];
        let blob_store = root.join("blob").join("state.vscdb");
        write_vscdb(&blob_store, &value, true);
        assert_eq!(vs_code_recent_from_vscdb(&blob_store), expected);
        let text_store = root.join("text").join("state.vscdb");
        write_vscdb(&text_store, &value, false);
        assert_eq!(vs_code_recent_from_vscdb(&text_store), expected);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn scan_sources_reads_appdata_shaped_editors_and_keeps_each_source_list() {
        let root = scratch_root("e2e");
        let alpha = create_project(&root, "alpha");
        let beta = create_project(&root, "beta");
        let gamma = create_project(&root, "gamma");
        let delta = create_project(&root, "delta");
        let appdata = root.join("appdata");

        // VS Code and Cursor both remember alpha; the same path must stay
        // under both sources because the dialog imports one source at a time.
        for editor in ["Code", "Cursor"] {
            write_vscdb(
                &appdata
                    .join(editor)
                    .join("User")
                    .join("globalStorage")
                    .join("state.vscdb"),
                &format!(r#"{{"entries":[{{"folderUri":"{}"}}]}}"#, file_uri(&alpha)),
                editor == "Code",
            );
        }
        // A profile keeps its own store next to the default one.
        write_vscdb(
            &appdata
                .join("Code")
                .join("User")
                .join("profiles")
                .join("team")
                .join("globalStorage")
                .join("state.vscdb"),
            &format!(r#"{{"entries":[{{"folderUri":"{}"}}]}}"#, file_uri(&gamma)),
            true,
        );
        // An editor that only ever wrote the legacy storage.json still counts.
        let legacy = appdata.join("Windsurf").join("User").join("globalStorage");
        std::fs::create_dir_all(&legacy).expect("legacy directory");
        std::fs::write(
            legacy.join("storage.json"),
            format!(
                r#"{{"history":{{"recentlyOpenedPathsList":{{"entries":[{{"folderUri":"{}"}}]}}}}}}"#,
                file_uri(&delta)
            ),
        )
        .expect("legacy store");
        let jetbrains_options = appdata.join("JetBrains").join("Idea2024.3").join("options");
        std::fs::create_dir_all(&jetbrains_options).expect("jetbrains options");
        std::fs::write(
            jetbrains_options.join("recentProjects.xml"),
            format!(
                r#"<application><component name="RecentProjectsManager"><option name="additionalInfo"><map><entry key="{}"><value><RecentProjectMetaInfo/></value></entry></map></option></component></application>"#,
                beta.to_string_lossy().replace('\\', "/")
            ),
        )
        .expect("jetbrains store");
        let local = root.join("local");
        write_zed_db(
            &local
                .join("Zed")
                .join("db")
                .join("0-stable")
                .join("db.sqlite"),
            &[&format!(
                "{}\n{}",
                gamma.to_string_lossy(),
                beta.to_string_lossy()
            )],
        );

        let projects = scan_sources(&appdata, Some(&local), Some(&root));

        let alpha_path = alpha.to_string_lossy().into_owned();
        let beta_path = beta.to_string_lossy().into_owned();
        let gamma_path = gamma.to_string_lossy().into_owned();
        let delta_path = delta.to_string_lossy().into_owned();
        for source in ["vscode", "cursor"] {
            assert!(
                projects
                    .iter()
                    .any(|project| project.source_id == source && project.path == alpha_path),
                "{source} should keep alpha: {projects:?}"
            );
        }
        assert!(
            projects
                .iter()
                .any(|project| project.source_id == "vscode" && project.path == gamma_path),
            "the Code profile store should contribute gamma: {projects:?}"
        );
        assert!(
            projects
                .iter()
                .any(|project| project.source_id == "windsurf" && project.path == delta_path),
            "the legacy storage.json should contribute delta: {projects:?}"
        );
        assert!(
            projects
                .iter()
                .any(|project| project.source_id == "jetbrains" && project.path == beta_path),
            "{projects:?}"
        );
        for path in [&gamma_path, &beta_path] {
            assert!(
                projects
                    .iter()
                    .any(|project| project.source_id == "zed" && &project.path == path),
                "{projects:?}"
            );
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_locked_store_fails_within_the_busy_timeout() {
        let root = scratch_root("locked-store");
        let store = root.join("state.vscdb");
        write_vscdb(&store, r#"{"entries":[]}"#, true);
        let locker = Connection::open(&store).expect("locker");
        locker
            .execute_batch(
                "PRAGMA locking_mode = EXCLUSIVE;\nCREATE TABLE guard (value INTEGER);\nINSERT INTO guard VALUES (1);",
            )
            .expect("exclusive lock");
        let started = std::time::Instant::now();
        let paths = vs_code_recent_from_vscdb(&store);
        let elapsed = started.elapsed();
        drop(locker);
        assert!(paths.is_empty(), "{paths:?}");
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_local_and_relative_paths_are_refused() {
        let mut projects = Vec::new();
        let mut seen = HashSet::new();
        push_projects(
            &mut projects,
            &mut seen,
            vec![
                r"\\server\share\project".to_string(),
                r"\\wsl$\Ubuntu\home\dev\project".to_string(),
                "relative/project".to_string(),
                r".\another".to_string(),
            ],
            "jetbrains",
            "JetBrains",
        );
        assert!(projects.is_empty(), "{projects:?}");
    }

    #[test]
    fn repeated_entries_do_not_consume_the_per_source_budget() {
        let root = scratch_root("budget");
        let alpha = create_project(&root, "alpha");
        let beta = create_project(&root, "beta");
        let mut paths = vec![alpha.to_string_lossy().into_owned(); 100];
        paths.push(beta.to_string_lossy().into_owned());
        let mut projects = Vec::new();
        let mut seen = HashSet::new();
        push_projects(&mut projects, &mut seen, paths, "vscode", "VS Code");
        assert_eq!(projects.len(), 2, "{projects:?}");
        assert!(
            projects
                .iter()
                .any(|project| project.path == beta.to_string_lossy()),
            "{projects:?}"
        );
        let _ = std::fs::remove_dir_all(root);
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
