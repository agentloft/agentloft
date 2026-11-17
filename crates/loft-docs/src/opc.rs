//! Open Packaging Conventions: the zip container, part lookup and relationships
//! shared by docx and pptx.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use anyhow::{Context as _, bail};
use quick_xml::events::Event;
use zip::ZipArchive;

use crate::xml::{Xml, attr, local};

/// Largest decompressed size accepted for a single part (zip-bomb guard).
const MAX_PART_BYTES: u64 = 256 * 1024 * 1024;
/// Largest total decompressed size read from one package.
const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
/// Total decompressed image bytes kept per document; images beyond this are
/// still referenced from the Markdown but carry no bytes.
pub const MAX_IMAGE_BYTES: u64 = 256 * 1024 * 1024;

pub const REL_OFFICE_DOCUMENT: &str = "/officeDocument";

pub struct Package<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
    /// Lower-cased part name → zip index. OPC part names are case-insensitive.
    names: HashMap<String, usize>,
    pub image_budget: u64,
    /// Decompressed bytes still allowed, across all parts.
    read_budget: u64,
}

impl<'a> Package<'a> {
    pub fn open(bytes: &'a [u8]) -> anyhow::Result<Self> {
        let zip = ZipArchive::new(Cursor::new(bytes))
            .context("not a valid Office Open XML package (zip)")?;
        let mut names = HashMap::with_capacity(zip.len());
        for (i, name) in zip.file_names().enumerate() {
            if let Ok(name) = name {
                names.insert(normalize_name(&name), i);
            }
        }
        Ok(Self {
            zip,
            names,
            image_budget: MAX_IMAGE_BYTES,
            read_budget: MAX_TOTAL_BYTES,
        })
    }

    pub fn has_part(&self, name: &str) -> bool {
        self.names.contains_key(&normalize_name(name))
    }

    /// All part names (lower-cased, without a leading slash).
    pub fn part_names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }

    /// Reads a part, or `Ok(None)` when it does not exist.
    pub fn read_opt(&mut self, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(&index) = self.names.get(&normalize_name(name)) else {
            return Ok(None);
        };
        let file = self
            .zip
            .by_index(index)
            .with_context(|| format!("cannot open part {name}"))?;
        let limit = MAX_PART_BYTES.min(self.read_budget);
        let mut out = Vec::with_capacity(file.size().min(limit).min(16 * 1024 * 1024) as usize);
        file.take(limit + 1)
            .read_to_end(&mut out)
            .with_context(|| format!("cannot decompress part {name}"))?;
        if out.len() as u64 > MAX_PART_BYTES {
            bail!("part {name} is larger than {} MB", MAX_PART_BYTES >> 20);
        }
        if out.len() as u64 > self.read_budget {
            bail!(
                "the document expands to more than {} MB",
                MAX_TOTAL_BYTES >> 20
            );
        }
        self.read_budget -= out.len() as u64;
        Ok(Some(out))
    }

    pub fn read(&mut self, name: &str) -> anyhow::Result<Vec<u8>> {
        self.read_opt(name)?
            .with_context(|| format!("the package has no part {name}"))
    }

    /// Reads a part as UTF-8 text (BOM stripped, UTF-16 decoded).
    pub fn read_xml_opt(&mut self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.read_opt(name)?.map(decode_text))
    }

    pub fn read_xml(&mut self, name: &str) -> anyhow::Result<String> {
        Ok(decode_text(self.read(name)?))
    }

    /// Reads the relationships of `part` (from `dir/_rels/file.rels`).
    pub fn rels_for(&mut self, part: &str) -> anyhow::Result<Rels> {
        let rels_name = rels_path(part);
        match self.read_xml_opt(&rels_name)? {
            Some(xml) => Rels::parse(&xml, part)
                .with_context(|| format!("malformed relationships part {rels_name}")),
            None => Ok(Rels::default()),
        }
    }

    /// Finds the main document part from the package relationships, falling
    /// back to `default` when `_rels/.rels` is missing or unhelpful.
    pub fn main_part(&mut self, default: &str) -> anyhow::Result<String> {
        let rels = self.rels_for("")?;
        if let Some(rel) = rels.first_of_type(REL_OFFICE_DOCUMENT)
            && !rel.external
            && self.has_part(&rel.target)
        {
            return Ok(rel.target.clone());
        }
        if self.has_part(default) {
            return Ok(default.to_string());
        }
        bail!("the package has no main document part (expected {default})")
    }

    /// Loads an image part, charging it against the image budget. Returns
    /// `None` when the part is missing or the budget is spent.
    pub fn read_image(&mut self, part: &str) -> Option<Vec<u8>> {
        let &index = self.names.get(&normalize_name(part))?;
        let size = self.zip.by_index(index).ok()?.size();
        if size > self.image_budget {
            return None;
        }
        let bytes = self.read_opt(part).ok()??;
        self.image_budget = self.image_budget.saturating_sub(bytes.len() as u64);
        Some(bytes)
    }

    /// Reads the `dc:title` core property, if any.
    pub fn core_title(&mut self) -> Option<String> {
        let rels = self.rels_for("").ok()?;
        let part = rels
            .first_of_type("/metadata/core-properties")
            .map(|r| r.target.clone())
            .unwrap_or_else(|| "docProps/core.xml".to_string());
        let xml = self.read_xml_opt(&part).ok()??;
        let mut x = Xml::new(&xml);
        while let Ok(ev) = x.next() {
            match ev {
                Event::Start(e) if local(&e) == "title" => {
                    let title = x.text_until_end().ok()?;
                    let title = title.trim();
                    return (!title.is_empty()).then(|| title.to_string());
                }
                Event::Eof => break,
                _ => {}
            }
        }
        None
    }
}

/// One relationship, with `target` resolved to a package part name (or left
/// as-is for external targets).
#[derive(Debug, Clone)]
pub struct Rel {
    pub rel_type: String,
    pub target: String,
    pub external: bool,
}

#[derive(Debug, Default)]
pub struct Rels {
    by_id: HashMap<String, Rel>,
    in_order: Vec<String>,
}

impl Rels {
    pub fn parse(xml: &str, source_part: &str) -> anyhow::Result<Self> {
        let base = part_dir(source_part);
        let mut rels = Rels::default();
        let mut x = Xml::new(xml);
        loop {
            match x.next()? {
                Event::Start(e) if local(&e) == "Relationship" => {
                    let id = attr(&e, "Id").unwrap_or_default();
                    let rel_type = attr(&e, "Type").unwrap_or_default();
                    let raw_target = attr(&e, "Target").unwrap_or_default();
                    let external =
                        attr(&e, "TargetMode").is_some_and(|m| m.eq_ignore_ascii_case("external"));
                    let target = if external {
                        raw_target.trim().to_string()
                    } else {
                        resolve_target(base, &raw_target)
                    };
                    if !id.is_empty() {
                        rels.in_order.push(id.clone());
                        rels.by_id.insert(
                            id,
                            Rel {
                                rel_type,
                                target,
                                external,
                            },
                        );
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(rels)
    }

    pub fn get(&self, id: &str) -> Option<&Rel> {
        self.by_id.get(id)
    }

    /// The first relationship whose type URI ends with `suffix`
    /// (for example `/officeDocument`), matching both transitional and strict URIs.
    pub fn first_of_type(&self, suffix: &str) -> Option<&Rel> {
        self.in_order
            .iter()
            .filter_map(|id| self.by_id.get(id))
            .find(|r| r.rel_type.ends_with(suffix))
    }
}

/// `word/document.xml` → `word/_rels/document.xml.rels`; `""` → `_rels/.rels`.
pub fn rels_path(part: &str) -> String {
    let part = part.trim_start_matches('/');
    match part.rfind('/') {
        Some(i) => format!("{}/_rels/{}.rels", &part[..i], &part[i + 1..]),
        None => format!("_rels/{part}.rels"),
    }
}

fn part_dir(part: &str) -> &str {
    let part = part.trim_start_matches('/');
    match part.rfind('/') {
        Some(i) => &part[..i],
        None => "",
    }
}

/// Resolves a relative relationship target against the source part's folder.
pub fn resolve_target(base_dir: &str, target: &str) -> String {
    let target = percent_decode(target.trim());
    let target = target.split(['#', '?']).next().unwrap_or_default();
    let mut segments: Vec<&str> = Vec::new();
    let joined;
    let full = if let Some(abs) = target.strip_prefix('/') {
        abs
    } else {
        joined = if base_dir.is_empty() {
            target.to_string()
        } else {
            format!("{base_dir}/{target}")
        };
        &joined
    };
    for seg in full.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    segments.join("/")
}

fn normalize_name(name: &str) -> String {
    name.trim_start_matches('/')
        .replace('\\', "/")
        .to_lowercase()
}

fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Decodes an XML part to a `String`: strips a UTF-8 BOM, decodes UTF-16 with
/// a BOM, and replaces invalid UTF-8.
pub fn decode_text(bytes: Vec<u8>) -> String {
    let utf16 = |bytes: &[u8], le: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| {
                if le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes.as_slice() {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        },
    }
}

/// Guesses an image MIME type from a part name.
pub fn mime_for(name: &str) -> &'static str {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" | "jfif" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" | "dib" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "emf" => "image/emf",
        "wmf" => "image/wmf",
        "wdp" | "hdp" | "jxr" => "image/vnd.ms-photo",
        _ => "application/octet-stream",
    }
}

/// Collects the images a document or slide references, keyed by part name, and
/// hands out unique file names.
#[derive(Default)]
pub struct ImageSet {
    by_part: HashMap<String, String>,
    used_names: std::collections::HashSet<String>,
    pub images: Vec<crate::EmbeddedImage>,
}

impl ImageSet {
    /// Returns the file name to use as the Markdown URL for `part`, loading its
    /// bytes the first time it is seen. Returns `None` when the part is missing.
    pub fn add(&mut self, pkg: &mut Package<'_>, part: &str) -> Option<String> {
        if let Some(name) = self.by_part.get(part) {
            return Some(name.clone());
        }
        if !pkg.has_part(part) {
            return None;
        }
        let base = part.rsplit('/').next().unwrap_or(part);
        let base = if base.is_empty() { "image" } else { base };
        let mut name = base.to_string();
        let mut n = 2;
        while self.used_names.contains(&name) {
            name = match base.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem}-{n}.{ext}"),
                None => format!("{base}-{n}"),
            };
            n += 1;
        }
        self.used_names.insert(name.clone());
        self.by_part.insert(part.to_string(), name.clone());
        if let Some(bytes) = pkg.read_image(part) {
            self.images.push(crate::EmbeddedImage {
                name: name.clone(),
                mime: mime_for(&name).to_string(),
                bytes,
            });
        }
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_targets() {
        assert_eq!(
            resolve_target("word", "media/image1.png"),
            "word/media/image1.png"
        );
        assert_eq!(
            resolve_target("ppt/slides", "../media/a.png"),
            "ppt/media/a.png"
        );
        assert_eq!(
            resolve_target("ppt/slides", "/ppt/media/a.png"),
            "ppt/media/a.png"
        );
        assert_eq!(resolve_target("", "word/document.xml"), "word/document.xml");
        assert_eq!(
            resolve_target("word", "media/my%20pic.png"),
            "word/media/my pic.png"
        );
        assert_eq!(resolve_target("a", "../../../x"), "x");
    }

    #[test]
    fn rels_paths() {
        assert_eq!(
            rels_path("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        assert_eq!(rels_path(""), "_rels/.rels");
        assert_eq!(
            rels_path("ppt/slides/slide1.xml"),
            "ppt/slides/_rels/slide1.xml.rels"
        );
    }

    #[test]
    fn decodes_text() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi".to_vec()), "hi");
        assert_eq!(decode_text(vec![0xFF, 0xFE, b'h', 0, b'i', 0]), "hi");
        assert_eq!(decode_text(b"a\xFFb".to_vec()), "a\u{FFFD}b");
    }
}
