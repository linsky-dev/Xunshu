use crate::{session, Assets};
use anyhow::{ensure, Result};
use image::{imageops::FilterType, DynamicImage, ImageReader, Limits, RgbImage};
use ort::{session::Session, value::Tensor};
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SIDE: u32 = 960;
const RECOGNITION_HEIGHT: u32 = 48;
const MAX_LINES: usize = 512;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextLine {
    pub text: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub confidence: f32,
}

pub struct Ocr {
    det: Session,
    rec: Session,
    dictionary: Vec<String>,
}

#[derive(Clone, Copy)]
struct Region {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Ocr {
    pub fn new(assets: &Assets) -> Result<Self> {
        assets.initialize()?;
        let text = std::fs::read_to_string(assets.path("ppocr-dictionary.txt"))?;
        let dictionary: Vec<_> = text.lines().map(str::to_owned).collect();
        ensure!(
            !dictionary.is_empty() && dictionary.len() <= 32768,
            "OCR 字典不受支持，请修复安装"
        );
        Ok(Self {
            det: session(&assets.path("ppocr-det.onnx"))?,
            rec: session(&assets.path("ppocr-rec.onnx"))?,
            dictionary,
        })
    }

    pub fn recognize(&mut self, bytes: &[u8], cancel: &AtomicBool) -> Result<Vec<TextLine>> {
        ensure!(bytes.len() <= MAX_IMAGE_BYTES, "图片超过 OCR 输入上限");
        let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(96 * 1024 * 1024);
        reader.limits(limits);
        self.recognize_image(reader.decode()?, cancel)
    }

    pub fn recognize_image(
        &mut self,
        input: DynamicImage,
        cancel: &AtomicBool,
    ) -> Result<Vec<TextLine>> {
        ensure!(!cancel.load(Ordering::Relaxed), "OCR 已取消");
        let original_width = input.width();
        let original_height = input.height();
        ensure!(
            original_width > 0
                && original_height > 0
                && u64::from(original_width) * u64::from(original_height) <= 32 * 1024 * 1024,
            "图片像素数超过 OCR 预算"
        );
        let scale = (MAX_SIDE as f32 / original_width.max(original_height) as f32).min(1.0);
        let dimension = |length: u32| (((length as f32 * scale / 32.0).round() as u32).max(1)) * 32;
        let width = dimension(original_width);
        let height = dimension(original_height);
        let image = input
            .resize_exact(width, height, FilterType::Triangle)
            .to_rgb8();
        let tensor = Tensor::from_array((
            [1, 3, height as usize, width as usize],
            planar(&image, width, [0.485, 0.456, 0.406], [0.229, 0.224, 0.225]),
        ))?;
        let regions = {
            let outputs = self.det.run(ort::inputs![tensor])?;
            let (shape, map) = outputs[0].try_extract_tensor::<f32>()?;
            ensure!(
                shape.as_ref() == [1, 1, i64::from(height), i64::from(width)],
                "OCR 检测模型输出形状无效"
            );
            regions(map, width, height)
        };
        let mut lines = Vec::new();
        for region in regions {
            ensure!(!cancel.load(Ordering::Relaxed), "OCR 已取消");
            let scale_x =
                |x: u32| ((u64::from(x) * u64::from(original_width)) / u64::from(width)) as u32;
            let scale_y =
                |y: u32| ((u64::from(y) * u64::from(original_height)) / u64::from(height)) as u32;
            let x = scale_x(region.left);
            let y = scale_y(region.top);
            let w = (scale_x(region.right) - x).max(1);
            let h = (scale_y(region.bottom) - y).max(1);
            let crop = input.crop_imm(x, y, w, h);
            let resized_width = ((u64::from(w) * u64::from(RECOGNITION_HEIGHT))
                .div_ceil(u64::from(h)) as u32)
                .clamp(1, 1600);
            let crop = crop
                .resize_exact(resized_width, RECOGNITION_HEIGHT, FilterType::Triangle)
                .to_rgb8();
            let padded_width = resized_width.max(320);
            let tensor = Tensor::from_array((
                [1, 3, RECOGNITION_HEIGHT as usize, padded_width as usize],
                planar(&crop, padded_width, [0.5; 3], [0.5; 3]),
            ))?;
            let outputs = self.rec.run(ort::inputs![tensor])?;
            let (shape, values) = outputs[0].try_extract_tensor::<f32>()?;
            ensure!(
                shape.len() == 3 && shape[0] == 1,
                "OCR 识别模型输出形状无效"
            );
            let classes = usize::try_from(shape[2])?;
            let (text, confidence) = decode(values, classes, &self.dictionary)?;
            if !text.trim().is_empty() {
                lines.push(TextLine {
                    text,
                    x,
                    y,
                    width: w,
                    height: h,
                    confidence,
                });
            }
        }
        Ok(lines)
    }
}

fn planar(image: &RgbImage, padded_width: u32, mean: [f32; 3], std: [f32; 3]) -> Vec<f32> {
    let row = padded_width as usize;
    let area = row * image.height() as usize;
    let mut result = vec![0.0; area * 3];
    for (x, y, pixel) in image.enumerate_pixels() {
        let at = y as usize * row + x as usize;
        for channel in 0..3 {
            result[channel * area + at] =
                (f32::from(pixel[2 - channel]) / 255.0 - mean[channel]) / std[channel];
        }
    }
    result
}

/// 在检测概率图上找相连区域并为裁剪补足边距，坐标始终位于输入内。
fn regions(map: &[f32], width: u32, height: u32) -> Vec<Region> {
    let mut mask: Vec<_> = map.iter().map(|&value| value >= 0.3).collect();
    let mut stack = Vec::new();
    let mut regions = Vec::new();
    for position in 0..mask.len() {
        if !mask[position] {
            continue;
        }
        mask[position] = false;
        let x = position as u32 % width;
        let y = position as u32 / width;
        let mut bounds = Region {
            left: x,
            top: y,
            right: x,
            bottom: y,
        };
        let mut count = 0;
        stack.push(position as u32);
        while let Some(position) = stack.pop() {
            let x = position % width;
            let y = position / width;
            count += 1;
            bounds.left = bounds.left.min(x);
            bounds.right = bounds.right.max(x);
            bounds.top = bounds.top.min(y);
            bounds.bottom = bounds.bottom.max(y);
            for yy in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for xx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let neighbor = (yy * width + xx) as usize;
                    if mask[neighbor] {
                        mask[neighbor] = false;
                        stack.push(neighbor as u32);
                    }
                }
            }
        }
        let w = bounds.right - bounds.left + 1;
        let h = bounds.bottom - bounds.top + 1;
        if count < 12 || w < 3 || h < 3 {
            continue;
        }
        let pad = ((w * h) / (w + h)).max(2) / 2 + 1;
        bounds.left = bounds.left.saturating_sub(pad);
        bounds.top = bounds.top.saturating_sub(pad);
        bounds.right = (bounds.right + pad + 1).min(width);
        bounds.bottom = (bounds.bottom + pad + 1).min(height);
        regions.push(bounds);
        if regions.len() == MAX_LINES {
            break;
        }
    }
    regions.sort_by_key(|region| (region.top / 12, region.left, region.top));
    regions
}

fn decode(values: &[f32], classes: usize, dictionary: &[String]) -> Result<(String, f32)> {
    ensure!(
        (classes == dictionary.len() + 1 || classes == dictionary.len() + 2)
            && values.len().is_multiple_of(classes),
        "OCR 识别模型与字典不匹配"
    );
    let mut text = String::new();
    let mut previous = 0;
    let mut count = 0;
    let mut score = 0.0;
    for step in values.chunks_exact(classes) {
        let Some((id, confidence)) = step
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, value)| value.is_finite())
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            continue;
        };
        if id != 0 && id != previous {
            text.push_str(dictionary.get(id - 1).map_or(" ", String::as_str));
            count += 1;
            score += confidence;
        }
        previous = id;
    }
    Ok((text, if count > 0 { score / count as f32 } else { 0.0 }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ctc_blank_and_duplicate_collapse() {
        let predictions = [
            0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0,
        ];
        assert_eq!(
            super::decode(&predictions, 3, &["中".into(), "文".into()]).unwrap(),
            ("中文".replace('中', "中中"), 1.0)
        );
    }
}
