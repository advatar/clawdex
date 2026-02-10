use std::fs;
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::{resolve_workspace_path, ClawdPaths};
use crate::task_db::TaskStore;

const MIME_XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const MIME_PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const MIME_DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const MIME_PDF: &str = "application/pdf";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XlsxSpec {
    #[serde(alias = "output_path")]
    output_path: String,
    #[serde(default, alias = "task_run_id")]
    task_run_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    sheets: Vec<XlsxSheet>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XlsxSheet {
    name: String,
    #[serde(default)]
    cells: Vec<XlsxCell>,
    #[serde(default)]
    rows: Vec<Vec<Value>>,
    #[serde(default)]
    columns: Vec<XlsxColumn>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XlsxCell {
    row: u32,
    col: u32,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    formula: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct XlsxColumn {
    col: u32,
    width: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PptxSpec {
    #[serde(alias = "output_path")]
    output_path: String,
    #[serde(default, alias = "task_run_id")]
    task_run_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    slides: Vec<PptxSlide>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PptxSlide {
    title: String,
    #[serde(default)]
    bullets: Vec<String>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocxSpec {
    #[serde(alias = "output_path")]
    output_path: String,
    #[serde(default, alias = "task_run_id")]
    task_run_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    sections: Vec<DocSection>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PdfSpec {
    #[serde(alias = "output_path")]
    output_path: String,
    #[serde(default, alias = "task_run_id")]
    task_run_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    sections: Vec<DocSection>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocSection {
    #[serde(default)]
    heading: Option<String>,
    #[serde(default)]
    level: Option<u8>,
    #[serde(default)]
    paragraphs: Vec<String>,
    #[serde(default)]
    bullets: Vec<String>,
}

pub fn create_xlsx(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    ensure_writable(paths)?;
    let spec: XlsxSpec = serde_json::from_value(args.clone()).context("parse xlsx spec")?;
    let output_path = resolve_output_path(paths, &spec.output_path, "xlsx")?;

    let mut workbook = rust_xlsxwriter::Workbook::new();
    if let Some(title) = spec.title.as_ref() {
        let properties = rust_xlsxwriter::DocProperties::new().set_title(title);
        workbook.set_properties(&properties);
    }
    for sheet in spec.sheets {
        let mut worksheet = workbook.add_worksheet();
        if !sheet.name.trim().is_empty() {
            worksheet
                .set_name(&sheet.name)
                .map_err(|err| anyhow::anyhow!("invalid sheet name: {err}"))?;
        }
        for column in sheet.columns {
            if column.col == 0 {
                continue;
            }
            let col = (column.col - 1) as u16;
            worksheet
                .set_column_width(col, column.width)
                .map_err(|err| anyhow::anyhow!("set column width: {err}"))?;
        }
        for cell in sheet.cells {
            write_xlsx_cell(&mut worksheet, &cell)?;
        }
        for (row_idx, row) in sheet.rows.iter().enumerate() {
            for (col_idx, value) in row.iter().enumerate() {
                write_xlsx_value(&mut worksheet, row_idx as u32, col_idx as u32, value)?;
            }
        }
    }

    workbook
        .save(&output_path)
        .map_err(|err| anyhow::anyhow!("write xlsx: {err}"))?;

    finalize_artifact(paths, &output_path, MIME_XLSX, spec.task_run_id)
}

pub fn create_pptx(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    ensure_writable(paths)?;
    let spec: PptxSpec = serde_json::from_value(args.clone()).context("parse pptx spec")?;
    let output_path = resolve_output_path(paths, &spec.output_path, "pptx")?;

    let title = spec
        .title
        .clone()
        .unwrap_or_else(|| "Clawdex Presentation".to_string());
    let mut slides = Vec::new();
    for slide in spec.slides {
        let mut content = ppt_rs::SlideContent::new(&slide.title);
        for bullet in slide.bullets {
            if !bullet.trim().is_empty() {
                content = content.add_bullet(&bullet);
            }
        }
        if let Some(notes) = slide.notes {
            if !notes.trim().is_empty() {
                content.notes = Some(notes);
            }
        }
        slides.push(content);
    }

    let data = ppt_rs::create_pptx_with_content(&title, slides)
        .map_err(|err| anyhow::anyhow!("create pptx: {err}"))?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(&output_path, data)
        .with_context(|| format!("write {}", output_path.display()))?;

    finalize_artifact(paths, &output_path, MIME_PPTX, spec.task_run_id)
}

pub fn create_docx(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    ensure_writable(paths)?;
    let spec: DocxSpec = serde_json::from_value(args.clone()).context("parse docx spec")?;
    let output_path = resolve_output_path(paths, &spec.output_path, "docx")?;

    let mut docx = docx::Docx::default();
    if let Some(title) = spec.title.as_ref() {
        let heading = docx_heading(title, 1);
        docx.document.push(heading);
    }
    for section in spec.sections {
        if let Some(heading) = section.heading {
            let level = section.level.unwrap_or(2).max(1).min(6);
            let paragraph = docx_heading(&heading, level);
            docx.document.push(paragraph);
        }
        for paragraph in section.paragraphs {
            if paragraph.trim().is_empty() {
                continue;
            }
            docx.document
                .push(docx::document::Paragraph::default().push_text(paragraph));
        }
        for bullet in section.bullets {
            if bullet.trim().is_empty() {
                continue;
            }
            let text = format!("• {}", bullet.trim());
            docx.document
                .push(docx::document::Paragraph::default().push_text(text));
        }
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    docx.write_file(&output_path)
        .map_err(|err| anyhow::anyhow!("write docx: {err:?}"))?;

    finalize_artifact(paths, &output_path, MIME_DOCX, spec.task_run_id)
}

pub fn create_pdf(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    ensure_writable(paths)?;
    let spec: PdfSpec = serde_json::from_value(args.clone()).context("parse pdf spec")?;
    let output_path = resolve_output_path(paths, &spec.output_path, "pdf")?;

    let title = spec
        .title
        .clone()
        .unwrap_or_else(|| "Clawdex Report".to_string());
    let (doc, mut page, mut layer) =
        printpdf::PdfDocument::new(&title, printpdf::Mm(210.0), printpdf::Mm(297.0), "Layer 1");
    let font = doc
        .add_builtin_font(printpdf::BuiltinFont::Helvetica)
        .map_err(|err| anyhow::anyhow!("load font: {err}"))?;

    let mut cursor = PdfCursor::new(20.0, 277.0);
    cursor.write_wrapped(&doc, &mut page, &mut layer, &font, &title, 20.0)?;
    cursor.advance(4.0);

    for section in spec.sections {
        if let Some(heading) = section.heading {
            cursor.write_wrapped(&doc, &mut page, &mut layer, &font, &heading, 14.0)?;
            cursor.advance(2.0);
        }
        for paragraph in section.paragraphs {
            if paragraph.trim().is_empty() {
                continue;
            }
            cursor.write_wrapped(&doc, &mut page, &mut layer, &font, &paragraph, 11.0)?;
            cursor.advance(1.5);
        }
        for bullet in section.bullets {
            if bullet.trim().is_empty() {
                continue;
            }
            let text = format!("• {}", bullet.trim());
            cursor.write_wrapped(&doc, &mut page, &mut layer, &font, &text, 11.0)?;
        }
        cursor.advance(3.0);
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = fs::File::create(&output_path)
        .with_context(|| format!("create {}", output_path.display()))?;
    let mut writer = BufWriter::new(file);
    doc.save(&mut writer)
        .map_err(|err| anyhow::anyhow!("write pdf: {err}"))?;

    finalize_artifact(paths, &output_path, MIME_PDF, spec.task_run_id)
}

fn ensure_writable(paths: &ClawdPaths) -> Result<()> {
    if paths.workspace_policy.read_only {
        anyhow::bail!("workspace is read-only");
    }
    Ok(())
}

fn resolve_output_path(paths: &ClawdPaths, output: &str, extension: &str) -> Result<PathBuf> {
    let mut path = PathBuf::from(output);
    if path.extension().is_none() {
        path.set_extension(extension);
    }
    let raw = path.to_string_lossy().to_string();
    resolve_workspace_path(paths, &raw)
}

fn finalize_artifact(
    paths: &ClawdPaths,
    output_path: &Path,
    mime: &str,
    task_run_id: Option<String>,
) -> Result<Value> {
    let (sha256, size_bytes) = hash_and_size(output_path)?;
    let relative_path = relative_to_workspace(paths, output_path);
    let absolute_path = output_path.to_string_lossy().to_string();

    let mut recorded = false;
    let run_id = resolve_task_run_id(task_run_id);
    if let Some(ref run_id) = run_id {
        let store = TaskStore::open(paths)?;
        store.record_artifact(run_id, &relative_path, Some(mime.to_string()), Some(sha256.clone()))?;
        let _ = store.record_event(
            run_id,
            "artifact_created",
            &json!({
                "path": relative_path,
                "absolutePath": absolute_path,
                "mime": mime,
                "sha256": sha256,
                "sizeBytes": size_bytes,
            }),
        );
        recorded = true;
    }

    let mut response = json!({
        "ok": true,
        "path": relative_path,
        "absolutePath": absolute_path,
        "mime": mime,
        "sha256": sha256,
        "sizeBytes": size_bytes,
        "recorded": recorded,
    });
    if let Some(run_id) = run_id {
        response["taskRunId"] = Value::String(run_id);
    }
    Ok(response)
}

fn resolve_task_run_id(explicit: Option<String>) -> Option<String> {
    if let Some(value) = explicit {
        if !value.trim().is_empty() {
            return Some(value);
        }
    }
    std::env::var("CLAWDEX_TASK_RUN_ID")
        .ok()
        .and_then(|value| if value.trim().is_empty() { None } else { Some(value) })
}

fn relative_to_workspace(paths: &ClawdPaths, path: &Path) -> String {
    for root in &paths.workspace_policy.allowed_roots {
        if path.starts_with(root) {
            let rel = path.strip_prefix(root).unwrap_or(path);
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string_lossy().replace('\\', "/")
}

fn hash_and_size(path: &Path) -> Result<(String, u64)> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let hash = hasher.finalize();
    let size = fs::metadata(path)?.len();
    Ok((hex::encode(hash), size))
}

fn write_xlsx_cell(worksheet: &mut rust_xlsxwriter::Worksheet, cell: &XlsxCell) -> Result<()> {
    if cell.row == 0 || cell.col == 0 {
        anyhow::bail!("xlsx row/col are 1-based");
    }
    let row = cell.row - 1;
    let col = u16::try_from(cell.col - 1)
        .map_err(|_| anyhow::anyhow!("xlsx column out of range"))?;
    if let Some(formula) = cell.formula.as_ref().filter(|f| !f.trim().is_empty()) {
        worksheet
            .write_formula(row, col, formula.as_str())
            .map_err(|err| anyhow::anyhow!("write formula: {err}"))?;
        return Ok(());
    }
    if let Some(value) = &cell.value {
        write_xlsx_value(worksheet, row, u32::from(col), value)?;
    }
    Ok(())
}

fn write_xlsx_value(
    worksheet: &mut rust_xlsxwriter::Worksheet,
    row: u32,
    col: u32,
    value: &Value,
) -> Result<()> {
    let col = u16::try_from(col).map_err(|_| anyhow::anyhow!("xlsx column out of range"))?;
    match value {
        Value::Null => Ok(()),
        Value::Bool(v) => worksheet
            .write_boolean(row, col, *v)
            .map(|_| ())
            .map_err(|err| anyhow::anyhow!("write boolean: {err}")),
        Value::Number(n) => {
            let num = n.as_f64().unwrap_or(0.0);
            worksheet
                .write_number(row, col, num)
                .map(|_| ())
                .map_err(|err| anyhow::anyhow!("write number: {err}"))
        }
        Value::String(s) => worksheet
            .write_string(row, col, s)
            .map(|_| ())
            .map_err(|err| anyhow::anyhow!("write string: {err}")),
        Value::Object(map) => {
            if let Some(formula) = map.get("formula").and_then(|v| v.as_str()) {
                worksheet
                    .write_formula(row, col, formula)
                    .map_err(|err| anyhow::anyhow!("write formula: {err}"))?;
                return Ok(());
            }
            if let Some(inner) = map.get("value") {
                return write_xlsx_value(worksheet, row, u32::from(col), inner);
            }
            Ok(())
        }
        _ => worksheet
            .write_string(row, col, &value.to_string())
            .map(|_| ())
            .map_err(|err| anyhow::anyhow!("write string: {err}")),
    }
}

fn docx_heading(text: &str, level: u8) -> docx::document::Paragraph<'static> {
    let style = format!("Heading{}", level);
    let property = docx::formatting::ParagraphProperty::default().style_id(style);
    docx::document::Paragraph::default()
        .property(property)
        .push_text(text.to_string())
}

struct PdfCursor {
    margin_x: f32,
    y: f32,
    bottom: f32,
}

impl PdfCursor {
    fn new(margin_x: f32, start_y: f32) -> Self {
        Self {
            margin_x,
            y: start_y,
            bottom: 20.0,
        }
    }

    fn advance(&mut self, amount: f32) {
        self.y -= amount;
    }

    fn ensure_space(
        &mut self,
        doc: &printpdf::PdfDocumentReference,
        page: &mut printpdf::PdfPageIndex,
        layer: &mut printpdf::PdfLayerIndex,
        needed: f32,
    ) {
        if self.y - needed > self.bottom {
            return;
        }
        let (new_page, new_layer) =
            doc.add_page(printpdf::Mm(210.0), printpdf::Mm(297.0), "Layer 1");
        *page = new_page;
        *layer = new_layer;
        self.y = 277.0;
    }

    fn write_wrapped(
        &mut self,
        doc: &printpdf::PdfDocumentReference,
        page: &mut printpdf::PdfPageIndex,
        layer: &mut printpdf::PdfLayerIndex,
        font: &printpdf::IndirectFontRef,
        text: &str,
        font_size: f32,
    ) -> Result<()> {
        let max_chars = 90usize;
        let line_height = font_size * 0.3527 * 1.4;
        for line in wrap_text(text, max_chars) {
            self.ensure_space(doc, page, layer, line_height);
            let current_layer = doc.get_page(*page).get_layer(*layer);
            current_layer.use_text(
                line,
                font_size,
                printpdf::Mm(self.margin_x),
                printpdf::Mm(self.y),
                font,
            );
            self.y -= line_height;
        }
        Ok(())
    }
}

fn wrap_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.len() <= max_chars {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
            continue;
        }
        if current.len() + 1 + word.len() > max_chars {
            lines.push(current);
            current = word.to_string();
        } else {
            current.push(' ');
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}
