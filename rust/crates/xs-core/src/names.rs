//! 重复文件名词干前缀字典。索引仅保存短编号和余下的 UTF-8 字节。
use crate::records::RECORD_BYTES;
use std::collections::HashMap;

pub(crate) const MAX_NAME_BYTES: usize = 4096;
pub(crate) const MAX_PREFIXES: usize = 8192;
pub(crate) const MAX_PREFIX_BYTES: usize = 128;
pub(crate) const PREFIX_FLAG: u32 = 4;

fn stem_prefix(name: &str) -> Option<&str> {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let end = stem.rfind(['_', '-', ' '])? + 1;
    (8..=MAX_PREFIX_BYTES).contains(&end).then(|| &name[..end])
}

pub(crate) fn compact(records: &mut [u8], names: &mut Vec<u8>) -> Vec<String> {
    if records.len() < 1024 * RECORD_BYTES {
        return Vec::new();
    }
    let mut candidates: HashMap<String, Option<u16>> = HashMap::new();
    let mut prefixes = Vec::new();
    let mut write = 0;
    for row in records.as_chunks_mut::<RECORD_BYTES>().0 {
        let offset = u32::from_le_bytes(row[..4].try_into().unwrap()) as usize;
        let length = u32::from_le_bytes(row[4..8].try_into().unwrap()) as usize;
        let end = offset + length;
        let name = std::str::from_utf8(&names[offset..end]).expect("构建时已校验 UTF-8");
        let encoded = stem_prefix(name).and_then(|prefix| {
            if let Some(id) = candidates.get_mut(prefix) {
                let id = *id.get_or_insert_with(|| {
                    let index = prefixes.len() as u16;
                    prefixes.push(prefix.to_owned());
                    index
                });
                Some((id, prefix.len()))
            } else {
                if candidates.len() < MAX_PREFIXES {
                    candidates.insert(prefix.to_owned(), None);
                }
                None
            }
        });
        let stored_length = if let Some((id, prefix_len)) = encoded {
            // 每行只向前移动，保留尚未读取的下一行。先复制后写编号可安全处理重叠。
            names.copy_within(offset + prefix_len..end, write + 2);
            names[write..write + 2].copy_from_slice(&id.to_le_bytes());
            let flags = u32::from_le_bytes(row[32..36].try_into().unwrap()) | PREFIX_FLAG;
            row[32..36].copy_from_slice(&flags.to_le_bytes());
            length - prefix_len + 2
        } else {
            names.copy_within(offset..end, write);
            length
        };
        row[..4].copy_from_slice(&(write as u32).to_le_bytes());
        row[4..8].copy_from_slice(&(stored_length as u32).to_le_bytes());
        write += stored_length;
    }
    names.truncate(write);
    prefixes
}
