//! 安装激活与回退都使用经过摘要校验及真实推理自检的独立版本目录。
use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MANIFEST: &str = "release.json";
const POINTER: &str = "active.json";
const RELEASE_OWNER: &str = ".xunshu-release-owner";
const PROBE_OWNER: &str = ".xunshu-self-test-owner";
const TEST_TEXT: &str = "本地文件";
const MAX_MANIFEST: u64 = 256 * 1024;
const PROGRAMS: [&str; 4] = ["xunshu", "xs-ui", "xs-content-worker", "xs-model-worker"];

#[derive(Parser)]
#[command(about = "寻枢本地安装、修复与回退")]
struct Args {
    #[arg(long)]
    root: PathBuf,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 校验并安装新版本，也可用原载荷修复当前版本
    Install {
        payload: PathBuf,
        #[arg(long)]
        sha256: String,
    },
    /// 校验并启用上一个安装版本
    Rollback,
    /// 从有效安装指针启动原生界面
    Launch {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// 仅在当前已授权目录下搜索，适用于目录菜单入口
        #[arg(long)]
        folder: Option<PathBuf>,
    },
    /// 清理历史安装版本及可核验归属的自检数据，不影响当前版/回退版
    Cleanup,
    /// 停用并移除安装版本，保留用户数据
    Uninstall,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    platform: String,
    files: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    directory: String,
    manifest_sha256: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    active: Option<Version>,
    previous: Option<Version>,
    retained: Vec<Version>,
    // Separate from releases: rollback probes must never authorize version deletion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rollback_probes: Vec<Version>,
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    xs_platform::open_local_read(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "安装元数据超过大小限制");
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn safe_component(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('.')
        && !part.ends_with('.')
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !matches!(
            part.split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn manifest(root: &Path, expected: &str) -> Result<(Manifest, Vec<u8>)> {
    let bytes = read_limited(&root.join(MANIFEST), MAX_MANIFEST)?;
    ensure!(
        digest(&bytes).eq_ignore_ascii_case(expected),
        "安装清单摘要不匹配"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.schema == 1
            && manifest.platform == std::env::consts::OS
            && safe_component(&manifest.version)
            && (1..=128).contains(&manifest.files.len()),
        "安装清单版本、平台或文件数量无效"
    );
    let mut names = BTreeSet::new();
    let mut directories = BTreeMap::<String, String>::new();
    for (name, entry) in &manifest.files {
        let folded = name.to_ascii_lowercase();
        ensure!(
            name.len() <= 160
                && name.split('/').all(safe_component)
                && folded != MANIFEST
                && names.insert(folded)
                && entry.sha256.len() == 64
                && entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                && entry.bytes <= 512 * 1024 * 1024,
            "安装载荷路径或摘要无效：{name}"
        );
        let mut parent = Path::new(name).parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            let spelling = relative
                .to_str()
                .context("无效的载荷目录")?
                .replace('\\', "/");
            let key = spelling.to_ascii_lowercase();
            if let Some(existing) = directories.get(&key) {
                ensure!(existing == &spelling, "安装目录大小写冲突：{name}");
            } else {
                directories.insert(key, spelling);
            }
            parent = relative.parent();
        }
    }
    ensure!(
        directories
            .keys()
            .all(|key| !names.contains(key) && key != MANIFEST),
        "安装清单文件与目录路径冲突"
    );
    for name in PROGRAMS.map(executable).into_iter().chain([
        "resources/inference.json".to_owned(),
        "resources/fonts/NotoSansSC-Regular.otf".to_owned(),
        "self-test/image.png".to_owned(),
        "self-test/scanned.pdf".to_owned(),
    ]) {
        ensure!(
            manifest.files.contains_key(&name),
            "安装载荷缺少必要文件：{name}"
        );
    }
    Ok((manifest, bytes))
}

fn checked_file(path: &Path, entry: &Entry, mut output: impl Write) -> Result<()> {
    let file = xs_platform::open_local_read(path)?;
    ensure!(
        file.metadata()?.len() == entry.bytes,
        "安装载荷长度不匹配：{}",
        path.display()
    );
    let mut file = file.take(entry.bytes + 1);
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        hash.update(&bytes[..n]);
        output.write_all(&bytes[..n])?;
    }
    ensure!(
        count == entry.bytes && hex(&hash.finalize()).eq_ignore_ascii_case(&entry.sha256),
        "安装载荷内容摘要不匹配：{}",
        path.display()
    );
    Ok(())
}

// Do not allow an unlisted DLL, executable, or other file in an installed
// release. The release owner marker is metadata, not an executable asset.
fn verify_release_tree(root: &Path, manifest: &Manifest) -> Result<()> {
    let mut directories = BTreeSet::new();
    for name in manifest.files.keys() {
        let mut parent = Path::new(name).parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            directories.insert(root.join(relative));
            parent = relative.parent();
        }
    }
    let files: BTreeSet<_> = manifest.files.keys().map(|name| root.join(name)).collect();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        xs_platform::validate_local_path(&directory, false)?;
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                ensure!(
                    directories.contains(&path),
                    "安装目录包含未登记目录：{}",
                    path.display()
                );
                pending.push(path);
            } else {
                ensure!(
                    kind.is_file()
                        && (files.contains(&path)
                            || path == root.join(MANIFEST)
                            || path == root.join(RELEASE_OWNER)),
                    "安装目录包含未登记文件或链接：{}",
                    path.display()
                );
                xs_platform::validate_local_path(&path, false)?;
            }
        }
    }
    Ok(())
}

fn verify(root: &Path, expected: &str) -> Result<()> {
    let (manifest, _) = manifest(root, expected)?;
    verify_release_tree(root, &manifest)?;
    for (name, entry) in manifest.files {
        checked_file(&root.join(name), &entry, io::sink())?;
    }
    xs_inference::Assets::verify(&root.join("resources"))?;
    Ok(())
}

fn version_root(root: &Path, version: &Version) -> Result<PathBuf> {
    ensure!(
        version.directory.starts_with("release-") && safe_component(&version.directory),
        "安装状态损坏"
    );
    ensure!(
        version.manifest_sha256.len() == 64
            && version
                .manifest_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "安装状态摘要无效"
    );
    Ok(root.join("versions").join(&version.directory))
}

fn read_state(root: &Path) -> Result<State> {
    match read_limited(&root.join(POINTER), MAX_MANIFEST) {
        Ok(bytes) => {
            let state: State = serde_json::from_slice(&bytes)?;
            validate_state(root, &state)?;
            Ok(state)
        },
        Err(error) if error.downcast_ref::<xs_platform::PlatformError>().is_some_and(|e| {
            matches!(e, xs_platform::PlatformError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
        }) => Ok(State::default()),
        Err(error) => Err(error),
    }
}

fn validate_state(root: &Path, state: &State) -> Result<()> {
    let mut retained = BTreeSet::new();
    for version in &state.retained {
        version_root(root, version)?;
        ensure!(
            retained.insert(version.directory.to_ascii_lowercase()),
            "安装状态包含重复版本"
        );
    }
    for probe in &state.rollback_probes {
        version_root(root, probe)?;
        ensure!(
            probe
                .directory
                .strip_prefix("release-rollback-")
                .is_some_and(|nonce| nonce.len() == 32
                    && nonce
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
            "回退自检目录名称无效"
        );
        ensure!(
            retained.insert(probe.directory.to_ascii_lowercase()),
            "自检目录归属冲突"
        );
    }
    for version in [&state.active, &state.previous].into_iter().flatten() {
        ensure!(
            !state
                .rollback_probes
                .iter()
                .any(|p| p.directory.eq_ignore_ascii_case(&version.directory)),
            "自检目录不能作为安装版本"
        );
        version_root(root, version)?;
        if let Some(entry) = state
            .retained
            .iter()
            .find(|v| v.directory.eq_ignore_ascii_case(&version.directory))
        {
            ensure!(
                entry.directory == version.directory
                    && entry.manifest_sha256 == version.manifest_sha256,
                "安装状态版本摘要冲突"
            );
        }
    }
    ensure!(
        !matches!((&state.active, &state.previous), (Some(a), Some(b)) if a.directory.eq_ignore_ascii_case(&b.directory)),
        "当前和回退版本不能相同"
    );
    Ok(())
}

// Older state files may not list the active and previous versions in retained.
// Preserve those identities before changing a pointer so later cleanup can
// account for every version, including the displaced previous release.
fn retain_pointer_versions(state: &mut State) {
    for version in [&state.active, &state.previous].into_iter().flatten() {
        if !state
            .retained
            .iter()
            .any(|v| v.directory == version.directory)
        {
            state.retained.push(version.clone());
        }
    }
}

fn save_state(root: &Path, state: &State) -> Result<()> {
    validate_state(root, state)?;
    let bytes = serde_json::to_vec(state)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST,
        "安装状态超过大小限制，请先清理历史记录"
    );
    let outcome = xs_platform::atomic_write_local(&root.join(POINTER), &bytes, true)?;
    if let Some(warning) = outcome.warning() {
        anyhow::bail!("入口可能已切换，但持久化未确认；禁止继续删除旧版本：{warning}");
    }
    Ok(())
}

fn launch_checked(command: &mut Command) -> Result<()> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "安装自检失败：{status}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("安装自检超过时间预算");
        }
        thread::sleep(Duration::from_millis(30));
    }
}

fn self_test(directory: &Path, data: &Path) -> Result<()> {
    let model = directory.join(executable("xs-model-worker"));
    launch_checked(
        Command::new(model)
            .args(["self-test", TEST_TEXT])
            .stdin(Stdio::from(xs_platform::open_local_read(
                &directory.join("self-test/image.png"),
            )?)),
    )?;
    let cli = directory.join(executable("xunshu"));
    let mut configure = Command::new(&cli);
    launch_checked(
        configure
            .arg("--data-dir")
            .arg(data)
            .args(["content-configure", "--ocr", "--root"])
            .arg(directory.join("self-test")),
    )?;
    launch_checked(
        Command::new(&cli)
            .arg("--data-dir")
            .arg(data)
            .arg("content-rebuild"),
    )?;
    // 此入口核对扫描 PDF 的实际命中，而不是只核对查询进程退出码。
    launch_checked(
        Command::new(&cli)
            .arg("--data-dir")
            .arg(data)
            .args(["content-self-test", TEST_TEXT]),
    )?;
    Ok(())
}

/// 仅清理安装器创建且与发布 SHA 一致的自检目录，绝不清理用户数据目录。
fn clean_probe(root: &Path, version: &Version) -> Result<()> {
    let probe = root.join("self-test-data").join(&version.directory);
    // `exists` also returns false on dangling links and access failures.
    xs_platform::validate_local_path(&probe, true)?;
    match std::fs::symlink_metadata(&probe) {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    xs_platform::validate_local_path(&probe, false)?;
    let owner = probe.join(PROBE_OWNER);
    match read_limited(&owner, 128) {
        Ok(bytes) if bytes == version.manifest_sha256.as_bytes() => {
            // Refuse links/reparse points and unsupported mounts throughout the
            // tree. Keep ownership on a failed preflight; never traverse them.
            let mut pending = vec![probe.clone()];
            while let Some(dir) = pending.pop() {
                xs_platform::validate_local_path(&dir, false)?;
                for entry in std::fs::read_dir(dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    xs_platform::validate_local_path(&path, false)?;
                    let kind = entry.file_type()?;
                    if kind.is_dir() {
                        pending.push(path);
                    } else {
                        ensure!(kind.is_file(), "自检目录包含非普通文件，拒绝清理");
                    }
                }
            }
            std::fs::remove_dir_all(&probe)?;
            Ok(())
        }
        Ok(_) => anyhow::bail!("自检目录归属摘要不匹配，拒绝清理：{}", probe.display()),
        Err(_) => anyhow::bail!(
            "旧版自检目录缺少归属标记，保留供人工确认：{}",
            probe.display()
        ),
    }
}

fn prepare_probe(root: &Path, version: &Version) -> Result<PathBuf> {
    let probe = root.join("self-test-data").join(&version.directory);
    xs_platform::create_local_dir_all(&root.join("self-test-data"))?;
    xs_platform::create_local_dir_new(&probe)?;
    ensure!(
        xs_platform::atomic_write_local(
            &probe.join(PROBE_OWNER),
            version.manifest_sha256.as_bytes(),
            false,
        )?
        .is_durable(),
        "自检目录归属持久化未确认"
    );
    Ok(probe)
}

// Rollback must exercise a clean profile, not an old settings/index directory.
// A separate random sibling also avoids touching legacy unowned probe data.
fn prepare_rollback_probe(
    root: &Path,
    version: &Version,
    state: &mut State,
) -> Result<(Version, PathBuf)> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let probe_version = Version {
        directory: format!("release-rollback-{}", hex(&nonce)),
        manifest_sha256: version.manifest_sha256.clone(),
    };
    // Journal the identity before creating or using the probe. A crash after
    // this write is recoverable; an unmarked directory is never removed.
    state.rollback_probes.push(probe_version.clone());
    save_state(root, state)?;
    let path = prepare_probe(root, &probe_version)?;
    Ok((probe_version, path))
}

fn rollback_self_test(
    root: &Path,
    directory: &Path,
    version: &Version,
    state: &mut State,
) -> Result<()> {
    let (probe_version, probe) = prepare_rollback_probe(root, version, state)?;
    let result = self_test(directory, &probe);
    if let Err(error) = clean_probe(root, &probe_version) {
        eprintln!(
            "回退自检目录保留待 cleanup 重试 {}：{error:#}",
            probe.display()
        );
    } else {
        state
            .rollback_probes
            .retain(|p| p.directory != probe_version.directory);
        // Also persist on a failed self-test, without switching any pointer.
        save_state(root, state)?;
    }
    result
}

/// 版本目录只能删除清单拥有的文件，目录必须确认为空后才移除。
fn remove_empty_release_dirs(directory: &Path, names: impl Iterator<Item = String>) -> Result<()> {
    let mut parents: Vec<PathBuf> = Vec::new();
    for name in names {
        let mut parent = Path::new(&name).parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            parents.push(directory.join(relative));
            parent = relative.parent();
        }
    }
    parents.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    parents.dedup();
    for path in parents {
        match std::fs::remove_dir(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// 在指针持久化后清理既非当前也非回退的版本。
/// 个别文件被占用时保留记录并报告，而不是误报清理成功或阻断已完成安装。
#[derive(Debug, Default)]
struct CleanupReport {
    completed: usize,
    pending: usize,
}

fn remove_release_and_probe(root: &Path, version: &Version) -> Result<()> {
    remove_version(root, version)?;
    clean_probe(root, version)
}

fn cleanup_old_versions(root: &Path, state: &mut State) -> Result<CleanupReport> {
    retain_pointer_versions(state);
    save_state(root, state)?;
    let mut keep = Vec::new();
    let mut report = CleanupReport::default();
    for version in &state.retained {
        if state
            .active
            .as_ref()
            .is_some_and(|v| v.directory == version.directory)
            || state
                .previous
                .as_ref()
                .is_some_and(|v| v.directory == version.directory)
        {
            keep.push(version.clone());
            continue;
        }
        match remove_release_and_probe(root, version) {
            Ok(()) => report.completed += 1,
            Err(error) => {
                eprintln!(
                    "历史安装或自检目录保留待下次重试 {}：{error:#}",
                    version.directory
                );
                keep.push(version.clone());
                report.pending += 1;
            }
        }
    }
    state.retained = keep;
    state
        .rollback_probes
        .retain(|probe| match clean_probe(root, probe) {
            Ok(()) => {
                report.completed += 1;
                false
            }
            Err(error) => {
                eprintln!("回退自检目录保留待下次重试 {}：{error:#}", probe.directory);
                report.pending += 1;
                true
            }
        });
    save_state(root, state)?;
    Ok(report)
}

// Compensate only after verifying the durable pointer and preserving the
// identity. Both release and probe must be gone before dropping the record.
fn compensate_failed_install(root: &Path, persisted: &mut State, version: &Version) -> Result<()> {
    if [&persisted.active, &persisted.previous]
        .into_iter()
        .flatten()
        .any(|v| v.directory.eq_ignore_ascii_case(&version.directory))
    {
        return Ok(());
    }
    if !persisted
        .retained
        .iter()
        .any(|v| v.directory == version.directory)
    {
        persisted.retained.push(version.clone());
    }
    save_state(root, persisted)?;
    remove_release_and_probe(root, version)?;
    persisted
        .retained
        .retain(|v| v.directory != version.directory);
    save_state(root, persisted)
}

fn install(root: &Path, payload: &Path, expected: &str, state: &mut State) -> Result<()> {
    let (manifest, raw) = manifest(payload, expected)?;
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let version = Version {
        directory: format!("release-{}-{}", manifest.version, hex(&nonce)),
        manifest_sha256: digest(&raw),
    };
    let destination = version_root(root, &version)?;
    xs_platform::create_local_dir_all(&root.join("versions"))?;
    // Journal before creating any version directory. If the write fails, no
    // release or probe directory is created. Never take over an existing name.
    retain_pointer_versions(state);
    state.retained.push(version.clone());
    save_state(root, state)?;
    xs_platform::create_local_dir_new(&destination)?;
    let result = (|| -> Result<()> {
        ensure!(
            xs_platform::atomic_write_local(
                &destination.join(RELEASE_OWNER),
                version.manifest_sha256.as_bytes(),
                false,
            )?
            .is_durable(),
            "安装归属持久化未确认"
        );
        // Commit the exact manifest before copying payload files. A crash now
        // leaves an allowlist for safe compensation, not an arbitrary tree.
        ensure!(
            xs_platform::atomic_write_local(&destination.join(MANIFEST), &raw, false)?.is_durable(),
            "安装清单持久化未确认"
        );
        for (name, entry) in &manifest.files {
            let source = payload.join(name);
            let target = destination.join(name);
            xs_platform::create_local_dir_all(target.parent().context("无效的载荷目录")?)?;
            let outcome = xs_platform::write_local_new(&target, |file| {
                checked_file(&source, entry, file).map_err(io::Error::other)
            })?;
            ensure!(outcome.is_durable(), "安装载荷写入未确认持久化");
            #[cfg(unix)]
            if PROGRAMS.contains(&name.as_str()) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        verify(&destination, &version.manifest_sha256)?;
        policy(&destination, "install")?;
        let probe = prepare_probe(root, &version)?;
        self_test(&destination, &probe)?;
        state.previous = state.active.replace(version.clone());
        // 激活指针持久化完成后才能清理旧版本。
        save_state(root, state)?;
        // A committed upgrade remains successful even if historical cleanup
        // cannot finish. Report the issue separately from activation.
        match cleanup_old_versions(root, state) {
            Ok(report) if report.pending > 0 => eprintln!(
                "安装已激活；仍有 {} 项历史记录待 cleanup 重试",
                report.pending
            ),
            Err(error) => eprintln!("安装已激活；历史清理未完成：{error:#}"),
            _ => (),
        }
        Ok(())
    })();
    if let Err(error) = result {
        // 只按磁盘持久化状态判断是否可回收，绝不能误删刚刚成功激活的版本。
        match read_state(root) {
            Ok(mut persisted) => {
                if let Err(problem) = compensate_failed_install(root, &mut persisted, &version) {
                    eprintln!("失败安装暂存目录未完全清理，可重试 cleanup：{problem:#}");
                }
                *state = persisted;
            }
            Err(problem) => eprintln!("安装状态无法核验，保留临时文件以防误删：{problem:#}"),
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(not(windows))]
fn policy(_directory: &Path, _action: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn policy(directory: &Path, action: &str) -> Result<()> {
    let system =
        PathBuf::from(std::env::var_os("SystemRoot").context("无法定位 Windows 系统目录")?);
    let powershell = system.join("System32/WindowsPowerShell/v1.0/powershell.exe");
    for name in PROGRAMS.map(executable) {
        let program = directory.join(name);
        let rule = format!("Xunshu-{}", digest(program.to_string_lossy().as_bytes()));
        let script = r#"
$ErrorActionPreference = 'Stop'
$name = $env:XUNSHU_INSTALL_RULE
$program = $env:XUNSHU_INSTALL_PROGRAM
$rule = Get-NetFirewallRule -PolicyStore PersistentStore -Name $name -ErrorAction SilentlyContinue
if ($rule) {
    $application = $rule | Get-NetFirewallApplicationFilter
    if ($application.Program -ne $program -or $rule.Group -ne 'Xunshu Offline') { throw '防火墙规则归属不符' }
}
if ($env:XUNSHU_INSTALL_ACTION -eq 'remove') {
    if ($rule) {
        if (Get-CimInstance Win32_Process | Where-Object ExecutablePath -eq $program) { throw '请先退出寻枢' }
        $rule | Remove-NetFirewallRule
    }
    exit 0
}
if ($env:XUNSHU_INSTALL_ACTION -eq 'install' -and -not $rule) {
    New-NetFirewallRule -PolicyStore PersistentStore -Name $name -DisplayName '寻枢离线运行' -Group 'Xunshu Offline' -Program $program -Direction Outbound -Action Block -Profile Any -Enabled True | Out-Null
}
$effective = Get-NetFirewallRule -PolicyStore ActiveStore -Name $name
if ($effective.Action -ne 'Block' -or $effective.Direction -ne 'Outbound' -or $effective.Enabled -ne 'True' -or ($effective | Get-NetFirewallApplicationFilter).Program -ne $program) { throw '离线规则没有生效' }
if (Get-NetFirewallProfile -PolicyStore ActiveStore | Where-Object Enabled -ne 'True') { throw '防火墙未对所有配置启用' }
"#;
        let result = Command::new(&powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("XUNSHU_INSTALL_RULE", rule)
            .env("XUNSHU_INSTALL_PROGRAM", &program)
            .env("XUNSHU_INSTALL_ACTION", action)
            .output()?;
        ensure!(
            result.status.success(),
            "Windows 应用离线规则操作失败：{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

fn remove_version(root: &Path, version: &Version) -> Result<()> {
    let directory = version_root(root, version)?;
    xs_platform::validate_local_path(&directory, true)?;
    match std::fs::symlink_metadata(&directory) {
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    xs_platform::validate_local_path(&directory, false)?;
    let owner_path = directory.join(RELEASE_OWNER);
    let owned = read_limited(&owner_path, 128)
        .is_ok_and(|bytes| bytes == version.manifest_sha256.as_bytes());
    let (manifest, _) = match manifest(&directory, &version.manifest_sha256) {
        Ok(found) => found,
        Err(_error) if owned && !directory.join(MANIFEST).exists() => {
            // With no manifest there is no proof that other files belong to us.
            // Only an owner-marker-only interrupted directory may be removed.
            for item in std::fs::read_dir(&directory)? {
                ensure!(
                    item?.file_name() == RELEASE_OWNER,
                    "缺少清单且包含未知文件，保留待人工确认"
                );
            }
            xs_platform::remove_local_file(&owner_path)?;
            std::fs::remove_dir(&directory)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    policy(&directory, "remove")?;
    for name in manifest.files.keys() {
        let path = directory.join(name);
        match xs_platform::remove_local_file(&path) {
            Ok(()) => (),
            Err(xs_platform::PlatformError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    // 先清理子目录，未知文件将导致拒绝删除整个版本目录。
    remove_empty_release_dirs(&directory, manifest.files.keys().cloned())?;
    // 在删除归属清单前确认顶层没有不属于安装器的其他文件。
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            name.to_str()
                .is_some_and(|n| n == MANIFEST || (owned && n == RELEASE_OWNER)),
            "安装目录出现未受管文件，拒绝删除：{}",
            entry.path().display()
        );
    }
    xs_platform::remove_local_file(&directory.join(MANIFEST))?;
    if owned {
        xs_platform::remove_local_file(&owner_path)?;
    }
    std::fs::remove_dir(&directory)?;
    Ok(())
}

fn run() -> Result<()> {
    let args = Args::parse();
    let root = std::path::absolute(args.root)?;
    xs_platform::create_local_dir_all(&root)?;
    let lock: File = xs_platform::open_or_create_local_lock(&root.join("install.lock"))?;
    FileExt::lock_exclusive(&lock)?;
    let mut state = read_state(&root)?;
    match args.command {
        Action::Install { payload, sha256 } => {
            install(&root, &std::path::absolute(payload)?, &sha256, &mut state)
        }
        Action::Rollback => {
            let previous = state.previous.clone().context("没有可回退的安装版本")?;
            let directory = version_root(&root, &previous)?;
            verify(&directory, &previous.manifest_sha256)?;
            policy(&directory, "verify")?;
            rollback_self_test(&root, &directory, &previous, &mut state)?;
            std::mem::swap(&mut state.active, &mut state.previous);
            save_state(&root, &state)
        }
        Action::Launch { data_dir, folder } => {
            let version = state.active.as_ref().context("寻枢尚未安装")?;
            let directory = version_root(&root, version)?;
            verify(&directory, &version.manifest_sha256)?;
            policy(&directory, "verify")?;
            let program = directory.join(executable("xs-ui"));
            let mut command = Command::new(program);
            if let Some(data) = data_dir {
                let data = std::path::absolute(data)?;
                if let Some(folder) = folder {
                    let folder = std::path::absolute(folder)?;
                    let settings = xs_platform::Settings::load(data.join("settings.json"))?;
                    ensure!(
                        settings.allows_filename(&folder, true)?,
                        "目录未获当前配置授权"
                    );
                    xs_platform::validate_local_path(&folder, false)?;
                    ensure!(folder.is_dir(), "所选路径不是本地目录");
                    let query = folder
                        .to_str()
                        .filter(|value| !value.contains('"'))
                        .context("目录无法表示为安全查询")?;
                    command.arg("--query").arg(format!("ancestor:\"{query}\""));
                }
                command.arg("--data-dir").arg(data);
            } else {
                ensure!(folder.is_none(), "目录入口需要明确的 --data-dir");
            }
            command.spawn()?;
            Ok(())
        }
        Action::Cleanup => {
            let report = cleanup_old_versions(&root, &mut state)?;
            ensure!(
                report.pending == 0,
                "历史清理部分完成：{} 项完成，{} 项保留待重试 cleanup；当前版及回退版均已保留",
                report.completed,
                report.pending
            );
            eprintln!(
                "历史清理完成：{} 项；当前版及回退版均已保留",
                report.completed
            );
            Ok(())
        }
        Action::Uninstall => {
            // 兼容老版本缺少 retained 记录但 active/previous 仍有效的情况。
            let mut versions = state.retained.clone();
            for version in [&state.active, &state.previous].into_iter().flatten() {
                if !versions.iter().any(|v| v.directory == version.directory) {
                    versions.push(version.clone());
                }
            }
            state.active = None;
            state.previous = None;
            state.retained = versions.clone();
            save_state(&root, &state)?;
            let report = cleanup_old_versions(&root, &mut state)?;
            ensure!(
                report.pending == 0,
                "已停用寻枢并保留用户数据，但仍有未清理的历史目录；可重试 cleanup"
            );
            Ok(())
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("安装操作失败：{error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn version() -> Version {
        Version {
            directory: "release-test-123".into(),
            manifest_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn state_validation_refuses_conflicting_or_duplicate_version_records() {
        let root = Path::new("/test");
        let v = version();
        let mut state = State {
            active: Some(v.clone()),
            previous: None,
            retained: vec![v.clone()],
            rollback_probes: vec![],
        };
        assert!(validate_state(root, &state).is_ok());
        state.previous = Some(v.clone());
        assert!(validate_state(root, &state).is_err());
        state.previous = None;
        state.retained.push(v.clone());
        assert!(validate_state(root, &state).is_err());
        state.retained.pop();
        state.retained[0].manifest_sha256 = "b".repeat(64);
        assert!(validate_state(root, &state).is_err());
        state.retained.clear();
        assert!(
            validate_state(root, &state).is_ok(),
            "legacy retained records may be absent"
        );
        state.active.as_mut().unwrap().manifest_sha256 = "not-a-hash".into();
        assert!(validate_state(root, &state).is_err());
    }

    #[test]
    fn legacy_pointer_versions_are_retained_before_upgrade() {
        let a = version();
        let mut b = a.clone();
        b.directory = "release-test-456".into();
        let mut state = State {
            active: Some(a.clone()),
            previous: Some(b.clone()),
            retained: vec![],
            rollback_probes: vec![],
        };
        retain_pointer_versions(&mut state);
        retain_pointer_versions(&mut state);
        assert_eq!(state.retained.len(), 2);
        assert_eq!(state.retained[0].directory, a.directory);
        assert_eq!(state.retained[1].directory, b.directory);
        assert!(validate_state(Path::new("/test"), &state).is_ok());
        state.previous.as_mut().unwrap().directory = a
            .directory
            .to_ascii_uppercase()
            .replace("RELEASE-", "release-");
        assert!(validate_state(Path::new("/test"), &state).is_err());
    }

    #[test]
    fn release_tree_rejects_unlisted_files_and_manifest_path_conflicts() -> Result<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-tree-test-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        let result = (|| -> Result<()> {
            let mut files = serde_json::Map::new();
            let entry = serde_json::json!({"bytes": 0, "sha256": digest(b"")});
            for name in PROGRAMS.map(executable).into_iter().chain([
                "resources/inference.json".to_owned(),
                "resources/fonts/NotoSansSC-Regular.otf".to_owned(),
                "self-test/image.png".to_owned(),
                "self-test/scanned.pdf".to_owned(),
            ]) {
                let path = root.join(&name);
                xs_platform::create_local_dir_all(path.parent().unwrap())?;
                std::fs::write(path, [])?;
                files.insert(name, entry.clone());
            }
            let write_manifest =
                |files: &serde_json::Map<String, serde_json::Value>| -> Result<String> {
                    let bytes = serde_json::to_vec(&serde_json::json!({
                        "schema":1, "version":"test", "platform":std::env::consts::OS, "files":files
                    }))?;
                    std::fs::write(root.join(MANIFEST), &bytes)?;
                    Ok(digest(&bytes))
                };
            let sha = write_manifest(&files)?;
            let (valid, _) = manifest(&root, &sha)?;
            verify_release_tree(&root, &valid)?;
            let unknown = root.join("resources/unlisted.dll");
            std::fs::write(&unknown, b"synthetic unknown")?;
            assert!(verify_release_tree(&root, &valid).is_err());
            assert_eq!(std::fs::read(&unknown)?, b"synthetic unknown");
            std::fs::remove_file(unknown)?;
            std::fs::create_dir(root.join("unlisted"))?;
            assert!(verify_release_tree(&root, &valid).is_err());
            std::fs::remove_dir(root.join("unlisted"))?;
            #[cfg(unix)]
            {
                let path = root.join("unlisted-link");
                std::os::unix::fs::symlink(root.join(MANIFEST), &path)?;
                assert!(verify_release_tree(&root, &valid).is_err());
                std::fs::remove_file(path)?;
            }
            verify_release_tree(&root, &valid)?;
            let model = root.join("resources/inference.json");
            std::fs::write(&model, b"tampered")?;
            assert!(
                checked_file(&model, &valid.files["resources/inference.json"], io::sink()).is_err()
            );
            std::fs::write(&model, [])?;
            for conflict in [
                "Resources/other.bin",
                "resources",
                "release.json/child",
                "resources/inference.json/child",
            ] {
                let mut invalid = files.clone();
                invalid.insert(conflict.to_owned(), entry.clone());
                let sha = write_manifest(&invalid)?;
                assert!(manifest(&root, &sha).is_err(), "accepted {conflict}");
            }
            Ok(())
        })();
        std::fs::remove_dir_all(&root)?;
        result
    }

    #[test]
    fn rollback_probe_is_fresh_and_does_not_touch_legacy_data() -> Result<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-rollback-test-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        let v = version();
        let old = root.join("self-test-data").join(&v.directory);
        xs_platform::create_local_dir_all(&old)?;
        std::fs::write(old.join("settings.json"), b"legacy user data")?;
        let mut state = State::default();
        let (a, first) = prepare_rollback_probe(&root, &v, &mut state)?;
        let (b, second) = prepare_rollback_probe(&root, &v, &mut state)?;
        assert_eq!(read_state(&root)?.rollback_probes.len(), 2);
        assert_ne!(first, second);
        assert_ne!(first, old);
        assert_eq!(std::fs::read_dir(&first)?.count(), 1);
        assert_eq!(std::fs::read_dir(&second)?.count(), 1);
        assert_eq!(
            std::fs::read(first.join(PROBE_OWNER))?,
            v.manifest_sha256.as_bytes()
        );
        clean_probe(&root, &a)?;
        clean_probe(&root, &b)?;
        assert!(!first.exists() && !second.exists());
        assert_eq!(
            std::fs::read(old.join("settings.json"))?,
            b"legacy user data"
        );
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn cleanup_preserves_retryable_probe_identity_and_legacy_pointers() -> Result<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-cleanup-test-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        let active = version();
        let mut obsolete = active.clone();
        obsolete.directory = "release-old-obsolete".into();
        let mut state = State {
            active: Some(active.clone()),
            previous: None,
            retained: vec![obsolete.clone()],
            rollback_probes: vec![],
        };
        let old = root.join("self-test-data").join(&obsolete.directory);
        xs_platform::create_local_dir_all(&old)?;
        let unknown = old.join("keep.txt");
        std::fs::write(&unknown, b"unowned data")?;
        let report = cleanup_old_versions(&root, &mut state)?;
        assert_eq!((report.completed, report.pending), (0, 1));
        let persisted = read_state(&root)?;
        assert_eq!(persisted.retained.len(), 2);
        assert!(persisted
            .retained
            .iter()
            .any(|v| v.directory == active.directory));
        assert!(persisted
            .retained
            .iter()
            .any(|v| v.directory == obsolete.directory));
        assert_eq!(std::fs::read(&unknown)?, b"unowned data");
        // The user resolves the unowned directory themselves; cleanup may then
        // clear the pending record without touching the live version.
        std::fs::remove_file(unknown)?;
        std::fs::remove_dir(old)?;
        let report = cleanup_old_versions(&root, &mut state)?;
        assert_eq!((report.completed, report.pending), (1, 0));
        assert_eq!(read_state(&root)?.retained.len(), 1);
        assert_eq!(state.retained[0].directory, active.directory);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn failed_install_compensation_keeps_unknown_probe_and_live_pointers() -> Result<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-compensate-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        let active = version();
        let mut failed = active.clone();
        failed.directory = "release-failed".into();
        let release = version_root(&root, &failed)?;
        xs_platform::create_local_dir_all(&release)?;
        std::fs::write(
            release.join(RELEASE_OWNER),
            failed.manifest_sha256.as_bytes(),
        )?;
        let probe = root.join("self-test-data").join(&failed.directory);
        xs_platform::create_local_dir_all(&probe)?;
        std::fs::write(probe.join("unknown"), b"keep")?;
        let mut state = State {
            active: Some(active.clone()),
            ..State::default()
        };
        assert!(compensate_failed_install(&root, &mut state, &failed).is_err());
        assert!(!release.exists());
        assert_eq!(std::fs::read(probe.join("unknown"))?, b"keep");
        let mut persisted = read_state(&root)?;
        assert_eq!(
            persisted.active.as_ref().unwrap().directory,
            active.directory
        );
        assert_eq!(persisted.retained.len(), 1);
        assert_eq!(persisted.retained[0].directory, failed.directory);
        std::fs::remove_file(probe.join("unknown"))?;
        std::fs::remove_dir(probe)?;
        let report = cleanup_old_versions(&root, &mut persisted)?;
        assert_eq!((report.completed, report.pending), (1, 0));
        assert_eq!(read_state(&root)?.retained.len(), 1);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn rollback_journal_survives_restart_and_refuses_unowned_probe() -> Result<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-rollback-journal-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        let mut state = State {
            active: Some(version()),
            ..State::default()
        };
        let (owned, path) = prepare_rollback_probe(&root, &version(), &mut state)?;
        std::fs::write(path.join("synthetic"), b"probe")?;
        let mut unknown = owned.clone();
        unknown.directory = format!("release-rollback-{}", "1".repeat(32));
        let unknown_path = root.join("self-test-data").join(&unknown.directory);
        xs_platform::create_local_dir_new(&unknown_path)?;
        std::fs::write(unknown_path.join("keep"), b"unknown")?;
        state.rollback_probes.push(unknown.clone());
        save_state(&root, &state)?;
        let mut restarted = read_state(&root)?;
        let report = cleanup_old_versions(&root, &mut restarted)?;
        assert_eq!((report.completed, report.pending), (1, 1));
        assert!(!path.exists());
        assert_eq!(std::fs::read(unknown_path.join("keep"))?, b"unknown");
        assert_eq!(
            read_state(&root)?.rollback_probes[0].directory,
            unknown.directory
        );
        restarted.active = Some(unknown);
        assert!(validate_state(&root, &restarted).is_err());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn rollback_does_not_create_probe_when_journal_cannot_be_saved() -> Result<()> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-rollback-write-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        xs_platform::create_local_dir_new(&root.join(POINTER))?;
        assert!(prepare_rollback_probe(&root, &version(), &mut State::default()).is_err());
        assert!(!root.join("self-test-data").exists());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn probe_cleanup_refuses_nested_and_dangling_links() -> Result<()> {
        use std::os::unix::fs::symlink;
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let root = std::env::temp_dir().join(format!("xs-probe-link-{}", hex(&random)));
        xs_platform::create_local_dir_new(&root)?;
        let v = version();
        let user = root.join("user-data");
        std::fs::write(&user, b"keep")?;
        let probe = prepare_probe(&root, &v)?;
        symlink(&user, probe.join("link"))?;
        assert!(clean_probe(&root, &v).is_err());
        assert_eq!(std::fs::read(&user)?, b"keep");
        assert!(probe.join(PROBE_OWNER).is_file());
        std::fs::remove_file(probe.join("link"))?;
        clean_probe(&root, &v)?;
        symlink(root.join("missing"), &probe)?;
        assert!(clean_probe(&root, &v).is_err());
        assert!(std::fs::symlink_metadata(&probe)?.file_type().is_symlink());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interrupted_install_without_manifest_preserves_unknown_files() {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!("xs-install-test-{}", hex(&random)));
        xs_platform::create_local_dir_all(&root).unwrap();
        let v = version();
        let directory = version_root(&root, &v).unwrap();
        xs_platform::create_local_dir_all(&directory).unwrap();
        let marker = directory.join(RELEASE_OWNER);
        std::fs::write(&marker, v.manifest_sha256.as_bytes()).unwrap();
        let unknown = directory.join("keep.txt");
        std::fs::write(&unknown, "not owned by the installer").unwrap();
        assert!(remove_version(&root, &v).is_err());
        assert_eq!(
            std::fs::read_to_string(&unknown).unwrap(),
            "not owned by the installer"
        );
        std::fs::remove_file(&unknown).unwrap();
        remove_version(&root, &v).unwrap();
        assert!(!directory.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
