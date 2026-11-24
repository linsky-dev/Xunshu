#![no_main]

use libfuzzer_sys::fuzz_target;
use xs_core::interchange::{export_efu, fuzz_efu};

fuzz_target!(|bytes: &[u8]| {
    let Ok(index) = fuzz_efu(bytes) else { return };
    let mut canonical = Vec::new();
    export_efu(&index, &mut canonical).expect("通过导入的 EFU 必须可以导出");
    let restored = fuzz_efu(&canonical).expect("导出的 EFU 必须可以重新导入");
    assert_eq!(restored.len(), index.len());
});
