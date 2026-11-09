//! PresentationML (`.pptx`) → one Markdown body per slide.

use std::collections::HashMap;

use anyhow::Context as _;
use quick_xml::events::{BytesStart, Event};

use crate::docx::{is_monospace, safe_url};
use crate::md::{self, BlockWriter, InlineBuf, ListState, Marks, RenderOpts};
use crate::opc::{ImageSet, Package, Rels};
use crate::xml::{MAX_DEPTH, Xml, attr, attr_prefixed, local};
use crate::{Deck, Slide};

/// Decks with more slides than this are cut off (each slide is still small,
/// but a hostile file could list millions).
const MAX_SLIDES: usize = 5_000;

pub fn parse(bytes: &[u8]) -> anyhow::Result<Deck> {
    let mut pkg = Package::open(bytes)?;
    let main = pkg.main_part("ppt/presentation.xml")?;
    let rels = pkg.rels_for(&main)?;
    let pres = pkg.read_xml(&main)?;

    let mut parts = slide_order(&pres, &rels).context("malformed presentation.xml")?;
    if parts.is_empty() {
        parts = slides_by_name(&pkg);
    }

    let mut slides = Vec::new();
    for (i, part) in parts.iter().take(MAX_SLIDES).enumerate() {
        let slide = parse_slide(&mut pkg, part, i + 1)
            .with_context(|| format!("malformed slide {part}"))?;
        slides.push(slide);
    }
    Ok(Deck { slides })
}

/// Slide parts in presentation order, from `p:sldIdLst`.
fn slide_order(pres: &str, rels: &Rels) -> anyhow::Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut x = Xml::new(pres);
    loop {
        match x.next()? {
            Event::Start(e) if local(&e) == "sldId" => {
                if let Some(rel) = attr_prefixed(&e, "id").and_then(|id| rels.get(&id))
                    && !rel.external
                    && !parts.contains(&rel.target)
                {
                    parts.push(rel.target.clone());
                }
                x.skip()?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(parts)
}

/// Fallback when `presentation.xml` lists no slides: `ppt/slides/slideN.xml`
/// sorted by N.
fn slides_by_name(pkg: &Package<'_>) -> Vec<String> {
    let mut found: Vec<(u32, String)> = pkg
        .part_names()
        .filter_map(|name| {
            let n = name
                .strip_prefix("ppt/slides/slide")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((n, name.to_string()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, n)| n).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Bullet {
    /// Not specified on the slide: inherited, assumed to be a bullet.
    Inherited,
    None,
    Char,
    AutoNum(u32),
}

#[derive(Debug)]
struct Para {
    level: usize,
    bullet: Bullet,
    content: InlineBuf,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    Title,
    Subtitle,
    Body,
    /// Date, footer, slide number and slide-image placeholders.
    Ignored,
}

#[derive(Debug)]
enum Item {
    Text { role: Role, paras: Vec<Para> },
    Table(Vec<Vec<String>>),
    Image { alt: Option<String>, rel_id: String },
}

fn parse_slide(pkg: &mut Package<'_>, part: &str, index: usize) -> anyhow::Result<Slide> {
    let xml = pkg.read_xml(part)?;
    let rels = pkg.rels_for(part)?;
    let items = parse_shapes(&xml, &rels)?;

    let mut images = ImageSet::default();
    let mut title: Option<String> = None;
    let mut w = BlockWriter::default();
    // Lists from different text frames alternate `-` and `*` bullets so
    // CommonMark keeps them as separate lists.
    let mut lists = 0usize;

    for item in items {
        match item {
            Item::Text {
                role: Role::Title,
                paras,
            } if title.is_none() => {
                let text = paras
                    .iter()
                    .map(|p| p.content.plain_line())
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.is_empty() {
                    title = Some(text);
                }
            }
            Item::Text { role, paras } => {
                if role == Role::Ignored {
                    continue;
                }
                let bullet = if lists.is_multiple_of(2) { "-" } else { "*" };
                if write_text(&mut w, &paras, role, bullet) {
                    lists += 1;
                }
            }
            Item::Table(rows) => {
                if let Some(t) = md::table(&rows) {
                    w.block(&t);
                }
            }
            Item::Image { alt, rel_id } => {
                let Some(rel) = rels.get(&rel_id) else {
                    continue;
                };
                let mut buf = InlineBuf::default();
                if rel.external {
                    if let Some(url) = safe_url(&rel.target) {
                        buf.push_image(alt.as_deref().unwrap_or("image"), &url, None);
                    }
                } else if let Some(name) = images.add(pkg, &rel.target) {
                    buf.push_image(alt.as_deref().unwrap_or(&name), &name, None);
                }
                w.block(&buf.render(RenderOpts::default()));
            }
        }
    }

    let notes = match rels.first_of_type("/notesSlide").filter(|r| !r.external) {
        Some(rel) => {
            let target = rel.target.clone();
            parse_notes(pkg, &target)?
        }
        None => None,
    };

    Ok(Slide {
        index,
        title,
        markdown: w.finish(),
        notes,
        images: images.images,
    })
}

/// Writes one text frame; returns whether it produced any list items.
fn write_text(w: &mut BlockWriter, paras: &[Para], role: Role, bullet: &str) -> bool {
    let mut list = ListState::default();
    let mut wrote_list = false;
    let mut counters: HashMap<usize, u32> = HashMap::new();
    for p in paras {
        if p.content.is_blank() {
            continue;
        }
        let text = md::escape_line_starts(&p.content.render(RenderOpts::default()));
        let as_list = role == Role::Body && p.bullet != Bullet::None;
        if !as_list {
            list.reset();
            counters.clear();
            w.block(&text);
            continue;
        }
        counters.retain(|lvl, _| *lvl <= p.level);
        let marker = match p.bullet {
            Bullet::AutoNum(start) => {
                let n = counters
                    .entry(p.level)
                    .and_modify(|n| *n = n.saturating_add(1))
                    .or_insert(start);
                format!("{n}.")
            }
            _ => {
                counters.remove(&p.level);
                bullet.to_string()
            }
        };
        w.list_item(&list.item(p.level, &marker, &text));
        wrote_list = true;
    }
    w.end_list();
    wrote_list
}

/// Walks a slide's shape tree in document order.
fn parse_shapes(xml: &str, rels: &Rels) -> anyhow::Result<Vec<Item>> {
    let mut items = Vec::new();
    let mut x = Xml::new(xml);
    let mut level = 0usize;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "sp" => {
                    if let Some(item) = parse_sp(&mut x, rels)? {
                        items.push(item);
                    }
                }
                "pic" => {
                    if let Some(item) = parse_pic(&mut x)? {
                        items.push(item);
                    }
                }
                "graphicFrame" => {
                    if let Some(item) = parse_graphic_frame(&mut x, rels)? {
                        items.push(item);
                    }
                }
                "Fallback" | "grpSpPr" | "nvGrpSpPr" | "cxnSp" | "extLst" | "clrMapOvr"
                | "transition" | "timing" | "bg" => x.skip()?,
                _ => {
                    level += 1;
                    if level > MAX_DEPTH {
                        anyhow::bail!("slide nesting is too deep");
                    }
                }
            },
            Event::End(_) => level = level.saturating_sub(1),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(items)
}

fn placeholder_role(e: &BytesStart<'_>) -> Role {
    match attr(e, "type").as_deref() {
        Some("title" | "ctrTitle") => Role::Title,
        Some("subTitle") => Role::Subtitle,
        Some("dt" | "ftr" | "sldNum" | "hdr" | "sldImg") => Role::Ignored,
        _ => Role::Body,
    }
}

fn parse_sp(x: &mut Xml<'_>, rels: &Rels) -> anyhow::Result<Option<Item>> {
    let mut role = Role::Body;
    let mut paras = Vec::new();
    let mut level = 0usize;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "ph" => {
                    role = placeholder_role(&e);
                    x.skip()?;
                }
                "txBody" => paras = parse_text_body(x, rels)?,
                "spPr" | "style" | "extLst" => x.skip()?,
                _ => {
                    level += 1;
                    if level > MAX_DEPTH {
                        anyhow::bail!("shape nesting is too deep");
                    }
                }
            },
            Event::End(_) => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((!paras.is_empty()).then_some(Item::Text { role, paras }))
}

fn parse_pic(x: &mut Xml<'_>) -> anyhow::Result<Option<Item>> {
    let mut alt = None;
    let mut rel_id = None;
    let mut level = 0usize;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "cNvPr" => {
                    alt = attr(&e, "descr")
                        .or_else(|| attr(&e, "title"))
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    x.skip()?;
                }
                "blip" => {
                    if rel_id.is_none() {
                        rel_id = attr_prefixed(&e, "embed").or_else(|| attr_prefixed(&e, "link"));
                    }
                    x.skip()?;
                }
                "extLst" | "spPr" => x.skip()?,
                _ => level += 1,
            },
            Event::End(_) => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
        if level > MAX_DEPTH {
            anyhow::bail!("picture nesting is too deep");
        }
    }
    Ok(rel_id.map(|rel_id| Item::Image { alt, rel_id }))
}

/// Tables inside a graphic frame; charts, diagrams and OLE objects are skipped.
fn parse_graphic_frame(x: &mut Xml<'_>, rels: &Rels) -> anyhow::Result<Option<Item>> {
    let mut table = None;
    let mut level = 0usize;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "tbl" if table.is_none() => table = Some(parse_table(x, rels)?),
                "Fallback" | "extLst" | "tbl" => x.skip()?,
                _ => level += 1,
            },
            Event::End(_) => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
        if level > MAX_DEPTH {
            anyhow::bail!("graphic frame nesting is too deep");
        }
    }
    Ok(table.map(Item::Table))
}

fn parse_table(x: &mut Xml<'_>, rels: &Rels) -> anyhow::Result<Vec<Vec<String>>> {
    let opts = RenderOpts {
        in_table: true,
        ..RenderOpts::default()
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut level = 0usize;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "tr" => {
                    rows.push(Vec::new());
                    level += 1;
                }
                "tc" => {
                    // Cells covered by a merge are placeholders: keep the column, drop the text.
                    let merged = ["hMerge", "vMerge"]
                        .iter()
                        .any(|a| attr(&e, a).is_some_and(|v| matches!(v.as_str(), "1" | "true")));
                    let mut paras = Vec::new();
                    loop {
                        match x.next()? {
                            Event::Start(e) if local(&e) == "txBody" => {
                                paras = parse_text_body(x, rels)?;
                            }
                            Event::Start(_) => x.skip()?,
                            Event::End(_) | Event::Eof => break,
                            _ => {}
                        }
                    }
                    let text = if merged {
                        String::new()
                    } else {
                        paras
                            .iter()
                            .filter(|p| !p.content.is_blank())
                            .map(|p| p.content.render(opts))
                            .collect::<Vec<_>>()
                            .join("<br>")
                    };
                    if let Some(row) = rows.last_mut() {
                        row.push(text);
                    }
                }
                "tblPr" | "tblGrid" | "extLst" => x.skip()?,
                _ => level += 1,
            },
            Event::End(_) => {
                if level == 0 {
                    break;
                }
                level -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
        if level > MAX_DEPTH {
            anyhow::bail!("table nesting is too deep");
        }
    }
    rows.retain(|r| !r.is_empty());
    Ok(rows)
}

/// Paragraphs of an `a:txBody` (or `p:txBody`).
fn parse_text_body(x: &mut Xml<'_>, rels: &Rels) -> anyhow::Result<Vec<Para>> {
    let mut paras = Vec::new();
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "p" => paras.push(parse_para(x, rels)?),
                _ => x.skip()?,
            },
            Event::End(_) | Event::Eof => return Ok(paras),
            _ => {}
        }
    }
}

fn parse_para(x: &mut Xml<'_>, rels: &Rels) -> anyhow::Result<Para> {
    let mut para = Para {
        level: 0,
        bullet: Bullet::Inherited,
        content: InlineBuf::default(),
    };
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "pPr" => {
                    para.level = attr(&e, "lvl")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0)
                        .min(8);
                    loop {
                        match x.next()? {
                            Event::Start(e) => {
                                match local(&e) {
                                    "buNone" => para.bullet = Bullet::None,
                                    "buChar" | "buBlip" => para.bullet = Bullet::Char,
                                    "buAutoNum" => {
                                        let start = attr(&e, "startAt")
                                            .and_then(|v| v.trim().parse().ok())
                                            .unwrap_or(1);
                                        para.bullet = Bullet::AutoNum(start);
                                    }
                                    _ => {}
                                }
                                x.skip()?;
                            }
                            Event::End(_) | Event::Eof => break,
                            _ => {}
                        }
                    }
                }
                "r" | "fld" => parse_run(x, rels, &mut para.content)?,
                "br" => {
                    para.content.push_break();
                    x.skip()?;
                }
                _ => x.skip()?,
            },
            Event::End(_) | Event::Eof => return Ok(para),
            _ => {}
        }
    }
}

fn parse_run(x: &mut Xml<'_>, rels: &Rels, out: &mut InlineBuf) -> anyhow::Result<()> {
    let mut marks = Marks::default();
    let mut link: Option<String> = None;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "rPr" => {
                    let on = |name: &str| {
                        attr(&e, name).is_some_and(|v| matches!(v.trim(), "1" | "true"))
                    };
                    marks.bold = on("b");
                    marks.italic = on("i");
                    marks.strike = attr(&e, "strike")
                        .is_some_and(|v| matches!(v.as_str(), "sngStrike" | "dblStrike"));
                    loop {
                        match x.next()? {
                            Event::Start(e) => {
                                match local(&e) {
                                    "hlinkClick" => {
                                        link = attr_prefixed(&e, "id")
                                            .and_then(|id| rels.get(&id))
                                            .filter(|r| r.external)
                                            .and_then(|r| safe_url(&r.target));
                                    }
                                    "latin" | "cs"
                                        if attr(&e, "typeface")
                                            .is_some_and(|f| is_monospace(&f)) =>
                                    {
                                        marks.code = true;
                                    }
                                    _ => {}
                                }
                                x.skip()?;
                            }
                            Event::End(_) | Event::Eof => break,
                            _ => {}
                        }
                    }
                }
                "t" => {
                    let text = x.text_until_end()?;
                    out.push_text(&text, marks, link.as_deref());
                }
                _ => x.skip()?,
            },
            Event::End(_) | Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

/// Speaker notes: the body placeholder of the notes slide, as plain text.
fn parse_notes(pkg: &mut Package<'_>, part: &str) -> anyhow::Result<Option<String>> {
    let Some(xml) = pkg.read_xml_opt(part)? else {
        return Ok(None);
    };
    let rels = pkg.rels_for(part)?;
    let items = parse_shapes(&xml, &rels).with_context(|| format!("malformed notes {part}"))?;
    let mut lines = Vec::new();
    for item in items {
        if let Item::Text {
            role: Role::Body,
            paras,
        } = item
        {
            for p in paras {
                let text = p.content.plain_text();
                let text = text.trim_end();
                if !text.trim().is_empty() {
                    lines.push(text.to_string());
                }
            }
        }
    }
    Ok((!lines.is_empty()).then(|| lines.join("\n")))
}
