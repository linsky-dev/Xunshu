//! 从已授权的文档字节中抽取正文。解析器不读取任何路径或外部引用。

use anyhow::{bail, ensure, Context, Result};
use calamine::{
    expand_shared_formula, open_workbook_from_rs, Data, DataRef, Dimensions, Ods, Range,
    Reader as WorkbookReader, SheetType, Xlsx, XlsxFormulaMetadata,
};
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
            xlsx_workbook(book, &mut result)?;
        }
        "ods" => {
            {
                let mut archive = check_archive(bytes)?;
                let mut budget = MAX_EXPANDED;
                let content = member(&mut archive, "content.xml", &mut budget)?;
                validate_ods_budget(&content)?;
            }
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

// Sparse XLSX worksheets must not allocate the enclosing row/column rectangle.
// The metadata reader also avoids Calamine's shared-index-sized template vector.
fn xlsx_workbook<R: Read + std::io::Seek>(mut book: Xlsx<R>, result: &mut Segments) -> Result<()> {
    const MAX_VISITED_CELLS: usize = 1_000_000;
    const MAX_FORMULA_BYTES: usize = 64 * 1024;
    let mut visited = 0;
    let mut stored_bytes = 0usize;
    let mut stored_cells = 0usize;
    let sheets = book.sheets_metadata().to_vec();
    ensure!(sheets.len() <= MAX_SEGMENTS, "工作表数量超出抽取预算");
    for sheet in sheets {
        if sheet.typ != SheetType::WorkSheet {
            continue;
        }
        ensure!(sheet.name.len() <= 1024, "工作表名称超出预算");
        let mut reader = book.worksheet_cells_reader(&sheet.name)?;
        let mut entries = BTreeMap::new();
        let mut shared: BTreeMap<usize, (Dimensions, String)> = BTreeMap::new();
        let mut derived = Vec::new();
        while let Some(cell) = reader.next_cell_with_formula_metadata()? {
            visited += 1;
            ensure!(visited <= MAX_VISITED_CELLS, "XLSX 单元格访问数量超出预算");
            let pos = cell.pos;
            ensure!(xlsx_position(pos), "XLSX 单元格坐标超出范围");
            if matches!(cell.value, DataRef::Empty) && cell.formula.is_none() {
                continue;
            }
            stored_cells += 1;
            ensure!(stored_cells <= MAX_SEGMENTS, "工作簿单元格数超出抽取预算");
            let mut text = match cell.value {
                DataRef::Empty => String::new(),
                value => Data::from(value).to_string(),
            };
            match cell.formula {
                None => {}
                Some(XlsxFormulaMetadata::Normal { formula }) => {
                    append_formula(&mut text, &formula, MAX_FORMULA_BYTES)?;
                }
                Some(XlsxFormulaMetadata::Shared {
                    shared_index,
                    range,
                    formula,
                }) => {
                    let range = range.context("XLSX 共享公式缺少范围")?;
                    ensure!(
                        xlsx_position(range.start)
                            && xlsx_position(range.end)
                            && range.start == pos
                            && range.contains(range.start.0, range.start.1),
                        "XLSX 共享公式锚点或范围无效"
                    );
                    append_formula(&mut text, &formula, MAX_FORMULA_BYTES)?;
                    ensure!(
                        shared.insert(shared_index, (range, formula)).is_none(),
                        "XLSX 共享公式锚点重复"
                    );
                }
                Some(XlsxFormulaMetadata::SharedDerived { shared_index }) => {
                    derived.push((pos, shared_index));
                }
                Some(_) => bail!("不支持的 XLSX 公式元数据"),
            }
            stored_bytes = stored_bytes
                .checked_add(text.len())
                .context("XLSX 正文预算溢出")?;
            ensure!(stored_bytes <= MAX_TEXT, "XLSX 正文超出抽取预算");
            ensure!(entries.insert(pos, text).is_none(), "XLSX 单元格坐标重复");
        }
        // Resolve shared formulas only after collecting bounded anchors. This
        // preserves derived cells that appear before their anchor in XML order.
        for (pos, index) in derived {
            let (range, template) = shared.get(&index).context("XLSX 共享公式缺少锚点")?;
            ensure!(
                range.contains(pos.0, pos.1),
                "XLSX 共享公式派生单元格超出范围"
            );
            let formula = expand_shared_formula(template, range.start, pos)?;
            let text = entries.get_mut(&pos).context("XLSX 共享公式单元格缺失")?;
            let before = text.len();
            append_formula(text, &formula, MAX_FORMULA_BYTES)?;
            stored_bytes = stored_bytes
                .checked_add(text.len() - before)
                .context("XLSX 正文预算溢出")?;
            ensure!(stored_bytes <= MAX_TEXT, "XLSX 正文超出抽取预算");
        }
        for ((row, col), text) in entries {
            result.add(
                &text,
                Location::Cell {
                    sheet: sheet.name.clone(),
                    address: a1(row, col),
                },
            )?;
        }
    }
    Ok(())
}

fn xlsx_position((row, col): (u32, u32)) -> bool {
    row < 1_048_576 && col < 16_384
}

fn append_formula(text: &mut String, formula: &str, limit: usize) -> Result<()> {
    ensure!(
        !formula.is_empty() && formula.len() <= limit,
        "XLSX 公式为空或超出预算"
    );
    if !text.is_empty() {
        text.push('\n');
    }
    text.push('=');
    text.push_str(formula);
    Ok(())
}

// Calamine materializes ODS ranges and expands repeated text before our
// segment budget is applied. Validate the XML and bound amplification first.
// Trailing repeated blank rows/cells remain cheap, as in ordinary LibreOffice
// files. Huge sparse rectangles are explicitly rejected, not partially indexed.
fn validate_ods_budget(bytes: &[u8]) -> Result<()> {
    const MAX_SLOTS: u64 = 1_000_000;
    const MAX_ROWS: u64 = 1_048_576;
    const MAX_COLS: u64 = 16_384;
    #[derive(Default)]
    struct Table {
        rows: u64,
        bounds: Option<(u64, u64, u64, u64)>,
    }
    struct Row {
        repeat: u64,
        cols: u64,
        used: u64,
    }
    struct Cell {
        repeat: u64,
        start: u64,
        spaces: u64,
        occupied: bool,
    }
    fn repeat(
        tag: &quick_xml::events::BytesStart<'_>,
        key: &[u8],
        decoder: quick_xml::encoding::Decoder,
        max: u64,
    ) -> Result<u64> {
        let mut count = 1;
        for attr in tag.attributes() {
            let attr = attr?;
            if attr.key.as_ref() == key {
                count = attr
                    .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)?
                    .parse::<u64>()
                    .context("非法 ODS 重复计数")?;
            }
        }
        ensure!((1..=max).contains(&count), "ODS 重复计数超出预算");
        Ok(count)
    }
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut table: Option<Table> = None;
    let mut row: Option<Row> = None;
    let mut cell: Option<Cell> = None;
    let mut sheets = 0usize;
    let mut slots = 0u64;
    let mut expanded = 0u64;
    let mut spaces = 0u64;
    loop {
        let start = reader.buffer_position();
        match reader.read_event()? {
            Event::Start(tag) => {
                depth += 1;
                ensure!(depth <= 128, "ODS XML 嵌套过深");
                match tag.name().as_ref() {
                    b"table:table" => {
                        ensure!(table.is_none(), "不支持嵌套 ODS 工作表");
                        sheets += 1;
                        ensure!(sheets <= MAX_SEGMENTS, "ODS 工作表数量超出预算");
                        table = Some(Table::default());
                    }
                    b"table:table-row" if table.is_some() => {
                        ensure!(row.is_none(), "ODS 行嵌套无效");
                        row = Some(Row {
                            repeat: repeat(
                                &tag,
                                b"table:number-rows-repeated",
                                reader.decoder(),
                                MAX_ROWS,
                            )?,
                            cols: 0,
                            used: 0,
                        });
                    }
                    b"table:table-cell" | b"table:covered-table-cell" if row.is_some() => {
                        ensure!(cell.is_none(), "ODS 单元格嵌套无效");
                        let mut occupied = false;
                        for attr in tag.attributes() {
                            let attr = attr?;
                            occupied |= matches!(
                                attr.key.as_ref(),
                                b"table:formula"
                                    | b"office:value"
                                    | b"office:string-value"
                                    | b"office:date-value"
                                    | b"office:time-value"
                                    | b"office:boolean-value"
                                    | b"office:value-type"
                                    | b"calcext:value-type"
                            );
                        }
                        cell = Some(Cell {
                            repeat: repeat(
                                &tag,
                                b"table:number-columns-repeated",
                                reader.decoder(),
                                MAX_COLS,
                            )?,
                            start,
                            spaces: 0,
                            occupied,
                        });
                    }
                    b"text:s" => {
                        let count = repeat(&tag, b"text:c", reader.decoder(), MAX_TEXT as u64)?;
                        spaces += count;
                        ensure!(spaces <= MAX_TEXT as u64, "ODS 空格计数超出预算");
                        if let Some(cell) = &mut cell {
                            cell.spaces += count;
                        }
                    }
                    _ => {}
                }
            }
            Event::End(tag) => {
                depth = depth.checked_sub(1).context("ODS XML 结束标签无效")?;
                match tag.name().as_ref() {
                    b"table:table-cell" | b"table:covered-table-cell" if row.is_some() => {
                        let cell = cell.take().context("ODS 单元格未开始")?;
                        let row = row.as_mut().context("ODS 行未开始")?;
                        let first = row.cols;
                        row.cols += cell.repeat;
                        ensure!(row.cols <= MAX_COLS, "ODS 列坐标超出范围");
                        if cell.occupied {
                            row.used = row.cols;
                            let table = table.as_mut().context("ODS 工作表未开始")?;
                            let last_row = table.rows + row.repeat - 1;
                            let last_col = row.cols - 1;
                            table.bounds = Some(match table.bounds {
                                Some((r0, r1, c0, c1)) => (
                                    r0.min(table.rows),
                                    r1.max(last_row),
                                    c0.min(first),
                                    c1.max(last_col),
                                ),
                                None => (table.rows, last_row, first, last_col),
                            });
                            let encoded = reader
                                .buffer_position()
                                .checked_sub(cell.start)
                                .context("ODS XML 位置无效")?;
                            let cost = (encoded + cell.spaces)
                                .checked_mul(cell.repeat)
                                .and_then(|v| v.checked_mul(row.repeat))
                                .context("ODS 内容展开预算溢出")?;
                            expanded =
                                expanded.checked_add(cost).context("ODS 内容展开预算溢出")?;
                            ensure!(expanded <= MAX_EXPANDED, "ODS 内容展开超出预算");
                        }
                    }
                    b"table:table-row" if table.is_some() => {
                        let row = row.take().context("ODS 行未开始")?;
                        let table = table.as_mut().context("ODS 工作表未开始")?;
                        table.rows += row.repeat;
                        ensure!(table.rows <= MAX_ROWS, "ODS 行坐标超出范围");
                        // The decoder also creates intermediate column vectors.
                        slots += row.used;
                        ensure!(slots <= MAX_SLOTS, "ODS 单元格展开超出预算");
                    }
                    b"table:table" => {
                        let table = table.take().context("ODS 工作表未开始")?;
                        if let Some((r0, r1, c0, c1)) = table.bounds {
                            slots += (r1 - r0 + 1) * (c1 - c0 + 1);
                            ensure!(slots <= MAX_SLOTS, "ODS 单元格展开超出预算");
                        }
                    }
                    _ => {}
                }
            }
            Event::DocType(_) => bail!("ODS XML 禁止文档类型和自定义实体"),
            Event::Eof => {
                ensure!(
                    depth == 0 && table.is_none() && row.is_none() && cell.is_none(),
                    "ODS XML 未完整结束"
                );
                break;
            }
            _ => {}
        }
    }
    Ok(())
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
mod ods_budget_tests {
    use super::*;
    use std::io::Write;

    fn document(rows: &str) -> String {
        format!(
            r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:body><office:spreadsheet><table:table table:name="台账">{rows}</table:table></office:spreadsheet></office:body></office:document-content>"#
        )
    }

    fn ods_bytes(rows: &str) -> Result<Vec<u8>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            ("content.xml", &document(rows)),
            (
                "META-INF/manifest.xml",
                r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#,
            ),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())?;
            zip.write_all(text.as_bytes())?;
        }
        Ok(zip.finish()?.into_inner())
    }

    fn extract_rows(rows: &str) -> Result<Vec<Segment>> {
        extract("ods", &ods_bytes(rows)?, false, &mut None)
    }

    #[test]
    fn old_decoder_silently_caps_out_of_range_rows() {
        let rows = r#"<table:table-row table:number-rows-repeated="1048577"/>"#;
        let bytes = ods_bytes(rows).unwrap();
        let old: Ods<_> = open_workbook_from_rs(Cursor::new(bytes)).unwrap();
        assert_eq!(old.sheet_names(), ["台账"]);
        assert!(extract_rows(rows)
            .unwrap_err()
            .to_string()
            .contains("重复计数超出预算"));
    }

    #[test]
    fn ordinary_values_and_trailing_empty_repeats_remain_searchable() {
        let segments = extract_rows(r#"<table:table-row><table:table-cell office:value-type="string"><text:p>干部<text:s text:c="2"/>台账</text:p></table:table-cell><table:table-cell table:number-columns-repeated="16383"/></table:table-row><table:table-row table:number-rows-repeated="1048575"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>"#).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "干部  台账");
        assert_eq!(
            segments[0].location,
            Location::Cell {
                sheet: "台账".into(),
                address: "A1".into()
            }
        );
    }

    #[test]
    fn dense_sparse_rectangle_is_rejected_before_calamine() {
        let error = extract_rows(r#"<table:table-row><table:table-cell office:value="1"/></table:table-row><table:table-row table:number-rows-repeated="100000"/><table:table-row><table:table-cell table:number-columns-repeated="1000"/><table:table-cell office:value="2"/></table:table-row>"#).unwrap_err().to_string();
        assert!(error.contains("单元格展开超出预算"), "{error}");
    }

    #[test]
    fn repeat_overflow_and_zero_are_not_silently_capped() {
        for rows in [
            r#"<table:table-row table:number-rows-repeated="1048577"/>"#,
            r#"<table:table-row table:number-rows-repeated="0"/>"#,
            r#"<table:table-row><table:table-cell table:number-columns-repeated="16384"/><table:table-cell office:value="2"/></table:table-row>"#,
            r#"<table:table-row table:number-rows-repeated="1048576"/><table:table-row/>"#,
            r#"<table:table-row><table:table-cell office:value-type="string"><text:p><text:s text:c="999999999999"/></text:p></table:table-cell></table:table-row>"#,
        ] {
            assert!(extract_rows(rows).is_err(), "{rows}");
        }
    }

    #[test]
    fn malformed_or_dtd_xml_fails_before_library_parser() {
        for xml in [
            "<table:table><table:table-row>",
            "<table:table></wrong>",
            "<!DOCTYPE foo><foo/>",
        ] {
            assert!(validate_ods_budget(xml.as_bytes()).is_err(), "{xml}");
        }
    }

    #[test]
    fn repeated_string_payload_is_bounded_even_with_small_cell_count() {
        let text = "x".repeat(100_000);
        let rows = format!(
            r#"<table:table-row table:number-rows-repeated="1000"><table:table-cell office:value-type="string"><text:p>{text}</text:p></table:table-cell></table:table-row>"#
        );
        let error = extract_rows(&rows).unwrap_err().to_string();
        assert!(error.contains("内容展开超出预算"), "{error}");
    }
}

#[cfg(test)]
mod xlsx_sparse_tests {
    use super::*;
    use std::io::Write;

    fn extract_sheet(rows: &str) -> Result<Vec<Segment>> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let sheet = format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
        );
        let parts = [
            (
                "[Content_Types].xml",
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="台账" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", &sheet),
        ];
        for (name, text) in parts {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        extract("xlsx", &bytes, false, &mut None)
    }

    #[test]
    fn opposite_corners_preserve_sparse_cells_and_absolute_locations() {
        let segments = extract_sheet(r#"<row r="1"><c r="A1" t="inlineStr"><is><t>年初台账</t></is></c></row><row r="1048576"><c r="XFD1048576" t="inlineStr"><is><t>年末台账</t></is></c></row>"#).unwrap();
        assert_eq!(segments.len(), 2);
        for (segment, (address, text)) in segments
            .iter()
            .zip([("A1", "年初台账"), ("XFD1048576", "年末台账")])
        {
            assert_eq!(
                segment.location,
                Location::Cell {
                    sheet: "台账".into(),
                    address: address.into()
                }
            );
            assert_eq!(segment.text, text);
        }
    }

    #[test]
    fn shared_formula_uses_bounded_map_and_resolves_late_anchor() {
        let segments = extract_sheet(r#"<row r="3"><c r="B3"><f t="shared" si="4000000000"/><v>8</v></c></row><row r="2"><c r="B2"><f t="shared" si="4000000000" ref="B2:B3">A2+$C$1</f><v>7</v></c><c r="D2"><f>SUM(A1:A2)</f></c></row>"#).unwrap();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text, "7\n=A2+$C$1");
        assert_eq!(segments[1].text, "=SUM(A1:A2)");
        assert_eq!(segments[2].text, "8\n=A3+$C$1");
    }

    #[test]
    fn corrupt_shared_formula_and_duplicate_cells_fail_closed() {
        for (rows, expected) in [
            (
                r#"<row r="1"><c r="A1"><f t="shared" si="9"/><v>1</v></c></row>"#,
                "缺少锚点",
            ),
            (
                r#"<row r="1"><c r="A1"><f t="shared" si="1" ref="A1:A2">B1</f></c><c r="B1"><f t="shared" si="1"/></c></row>"#,
                "超出范围",
            ),
            (
                r#"<row r="1"><c r="A1"><v>1</v></c><c r="A1"><v>2</v></c></row>"#,
                "坐标重复",
            ),
            (
                r#"<row r="1"><c r="XFE1"><v>1</v></c></row>"#,
                "坐标超出范围",
            ),
        ] {
            let error = extract_sheet(rows).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn sparse_workbook_enforces_meaningful_cell_budget_before_dense_allocation() {
        let mut rows = String::new();
        for n in 1..=MAX_SEGMENTS + 1 {
            rows.push_str(&format!(r#"<row r="{n}"><c r="A{n}"><v>1</v></c></row>"#));
        }
        let error = extract_sheet(&rows).unwrap_err().to_string();
        assert!(error.contains("单元格数超出"), "{error}");
    }
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
