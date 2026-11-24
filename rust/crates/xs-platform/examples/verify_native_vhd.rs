#[cfg(windows)]
fn main() -> xs_platform::Result<()> {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use xs_platform::{
        read_scoped_journal_changes, scan, scan_with_batches, Scope, Settings, SpeedMode,
    };

    let mut args = std::env::args_os().skip(1);
    let root = PathBuf::from(args.next().expect("需要隔离 VHD 的盘符根"));
    let nonce = args.next().expect("需要合成卷随机标记");
    let nonce = nonce.to_str().expect("随机标记必须是 UTF-8");
    assert_eq!(nonce.len(), 32);
    assert!(nonce.bytes().all(|value| value.is_ascii_hexdigit()));
    let root_text = root.to_str().expect("盘符根必须是 UTF-8");
    assert!(root_text
        .as_bytes()
        .first()
        .is_some_and(|letter| (b'E'..=b'Z').contains(letter)));
    assert_eq!(root.components().count(), 2);
    assert!(xs_platform::volumes()?
        .iter()
        .any(|volume| volume.root == root_text
            && volume.label == "XunshuSynth"
            && volume.filesystem == "NTFS"));
    assert_eq!(
        std::fs::read_to_string(root.join("xunshu-synthetic-marker")).unwrap(),
        nonce
    );

    let directory = root.join("docs");
    std::fs::create_dir(&directory).unwrap();
    let old = directory.join("old.txt");
    let moved = directory.join("new.txt");
    let primary = directory.join("hard.txt");
    let alias = root.join("alias.txt");
    std::fs::write(&old, "合成改名样本").unwrap();
    std::fs::write(&primary, "合成硬链接样本").unwrap();
    std::fs::hard_link(&primary, &alias).unwrap();

    let mut scope = Scope::new(root_text);
    scope.smart_excludes = false;
    let settings = Settings {
        mode: SpeedMode::OnDemandElevated,
        roots: vec![scope],
        ..Settings::default()
    };
    let cancel = AtomicBool::new(false);
    let report = scan(&settings, &cancel, |_| {})?;
    assert!(report.complete, "{:?}", report.errors);
    let checkpoint = report
        .native_checkpoint
        .expect("完整原生扫描应提供续读游标");
    let entry = |path: &Path| {
        report
            .entries
            .iter()
            .find(|entry| Path::new(&entry.path) == path)
            .unwrap()
    };
    assert_eq!(
        entry(&primary).revision.unwrap().identity,
        entry(&alias).revision.unwrap().identity
    );
    assert_eq!(entry(&old).size, "合成改名样本".len() as u64);
    assert!(!report
        .entries
        .iter()
        .any(|entry| entry.path.contains("System Volume Information")
            || entry.path.contains("$Extend")));
    println!(
        "MFT 快照收录 {} 条记录，起始游标 {}，硬链接双路径一致",
        report.entries.len(),
        checkpoint.cursor
    );

    std::fs::rename(&old, &moved).unwrap();
    let rename = read_scoped_journal_changes(&settings, &checkpoint)?;
    assert!(!rename.requires_rebuild, "{rename:?}");
    assert!(rename.checkpoint.cursor > checkpoint.cursor);
    assert!(rename.paths.contains(&old) && rename.paths.contains(&moved));
    println!(
        "原生改名返回旧名和新名，游标推进到 {}",
        rename.checkpoint.cursor
    );

    std::fs::write(&alias, "通过别名改写合成硬链接样本").unwrap();
    let update = read_scoped_journal_changes(&settings, &rename.checkpoint)?;
    assert!(!update.requires_rebuild, "{update:?}");
    assert!(update.paths.contains(&alias) && update.paths.contains(&primary));
    println!(
        "硬链接改写返回两个现存路径，游标推进到 {}",
        update.checkpoint.cursor
    );

    std::fs::create_dir(directory.join("subfolder")).unwrap();
    let topology = read_scoped_journal_changes(&settings, &update.checkpoint)?;
    assert!(topology.requires_rebuild);
    assert_eq!(topology.checkpoint, update.checkpoint);
    let mut wrong_id = update.checkpoint;
    wrong_id.journal_id ^= 1;
    let replacement = read_scoped_journal_changes(&settings, &wrong_id)?;
    assert!(replacement.requires_rebuild);
    assert_eq!(replacement.checkpoint, wrong_id);
    let cancelled = scan_with_batches(
        &settings,
        &cancel,
        |_| {},
        &[],
        1,
        |_| {
            cancel.store(true, Ordering::Release);
            Ok(())
        },
    )?;
    assert!(cancelled.cancelled && !cancelled.complete && cancelled.native_checkpoint.is_none());
    println!("目录变更和错误日志标识均保留旧游标，取消不返回可提交游标");
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("本程序需在 Windows 上通过 verify-native-vhd.ps1 运行。");
}
