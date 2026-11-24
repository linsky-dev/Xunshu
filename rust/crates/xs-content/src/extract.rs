//! 从已授权的文档字节中抽取正文。解析器不读取任何路径或外部引用。

use anyhow::{bail, ensure, Context, Result};
use calamine::{open_workbook_from_rs, Data, Ods, Range, Reader as WorkbookReader, Xlsx};
use quick_xml::{events::Event, Reader, XmlVersion};
use scraper::{Html, Node};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
    sync::atomic::AtomicBool,
};
use xs_inference::{Assets, Ocr, TextLine};
use zip::ZipArchive;

pub(crate) const MAX_INPUT: usize = 32 * 1024 * 1024;
pub(crate) const MAX_WORKER_OUTPUT: usize = 16 * 1024 * 1024;
const MAX_EXPANDED: u64 = 64 * 1024 * 1024;
const MAX_TEXT: usize = 2 * 1024 * 1024;
const MAX_SEGMENTS: usize = 8192;
const MAX_ARCHIVE_ENTRIES: usize = 4096;
const CHUNK_CHARS: usize = 2048;
const OVERLAP_CHARS: usize = 256;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Location {
    Text {
        line: u32,
    },
    Paragraph {
        number: u32,
    },
    Slide {
        number: u32,
    },
    Page {
        number: u32,
    },
    PageRegion {
        number: u32,
        image: u32,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    ImageRegion {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    Cell {
        sheet: String,
        address: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Segment {
    pub location: Location,
    pub text: String,
}

#[derive(Default)]
struct Segments {
    parts: Vec<Segment>,
    bytes: usize,
}

impl Segments {
    fn add(&mut self, text: &str, mut location: Location) -> Result<()> {
        let mut tail = text;
        while !tail.is_empty() {
            let mut positions = tail.char_indices();
            let mut next = 0;
            let mut end = tail.len();
            for (count, (at, _)) in (&mut positions).enumerate().take(CHUNK_CHARS + 1) {
                if count == CHUNK_CHARS - OVERLAP_CHARS {
                    next = at;
                }
                if count == CHUNK_CHARS {
                    end = at;
                }
            }
            let chunk = tail[..end].trim();
            if !chunk.is_empty() {
                ensure!(
                    self.bytes + chunk.len() <= MAX_TEXT && self.parts.len() < MAX_SEGMENTS,
                    "文档正文超出抽取预算"
                );
                self.bytes += chunk.len();
                self.parts.push(Segment {
                    location: location.clone(),
                    text: chunk.to_owned(),
                });
            }
            if end == tail.len() {
                break;
            }
            if let Location::Text { line } = &mut location {
                *line += tail[..next].bytes().filter(|b| *b == b'\n').count() as u32;
            }
            tail = &tail[next..];
        }
        Ok(())
    }
}

pub(crate) fn supports(extension: &str, ocr: bool) -> bool {
    matches!(
        extension,
        "txt"
            | "md"
            | "markdown"
            | "htm"
            | "html"
            | "docx"
            | "pptx"
            | "xlsx"
            | "odt"
            | "odp"
            | "ods"
            | "pdf"
    ) || (ocr && is_image(extension))
}

pub(crate) fn is_image(extension: &str) -> bool {
    matches!(extension, "png" | "jpg" | "jpeg" | "tif" | "tiff" | "webp")
}

fn decode(bytes: &[u8]) -> Result<std::borrow::Cow<'_, str>> {
    if let Some((encoding, skip)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding
            .decode_without_bom_handling_and_without_replacement(&bytes[skip..])
            .context("文档编码与字节顺序标记不一致");
    }
    if let Ok(value) = std::str::from_utf8(bytes) {
        return Ok(value.into());
    }
    encoding_rs::GB18030
        .decode_without_bom_handling_and_without_replacement(bytes)
        .context("正文不是有效的 UTF-8、UTF-16 BOM 或 GB18030")
}

pub(crate) fn extract(
    extension: &str,
    bytes: &[u8],
    ocr: bool,
    recognizer: &mut Option<Ocr>,
) -> Result<Vec<Segment>> {
    ensure!(bytes.len() <= MAX_INPUT, "文档大小超出抽取预算");
    let mut result = Segments::default();
    match extension {
        "txt" | "md" | "markdown" => result.add(&decode(bytes)?, Location::Text { line: 1 })?,
        "html" | "htm" => html(&decode(bytes)?, &mut result)?,
        "docx" | "pptx" | "odt" | "odp" => office(extension, bytes, &mut result)?,
        "xlsx" => {
            check_archive(bytes)?;
            let book: Xlsx<_> = open_workbook_from_rs(Cursor::new(bytes))?;
            workbook(book, &mut result)?;
        }
        "ods" => {
            check_archive(bytes)?;
            let book: Ods<_> = open_workbook_from_rs(Cursor::new(bytes))?;
            workbook(book, &mut result)?;
        }
        "pdf" => pdf(bytes, ocr, recognizer, &mut result)?,
        image if ocr && is_image(image) => {
            let recognizer = load_ocr(recognizer)?;
            let lines = recognizer.recognize(bytes, &AtomicBool::new(false))?;
            for TextLine {
                text,
                x,
                y,
                width,
                height,
                ..
            } in lines
            {
                result.add(
                    &text,
                    Location::ImageRegion {
                        x,
                        y,
                        width,
                        height,
                    },
                )?;
            }
        }
        _ => bail!("不支持的正文格式"),
    }
    Ok(result.parts)
}

fn html(text: &str, result: &mut Segments) -> Result<()> {
    let html = Html::parse_document(text);
    let mut visible = String::new();
    for node in html.root_element().descendants() {
        let hidden = node.ancestors().any(|parent| {
            parent.value().as_element().is_some_and(|element| {
                matches!(
                    element.name(),
                    "head" | "script" | "style" | "template" | "noscript" | "svg"
                ) || element.attr("hidden").is_some()
            })
        });
        if hidden {
            continue;
        }
        match node.value() {
            Node::Text(text) => visible.push_str(text.text.as_ref()),
            Node::Element(element)
                if matches!(
                    element.name(),
                    "address"
                        | "article"
                        | "blockquote"
                        | "br"
                        | "div"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "li"
                        | "p"
                        | "pre"
                        | "section"
                        | "td"
                        | "th"
                        | "tr"
                ) =>
            {
                visible.push('\n');
            }
            _ => {}
        }
        ensure!(visible.len() <= MAX_TEXT, "HTML 正文超出抽取预算");
    }
    result.add(&visible, Location::Text { line: 1 })
}

fn pdf(bytes: &[u8], ocr: bool, recognizer: &mut Option<Ocr>, result: &mut Segments) -> Result<()> {
    let document = lopdf::Document::load_mem(bytes)?;
    ensure!(!document.is_encrypted(), "不支持加密 PDF 的正文抽取");
    let pages = document.get_pages();
    ensure!(pages.len() <= MAX_SEGMENTS, "PDF 页数超出抽取预算");
    for (number, id) in pages {
        let text = document.extract_text(&[number])?;
        // 混合页面既可能有可提取文字，也可能有扫描图片，二者都需要收录。
        // 未开启 OCR 时不访问图像；开启后扫描页内图像，避免文字存在即跳过 OCR。
        if !text.trim().is_empty() {
            result.add(&text, Location::Page { number })?;
        }
        if ocr {
            scan_page(&document, id, number, recognizer, result)?;
        }
    }
    Ok(())
}

fn load_ocr(recognizer: &mut Option<Ocr>) -> Result<&mut Ocr> {
    if recognizer.is_none() {
        *recognizer = Some(Ocr::new(&Assets::installed()?)?);
    }
    recognizer.as_mut().context("OCR 未就绪")
}

// PDF resources are inheritable, and images may be drawn inside Form XObjects.
// Resolve only actually invoked objects, not unused resource dictionary entries.
fn page_resources(
    document: &lopdf::Document,
    page: lopdf::ObjectId,
) -> Result<Option<&lopdf::Dictionary>> {
    let mut node = page;
    let mut seen = BTreeSet::new();
    loop {
        ensure!(
            seen.len() < 64 && seen.insert(node),
            "PDF 页面资源继承循环或超出深度限制"
        );
        let dict = document.get_dictionary(node)?;
        if dict.has(b"Resources") {
            return Ok(Some(document.get_dict_in_dict(dict, b"Resources")?));
        }
        if !dict.has(b"Parent") {
            return Ok(None);
        }
        node = dict.get(b"Parent")?.as_reference()?;
    }
}

fn decoded_stream(stream: &lopdf::Stream) -> Result<Vec<u8>> {
    // A malformed Filter must not be interpreted as an uncompressed stream.
    let bytes = if stream.dict.has(b"Filter") {
        stream.filters()?;
        stream.decompressed_content()?
    } else {
        stream.content.clone()
    };
    ensure!(bytes.len() <= MAX_EXPANDED as usize, "PDF 内容流超出预算");
    Ok(bytes)
}

#[derive(Default)]
struct DrawnImages {
    images: Vec<lopdf::ObjectId>,
    seen: BTreeSet<lopdf::ObjectId>,
    forms: BTreeSet<lopdf::ObjectId>,
    operations: usize,
    decoded_bytes: usize,
}

impl DrawnImages {
    fn visit<'a>(
        &mut self,
        document: &'a lopdf::Document,
        bytes: &[u8],
        resources: Option<&'a lopdf::Dictionary>,
    ) -> Result<()> {
        self.decoded_bytes = self
            .decoded_bytes
            .checked_add(bytes.len())
            .context("PDF 内容流预算溢出")?;
        ensure!(
            self.decoded_bytes <= MAX_EXPANDED as usize,
            "PDF 页面内容流累计超出预算"
        );
        let content = lopdf::content::Content::decode_strict(bytes)?;
        self.operations = self
            .operations
            .checked_add(content.operations.len())
            .context("PDF 操作预算溢出")?;
        ensure!(self.operations <= 250_000, "PDF 页面绘制操作过多");
        for op in content.operations {
            // The parser can identify inline images but their rendering is not
            // implemented here. Fail explicitly instead of reporting full OCR.
            ensure!(op.operator != "BI", "暂不支持 PDF 内联图像 OCR");
            if op.operator != "Do" {
                continue;
            }
            ensure!(op.operands.len() == 1, "PDF XObject 绘制参数无效");
            let name = op.operands[0].as_name()?;
            let resources = resources.context("PDF 绘制对象缺少资源字典")?;
            let objects = document.get_dict_in_dict(resources, b"XObject")?;
            let (id, object) = document.dereference(objects.get(name)?)?;
            let id = id.context("PDF XObject 缺少间接对象标识")?;
            let stream = object.as_stream()?;
            match stream.dict.get(b"Subtype")?.as_name()? {
                b"Image" => {
                    if self.seen.insert(id) {
                        ensure!(self.images.len() < 64, "扫描 PDF 单页图像过多");
                        self.images.push(id);
                    }
                }
                b"Form" => {
                    ensure!(
                        self.forms.len() < 32 && self.forms.insert(id),
                        "PDF Form 循环或超出深度限制"
                    );
                    let nested = if stream.dict.has(b"Resources") {
                        Some(document.get_dict_in_dict(&stream.dict, b"Resources")?)
                    } else {
                        Some(resources)
                    };
                    self.visit(document, &decoded_stream(stream)?, nested)?;
                    self.forms.remove(&id);
                }
                _ => bail!("暂不支持 PDF XObject 绘制类型"),
            }
        }
        Ok(())
    }
}

fn drawn_images(document: &lopdf::Document, page: lopdf::ObjectId) -> Result<Vec<lopdf::ObjectId>> {
    let dict = document.get_dictionary(page)?;
    let Some(contents) = dict.get(b"Contents").ok() else {
        return Ok(Vec::new());
    };
    let (_, contents) = document.dereference(contents)?;
    let streams: Vec<&lopdf::Object> = match contents {
        lopdf::Object::Array(items) => {
            ensure!(items.len() <= 4096, "PDF 页面内容流数量超出预算");
            items.iter().collect()
        }
        lopdf::Object::Stream(_) => vec![contents],
        _ => bail!("PDF 页面内容流类型无效"),
    };
    // Contents arrays form a single logical content stream.
    let mut bytes = Vec::new();
    for object in streams {
        let (_, object) = document.dereference(object)?;
        let part = decoded_stream(object.as_stream()?)?;
        ensure!(
            bytes.len().saturating_add(part.len()).saturating_add(1) <= MAX_EXPANDED as usize,
            "PDF 页面内容流累计超出预算"
        );
        bytes.extend(part);
        bytes.push(b'\n');
    }
    let mut drawn = DrawnImages::default();
    drawn.visit(document, &bytes, page_resources(document, page)?)?;
    Ok(drawn.images)
}

fn scan_page(
    document: &lopdf::Document,
    page: lopdf::ObjectId,
    number: u32,
    recognizer: &mut Option<Ocr>,
    result: &mut Segments,
) -> Result<()> {
    for (index, id) in drawn_images(document, page)?.into_iter().enumerate() {
        let image = document.get_object(id)?.as_stream()?;
        let width = u32::try_from(image.dict.get(b"Width")?.as_i64()?)?;
        let height = u32::try_from(image.dict.get(b"Height")?.as_i64()?)?;
        let pixels = u64::from(width) * u64::from(height);
        ensure!(
            width > 0 && height > 0 && pixels <= 32 * 1024 * 1024,
            "扫描 PDF 图像像素数超出预算"
        );
        let recognizer = load_ocr(recognizer)?;
        let cancel = AtomicBool::new(false);
        let filters = if image.dict.has(b"Filter") {
            image.filters()?
        } else {
            Vec::new()
        };
        let lines = if matches!(filters.as_slice(), [filter] if *filter == b"DCTDecode") {
            recognizer.recognize(&image.content, &cancel)?
        } else {
            let bytes = decoded_stream(image)?;
            ensure!(
                image.dict.get(b"BitsPerComponent")?.as_i64()? == 8,
                "不支持此扫描 PDF 图像位深"
            );
            let source = match image.dict.get(b"ColorSpace")?.as_name()? {
                b"DeviceGray" if bytes.len() as u64 == pixels => image::DynamicImage::ImageLuma8(
                    image::GrayImage::from_raw(width, height, bytes)
                        .context("扫描 PDF 灰度像素无效")?,
                ),
                b"DeviceRGB" if bytes.len() as u64 == pixels * 3 => image::DynamicImage::ImageRgb8(
                    image::RgbImage::from_raw(width, height, bytes)
                        .context("扫描 PDF RGB 像素无效")?,
                ),
                _ => bail!("不支持此扫描 PDF 图像颜色空间或像素布局"),
            };
            recognizer.recognize_image(source, &cancel)?
        };
        for TextLine {
            text,
            x,
            y,
            width,
            height,
            ..
        } in lines
        {
            result.add(
                &text,
                Location::PageRegion {
                    number,
                    image: index as u32 + 1,
                    x,
                    y,
                    width,
                    height,
                },
            )?;
        }
    }
    Ok(())
}

fn check_archive(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    ensure!(
        archive.len() <= MAX_ARCHIVE_ENTRIES,
        "压缩文档文件数超出预算"
    );
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index)?;
        expanded = expanded
            .checked_add(entry.size())
            .context("压缩文档展开大小溢出")?;
        ensure!(expanded <= MAX_EXPANDED, "压缩文档展开大小超出预算");
    }
    Ok(archive)
}

fn member(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    path: &str,
    budget: &mut u64,
) -> Result<Vec<u8>> {
    let entry = archive.by_name(path)?;
    let mut bytes = Vec::new();
    entry.take(*budget + 1).read_to_end(&mut bytes)?;
    *budget = budget
        .checked_sub(bytes.len() as u64)
        .context("压缩文档展开大小超出预算")?;
    Ok(bytes)
}

fn office(extension: &str, bytes: &[u8], result: &mut Segments) -> Result<()> {
    let mut archive = check_archive(bytes)?;
    let mut budget = MAX_EXPANDED;
    match extension {
        "docx" => {
            let bytes = member(&mut archive, "word/document.xml", &mut budget)?;
            for (number, text) in xml_paragraphs(&bytes, false, false)? {
                result.add(&text, Location::Paragraph { number })?;
            }
        }
        "pptx" => {
            let presentation = member(&mut archive, "ppt/presentation.xml", &mut budget)?;
            let rels = member(&mut archive, "ppt/_rels/presentation.xml.rels", &mut budget)?;
            let slides = slide_targets(&presentation, &rels)?;
            for (index, path) in slides.into_iter().enumerate() {
                let bytes = member(&mut archive, &path, &mut budget)?;
                let text = xml_paragraphs(&bytes, false, false)?
                    .into_iter()
                    .map(|(_, text)| text)
                    .collect::<Vec<_>>()
                    .join("\n");
                result.add(
                    &text,
                    Location::Slide {
                        number: index as u32 + 1,
                    },
                )?;
            }
        }
        "odt" | "odp" => {
            let bytes = member(&mut archive, "content.xml", &mut budget)?;
            let slides = extension == "odp";
            for (number, text) in xml_paragraphs(&bytes, true, slides)? {
                let location = if slides {
                    Location::Slide { number }
                } else {
                    Location::Paragraph { number }
                };
                result.add(&text, location)?;
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn entity(reference: &quick_xml::events::BytesRef<'_>) -> Result<char> {
    if let Some(value) = reference.resolve_char_ref()? {
        return Ok(value);
    }
    let name: &[u8] = reference.as_ref();
    match name {
        b"amp" => Ok('&'),
        b"lt" => Ok('<'),
        b"gt" => Ok('>'),
        b"quot" => Ok('"'),
        b"apos" => Ok('\''),
        _ => bail!("文档使用了不支持的 XML 实体"),
    }
}

fn xml_paragraphs(bytes: &[u8], odf: bool, slides: bool) -> Result<Vec<(u32, String)>> {
    let mut reader = Reader::from_reader(bytes);
    let mut text = String::new();
    let mut parts = Vec::new();
    let mut paragraph = None;
    let mut text_run = None;
    let mut depth = 0;
    let mut total = 0;
    let mut paragraph_number = 0;
    let mut slide_number = 0;
    let mut slide = None;
    loop {
        let capture = paragraph.is_some() && (odf || text_run.is_some());
        match reader.read_event()? {
            Event::Start(tag) => {
                depth += 1;
                ensure!(depth <= 128, "文档 XML 嵌套过深");
                let in_scope = !slides || slide.is_some();
                match tag.local_name().as_ref() {
                    b"page" if slides && slide.is_none() => {
                        slide_number += 1;
                        slide = Some(depth);
                    }
                    b"p" if in_scope && paragraph.is_none() => paragraph = Some(depth),
                    b"h" if in_scope && odf && paragraph.is_none() => paragraph = Some(depth),
                    b"t" if paragraph.is_some() && !odf => text_run = Some(depth),
                    _ => {}
                }
            }
            Event::Empty(tag)
                if slides && slide.is_none() && tag.local_name().as_ref() == b"page" =>
            {
                slide_number += 1;
            }
            Event::Empty(tag)
                if (!slides || slide.is_some())
                    && paragraph.is_none()
                    && (tag.local_name().as_ref() == b"p"
                        || (odf && tag.local_name().as_ref() == b"h")) =>
            {
                paragraph_number += 1;
            }
            Event::Empty(tag) if paragraph.is_some() => match tag.local_name().as_ref() {
                b"tab" => text.push('\t'),
                b"br" | b"line-break" => text.push('\n'),
                b"s" if odf => {
                    let count = tag
                        .attributes()
                        .filter_map(std::result::Result::ok)
                        .find(|a| a.key.local_name().as_ref() == b"c")
                        .map(|a| {
                            a.decoded_and_normalized_value(
                                XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )?
                            .parse::<usize>()
                            .context("非法 ODF 空格计数")
                        })
                        .transpose()?
                        .unwrap_or(1);
                    ensure!(
                        count <= MAX_TEXT.saturating_sub(total + text.len()),
                        "ODF 空格计数超出预算"
                    );
                    text.extend(std::iter::repeat_n(' ', count));
                }
                _ => {}
            },
            Event::End(_) => {
                if paragraph == Some(depth) {
                    total += text.len();
                    paragraph_number += 1;
                    if !text.trim().is_empty() {
                        let number = if slides {
                            slide_number
                        } else {
                            paragraph_number
                        };
                        parts.push((number, std::mem::take(&mut text)));
                    } else {
                        text.clear();
                    }
                    paragraph = None;
                }
                if text_run == Some(depth) {
                    text_run = None;
                }
                if slide == Some(depth) {
                    slide = None;
                }
                depth -= 1;
            }
            Event::Text(value) if capture => text.push_str(&value.xml10_content()?),
            Event::CData(value) if capture => text.push_str(&value.decode()?),
            Event::GeneralRef(value) if capture => text.push(entity(&value)?),
            Event::DocType(_) => bail!("正文 XML 禁止文档类型和自定义实体"),
            Event::Eof => {
                ensure!(depth == 0, "正文 XML 未完整结束");
                break;
            }
            _ => {}
        }
        ensure!(
            total + text.len() <= MAX_TEXT && parts.len() <= MAX_SEGMENTS,
            "文档正文超出抽取预算"
        );
    }
    Ok(parts)
}

fn slide_targets(presentation: &[u8], rels: &[u8]) -> Result<Vec<String>> {
    let mut relationships = BTreeMap::new();
    let mut reader = Reader::from_reader(rels);
    loop {
        match reader.read_event()? {
            Event::Start(tag) | Event::Empty(tag)
                if tag.local_name().as_ref() == b"Relationship" =>
            {
                let mut id = None;
                let mut target = None;
                let mut is_slide = false;
                let mut external = false;
                for a in tag.attributes() {
                    let a = a?;
                    match a.key.as_ref() {
                        b"Id" => {
                            id = Some(
                                a.decoded_and_normalized_value(
                                    XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )?
                                .into_owned(),
                            )
                        }
                        b"Target" => {
                            target = Some(
                                a.decoded_and_normalized_value(
                                    XmlVersion::Implicit1_0,
                                    reader.decoder(),
                                )?
                                .into_owned(),
                            )
                        }
                        b"Type" => is_slide = a.value.ends_with(b"/slide"),
                        b"TargetMode" => external = a.value.as_ref() != b"Internal",
                        _ => {}
                    }
                }
                if is_slide && !external {
                    let id = id.context("演示文稿关系缺少 Id")?;
                    let target = target.context("演示文稿关系缺少 Target")?;
                    let path = target.strip_prefix("/ppt/").unwrap_or(&target);
                    let slide = path
                        .strip_prefix("slides/")
                        .filter(|name| {
                            name.strip_suffix(".xml").is_some_and(|name| {
                                !name.is_empty()
                                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                            })
                        })
                        .context("幻灯片关系超出 PPTX 内部允许路径")?;
                    ensure!(
                        relationships
                            .insert(id, format!("ppt/slides/{slide}"))
                            .is_none(),
                        "重复的幻灯片关系"
                    );
                }
            }
            Event::DocType(_) => bail!("演示文稿 XML 禁止自定义实体"),
            Event::Eof => break,
            _ => {}
        }
        ensure!(relationships.len() <= MAX_SEGMENTS, "幻灯片数量超出预算");
    }
    let mut targets = Vec::new();
    let mut reader = Reader::from_reader(presentation);
    loop {
        match reader.read_event()? {
            Event::Start(tag) | Event::Empty(tag) if tag.local_name().as_ref() == b"sldId" => {
                let id = tag
                    .attributes()
                    .find_map(|attribute| match attribute {
                        Ok(a)
                            if a.key.prefix().is_some() && a.key.local_name().as_ref() == b"id" =>
                        {
                            Some(Ok(a))
                        }
                        Err(error) => Some(Err(error)),
                        _ => None,
                    })
                    .transpose()?
                    .context("演示文稿幻灯片缺少关系 ID")?
                    .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())?;
                let target = relationships
                    .get(id.as_ref())
                    .context("演示文稿引用了缺失或外部幻灯片")?;
                targets.push(target.clone());
            }
            Event::DocType(_) => bail!("演示文稿 XML 禁止自定义实体"),
            Event::Eof => break,
            _ => {}
        }
        ensure!(targets.len() <= MAX_SEGMENTS, "幻灯片数量超出预算");
    }
    Ok(targets)
}

fn cells<T: calamine::CellType>(range: &Range<T>) -> impl Iterator<Item = ((u32, u32), &T)> {
    let (row, col) = range.start().unwrap_or_default();
    range
        .used_cells()
        .map(move |(r, c, value)| ((row + r as u32, col + c as u32), value))
}

fn workbook<R: Read + std::io::Seek>(
    mut book: impl WorkbookReader<R, Error: std::error::Error + Send + Sync + 'static>,
    result: &mut Segments,
) -> Result<()> {
    for sheet in book.sheet_names() {
        ensure!(sheet.len() <= 1024, "工作表名称超出预算");
        let values = book.worksheet_range(&sheet)?;
        let formulas = book.worksheet_formula(&sheet)?;
        let mut entries = BTreeMap::new();
        for (position, value) in cells(&values) {
            if !matches!(value, Data::Empty) {
                entries.insert(position, value.to_string());
                ensure!(entries.len() <= MAX_SEGMENTS, "工作表单元格数超出抽取预算");
            }
        }
        for (position, formula) in cells(&formulas) {
            let entry = entries.entry(position).or_insert_with(String::new);
            if !entry.is_empty() {
                entry.push('\n');
            }
            entry.push('=');
            entry.push_str(formula);
            ensure!(entries.len() <= MAX_SEGMENTS, "工作表单元格数超出抽取预算");
        }
        for ((row, col), text) in entries {
            result.add(
                &text,
                Location::Cell {
                    sheet: sheet.clone(),
                    address: a1(row, col),
                },
            )?;
        }
    }
    Ok(())
}

fn a1(row: u32, col: u32) -> String {
    let mut column = u64::from(col) + 1;
    let mut letters = Vec::new();
    while column > 0 {
        column -= 1;
        letters.push(char::from(b'A' + (column % 26) as u8));
        column /= 26;
    }
    let label: String = letters.into_iter().rev().collect();
    format!("{label}{}", u64::from(row) + 1)
}

#[cfg(test)]
mod pdf_image_tests {
    use super::*;
    use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

    fn image(doc: &mut Document) -> ObjectId {
        doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
            },
            vec![255],
        ))
    }

    fn fixture() -> (Document, ObjectId, ObjectId, ObjectId, ObjectId) {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let direct = image(&mut doc);
        let nested = image(&mut doc);
        let unused = image(&mut doc);
        let form = doc.add_object(Stream::new(dictionary! {
            "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),1.into(),1.into()],
            "Resources" => dictionary! { "XObject" => dictionary! { "Inner" => nested } },
        }, b"/Inner Do /Inner Do".to_vec()));
        let resources = doc.add_object(dictionary! { "XObject" => dictionary! {
            "Direct" => direct, "Form" => form, "Unused" => unused,
        }});
        let first = doc.add_object(Stream::new(Dictionary::new(), b"q /Direct Do".to_vec()));
        let mut second = Stream::new(Dictionary::new(), b"/Form Do /Direct Do Q".to_vec());
        // Exercise the compressed content stream as well as multiple streams.
        second.compress().unwrap();
        let second = doc.add_object(second);
        let page = doc.add_object(dictionary! { "Type" => "Page", "Parent" => pages,
        "Contents" => vec![Object::Reference(first), Object::Reference(second)] });
        doc.objects.insert(pages, Object::Dictionary(dictionary! {"Type" => "Pages", "Count" => 1,
            "Kids" => vec![Object::Reference(page)], "MediaBox" => vec![0.into(),0.into(),100.into(),100.into()],
            "Resources" => resources }));
        let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
        doc.trailer.set("Root", catalog);
        (doc, page, direct, nested, form)
    }

    #[test]
    fn inherited_and_nested_images_roundtrip_pdf_excludes_unused_and_duplicates() {
        let (mut doc, _, direct, nested, _) = fixture();
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();
        let loaded = Document::load_mem(&pdf).unwrap();
        let page = loaded.get_pages()[&1];
        assert_eq!(drawn_images(&loaded, page).unwrap(), vec![direct, nested]);
    }

    #[test]
    fn local_resources_replace_inherited_resources() {
        let (mut doc, page, direct, _, form) = fixture();
        let replacement = image(&mut doc);
        doc.get_dictionary_mut(page).unwrap().set(
            "Resources",
            dictionary! {
                "XObject" => dictionary! { "Direct" => replacement, "Form" => form },
            },
        );
        let found = drawn_images(&doc, page).unwrap();
        assert_eq!(found[0], replacement);
        assert!(!found.contains(&direct));
    }

    #[test]
    fn rejects_missing_resources_and_corrupt_content() {
        let (mut doc, page, _, _, _) = fixture();
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Resources", Dictionary::new());
        assert!(drawn_images(&doc, page).is_err());
        let broken = doc.add_object(Stream::new(dictionary! { "Filter" => 42 }, b"q Q".to_vec()));
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Contents", broken);
        assert!(drawn_images(&doc, page).is_err());
        let broken = doc.add_object(Stream::new(Dictionary::new(), b"q Q [".to_vec()));
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Contents", broken);
        assert!(drawn_images(&doc, page).is_err());
    }

    #[test]
    fn rejects_form_and_page_inheritance_cycles() {
        let (mut doc, page, _, _, form) = fixture();
        let stream = doc.get_object_mut(form).unwrap().as_stream_mut().unwrap();
        stream.dict.set(
            "Resources",
            dictionary! { "XObject" => dictionary! { "Loop" => form } },
        );
        stream.set_content(b"/Loop Do".to_vec());
        assert!(drawn_images(&doc, page)
            .unwrap_err()
            .to_string()
            .contains("Form"));
        doc.get_dictionary_mut(page).unwrap().set("Parent", page);
        assert!(drawn_images(&doc, page)
            .unwrap_err()
            .to_string()
            .contains("继承循环"));
    }

    #[test]
    fn unsupported_inline_image_is_not_silently_counted_as_complete() {
        let (mut doc, page, _, _, _) = fixture();
        let inline = doc.add_object(Stream::new(
            Dictionary::new(),
            b"BI /W 1 /H 1 /BPC 8 /CS /G ID x EI ".to_vec(),
        ));
        doc.get_dictionary_mut(page)
            .unwrap()
            .set("Contents", inline);
        let err = drawn_images(&doc, page).unwrap_err().to_string();
        assert!(err.contains("内联图像"), "{err}");
    }
}
