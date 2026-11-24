//! mmap 可直接读取的记录压缩。每列保存最小值，记录只保存定宽差值。
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) const RECORD_BYTES: usize = 40;
const COLUMN_WIDTHS: [u8; 8] = [4, 4, 4, 4, 8, 8, 4, 4];
const SIGN_BIT: u64 = 1 << 63;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackedRecords {
    widths: [u8; 8],
    minimums: [u64; 8],
}

fn read_value(bytes: &[u8]) -> u64 {
    let mut wide = [0; 8];
    wide[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(wide)
}

fn columns(row: &[u8; RECORD_BYTES]) -> [u64; 8] {
    let mut values = [0; 8];
    let mut start = 0;
    for (i, width) in COLUMN_WIDTHS.into_iter().enumerate() {
        let end = start + usize::from(width);
        values[i] = read_value(&row[start..end]);
        start = end;
    }
    // 将 i64 时间顺序映射到 u64，使跨 Unix 纪元的相近日期仍可压缩。
    values[5] ^= SIGN_BIT;
    values
}

impl PackedRecords {
    pub(crate) fn compact(records: &mut Vec<u8>) -> Option<Self> {
        if records.len() < 1024 * RECORD_BYTES {
            return None;
        }
        let mut minimums = [u64::MAX; 8];
        let mut maximums = [0; 8];
        for row in records.as_chunks::<RECORD_BYTES>().0 {
            for (i, value) in columns(row).into_iter().enumerate() {
                minimums[i] = minimums[i].min(value);
                maximums[i] = maximums[i].max(value);
            }
        }
        let widths = std::array::from_fn(|i| {
            (u64::BITS - (maximums[i] - minimums[i]).leading_zeros()).div_ceil(8) as u8
        });
        let layout = Self { widths, minimums };
        if layout.width() > RECORD_BYTES * 4 / 5 {
            return None;
        }
        let mut write = 0;
        for read in (0..records.len()).step_by(RECORD_BYTES) {
            let values = columns(records[read..read + RECORD_BYTES].try_into().unwrap());
            for (i, value) in values.into_iter().enumerate() {
                let bytes = (value - minimums[i]).to_le_bytes();
                let width = usize::from(widths[i]);
                // 逐行向前重写，输出永不覆盖尚未读取的下一行。
                records[write..write + width].copy_from_slice(&bytes[..width]);
                write += width;
            }
        }
        records.truncate(write);
        Some(layout)
    }

    pub(crate) fn width(&self) -> usize {
        self.widths.iter().map(|&width| usize::from(width)).sum()
    }

    pub(crate) fn validate_layout(&self) -> Result<()> {
        for (i, width) in COLUMN_WIDTHS.into_iter().enumerate() {
            ensure!(self.widths[i] <= width, "invalid packed record width");
        }
        Ok(())
    }

    /// 若所有合法宽度的差值都能安全相加，校验每行时无需重复算术检查。
    pub(crate) fn arithmetic_safe(&self) -> bool {
        let maximum = |width: u8| {
            if width == 8 {
                u64::MAX
            } else {
                (1u64 << (u32::from(width) * 8)) - 1
            }
        };
        COLUMN_WIDTHS.iter().enumerate().all(|(i, &column_width)| {
            self.minimums[i]
                .checked_add(maximum(self.widths[i]))
                .is_some_and(|max| max <= maximum(column_width))
        })
    }

    pub(crate) fn decode<const CHECKED: bool>(&self, bytes: &[u8]) -> Result<[u8; RECORD_BYTES]> {
        let mut row = [0; RECORD_BYTES];
        let mut read = 0;
        let mut write = 0;
        for (i, column_width) in COLUMN_WIDTHS.into_iter().enumerate() {
            let width = usize::from(self.widths[i]);
            let delta = read_value(&bytes[read..read + width]);
            let mut value = if CHECKED {
                let value = self.minimums[i]
                    .checked_add(delta)
                    .context("packed record overflow")?;
                ensure!(
                    column_width == 8 || u32::try_from(value).is_ok(),
                    "invalid packed record value"
                );
                value
            } else {
                self.minimums[i].wrapping_add(delta)
            };
            if i == 5 {
                value ^= SIGN_BIT;
            }
            let column_width = usize::from(column_width);
            row[write..write + column_width].copy_from_slice(&value.to_le_bytes()[..column_width]);
            read += width;
            write += column_width;
        }
        Ok(row)
    }

    /// 零拷贝热列解码：直接返回 (name_off, name_len, parent_id, source_id, flags)，
    /// 跳过 40 字节行缓冲拷贝以及 size/modified/metadata_offset 的无用解包。
    #[inline]
    pub(crate) fn decode_hot_fields(&self, bytes: &[u8]) -> (u32, u32, u32, u32, u32) {
        let mut read = 0usize;
        let w0 = usize::from(self.widths[0]);
        let name_off = self.minimums[0].wrapping_add(read_value(&bytes[read..read + w0])) as u32;
        read += w0;
        let w1 = usize::from(self.widths[1]);
        let name_len = self.minimums[1].wrapping_add(read_value(&bytes[read..read + w1])) as u32;
        read += w1;
        let w2 = usize::from(self.widths[2]);
        let parent_id = self.minimums[2].wrapping_add(read_value(&bytes[read..read + w2])) as u32;
        read += w2;
        let w3 = usize::from(self.widths[3]);
        let source_id = self.minimums[3].wrapping_add(read_value(&bytes[read..read + w3])) as u32;
        read += w3;
        read += usize::from(self.widths[4]) + usize::from(self.widths[5]);
        let w6 = usize::from(self.widths[6]);
        let flags = self.minimums[6].wrapping_add(read_value(&bytes[read..read + w6])) as u32;
        (name_off, name_len, parent_id, source_id, flags)
    }
}

/// 冷列（size 与 modified）独立定宽差值压缩布局，避免纯文件名查询触碰冷页。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColdRecords {
    pub(crate) widths: [u8; 2],
    pub(crate) minimums: [u64; 2],
}

impl ColdRecords {
    pub(crate) fn extract(records: &[u8]) -> Option<(Self, Vec<u8>)> {
        if records.len() < 1024 * RECORD_BYTES {
            return None;
        }
        let mut minimums = [u64::MAX; 2];
        let mut maximums = [0u64; 2];
        for row in records.as_chunks::<RECORD_BYTES>().0 {
            let size = read_value(&row[16..24]);
            let modified = read_value(&row[24..32]) ^ SIGN_BIT;
            minimums[0] = minimums[0].min(size);
            maximums[0] = maximums[0].max(size);
            minimums[1] = minimums[1].min(modified);
            maximums[1] = maximums[1].max(modified);
        }
        let widths = std::array::from_fn(|i| {
            (u64::BITS - (maximums[i] - minimums[i]).leading_zeros()).div_ceil(8) as u8
        });
        let layout = Self { widths, minimums };
        let row_width = layout.width();
        let count = records.len() / RECORD_BYTES;
        let mut out = Vec::with_capacity(count * row_width);
        for row in records.as_chunks::<RECORD_BYTES>().0 {
            let vals = [
                read_value(&row[16..24]),
                read_value(&row[24..32]) ^ SIGN_BIT,
            ];
            for (i, val) in vals.into_iter().enumerate() {
                let bytes = (val - minimums[i]).to_le_bytes();
                out.extend_from_slice(&bytes[..usize::from(widths[i])]);
            }
        }
        Some((layout, out))
    }

    pub(crate) fn width(&self) -> usize {
        usize::from(self.widths[0]) + usize::from(self.widths[1])
    }

    pub(crate) fn validate_layout(&self) -> Result<()> {
        ensure!(
            self.widths[0] <= 8 && self.widths[1] <= 8,
            "invalid cold record width"
        );
        Ok(())
    }

    pub(crate) fn decode_into(&self, bytes: &[u8], row: &mut [u8; RECORD_BYTES]) {
        let w0 = usize::from(self.widths[0]);
        let w1 = usize::from(self.widths[1]);
        let size = self.minimums[0].wrapping_add(read_value(&bytes[..w0]));
        let modified = self.minimums[1].wrapping_add(read_value(&bytes[w0..w0 + w1])) ^ SIGN_BIT;
        row[16..24].copy_from_slice(&size.to_le_bytes());
        row[24..32].copy_from_slice(&modified.to_le_bytes());
    }
}
