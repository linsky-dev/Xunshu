//! Windows USN V2/V3 字节解码。此模块不访问卷或文件。

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsnRecord {
    pub object: [u8; 16],
    pub parent: [u8; 16],
    pub name: String,
    pub usn: i64,
    pub attributes: u32,
    pub reason: u32,
    /// V2 使用 64 位 NTFS 记录引用，V3 使用 128 位文件标识。
    pub version: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativePage<C> {
    pub next: C,
    pub records: Vec<UsnRecord>,
}

pub(super) fn integer_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub(super) fn integer_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub(super) fn decode(bytes: &[u8]) -> std::result::Result<NativePage<u64>, &'static str> {
    if bytes.len() < 8 {
        return Err("USN 页缺少续读游标");
    }
    let next = integer_u64(bytes, 0);
    let mut records = Vec::new();
    let mut remaining = &bytes[8..];
    while !remaining.is_empty() {
        let Some(header) = remaining.get(..8) else {
            return Err("USN 记录头被截断");
        };
        let length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        let (id_size, fixed) = match version {
            2 => (8, 60),
            3 => (16, 76),
            _ => return Err("USN 记录版本不受支持"),
        };
        if length < fixed || length > remaining.len() || !length.is_multiple_of(8) {
            return Err("USN 记录长度无效");
        }
        let row = &remaining[..length];
        let field =
            |offset| u16::from_le_bytes(row[offset..offset + 2].try_into().unwrap()) as usize;
        let size = field(fixed - 4);
        let offset = field(fixed - 2);
        if size == 0
            || !size.is_multiple_of(2)
            || offset < fixed
            || offset > length
            || size > length - offset
        {
            return Err("USN 文件名字段无效");
        }
        let utf16: Vec<_> = row[offset..offset + size]
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        let name = String::from_utf16(&utf16).map_err(|_| "USN 文件名不是有效的 UTF-16")?;
        if name.contains(['\0', '\\', '/']) {
            return Err("USN 文件名含路径分隔符或 NUL");
        }
        let mut object = [0; 16];
        let mut parent = [0; 16];
        object[..id_size].copy_from_slice(&row[8..8 + id_size]);
        parent[..id_size].copy_from_slice(&row[8 + id_size..8 + 2 * id_size]);
        let reason = u32::from_le_bytes(row[fixed - 20..fixed - 16].try_into().unwrap());
        let attributes = u32::from_le_bytes(row[fixed - 8..fixed - 4].try_into().unwrap());
        records.push(UsnRecord {
            object,
            parent,
            name,
            usn: integer_i64(row, 8 + 2 * id_size),
            attributes,
            reason,
            version,
        });
        remaining = &remaining[length..];
    }
    Ok(NativePage { next, records })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usn_pages_preserve_v2_and_v3_ids_and_reject_unbounded_fields() {
        let mut bytes = 99u64.to_le_bytes().to_vec();
        for (version, id_size, fixed, length) in [(2u16, 8, 60, 64usize), (3, 16, 76, 80)] {
            let mut row = vec![0; length];
            row[..4].copy_from_slice(&(length as u32).to_le_bytes());
            row[4..6].copy_from_slice(&version.to_le_bytes());
            row[8..8 + id_size].fill(0x23);
            row[8 + id_size..8 + 2 * id_size].fill(0x45);
            row[8 + 2 * id_size..16 + 2 * id_size].copy_from_slice(&33i64.to_le_bytes());
            row[fixed - 20..fixed - 16].copy_from_slice(&0x2000u32.to_le_bytes());
            row[fixed - 8..fixed - 4].copy_from_slice(&0x20u32.to_le_bytes());
            row[fixed - 4..fixed - 2].copy_from_slice(&2u16.to_le_bytes());
            row[fixed - 2..fixed].copy_from_slice(&(fixed as u16).to_le_bytes());
            row[fixed..fixed + 2].copy_from_slice(&('文' as u16).to_le_bytes());
            bytes.extend_from_slice(&row);
        }
        let page = decode(&bytes).unwrap();
        assert_eq!(page.next, 99);
        assert_eq!(page.records[0].object[8..], [0; 8]);
        assert_eq!(page.records[1].object, [0x23; 16]);
        assert_eq!(page.records[1].parent, [0x45; 16]);
        assert_eq!(page.records[0].reason, 0x2000);
        assert_eq!(page.records[0].attributes, 0x20);
        assert_eq!(page.records[1].name, "文");
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        bytes[8 + 56..8 + 58].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }
}
