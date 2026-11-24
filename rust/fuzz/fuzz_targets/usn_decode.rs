#![no_main]

use libfuzzer_sys::fuzz_target;

// 使用生产采集器的同一个纯解析模块，在 Linux 验证时也无需 Windows 卷。
#[path = "../../crates/xs-platform/src/usn_decode.rs"]
mod usn_decode;

fuzz_target!(|bytes: &[u8]| {
    if let Ok(page) = usn_decode::decode(bytes) {
        assert!(page.records.len() <= bytes.len() / 64);
        for record in page.records {
            assert!(matches!(record.version, 2 | 3));
            assert!(!record.name.is_empty());
            assert!(!record.name.contains(['\0', '\\', '/']));
        }
    }
});
