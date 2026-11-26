use crate::store::{FileRow, FileStatus, SourceKind, timestamp};
use anyhow::{Context, Result, bail};
use globset::GlobSetBuilder;
use ignore::WalkBuilder;
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io,
    path::{Path, PathBuf},
};

pub const SUPPORTED_EXTS: &[&str] = &[
    "md", "pdf", "docx", "doc", "pptx", "ppt", "xlsx", "png", "jpg", "jpeg", "heic", "webp",
];

/// Source files indexed in a code repository.
pub const CODE_EXTS: &[&str] = &[
    "java", "js", "mjs", "cjs", "jsx", "ts", "tsx", "vue", "py", "sql",
];
/// Markdown in a code repository: only orientation documents. Plans, specs and
/// generated indexes often describe code as it was planned, not as it is.
const CODE_DOC_NAMES: &[&str] = &["readme", "context", "architecture", "agents", "claude"];
/// Larger files in a code repository are almost always generated.
pub const MAX_CODE_FILE_BYTES: i64 = 512 * 1024;

/// Test code: excluded from a code repository's index by default, and tagged "test"
/// in exhaustive usage searches.
pub const CODE_TEST_GLOBS: &[&str] = &[
    "**/src/test",
    "**/src/integrationTest",
    "**/src/androidTest",
    "**/src/testFixtures",
    "**/__tests__",
    "**/__mocks__",
    "**/test",
    "**/tests",
    "**/e2e",
    "**/*.spec.*",
    "**/*.test.*",
    "**/*Test.java",
    "**/*Tests.java",
    "**/*IT.java",
];

/// Ignore globs a new code repository starts with: tests, build output, dependencies
/// and generated files. Editable per source like any other excluded folder.
pub fn default_code_ignores() -> Vec<String> {
    CODE_TEST_GLOBS
        .iter()
        .chain(&[
            "**/build",
            "**/dist",
            "**/target",
            "**/out",
            "**/coverage",
            "**/node_modules",
            "**/vendor",
            "**/venv",
            "**/generated",
            "**/tmp",
            "**/*.min.*",
        ])
        .map(|glob| glob.to_string())
        .collect()
}

fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

/// Whether a code repository indexes this file.
pub fn supported_code_file(path: &Path) -> bool {
    let Some(ext) = extension(path) else {
        return false;
    };
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" => CODE_DOC_NAMES.iter().any(|doc| name.starts_with(doc)),
        // Spring and similar configuration; other YAML (CI, compose) is noise.
        "yml" | "yaml" | "properties" => {
            name.starts_with("application")
                || name.starts_with("bootstrap")
                || path.components().any(|part| part.as_os_str() == "resources")
        }
        _ => CODE_EXTS.contains(&ext.as_str()),
    }
}

fn supported(path: &Path, kind: SourceKind) -> bool {
    match kind {
        SourceKind::Notes => supported_file(path),
        SourceKind::Code => supported_code_file(path),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanEntry {
    pub path: PathBuf,
    pub mtime: i64,
    pub size: i64,
}

pub fn literal_glob(path: &str) -> String {
    globset::escape(path)
}

pub fn supported_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            SUPPORTED_EXTS
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
}

pub fn validate_source(path: &Path) -> Result<()> {
    let metadata = path
        .metadata()
        .with_context(|| format!("cannot read {}", path.display()))?;
    anyhow::ensure!(
        metadata.is_dir() || (metadata.is_file() && supported_file(path)),
        "unsupported source: {}",
        path.display()
    );
    Ok(())
}

pub fn scan(root: &Path, ignore_globs: &[String], kind: SourceKind) -> Result<Vec<ScanEntry>> {
    let metadata = root
        .metadata()
        .with_context(|| format!("folder not found: {}", root.display()))?;
    let root = root.canonicalize()?;
    if metadata.is_file() && supported(&root, kind) {
        return Ok(vec![ScanEntry {
            path: root,
            mtime: timestamp(metadata.modified()?),
            size: metadata.len().try_into()?,
        }]);
    }
    anyhow::ensure!(metadata.is_dir(), "unsupported source: {}", root.display());
    let mut globs = GlobSetBuilder::new();
    for glob in ignore_globs {
        globs.add(
            globset::GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid ignore glob {glob}"))?,
        );
    }
    let globs = globs.build()?;
    let mut builder = WalkBuilder::new(&root);
    builder
        .require_git(false)
        .hidden(true)
        .follow_links(false)
        .filter_entry(move |entry| {
            entry.depth() == 0 || !globs.is_match(entry.path().strip_prefix(&root).unwrap())
        });
    let mut entries = Vec::new();
    for entry in builder.build() {
        // Never interpret an incomplete walk (for example a disconnected drive) as deletions.
        let entry = entry?;
        if let Some(error) = entry.error() {
            bail!("{error}");
        }
        let path = entry.path();
        if supported(path, kind) && entry.file_type().is_some_and(|kind| kind.is_file()) {
            let metadata = entry
                .metadata()
                .with_context(|| format!("stat {}", path.display()))?;
            let size: i64 = metadata.len().try_into()?;
            if kind == SourceKind::Code && size > MAX_CODE_FILE_BYTES {
                continue;
            }
            entries.push(ScanEntry {
                path: path.to_owned(),
                mtime: timestamp(metadata.modified()?),
                size,
            });
        }
    }
    entries.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

#[derive(Default, Debug)]
pub struct DiffPlan {
    pub parse: Vec<(ScanEntry, String)>,
    pub touch: Vec<(i64, ScanEntry)>,
    pub delete: Vec<i64>,
}

pub fn diff(
    scanned: &[ScanEntry],
    known: &[FileRow],
    force: bool,
    hash: &dyn Fn(&Path) -> Result<String>,
) -> Result<DiffPlan> {
    let existing: HashMap<_, _> = known
        .iter()
        .map(|file| (file.path.as_path(), file))
        .collect();
    let mut seen = HashSet::new();
    let mut plan = DiffPlan::default();
    for entry in scanned {
        let path = entry.path.as_path();
        seen.insert(path);
        let previous = existing
            .get(path)
            .filter(|file| !force && file.status == FileStatus::Ok);
        if previous.is_some_and(|file| file.mtime == entry.mtime && file.size == entry.size) {
            continue;
        }
        let digest = hash(path)?;
        if let Some(file) = previous.filter(|file| file.hash == digest) {
            plan.touch.push((file.id, entry.clone()));
        } else {
            plan.parse.push((entry.clone(), digest));
        }
    }
    plan.delete.extend(
        known
            .iter()
            .filter(|file| !seen.contains(file.path.as_path()))
            .map(|file| file.id),
    );
    Ok(plan)
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

pub(crate) fn source_volume(path: &Path, is_file: bool) -> Result<String> {
    let kind = if is_file { "file" } else { "folder" };
    let metadata = path
        .metadata()
        .with_context(|| format!("{kind} not found: {}", path.display()))?;
    anyhow::ensure!(
        if is_file {
            metadata.is_file()
        } else {
            metadata.is_dir()
        },
        "source type changed: {}",
        path.display()
    );
    volume_identity(path).with_context(|| format!("cannot identify volume for {}", path.display()))
}

#[cfg(target_os = "macos")]
fn volume_identity(path: &Path) -> Result<String> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = [0_u8; 20];
    // A volume UUID survives reboots and device-number changes.
    let success = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        ) == 0
    };
    if success && buffer[4..].iter().any(|byte| *byte != 0) {
        return Ok(blake3::hash(&buffer[4..]).to_hex().to_string());
    }
    // Some network filesystems lack volume UUIDs. Use the mount identity they report.
    let mut metadata = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let metadata = unsafe { metadata.assume_init() };
    let name = |chars: &[libc::c_char]| {
        chars
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect::<Vec<_>>()
    };
    let mut hash = blake3::Hasher::new();
    hash.update(&name(&metadata.f_mntonname));
    hash.update(&[0]);
    hash.update(&name(&metadata.f_mntfromname));
    Ok(hash.finalize().to_hex().to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn volume_identity(path: &Path) -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    Ok(path.metadata()?.dev().to_string())
}

#[cfg(not(unix))]
fn volume_identity(path: &Path) -> Result<String> {
    Ok(path
        .components()
        .next()
        .context("missing drive")?
        .as_os_str()
        .to_string_lossy()
        .into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    #[test]
    fn scan_filters_ext_hidden_gitignore_globs() -> Result<()> {
        let root = tempfile::tempdir()?;
        for dir in [".obsidian", "node", "app"] {
            fs::create_dir(root.path().join(dir))?;
        }
        for (path, content) in [
            ("a.md", ""),
            ("b.PDF", ""),
            ("c.txt", ""),
            (".obsidian/x.md", ""),
            ("node/y.md", ""),
            ("app/z.pdf", ""),
            (".gitignore", "node/\n"),
        ] {
            fs::write(root.path().join(path), content)?;
        }
        let paths = scan(root.path(), &["app/**".into()], SourceKind::Notes)?
            .into_iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(paths, ["a.md", "b.PDF"]);
        Ok(())
    }

    #[test]
    fn code_scan_keeps_sources_and_orientation_docs_only() -> Result<()> {
        let root = tempfile::tempdir()?;
        for (path, content) in [
            ("README.md", "x".to_string()),
            ("docs/plan.md", "x".into()),
            ("app/src/main/java/a/Lead.java", "x".into()),
            ("app/src/main/resources/application.yml", "x".into()),
            ("app/src/test/java/a/LeadTest.java", "x".into()),
            ("app/src/main/java/a/LeadServiceTest.java", "x".into()),
            ("web/src/components/Form.vue", "x".into()),
            ("web/src/components/Form.spec.js", "x".into()),
            ("web/src/__tests__/a.js", "x".into()),
            ("web/node_modules/lib/index.js", "x".into()),
            ("web/dist/app.js", "x".into()),
            ("web/src/big.js", "x".repeat(MAX_CODE_FILE_BYTES as usize + 1)),
            ("db/V1__init.sql", "x".into()),
            ("ci.yml", "x".into()),
            ("notes.pdf", "x".into()),
        ] {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, content)?;
        }
        let paths: Vec<_> = scan(root.path(), &default_code_ignores(), SourceKind::Code)?
            .into_iter()
            .map(|entry| {
                entry
                    .path
                    .strip_prefix(root.path().canonicalize().unwrap())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            paths,
            [
                "README.md",
                "app/src/main/java/a/Lead.java",
                "app/src/main/resources/application.yml",
                "db/V1__init.sql",
                "web/src/components/Form.vue",
            ]
        );
        Ok(())
    }

    #[test]
    fn scan_missing_root_errors() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("offline");
        assert_eq!(
            scan(&path, &[], SourceKind::Notes).unwrap_err().to_string(),
            format!("folder not found: {}", path.display())
        );
        Ok(())
    }

    #[test]
    fn diff_includes_retries_and_skips_hash_for_unchanged_files() -> Result<()> {
        let known: Vec<_> = ["same", "touch", "edited", "failed", "gone"]
            .into_iter()
            .enumerate()
            .map(|(id, name)| FileRow {
                id: id as i64,
                path: name.into(),
                source_id: 1,
                mtime: 1,
                size: 7,
                hash: "hash".into(),
                error: None,
                ingested_at: 1,
                status: if name == "failed" {
                    FileStatus::Failed
                } else {
                    FileStatus::Ok
                },
            })
            .collect();
        let entries: Vec<_> = [
            ("same", 1),
            ("touch", 2),
            ("edited", 2),
            ("failed", 1),
            ("new", 1),
        ]
        .into_iter()
        .map(|(path, mtime)| ScanEntry {
            path: path.into(),
            mtime,
            size: 7,
        })
        .collect();
        let calls = Cell::new(0);
        let hash = |path: &Path| {
            calls.set(calls.get() + 1);
            Ok(if path == Path::new("edited") {
                "different"
            } else {
                "hash"
            }
            .into())
        };
        let plan = diff(&entries, &known, false, &hash)?;
        assert_eq!(calls.get(), 4);
        assert_eq!(plan.touch, [(1, entries[1].clone())]);
        assert_eq!(plan.delete, [4]);
        assert_eq!(
            plan.parse
                .iter()
                .map(|(e, _)| e.path.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["edited", "failed", "new"]
        );
        let forced = diff(&entries, &known, true, &hash)?;
        assert_eq!(forced.parse.len(), entries.len());
        assert!(forced.touch.is_empty());
        Ok(())
    }
}
