use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};
use xs_core::{interchange, Index};
use xs_platform::FileRevision;

pub(crate) fn inspect(file: &Path, row: usize) -> Result<Value> {
    let revision = FileRevision::at_path(file)?;
    let index = interchange::import_efu(file, "offline:review", 1)?;
    ensure!((1..=index.len()).contains(&row), "EFU 行号不存在");
    let id = (row - 1) as u32;
    ensure!(
        revision == FileRevision::at_path(file)?,
        "检查期间 EFU 文件已变化"
    );
    Ok(
        json!({"row":row,"revision":serde_json::to_string(&revision)?,"metadata":index.record_metadata(id)?}),
    )
}

pub(crate) fn edit(
    input: &Path,
    output: &Path,
    row: usize,
    revision: &str,
    fields: &[String],
    delete: bool,
) -> Result<Value> {
    let expected: FileRevision =
        serde_json::from_str(revision).context("请传入 efu-inspect 返回的 revision 字符串")?;
    let updates: Vec<_> = fields
        .iter()
        .map(|field| {
            let (name, value) = field.split_once('=').context("--set 需要 名称=值")?;
            Ok((name.to_owned(), value.to_owned()))
        })
        .collect::<Result<_>>()?;
    let commit = interchange::edit_efu(
        input,
        output,
        &expected,
        row,
        (!delete).then_some(updates.as_slice()),
    )?;
    Ok(json!({"ok":true,"row":row,"output":output,"deleted":delete,"commit":commit}))
}

pub(crate) fn compare(left: &Path, right: &Path, limit: usize) -> Result<Value> {
    ensure!((1..=1000).contains(&limit), "比较结果上限为 1 至 1000 条");
    let left_revision = FileRevision::at_path(left)?;
    let right_revision = FileRevision::at_path(right)?;
    let before = interchange::import_efu(left, "offline:compare", 1)?;
    let after = interchange::import_efu(right, "offline:compare", 1)?;
    let by_path = |index: &Index| -> Result<BTreeMap<_, _>> {
        (0..index.len() as u32)
            .map(|id| Ok((index.record(id)?.path, id)))
            .collect()
    };
    let mut new_paths = by_path(&after)?;
    let mut counts = [0_usize; 3];
    let mut changes = Vec::new();
    let mut add = |kind: usize, path: String| {
        counts[kind] += 1;
        if changes.len() < limit {
            let kind = ["added", "removed", "changed"][kind];
            changes.push(json!({"kind":kind,"path":path}));
        }
    };
    for (path, id) in by_path(&before)? {
        if let Some(after_id) = new_paths.remove(&path) {
            if before.record_metadata(id)? != after.record_metadata(after_id)? {
                add(2, path);
            }
        } else {
            add(1, path);
        }
    }
    for path in new_paths.into_keys() {
        add(0, path);
    }
    ensure!(
        left_revision == FileRevision::at_path(left)?
            && right_revision == FileRevision::at_path(right)?,
        "比较期间清单已变化，请重试"
    );
    Ok(
        json!({"added":counts[0],"removed":counts[1],"changed":counts[2],"changes":changes,"complete":counts.iter().sum::<usize>() <= limit}),
    )
}

pub(crate) fn compare_folders(
    dir: &Path,
    left: &Path,
    right: &Path,
    limit: usize,
) -> Result<Value> {
    use xs_core::operations;
    ensure!((1..=1000).contains(&limit), "比较结果上限为 1 至 1000 条");
    let settings = operations::load_settings(dir)?;
    for root in [left, right] {
        ensure!(
            settings.allows_filename(root, true)?,
            "比较目录必须位于当前文件名范围内"
        );
    }
    let index = operations::load_index(dir)?;
    ensure!(
        index.coverage_complete(),
        "当前索引覆盖不完整，请完成扫描后再比较"
    );
    let mut folders = [BTreeMap::new(), BTreeMap::new()];
    let roots = [left, right];
    let prefix_lengths = roots.map(|root| root.components().count());
    for id in 0..index.len() as u32 {
        let entry = index.record(id)?;
        if entry.source != "local" {
            continue;
        }
        let path = Path::new(&entry.path);
        for (position, root) in roots.iter().enumerate() {
            if !xs_platform::local_path_within(path, root)? {
                continue;
            }
            let name = path
                .components()
                .skip(prefix_lengths[position])
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if name.is_empty() {
                continue;
            }
            let key = if cfg!(windows) {
                name.to_lowercase()
            } else {
                name.clone()
            };
            let revision = index.record_revision(id)?;
            let comparison = (
                entry.is_dir,
                entry.size,
                entry.modified,
                revision.map(|file| file.modified.nanoseconds),
            );
            ensure!(
                folders[position].insert(key, (name, comparison)).is_none(),
                "目录有比较时无法区分的名称"
            );
        }
    }
    let [before, mut after] = folders;
    let mut counts = [0_usize; 3];
    let mut changes = Vec::new();
    let mut add = |kind: usize, path: String| {
        counts[kind] += 1;
        if changes.len() < limit {
            let kind = ["added", "removed", "changed"][kind];
            changes.push(json!({"kind":kind,"path":path}));
        }
    };
    for (key, (name, value)) in before {
        match after.remove(&key) {
            Some((_, other)) if value == other => (),
            Some(_) => add(2, name),
            None => add(1, name),
        }
    }
    for (name, _) in after.into_values() {
        add(0, name);
    }
    operations::validate_index_scope(&index, &operations::load_settings(dir)?)?;
    Ok(
        json!({"generation":index.generation(),"basis":"indexed_metadata","added":counts[0],"removed":counts[1],"changed":counts[2],"changes":changes,"complete":counts.iter().sum::<usize>() <= limit}),
    )
}
