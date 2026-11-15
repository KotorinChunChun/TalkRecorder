//! EXE配下の任意ノイズ除去プラグイン。探索中はDLLを実行しない。
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, c_char, c_int, c_void},
    fs::{self, File, Metadata},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use serde::Deserialize;

pub const MANIFEST_NAME: &str = "talkrecorder-plugin.json";
pub const PLUGIN_API_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 65_536;
const MAX_ENTRIES: usize = 16_384;
const MAX_MANIFESTS: usize = 128;
const MAX_DEPTH: usize = 12;
const MAX_ISSUES: usize = 64;
const MAX_SAMPLES: usize = 8_000_000;
const ERROR_CAPACITY: usize = 4_096;

#[derive(Debug, Clone, Default)]
pub struct PluginCatalog {
    pub available: Vec<PluginDescriptor>,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    root: PathBuf,
    manifest: PathBuf,
    library: PathBuf,
    resources: BTreeMap<String, PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    id: String,
    name: String,
    library: String,
    #[serde(default)]
    resources: BTreeMap<String, String>,
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn component_eq(left: Component<'_>, right: Component<'_>) -> bool {
    if left == right {
        return true;
    }
    #[cfg(windows)]
    {
        // Windowsの通常のASCII表記差は許容する。非UTF-8等は保守的に拒否する。
        match (left.as_os_str().to_str(), right.as_os_str().to_str()) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => false,
        }
    }
    #[cfg(not(windows))]
    false
}

fn is_within(root: &Path, candidate: &Path) -> bool {
    let mut components = candidate.components();
    root.components().all(|component| {
        components
            .next()
            .is_some_and(|other| component_eq(component, other))
    })
}

fn plain_root(root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("プラグインの探索先を確認できません: {}", root.display()))?;
    ensure!(
        metadata.is_dir() && !is_link(&metadata),
        "プラグインの探索先が通常のフォルダーではありません: {}",
        root.display()
    );
    fs::canonicalize(root).context("プラグインの探索先を解決できません")
}

fn validate_plain_path(root: &Path, candidate: &Path) -> Result<PathBuf> {
    let relative = candidate
        .strip_prefix(root)
        .context("プラグインのパスがEXEフォルダー外です")?;
    ensure!(!relative.as_os_str().is_empty(), "ファイル名がありません");
    let mut current = root.to_path_buf();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "不正な相対パスです"
        );
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("必要なファイルがありません: {}", current.display()))?;
        ensure!(
            !is_link(&metadata),
            "リンク経由のプラグインは使用できません: {}",
            current.display()
        );
        if index + 1 == component_count {
            ensure!(
                metadata.is_file(),
                "通常のファイルではありません: {}",
                current.display()
            );
        } else {
            ensure!(
                metadata.is_dir(),
                "通常のフォルダーではありません: {}",
                current.display()
            );
        }
    }
    let resolved = fs::canonicalize(candidate)
        .with_context(|| format!("ファイルの場所を解決できません: {}", candidate.display()))?;
    ensure!(
        is_within(root, &resolved),
        "ファイルがEXEフォルダー外を指しています"
    );
    Ok(resolved)
}

fn resource_path(root: &Path, directory: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        !relative.trim().is_empty()
            && relative.len() <= 1_024
            && !relative.chars().any(|ch| ch.is_control() || ch == ':'),
        "必要ファイルの相対パスが不正です"
    );
    // Windows形式も他OSでの回帰試験時に同じ規則で検証する。
    let portable = relative.replace('\\', "/");
    let mut joined = directory.to_path_buf();
    let mut has_name = false;
    for component in Path::new(&portable).components() {
        match component {
            Component::Normal(name) => {
                joined.push(name);
                has_name = true;
            }
            Component::CurDir => {}
            _ => bail!("必要ファイルにはEXE配下の相対パスだけを指定してください"),
        }
    }
    ensure!(has_name, "必要ファイル名がありません");
    validate_plain_path(root, &joined)
}

fn read_descriptor(root: &Path, manifest: &Path) -> Result<PluginDescriptor> {
    let manifest = validate_plain_path(root, manifest)?;
    let mut file = File::open(&manifest).context("プラグイン定義を開けません")?;
    ensure!(
        file.metadata()?.len() <= MAX_MANIFEST_BYTES,
        "プラグイン定義が大きすぎます"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "プラグイン定義が大きすぎます"
    );
    let value: Manifest =
        serde_json::from_slice(&bytes).context("プラグイン定義のJSONが不正です")?;
    ensure!(value.schema_version == 1, "未対応のプラグイン定義形式です");
    ensure!(
        !value.id.is_empty()
            && value.id.len() <= 80
            && value.id.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || b"-_.".contains(&byte)),
        "プラグインIDは小文字英数字と -_. の80文字以内で指定してください"
    );
    ensure!(
        !value.name.trim().is_empty()
            && value.name.chars().count() <= 80
            && !value.name.chars().any(char::is_control),
        "プラグイン表示名が不正です"
    );
    ensure!(
        value.resources.len() <= 32,
        "必要ファイルの指定が多すぎます"
    );
    let directory = manifest
        .parent()
        .context("プラグイン定義のフォルダーがありません")?;
    let library = resource_path(root, directory, &value.library)?;
    let mut resources = BTreeMap::new();
    for (key, relative) in value.resources {
        ensure!(
            !key.is_empty()
                && key.len() <= 80
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
            "必要ファイルのキーが不正です"
        );
        resources.insert(key, resource_path(root, directory, &relative)?);
    }
    Ok(PluginDescriptor {
        id: value.id,
        name: value.name,
        root: root.to_path_buf(),
        manifest,
        library,
        resources,
    })
}

fn issue(catalog: &mut PluginCatalog, message: String) {
    if catalog.issues.len() < MAX_ISSUES {
        catalog.issues.push(message);
    }
}

/// 引数のフォルダーだけを下向きに探索する。実在確認でありDLLのロードではない。
pub fn discover(root: &Path) -> PluginCatalog {
    let mut catalog = PluginCatalog::default();
    let root = match plain_root(root) {
        Ok(root) => root,
        Err(error) => {
            issue(&mut catalog, format!("{error:#}"));
            return catalog;
        }
    };
    let mut stack = vec![(root.clone(), 0_usize)];
    let mut seen_entries = 0_usize;
    let mut manifests = Vec::new();
    let mut limit_reached = false;
    while let Some((directory, depth)) = stack.pop() {
        let listing = match fs::read_dir(&directory) {
            Ok(listing) => listing,
            Err(error) => {
                issue(
                    &mut catalog,
                    format!(
                        "プラグインのフォルダーを読めません: {}: {error}",
                        directory.display()
                    ),
                );
                continue;
            }
        };
        let mut entries = Vec::new();
        for entry in listing {
            seen_entries += 1;
            if seen_entries > MAX_ENTRIES {
                limit_reached = true;
                break;
            }
            match entry {
                Ok(entry) => entries.push(entry.path()),
                Err(error) => issue(
                    &mut catalog,
                    format!("プラグイン探索中の読取エラー: {error}"),
                ),
            }
        }
        if limit_reached {
            break;
        }
        entries.sort();
        for path in entries.into_iter().rev() {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    issue(
                        &mut catalog,
                        format!("プラグイン探索中の確認エラー: {}: {error}", path.display()),
                    );
                    continue;
                }
            };
            if is_link(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                } else {
                    issue(
                        &mut catalog,
                        format!("探索深度の上限で省略しました: {}", path.display()),
                    );
                }
            } else if metadata.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(MANIFEST_NAME))
            {
                manifests.push(path);
                if manifests.len() > MAX_MANIFESTS {
                    limit_reached = true;
                    break;
                }
            }
        }
        if limit_reached {
            break;
        }
    }
    if limit_reached {
        issue(
            &mut catalog,
            "プラグイン探索の件数上限を超えたため、候補を採用しませんでした".into(),
        );
        return catalog;
    }
    manifests.sort();
    let mut duplicates = BTreeSet::new();
    let mut by_id = BTreeMap::new();
    for manifest in manifests {
        match read_descriptor(&root, &manifest) {
            Ok(descriptor) => {
                if by_id
                    .insert(descriptor.id.clone(), descriptor.clone())
                    .is_some()
                {
                    duplicates.insert(descriptor.id);
                }
            }
            Err(error) => issue(
                &mut catalog,
                format!(
                    "プラグインを候補から除外しました: {}: {error:#}",
                    manifest.display()
                ),
            ),
        }
    }
    for duplicate in duplicates {
        by_id.remove(&duplicate);
        issue(
            &mut catalog,
            format!("同じIDのプラグインが複数あるため除外しました: {duplicate}"),
        );
    }
    catalog.available = by_id.into_values().collect();
    catalog.available.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    catalog
}

pub fn discover_for_current_exe() -> PluginCatalog {
    match std::env::current_exe() {
        Ok(path) => match path.parent() {
            Some(root) => discover(root),
            None => PluginCatalog {
                available: Vec::new(),
                issues: vec!["EXEのフォルダーがありません".into()],
            },
        },
        Err(error) => PluginCatalog {
            available: Vec::new(),
            issues: vec![format!("EXEの場所を確認できません: {error}")],
        },
    }
}

type ApiVersion = unsafe extern "C" fn() -> u32;
type Create = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> *mut c_void;
type Process = unsafe extern "C" fn(
    *mut c_void,
    *const f32,
    usize,
    u32,
    *mut f32,
    *mut c_char,
    usize,
) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);

pub struct LoadedPlugin {
    handle: *mut c_void,
    process: Process,
    destroy: Destroy,
    _library: Library,
}

fn plugin_error(buffer: &[u8]) -> String {
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}

unsafe fn load_library(path: &Path) -> Result<Library> {
    #[cfg(windows)]
    {
        // 依存DLLも選択DLLのフォルダーかSystem32から解決する。PATH/CWDを探索しない。
        Ok(
            unsafe { libloading::os::windows::Library::load_with_flags(path, 0x100 | 0x800)? }
                .into(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(unsafe { Library::new(path)? })
    }
}

impl PluginCatalog {
    /// 選択後にだけロードする。探索後のファイル削除・定義変更もここで再検証する。
    pub fn load(&self, id: &str) -> Result<LoadedPlugin> {
        self.load_with_options(id, &serde_json::Map::new())
    }

    /// resourcesのキーを上書きしない任意設定を、同じcreate JSONへ追加する。
    pub fn load_with_options(
        &self,
        id: &str,
        options: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<LoadedPlugin> {
        let descriptor = self
            .available
            .iter()
            .find(|plugin| plugin.id == id)
            .with_context(|| format!("ノイズ除去プラグインが見つかりません: {id}"))?;
        let root = plain_root(&descriptor.root)?;
        let current = read_descriptor(&root, &descriptor.manifest)?;
        ensure!(
            &current == descriptor,
            "プラグインの構成が変更されました。再取得してください"
        );
        ensure!(options.len() <= 32, "プラグインの追加設定が多すぎます");
        let mut config = serde_json::to_value(&descriptor.resources)?
            .as_object()
            .context("プラグインの必要ファイル情報が不正です")?
            .clone();
        for (key, value) in options {
            ensure!(
                !config.contains_key(key),
                "プラグイン設定と必要ファイルのキーが重複しています: {key}"
            );
            ensure!(
                !key.is_empty()
                    && key.len() <= 80
                    && key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)),
                "プラグインの追加設定キーが不正です"
            );
            config.insert(key.clone(), value.clone());
        }
        let config = serde_json::to_string(&config)?;
        ensure!(
            config.len() as u64 <= MAX_MANIFEST_BYTES,
            "プラグイン設定が大きすぎます"
        );
        let config = CString::new(config)?;
        let library = unsafe { load_library(&descriptor.library) }.with_context(|| {
            format!(
                "ノイズ除去DLLを読み込めません: {}",
                descriptor.library.display()
            )
        })?;
        let api_version: ApiVersion = unsafe { *library.get(b"tr_denoise_api_version\0")? };
        ensure!(
            unsafe { api_version() } == PLUGIN_API_VERSION,
            "ノイズ除去プラグインのAPI版が一致しません"
        );
        let create: Create = unsafe { *library.get(b"tr_denoise_create\0")? };
        let process: Process = unsafe { *library.get(b"tr_denoise_process\0")? };
        let destroy: Destroy = unsafe { *library.get(b"tr_denoise_destroy\0")? };
        let mut error = vec![0_u8; ERROR_CAPACITY];
        let handle = unsafe { create(config.as_ptr(), error.as_mut_ptr().cast(), error.len()) };
        ensure!(
            !handle.is_null(),
            "ノイズ除去プラグインを開始できません: {}",
            plugin_error(&error)
        );
        Ok(LoadedPlugin {
            handle,
            process,
            destroy,
            _library: library,
        })
    }
}

impl LoadedPlugin {
    pub fn process(&mut self, samples: &[f32], sample_rate: u32) -> Result<Vec<f32>> {
        ensure!(
            (1..=384_000).contains(&sample_rate),
            "プラグイン入力のサンプルレートが不正です"
        );
        ensure!(samples.len() <= MAX_SAMPLES, "プラグイン入力が大きすぎます");
        ensure!(
            samples.iter().all(|sample| sample.is_finite()),
            "プラグイン入力に非有限値があります"
        );
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        // 書き忘れを無音として受け入れない。
        let mut output = vec![f32::NAN; samples.len()];
        let mut error = vec![0_u8; ERROR_CAPACITY];
        let status = unsafe {
            (self.process)(
                self.handle,
                samples.as_ptr(),
                samples.len(),
                sample_rate,
                output.as_mut_ptr(),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        ensure!(
            status == 0,
            "ノイズ除去プラグインの処理に失敗しました ({status}): {}",
            plugin_error(&error)
        );
        ensure!(
            output.iter().all(|sample| sample.is_finite()),
            "ノイズ除去プラグインの出力が不完全か非有限値です"
        );
        Ok(output)
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{process::Command, sync::OnceLock};
    use tempfile::TempDir;

    fn put_plugin(root: &Path, directory: &str, id: &str) -> PathBuf {
        let directory = root.join(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("backend.dll"), b"not an executable DLL").unwrap();
        fs::write(directory.join("model.bin"), b"weights").unwrap();
        write_manifest(&directory, id, "backend.dll", json!({"model":"model.bin"}));
        directory
    }

    fn write_manifest(directory: &Path, id: &str, library: &str, resources: serde_json::Value) {
        fs::write(directory.join(MANIFEST_NAME), serde_json::to_vec(&json!({
            "schema_version":1,"id":id,"name":format!("Plugin {id}"),"library":library,"resources":resources
        })).unwrap()).unwrap();
    }

    #[test]
    fn discovery_never_loads_dll_and_requires_all_declared_files() {
        let root = tempfile::tempdir().unwrap();
        let directory = put_plugin(root.path(), "plugins/rnnoise", "rnnoise");
        let catalog = discover(root.path());
        assert_eq!(catalog.available.len(), 1); // 実行不能なDLLでも探索だけなら実在を確認する。
        assert!(catalog.load("rnnoise").is_err());
        fs::remove_file(directory.join("model.bin")).unwrap();
        assert!(discover(root.path()).available.is_empty());
        fs::write(directory.join("model.bin"), b"weights").unwrap();
        fs::remove_file(directory.join("backend.dll")).unwrap();
        assert!(discover(root.path()).available.is_empty());
    }

    #[test]
    fn only_descendants_are_discovered_and_bad_manifests_are_excluded() {
        let parent = tempfile::tempdir().unwrap();
        put_plugin(parent.path(), "parent-plugin", "parent");
        let executable_directory = parent.path().join("release/current");
        fs::create_dir_all(&executable_directory).unwrap();
        assert!(discover(&executable_directory).available.is_empty());
        let directory = put_plugin(&executable_directory, "plugins/present", "present");
        fs::write(directory.join(MANIFEST_NAME), b"{invalid").unwrap();
        let catalog = discover(&executable_directory);
        assert!(catalog.available.is_empty());
        assert!(!catalog.issues.is_empty());
        fs::write(
            directory.join(MANIFEST_NAME),
            vec![b' '; MAX_MANIFEST_BYTES as usize + 1],
        )
        .unwrap();
        assert!(discover(&executable_directory).available.is_empty());
    }

    #[test]
    fn manifest_filename_is_ascii_case_insensitive() {
        let root = tempfile::tempdir().unwrap();
        let directory = put_plugin(root.path(), "plugins/test", "test");
        let staging = directory.join("renaming.json");
        fs::rename(directory.join(MANIFEST_NAME), &staging).unwrap();
        fs::rename(staging, directory.join("TALKRECORDER-PLUGIN.JSON")).unwrap();
        assert_eq!(discover(root.path()).available.len(), 1);
    }

    #[test]
    fn path_traversal_absolute_paths_and_ads_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let directory = put_plugin(root.path(), "plugins/test", "test");
        for path in [
            "../model.bin",
            "..\\model.bin",
            "/model.bin",
            "C:\\model.bin",
            "C:model.bin",
            "model.bin:stream",
            "\\\\server\\share\\model.bin",
        ] {
            write_manifest(&directory, "test", "backend.dll", json!({"model":path}));
            assert!(
                discover(root.path()).available.is_empty(),
                "accepted {path}"
            );
        }
    }

    #[test]
    fn duplicate_ids_are_all_excluded_and_candidates_are_sorted() {
        let root = tempfile::tempdir().unwrap();
        put_plugin(root.path(), "plugins/z", "z");
        put_plugin(root.path(), "plugins/a", "a");
        put_plugin(root.path(), "plugins/duplicate-1", "same");
        put_plugin(root.path(), "plugins/duplicate-2", "same");
        let catalog = discover(root.path());
        assert_eq!(
            catalog
                .available
                .iter()
                .map(|plugin| plugin.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
        assert!(catalog.issues.iter().any(|issue| issue.contains("same")));
    }

    #[test]
    fn load_revalidates_files_and_manifest_after_discovery() {
        let root = tempfile::tempdir().unwrap();
        let directory = put_plugin(root.path(), "plugins/test", "test");
        let catalog = discover(root.path());
        fs::remove_file(directory.join("model.bin")).unwrap();
        assert!(format!("{:#}", catalog.load("test").err().unwrap()).contains("必要なファイル"));
        fs::write(directory.join("model.bin"), b"weights").unwrap();
        write_manifest(
            &directory,
            "changed",
            "backend.dll",
            json!({"model":"model.bin"}),
        );
        assert!(format!("{:#}", catalog.load("test").err().unwrap()).contains("構成が変更"));
    }

    #[test]
    fn options_cannot_replace_a_validated_resource_path() {
        let root = tempfile::tempdir().unwrap();
        put_plugin(root.path(), "plugins/test", "test");
        let catalog = discover(root.path());
        let mut options = serde_json::Map::new();
        options.insert("model".into(), json!("C:/outside/model.bin"));
        let error = catalog.load_with_options("test", &options).err().unwrap();
        // backend.dllは実行不能だが、重複の拒否がDLLロードより前に行われる。
        assert!(format!("{error:#}").contains("重複"));
    }

    #[test]
    fn linked_plugin_directory_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        put_plugin(outside.path(), "real", "outside");
        let linked = root.path().join("linked");
        #[cfg(windows)]
        let result = std::os::windows::fs::symlink_dir(outside.path().join("real"), &linked);
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(outside.path().join("real"), &linked);
        #[cfg(not(any(windows, unix)))]
        return;
        if let Err(error) = result {
            #[cfg(windows)]
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("symlink creation failed: {error}");
        }
        assert!(discover(root.path()).available.is_empty());
    }

    const FIXTURE: &str = r#"
use std::ffi::{c_char,c_int,c_void,CStr};
use std::sync::atomic::{AtomicUsize,Ordering};
static DESTROYED: AtomicUsize = AtomicUsize::new(0);
#[unsafe(no_mangle)] pub extern "C" fn tr_denoise_api_version() -> u32 { 1 }
#[unsafe(no_mangle)] pub unsafe extern "C" fn tr_denoise_create(config:*const c_char,_error:*mut c_char,_capacity:usize)->*mut c_void {
 let config=unsafe { CStr::from_ptr(config) }.to_string_lossy();
 let mode=if config.contains("nan.control") { 1 } else if config.contains("error.control") { 2 } else if config.contains("partial.control") { 3 } else { 0 };
 Box::into_raw(Box::new(mode as u8)).cast()
}
#[unsafe(no_mangle)] pub unsafe extern "C" fn tr_denoise_process(state:*mut c_void,input:*const f32,len:usize,rate:u32,output:*mut f32,error:*mut c_char,capacity:usize)->c_int {
 let mode=unsafe { *(state.cast::<u8>()) };
 if mode==2 || rate!=48000 { if capacity>0 { unsafe { *error=0; } } return 7; }
 for index in 0..len { if mode==3 && index+1==len { break; } unsafe { *output.add(index)=if mode==1 { f32::NAN } else { *input.add(index)*0.5 }; } }
 0
}
#[unsafe(no_mangle)] pub unsafe extern "C" fn tr_denoise_destroy(state:*mut c_void) { drop(unsafe { Box::from_raw(state.cast::<u8>()) }); DESTROYED.fetch_add(1,Ordering::SeqCst); }
#[unsafe(no_mangle)] pub extern "C" fn tr_fixture_destroyed()->usize { DESTROYED.load(Ordering::SeqCst) }
"#;

    fn compiled_fixture(bad_api: bool) -> &'static Path {
        static GOOD: OnceLock<TempDir> = OnceLock::new();
        static BAD: OnceLock<TempDir> = OnceLock::new();
        let slot = if bad_api { &BAD } else { &GOOD };
        slot.get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("fixture.rs");
            let text = if bad_api {
                FIXTURE.replace("-> u32 { 1 }", "-> u32 { 2 }")
            } else {
                FIXTURE.to_owned()
            };
            fs::write(&source, text).unwrap();
            let output = Command::new("rustc")
                .args(["--edition=2024", "--crate-type=cdylib", "-C", "panic=abort"])
                .arg(&source)
                .arg("-o")
                .arg(directory.path().join("fixture.dll"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture compile failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            directory
        })
        .path()
    }

    #[test]
    fn native_fixture_preserves_count_reports_failures_and_destroys_state() {
        let root = tempfile::tempdir().unwrap();
        let directory = put_plugin(root.path(), "plugins/fixture", "fixture");
        fs::copy(
            compiled_fixture(false).join("fixture.dll"),
            directory.join("backend.dll"),
        )
        .unwrap();
        let observer = unsafe { load_library(&directory.join("backend.dll")) }.unwrap();
        let destroyed: unsafe extern "C" fn() -> usize =
            unsafe { *observer.get(b"tr_fixture_destroyed\0").unwrap() };
        let initial = unsafe { destroyed() };
        let mut loaded = discover(root.path()).load("fixture").unwrap();
        assert_eq!(
            loaded.process(&[0.2, -0.6, 0.8], 48000).unwrap(),
            [0.1, -0.3, 0.4]
        );
        assert!(loaded.process(&[0.2], 44100).is_err());
        assert!(loaded.process(&[f32::NAN], 48000).is_err());
        assert!(loaded.process(&[], 48000).unwrap().is_empty());
        drop(loaded);
        assert_eq!(unsafe { destroyed() }, initial + 1);
        drop(observer);
        for name in ["nan.control", "error.control", "partial.control"] {
            fs::write(directory.join(name), b"control").unwrap();
            write_manifest(
                &directory,
                "fixture",
                "backend.dll",
                json!({"control":name}),
            );
            let mut loaded = discover(root.path()).load("fixture").unwrap();
            assert!(
                loaded.process(&[0.2, -0.6, 0.8], 48000).is_err(),
                "accepted {name}"
            );
        }
    }

    #[test]
    fn native_fixture_with_another_api_is_rejected_before_create() {
        let root = tempfile::tempdir().unwrap();
        let directory = put_plugin(root.path(), "plugins/fixture", "fixture");
        fs::copy(
            compiled_fixture(true).join("fixture.dll"),
            directory.join("backend.dll"),
        )
        .unwrap();
        let catalog = discover(root.path());
        assert_eq!(catalog.available.len(), 1);
        assert!(format!("{:#}", catalog.load("fixture").err().unwrap()).contains("API版"));
    }
}
