use anyhow::Result;
use serde_json::{json, Value};
use std::{io::ErrorKind, path::Path};
use xs_core::{operations, persistence::Store};
use xs_platform::{PlatformError, Settings};

fn state(error: &PlatformError) -> &'static str {
    match error {
        PlatformError::Io { source, .. } => match source.kind() {
            ErrorKind::NotFound => "missing",
            ErrorKind::PermissionDenied => "access_denied",
            _ => "unavailable",
        },
        PlatformError::InvalidPath(_) => "invalid_local_path",
        PlatformError::Json(_) | PlatformError::InvalidSettings(_) => "invalid_configuration",
        PlatformError::UnsupportedMode(_) => "unsupported",
    }
}

fn index_report(dir: &Path, settings: Option<&Settings>) -> Value {
    let result = (|| {
        let store = Store::open_read_only(dir.join("index"))?;
        if !store.has_snapshot()? {
            return Ok::<Value, anyhow::Error>(json!({"state":"missing"}));
        }
        let index = store.load()?;
        let scope_matches = settings.map(|s| operations::validate_index_scope(&index, s).is_ok());
        Ok(json!({
            "state": match scope_matches {
                Some(true) => "readable",
                Some(false) => "scope_mismatch",
                None => "scope_unverified",
            },
            "generation":index.generation(), "entries":index.len(),
            "coverage_complete":index.coverage_complete(), "snapshot_bytes":index.storage_bytes(),
        }))
    })();
    result.unwrap_or_else(|error| {
        let status = error
            .downcast_ref::<PlatformError>()
            .map_or("invalid_snapshot", state);
        json!({"state":status})
    })
}

/// 仅输出白名单字段。底层错误消息、路径、授权令牌及索引记录均不得进入诊断。
pub(crate) fn report(dir: &Path) -> Value {
    let (settings_status, settings) = match Settings::load(operations::settings_path(dir)) {
        Ok(settings) => ("readable", Some(settings)),
        Err(error) => (state(&error), None),
    };
    let disk = xs_platform::validate_local_path(dir, false);
    let (disk_status, index) = match &disk {
        Ok(()) => ("local_accessible", index_report(dir, settings.as_ref())),
        Err(error) => (state(error), json!({"state":state(error)})),
    };
    let roots = settings.as_ref().map(|settings| {
        let mut accessible = 0;
        let mut denied = 0;
        let mut unavailable = 0;
        for root in &settings.roots {
            match xs_platform::validate_local_path(Path::new(&root.path), false) {
                Ok(()) => accessible += 1,
                Err(error) if state(&error) == "access_denied" => denied += 1,
                Err(_) => unavailable += 1,
            }
        }
        json!({"accessible":accessible,"denied":denied,"unavailable":unavailable})
    });
    json!({
        "schema":1,"version":env!("CARGO_PKG_VERSION"),
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"network":false,
        "settings":{"state":settings_status,"configured_roots":roots,
            "requested_mode":settings.as_ref().map(|s| s.mode)},
        "disk":{"data_directory":disk_status},"index":index,
        "hotkey":{"state":if cfg!(windows) {"runtime_unverified"} else {"unsupported"}},
        "privileged_service":{"state":"unsupported"},
    })
}

pub(crate) fn export(path: &Path, report: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    let outcome = xs_platform::atomic_write_local(path, &bytes, false)?;
    if !outcome.is_durable() {
        eprintln!("诊断已导出，目录持久化未获确认。");
    }
    Ok(())
}

pub(crate) fn print_summary(report: &Value) {
    fn label(value: &Value) -> &str {
        match value.as_str() {
            Some("readable" | "local_accessible") => "可读",
            Some("missing") => "未创建",
            Some("scope_mismatch") => "索引范围已变化",
            Some("scope_unverified") => "索引范围未验证",
            Some("invalid_configuration") => "设置无效",
            Some("invalid_local_path") => "本地路径无效",
            Some("invalid_snapshot") => "索引无法校验",
            Some("access_denied") => "访问被拒绝",
            Some("runtime_unverified") => "需通过桌面操作验证",
            Some("unsupported") => "当前不可用",
            _ => "无法访问",
        }
    }
    println!("寻枢 {} 本机诊断（已脱敏）", env!("CARGO_PKG_VERSION"));
    for (name, value) in [
        ("设置", &report["settings"]["state"]),
        ("数据目录", &report["disk"]["data_directory"]),
        ("索引", &report["index"]["state"]),
        ("快捷键", &report["hotkey"]["state"]),
        ("提权服务", &report["privileged_service"]["state"]),
    ] {
        println!("{name}　{}", label(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn diagnostics_do_not_create_or_leak_paths_and_export_is_create_only() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private-account-marker");
        let empty = report(&private);
        assert_eq!(empty["index"]["state"], "missing");
        assert!(!private.exists());

        fs::create_dir(&private).unwrap();
        fs::write(private.join("settings.json"), b"private-query-marker").unwrap();
        let store = Store::open(private.join("index")).unwrap();
        fs::write(
            store.directory().join("CURRENT.json"),
            b"private-index-marker",
        )
        .unwrap();
        let invalid = report(&private);
        assert_eq!(invalid["settings"]["state"], "invalid_configuration");
        assert_eq!(invalid["index"]["state"], "invalid_snapshot");
        assert!(!invalid.to_string().contains("private-"));

        let output = temp.path().join("diagnostic.json");
        export(&output, &invalid).unwrap();
        assert!(export(&output, &empty).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
            invalid
        );
    }
}
