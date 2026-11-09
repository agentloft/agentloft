//! WordprocessingML (`.docx`) → Markdown.

use std::collections::{HashMap, HashSet};

use anyhow::Context as _;
use quick_xml::events::{BytesStart, Event};

use crate::WordDoc;
use crate::md::{self, BlockWriter, InlineBuf, ListState, Marks, RenderOpts};
use crate::opc::{ImageSet, Package, Rels};
use crate::xml::{MAX_DEPTH, Xml, attr, attr_prefixed, local, on_off};

/// Tables and text boxes nested deeper than this are skipped.
const MAX_NESTING: usize = 32;
/// Complex fields nest only a few deep in practice; cap the stack so a
/// hostile file cannot make every run scan a huge one.
const MAX_FIELDS: usize = 64;

/// Elements whose content is never shown: deleted or moved-away revisions,
/// fallbacks duplicating `mc:Choice` content, and property/revision records.
const SKIP: &[&str] = &[
    "del",
    "moveFrom",
    "Fallback",
    "sdtPr",
    "sdtEndPr",
    "sectPr",
    "rPrChange",
    "pPrChange",
    "tblPrChange",
    "trPrChange",
    "tcPrChange",
    "sectPrChange",
    "numberingChange",
    "customXmlPr",
    "smartTagPr",
    "delText",
    "delInstrText",
    "tblPr",
    "tblGrid",
    "trPr",
    "tblPrEx",
];

pub fn parse(bytes: &[u8]) -> anyhow::Result<WordDoc> {
    let mut pkg = Package::open(bytes)?;
    let main = pkg.main_part("word/document.xml")?;
    let rels = pkg.rels_for(&main)?;

    let styles = match related_part(&mut pkg, &rels, "/styles", "word/styles.xml")? {
        Some(xml) => Styles::parse(&xml).context("malformed styles.xml")?,
        None => Styles::default(),
    };
    let numbering = match related_part(&mut pkg, &rels, "/numbering", "word/numbering.xml")? {
        Some(xml) => Numbering::parse(&xml).context("malformed numbering.xml")?,
        None => Numbering::default(),
    };
    let xml = pkg.read_xml(&main)?;

    let mut cx = Converter {
        pkg,
        styles,
        numbering,
        images: ImageSet::default(),
        counters: HashMap::new(),
        seen_num_levels: HashSet::new(),
        fields: Vec::new(),
        fields_overflow: 0,
        note_refs: Vec::new(),
        note_ref_set: HashSet::new(),
    };
    let mut blocks = Vec::new();
    let mut x = Xml::new(&xml);
    cx.parse_blocks(&mut x, &rels, 0, &mut blocks, None)
        .context("malformed document.xml")?;

    let mut title = None;
    let mut markdown = render_blocks(&blocks, &mut title);

    let notes = cx.render_notes(&rels)?;
    if !notes.is_empty() {
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str(&notes);
    }
    if title.is_none() {
        title = cx.pkg.core_title();
    }
    Ok(WordDoc {
        title,
        markdown,
        images: cx.images.images,
    })
}

/// Reads the part a relationship of the given type points to, falling back to
/// a conventional path.
fn related_part(
    pkg: &mut Package<'_>,
    rels: &Rels,
    rel_suffix: &str,
    fallback: &str,
) -> anyhow::Result<Option<String>> {
    let part = rels
        .first_of_type(rel_suffix)
        .filter(|r| !r.external)
        .map(|r| r.target.clone())
        .unwrap_or_else(|| fallback.to_string());
    pkg.read_xml_opt(&part)
}

// ---------------------------------------------------------------------------
// Intermediate model

#[derive(Debug, Clone, PartialEq)]
enum ParaKind {
    Normal,
    Title,
    Heading(u8),
    List {
        level: usize,
        ordered: bool,
        number: u32,
    },
    Code,
    Quote,
}

#[derive(Debug)]
enum Block {
    Para { kind: ParaKind, content: InlineBuf },
    Table(Vec<Vec<TableCell>>),
    PageBreak,
}

#[derive(Debug, Default)]
struct TableCell {
    blocks: Vec<Block>,
    span: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct CellProps {
    span: usize,
}

// ---------------------------------------------------------------------------
// Properties

#[derive(Debug, Default, Clone)]
struct ParaProps {
    style: Option<String>,
    num_id: Option<i64>,
    ilvl: Option<usize>,
    outline: Option<u8>,
}

#[derive(Debug, Default, Clone)]
struct RunProps {
    bold: Option<bool>,
    italic: Option<bool>,
    strike: Option<bool>,
    hidden: Option<bool>,
    font: Option<String>,
    style: Option<String>,
}

impl RunProps {
    /// Fills unset fields from `parent`.
    fn inherit(&mut self, parent: &RunProps) {
        self.bold = self.bold.or(parent.bold);
        self.italic = self.italic.or(parent.italic);
        self.strike = self.strike.or(parent.strike);
        self.hidden = self.hidden.or(parent.hidden);
        if self.font.is_none() {
            self.font.clone_from(&parent.font);
        }
    }
}

fn parse_ppr(x: &mut Xml<'_>) -> anyhow::Result<ParaProps> {
    let mut props = ParaProps::default();
    let mut level = 0usize;
    loop {
        match x.next()? {
            Event::Start(e) => match local(&e) {
                "numPr" => level += 1,
                "pStyle" => {
                    props.style = attr(&e, "val");
                    x.skip()?;
                }
                "ilvl" => {
                    props.ilvl = attr(&e, "val").and_then(|v| v.trim().parse().ok());
                    x.skip()?;
                }
                "numId" => {
                    props.num_id = attr(&e, "val").and_then(|v| v.trim().parse().ok());
                    x.skip()?;
                }
                "outlineLvl" => {
                    props.outline = attr(&e, "val").and_then(|v| v.trim().parse().ok());
                    x.skip()?;
                }
                _ => x.skip()?,
            },
            Event::End(_) => {
                if level == 0 {
                    return Ok(props);
                }
                level -= 1;
            }
            Event::Eof => return Ok(props),
            _ => {}
        }
    }
}

fn parse_rpr(x: &mut Xml<'_>) -> anyhow::Result<RunProps> {
    let mut props = RunProps::default();
    loop {
        match x.next()? {
            Event::Start(e) => {
                match local(&e) {
                    "b" => props.bold = Some(on_off(&e, "val")),
                    "i" => props.italic = Some(on_off(&e, "val")),
                    "strike" | "dstrike" => {
                        let on = on_off(&e, "val");
                        props.strike = Some(props.strike.unwrap_or(false) || on);
                    }
                    "vanish" => props.hidden = Some(on_off(&e, "val")),
                    "rStyle" => props.style = attr(&e, "val"),
                    "rFonts" => {
                        props.font = attr(&e, "ascii")
                            .or_else(|| attr(&e, "hAnsi"))
                            .or_else(|| attr(&e, "cs"));
                    }
                    _ => {}
                }
                x.skip()?;
            }
            Event::End(_) | Event::Eof => return Ok(props),
            _ => {}
        }
    }
}

pub fn is_monospace(font: &str) -> bool {
    const MONO: &[&str] = &[
        "courier",
        "consolas",
        "mono",
        "menlo",
        "monaco",
        "lucida console",
        "lucida sans typewriter",
        "source code",
        "fira code",
        "inconsolata",
        "cascadia",
        "fixedsys",
        "hack",
        "jetbrains",
        "iosevka",
        "sf mono",
    ];
    let f = font.to_ascii_lowercase();
    MONO.iter().any(|m| f.contains(m))
}

// ---------------------------------------------------------------------------
// styles.xml

#[derive(Debug, Default)]
struct Style {
    name: Option<String>,
    based_on: Option<String>,
    para: ParaProps,
    run: RunProps,
}

#[derive(Debug, Default)]
struct Styles {
    by_id: HashMap<String, Style>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum HeadingKind {
    Title,
    Level(u8),
}

impl Styles {
    fn parse(xml: &str) -> anyhow::Result<Self> {
        let mut styles = Styles::default();
        let mut x = Xml::new(xml);
        loop {
            match x.next()? {
                Event::Start(e) if local(&e) == "style" => {
                    let id = attr(&e, "styleId").unwrap_or_default();
                    let style = Self::parse_style(&mut x)?;
                    if !id.is_empty() {
                        styles.by_id.entry(id).or_insert(style);
                    }
                }
                Event::Start(e) if local(&e) == "docDefaults" => x.skip()?,
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(styles)
    }

    fn parse_style(x: &mut Xml<'_>) -> anyhow::Result<Style> {
        let mut style = Style::default();
        loop {
            match x.next()? {
                Event::Start(e) => match local(&e) {
                    "name" => {
                        style.name = attr(&e, "val");
                        x.skip()?;
                    }
                    "basedOn" => {
                        style.based_on = attr(&e, "val");
                        x.skip()?;
                    }
                    "pPr" => style.para = parse_ppr(x)?,
                    "rPr" => style.run = parse_rpr(x)?,
                    _ => x.skip()?,
                },
                Event::End(_) | Event::Eof => return Ok(style),
                _ => {}
            }
        }
    }

    /// The style and its `basedOn` ancestors, nearest first. Cycles and
    /// absurdly deep chains are cut off.
    fn chain<'s>(&'s self, id: &'s str) -> Vec<(&'s str, Option<&'s Style>)> {
        let mut out: Vec<(&str, Option<&Style>)> = Vec::new();
        let mut current = Some(id);
        while let Some(id) = current {
            if out.len() >= 16 || out.iter().any(|(seen, _)| *seen == id) {
                break;
            }
            let style = self.by_id.get(id);
            out.push((id, style));
            current = style.and_then(|s| s.based_on.as_deref());
        }
        out
    }

    fn display_name<'s>(id: &'s str, style: Option<&'s Style>) -> String {
        style
            .and_then(|s| s.name.as_deref())
            .unwrap_or(id)
            .trim()
            .to_ascii_lowercase()
    }

    fn heading(&self, id: &str) -> Option<HeadingKind> {
        for (id, style) in self.chain(id) {
            let name = Self::display_name(id, style);
            if name == "title" {
                return Some(HeadingKind::Title);
            }
            if let Some(n) = name
                .strip_prefix("heading")
                .and_then(|rest| rest.trim().parse::<u8>().ok())
                .filter(|n| (1..=9).contains(n))
            {
                return Some(HeadingKind::Level(n.min(6)));
            }
            if let Some(lvl) = style.and_then(|s| s.para.outline).filter(|l| *l < 9) {
                return Some(HeadingKind::Level((lvl + 1).min(6)));
            }
        }
        None
    }

    fn is_code(&self, id: &str) -> bool {
        self.chain(id).into_iter().any(|(id, style)| {
            let name = Self::display_name(id, style);
            name.contains("code")
                || name.contains("verbatim")
                || name.contains("preformatted")
                || style
                    .and_then(|s| s.run.font.as_deref())
                    .is_some_and(is_monospace)
        })
    }

    fn is_quote(&self, id: &str) -> bool {
        self.chain(id).into_iter().any(|(id, style)| {
            matches!(
                Self::display_name(id, style).as_str(),
                "quote" | "intense quote" | "block text" | "block quote" | "block quotation"
            )
        })
    }

    fn numbering(&self, id: &str) -> (Option<i64>, Option<usize>) {
        let mut num_id = None;
        let mut ilvl = None;
        for (_, style) in self.chain(id) {
            if let Some(s) = style {
                num_id = num_id.or(s.para.num_id);
                ilvl = ilvl.or(s.para.ilvl);
            }
        }
        (num_id, ilvl)
    }

    /// Run properties of a character style, merged along its chain.
    fn run_props(&self, id: &str) -> RunProps {
        let mut props = RunProps::default();
        for (_, style) in self.chain(id) {
            if let Some(s) = style {
                props.inherit(&s.run);
            }
        }
        props
    }

    fn is_code_char_style(&self, id: &str) -> bool {
        self.chain(id).into_iter().any(|(id, style)| {
            let name = Self::display_name(id, style);
            name.contains("code") || name.contains("verbatim")
        })
    }
}

// ---------------------------------------------------------------------------
// numbering.xml

#[derive(Debug, Clone, Default)]
struct Level {
    ordered: bool,
    start: u32,
}

#[derive(Debug, Default)]
struct Num {
    abstract_id: String,
    start_overrides: HashMap<usize, u32>,
    level_overrides: HashMap<usize, Level>,
}

#[derive(Debug, Default)]
struct Numbering {
    abstracts: HashMap<String, HashMap<usize, Level>>,
    nums: HashMap<i64, Num>,
}

impl Numbering {
    fn parse(xml: &str) -> anyhow::Result<Self> {
        let mut numbering = Numbering::default();
        let mut x = Xml::new(xml);
        loop {
            match x.next()? {
                Event::Start(e) if local(&e) == "abstractNum" => {
                    let id = attr(&e, "abstractNumId").unwrap_or_default();
                    let mut levels = HashMap::new();
                    loop {
                        match x.next()? {
                            Event::Start(e) if local(&e) == "lvl" => {
                                let ilvl = parse_ilvl(&e);
                                levels.insert(ilvl, Self::parse_level(&mut x)?);
                            }
                            Event::Start(_) => x.skip()?,
                            Event::End(_) | Event::Eof => break,
                            _ => {}
                        }
                    }
                    numbering.abstracts.entry(id).or_insert(levels);
                }
                Event::Start(e) if local(&e) == "num" => {
                    let Some(id) = attr(&e, "numId").and_then(|v| v.trim().parse().ok()) else {
                        x.skip()?;
                        continue;
                    };
                    let mut num = Num::default();
                    loop {
                        match x.next()? {
                            Event::Start(e) if local(&e) == "abstractNumId" => {
                                num.abstract_id = attr(&e, "val").unwrap_or_default();
                                x.skip()?;
                            }
                            Event::Start(e) if local(&e) == "lvlOverride" => {
                                let ilvl = parse_ilvl(&e);
                                loop {
                                    match x.next()? {
                                        Event::Start(e) if local(&e) == "startOverride" => {
                                            if let Some(v) =
                                                attr(&e, "val").and_then(|v| v.trim().parse().ok())
                                            {
                                                num.start_overrides.insert(ilvl, v);
                                            }
                                            x.skip()?;
                                        }
                                        Event::Start(e) if local(&e) == "lvl" => {
                                            num.level_overrides
                                                .insert(ilvl, Self::parse_level(&mut x)?);
                                        }
                                        Event::Start(_) => x.skip()?,
                                        Event::End(_) | Event::Eof => break,
                                        _ => {}
                                    }
                                }
                            }
                            Event::Start(_) => x.skip()?,
                            Event::End(_) | Event::Eof => break,
                            _ => {}
                        }
                    }
                    numbering.nums.insert(id, num);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(numbering)
    }

    fn parse_level(x: &mut Xml<'_>) -> anyhow::Result<Level> {
        let mut level = Level {
            ordered: false,
            start: 1,
        };
        loop {
            match x.next()? {
                Event::Start(e) => {
                    match local(&e) {
                        "start" => {
                            if let Some(v) = attr(&e, "val").and_then(|v| v.trim().parse().ok()) {
                                level.start = v;
                            }
                        }
                        "numFmt" => {
                            let fmt = attr(&e, "val").unwrap_or_default();
                            level.ordered = !matches!(fmt.as_str(), "bullet" | "none" | "");
                        }
                        _ => {}
                    }
                    x.skip()?;
                }
                Event::End(_) | Event::Eof => return Ok(level),
                _ => {}
            }
        }
    }

    /// The abstract list id (shared counter), level format and any start override.
    fn level(&self, num_id: i64, ilvl: usize) -> (String, Level, Option<u32>) {
        let Some(num) = self.nums.get(&num_id) else {
            return (format!("num{num_id}"), Level::default(), None);
        };
        let level = num
            .level_overrides
            .get(&ilvl)
            .cloned()
            .or_else(|| {
                self.abstracts
                    .get(&num.abstract_id)
                    .and_then(|levels| levels.get(&ilvl).cloned())
            })
            .unwrap_or(Level {
                ordered: false,
                start: 1,
            });
        (
            num.abstract_id.clone(),
            level,
            num.start_overrides.get(&ilvl).copied(),
        )
    }
}

fn parse_ilvl(e: &BytesStart<'_>) -> usize {
    attr(e, "ilvl")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(8)
}

// ---------------------------------------------------------------------------
// Conversion

#[derive(Debug, Default)]
struct Field {
    instr: String,
    in_result: bool,
    link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum NoteKind {
    Foot,
    End,
}

enum DrawItem {
    Image { rel_id: String, alt: Option<String> },
    Text(Vec<Block>),
}

struct Converter<'a> {
    pkg: Package<'a>,
    styles: Styles,
    numbering: Numbering,
    images: ImageSet,
    /// List counters per abstract list, per level.
    counters: HashMap<String, [Option<u32>; 9]>,
    seen_num_levels: HashSet<(i64, usize)>,
    /// Open complex fields (`w:fldChar`), which may span paragraphs.
    fields: Vec<Field>,
    /// Field begins ignored because the stack hit [`MAX_FIELDS`].
    fields_overflow: usize,
    note_refs: Vec<(NoteKind, String)>,
    note_ref_set: HashSet<(NoteKind, String)>,
}

impl Converter<'_> {
    /// Parses block content (paragraphs, tables) until the end of the element
    /// whose Start was just read. Unknown wrappers such as `w:sdt`, `w:ins`
    /// and `w:customXml` are descended into.
    fn parse_blocks(
        &mut self,
        x: &mut Xml<'_>,
        rels: &Rels,
        nesting: usize,
        out: &mut Vec<Block>,
        mut cell: Option<&mut CellProps>,
    ) -> anyhow::Result<()> {
        let mut level = 0usize;
        loop {
            match x.next()? {
                Event::Start(e) => match local(&e) {
                    "p" => self.parse_paragraph(x, rels, nesting, out)?,
                    "tbl" if nesting < MAX_NESTING => {
                        let table = self.parse_table(x, rels, nesting + 1)?;
                        out.push(table);
                    }
                    "tcPr" if level == 0 && cell.is_some() => {
                        if let Some(props) = cell.as_deref_mut() {
                            *props = parse_tcpr(x)?;
                        }
                    }
                    name if SKIP.contains(&name) || name == "tbl" => x.skip()?,
                    _ => {
                        level += 1;
                        check_depth(level)?;
                    }
                },
                Event::End(_) => {
                    if level == 0 {
                        return Ok(());
                    }
                    level -= 1;
                }
                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    fn parse_table(
        &mut self,
        x: &mut Xml<'_>,
        rels: &Rels,
        nesting: usize,
    ) -> anyhow::Result<Block> {
        let mut rows = Vec::new();
        let mut level = 0usize;
        loop {
            match x.next()? {
                Event::Start(e) => match local(&e) {
                    "tr" => rows.push(self.parse_row(x, rels, nesting)?),
                    name if SKIP.contains(&name) => x.skip()?,
                    _ => {
                        level += 1;
                        check_depth(level)?;
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
        Ok(Block::Table(rows))
    }

    fn parse_row(
        &mut self,
        x: &mut Xml<'_>,
        rels: &Rels,
        nesting: usize,
    ) -> anyhow::Result<Vec<TableCell>> {
        let mut cells = Vec::new();
        let mut level = 0usize;
        loop {
            match x.next()? {
                Event::Start(e) => match local(&e) {
                    "tc" => {
                        let mut props = CellProps { span: 1 };
                        let mut blocks = Vec::new();
                        self.parse_blocks(x, rels, nesting, &mut blocks, Some(&mut props))?;
                        cells.push(TableCell {
                            blocks,
                            span: props.span.clamp(1, 64),
                        });
                    }
                    name if SKIP.contains(&name) => x.skip()?,
                    _ => {
                        level += 1;
                        check_depth(level)?;
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
        Ok(cells)
    }

    fn para_kind(&mut self, props: &ParaProps) -> ParaKind {
        let style = props.style.as_deref();
        match style.and_then(|s| self.styles.heading(s)) {
            Some(HeadingKind::Title) => return ParaKind::Title,
            Some(HeadingKind::Level(n)) => return ParaKind::Heading(n),
            None => {}
        }
        if let Some(lvl) = props.outline.filter(|l| *l < 9) {
            return ParaKind::Heading((lvl + 1).min(6));
        }
        let (style_num, style_ilvl) = style
            .map(|s| self.styles.numbering(s))
            .unwrap_or((None, None));
        let num_id = props.num_id.or(style_num);
        if let Some(num_id) = num_id.filter(|n| *n > 0) {
            let ilvl = props.ilvl.or(style_ilvl).unwrap_or(0).min(8);
            let (abstract_id, level, start_override) = self.numbering.level(num_id, ilvl);
            let counters = self.counters.entry(abstract_id).or_insert([None; 9]);
            if self.seen_num_levels.insert((num_id, ilvl))
                && let Some(start) = start_override
            {
                counters[ilvl] = Some(start.saturating_sub(1));
            }
            let number = match counters[ilvl] {
                Some(n) => n.saturating_add(1),
                None => level.start,
            };
            counters[ilvl] = Some(number);
            for c in counters.iter_mut().skip(ilvl + 1) {
                *c = None;
            }
            return ParaKind::List {
                level: ilvl,
                ordered: level.ordered,
                number,
            };
        }
        if let Some(s) = style {
            if self.styles.is_code(s) {
                return ParaKind::Code;
            }
            if self.styles.is_quote(s) {
                return ParaKind::Quote;
            }
        }
        ParaKind::Normal
    }

    fn marks(&self, run: &RunProps) -> Marks {
        let mut props = run.clone();
        let mut style_code = false;
        if let Some(style) = run.style.as_deref() {
            props.inherit(&self.styles.run_props(style));
            style_code = self.styles.is_code_char_style(style);
        }
        Marks {
            bold: props.bold.unwrap_or(false),
            italic: props.italic.unwrap_or(false),
            strike: props.strike.unwrap_or(false),
            code: style_code || props.font.as_deref().is_some_and(is_monospace),
        }
    }

    fn field_link(&self) -> Option<String> {
        self.fields
            .iter()
            .rev()
            .filter(|f| f.in_result)
            .find_map(|f| f.link.clone())
    }

    fn parse_paragraph(
        &mut self,
        x: &mut Xml<'_>,
        rels: &Rels,
        nesting: usize,
        out: &mut Vec<Block>,
    ) -> anyhow::Result<()> {
        let mut props = ParaProps::default();
        let mut kind: Option<ParaKind> = None;
        let mut content = InlineBuf::default();
        let mut extra: Vec<Block> = Vec::new();
        let mut run = RunProps::default();
        // Open `w:hyperlink` / `w:fldSimple` elements: (nesting level, URL).
        let mut links: Vec<(usize, Option<String>)> = Vec::new();
        let mut level = 0usize;

        loop {
            match x.next()? {
                Event::Start(e) => match local(&e) {
                    "pPr" if level == 0 => props = parse_ppr(x)?,
                    "r" => {
                        run = RunProps::default();
                        level += 1;
                    }
                    "rPr" => run = parse_rpr(x)?,
                    "t" => {
                        let text = x.text_until_end()?;
                        if !run.hidden.unwrap_or(false) {
                            let link = self.current_link(&links);
                            content.push_text(&text, self.marks(&run), link.as_deref());
                        }
                    }
                    "tab" | "ptab" => {
                        let link = self.current_link(&links);
                        content.push_text(" ", self.marks(&run), link.as_deref());
                        x.skip()?;
                    }
                    "noBreakHyphen" => {
                        let link = self.current_link(&links);
                        content.push_text("-", self.marks(&run), link.as_deref());
                        x.skip()?;
                    }
                    "sym" => {
                        // Symbol-font glyphs live in the private-use area and
                        // have no meaningful Unicode equivalent.
                        if let Some(c) = attr(&e, "char")
                            .and_then(|h| u32::from_str_radix(h.trim(), 16).ok())
                            .and_then(char::from_u32)
                            .filter(|c| !('\u{E000}'..='\u{F8FF}').contains(c))
                        {
                            let link = self.current_link(&links);
                            content.push_text(&c.to_string(), self.marks(&run), link.as_deref());
                        }
                        x.skip()?;
                    }
                    "br" | "cr" => {
                        let page = attr(&e, "type").is_some_and(|t| t == "page");
                        x.skip()?;
                        if page {
                            let k = kind.get_or_insert_with(|| self.para_kind(&props)).clone();
                            out.push(Block::Para {
                                kind: k,
                                content: std::mem::take(&mut content),
                            });
                            out.push(Block::PageBreak);
                        } else {
                            content.push_break();
                        }
                    }
                    "hyperlink" => {
                        let url = attr_prefixed(&e, "id")
                            .and_then(|id| rels.get(&id))
                            .filter(|r| r.external)
                            .and_then(|r| safe_url(&r.target));
                        links.push((level, url));
                        level += 1;
                    }
                    "fldSimple" => {
                        let url = attr(&e, "instr").and_then(|i| hyperlink_from_instr(&i));
                        links.push((level, url));
                        level += 1;
                    }
                    "fldChar" => {
                        match attr(&e, "fldCharType").as_deref() {
                            Some("begin") => {
                                if self.fields.len() < MAX_FIELDS {
                                    self.fields.push(Field::default());
                                } else {
                                    self.fields_overflow += 1;
                                }
                            }
                            Some("separate") => {
                                if let Some(f) = self.fields.last_mut() {
                                    f.in_result = true;
                                    f.link = hyperlink_from_instr(&f.instr);
                                }
                            }
                            Some("end") => {
                                if self.fields_overflow > 0 {
                                    self.fields_overflow -= 1;
                                } else {
                                    self.fields.pop();
                                }
                            }
                            _ => {}
                        }
                        x.skip()?;
                    }
                    "instrText" => {
                        let text = x.text_until_end()?;
                        if let Some(f) = self.fields.last_mut()
                            && !f.in_result
                            && f.instr.len() < 4096
                        {
                            f.instr.push_str(&text);
                        }
                    }
                    "drawing" | "pict" | "object" => {
                        let items = self.parse_drawing(x, rels, nesting)?;
                        let link = self.current_link(&links);
                        for item in items {
                            match item {
                                DrawItem::Image { rel_id, alt } => {
                                    self.push_image(
                                        &mut content,
                                        rels,
                                        &rel_id,
                                        alt,
                                        link.as_deref(),
                                    );
                                }
                                DrawItem::Text(blocks) => extra.extend(blocks),
                            }
                        }
                    }
                    "footnoteReference" | "endnoteReference" => {
                        let note = if local(&e) == "footnoteReference" {
                            NoteKind::Foot
                        } else {
                            NoteKind::End
                        };
                        if let Some(id) = attr(&e, "id").map(|s| s.trim().to_string())
                            && !id.is_empty()
                            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                        {
                            content.push_raw(note_label(note, &id));
                            if self.note_ref_set.insert((note, id.clone())) {
                                self.note_refs.push((note, id));
                            }
                        }
                        x.skip()?;
                    }
                    // A paragraph or table cannot nest here; ignore malformed input.
                    "p" | "tbl" => x.skip()?,
                    name if SKIP.contains(&name) => x.skip()?,
                    _ => {
                        level += 1;
                        check_depth(level)?;
                    }
                },
                Event::End(_) => {
                    if level == 0 {
                        break;
                    }
                    level -= 1;
                    while links.last().is_some_and(|(l, _)| *l >= level) {
                        links.pop();
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        let kind = kind.unwrap_or_else(|| self.para_kind(&props));
        out.push(Block::Para { kind, content });
        out.extend(extra);
        Ok(())
    }

    fn current_link(&self, links: &[(usize, Option<String>)]) -> Option<String> {
        match links.last() {
            Some((_, url)) => url.clone(),
            None => self.field_link(),
        }
    }

    fn push_image(
        &mut self,
        content: &mut InlineBuf,
        rels: &Rels,
        rel_id: &str,
        alt: Option<String>,
        link: Option<&str>,
    ) {
        let Some(rel) = rels.get(rel_id) else {
            return;
        };
        if rel.external {
            if let Some(url) = safe_url(&rel.target) {
                let alt = alt.unwrap_or_else(|| "image".to_string());
                content.push_image(&alt, &url, link);
            }
            return;
        }
        let target = rel.target.clone();
        if let Some(name) = self.images.add(&mut self.pkg, &target) {
            let alt = alt.unwrap_or_else(|| name.clone());
            content.push_image(&alt, &name, link);
        }
    }

    /// Collects pictures and text-box content from a `w:drawing`, `w:pict`
    /// or `w:object`.
    fn parse_drawing(
        &mut self,
        x: &mut Xml<'_>,
        rels: &Rels,
        nesting: usize,
    ) -> anyhow::Result<Vec<DrawItem>> {
        let mut items = Vec::new();
        let mut alt: Option<String> = None;
        let mut level = 0usize;
        loop {
            match x.next()? {
                Event::Start(e) => match local(&e) {
                    "docPr" | "cNvPr" => {
                        // `wp:docPr` carries the alt text; the picture's own
                        // `pic:cNvPr` usually has none and must not erase it.
                        if let Some(text) = attr(&e, "descr")
                            .or_else(|| attr(&e, "title"))
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                        {
                            alt = Some(text);
                        }
                        x.skip()?;
                    }
                    "blip" => {
                        if let Some(id) =
                            attr_prefixed(&e, "embed").or_else(|| attr_prefixed(&e, "link"))
                        {
                            items.push(DrawItem::Image {
                                rel_id: id,
                                alt: alt.clone(),
                            });
                        }
                        x.skip()?;
                    }
                    "imagedata" => {
                        if let Some(id) =
                            attr_prefixed(&e, "id").or_else(|| attr_prefixed(&e, "relid"))
                        {
                            let title = attr(&e, "title").filter(|s| !s.trim().is_empty());
                            items.push(DrawItem::Image {
                                rel_id: id,
                                alt: title.or_else(|| alt.clone()),
                            });
                        }
                        x.skip()?;
                    }
                    "txbxContent" if nesting < MAX_NESTING => {
                        let mut blocks = Vec::new();
                        self.parse_blocks(x, rels, nesting + 1, &mut blocks, None)?;
                        items.push(DrawItem::Text(blocks));
                    }
                    "txbxContent" | "Fallback" | "chart" | "extLst" => x.skip()?,
                    _ => {
                        level += 1;
                        check_depth(level)?;
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
        Ok(items)
    }

    /// Footnote and endnote definitions for every note referenced in the body.
    fn render_notes(&mut self, rels: &Rels) -> anyhow::Result<String> {
        if self.note_refs.is_empty() {
            return Ok(String::new());
        }
        let mut texts: HashMap<(NoteKind, String), String> = HashMap::new();
        for (kind, rel_type, fallback, element) in [
            (
                NoteKind::Foot,
                "/footnotes",
                "word/footnotes.xml",
                "footnote",
            ),
            (NoteKind::End, "/endnotes", "word/endnotes.xml", "endnote"),
        ] {
            if !self.note_refs.iter().any(|(k, _)| *k == kind) {
                continue;
            }
            let part = rels
                .first_of_type(rel_type)
                .filter(|r| !r.external)
                .map(|r| r.target.clone())
                .unwrap_or_else(|| fallback.to_string());
            let Some(xml) = self.pkg.read_xml_opt(&part)? else {
                continue;
            };
            let note_rels = self.pkg.rels_for(&part)?;
            let mut x = Xml::new(&xml);
            self.fields.clear();
            self.fields_overflow = 0;
            loop {
                match x.next().with_context(|| format!("malformed {part}"))? {
                    Event::Start(e) if local(&e) == element => {
                        let id = attr(&e, "id").unwrap_or_default().trim().to_string();
                        let wanted = self.note_ref_set.contains(&(kind, id.clone()));
                        if !wanted {
                            x.skip()?;
                            continue;
                        }
                        let mut blocks = Vec::new();
                        self.parse_blocks(&mut x, &note_rels, 1, &mut blocks, None)?;
                        let text = flatten_blocks(&blocks, " ");
                        texts.insert((kind, id), text);
                    }
                    Event::Eof => break,
                    _ => {}
                }
            }
        }
        let mut writer = BlockWriter::default();
        for (kind, id) in &self.note_refs {
            let text = texts
                .get(&(*kind, id.clone()))
                .map(String::as_str)
                .unwrap_or("");
            writer.block(&format!("{}: {}", note_label(*kind, id), text.trim()));
        }
        Ok(writer.finish())
    }
}

fn note_label(kind: NoteKind, id: &str) -> String {
    match kind {
        NoteKind::Foot => format!("[^{id}]"),
        NoteKind::End => format!("[^e{id}]"),
    }
}

fn check_depth(level: usize) -> anyhow::Result<()> {
    if level > MAX_DEPTH {
        anyhow::bail!("document nesting is too deep");
    }
    Ok(())
}

fn parse_tcpr(x: &mut Xml<'_>) -> anyhow::Result<CellProps> {
    let mut props = CellProps { span: 1 };
    loop {
        match x.next()? {
            Event::Start(e) => {
                if local(&e) == "gridSpan" {
                    props.span = attr(&e, "val")
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(1);
                }
                x.skip()?;
            }
            Event::End(_) | Event::Eof => return Ok(props),
            _ => {}
        }
    }
}

/// The URL of a `HYPERLINK "…"` field instruction. Links to bookmarks
/// (`\l`) are dropped: they have no meaning outside Word.
fn hyperlink_from_instr(instr: &str) -> Option<String> {
    let instr = instr.trim();
    let rest = instr
        .strip_prefix("HYPERLINK")
        .or_else(|| instr.strip_prefix("hyperlink"))?;
    let mut tokens = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut tok = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                tok.push(c);
            }
            tokens.push((tok, true));
        } else {
            let mut tok = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                tok.push(c);
                chars.next();
            }
            tokens.push((tok, false));
        }
    }
    let mut iter = tokens.into_iter();
    while let Some((tok, quoted)) = iter.next() {
        if !quoted && tok.starts_with('\\') {
            if tok == "\\l" {
                return None;
            }
            // Switches with an argument: skip it.
            if matches!(tok.as_str(), "\\o" | "\\t") {
                iter.next();
            }
            continue;
        }
        return safe_url(&tok);
    }
    None
}

/// Only links a viewer can safely open: web, mail and phone links.
pub fn safe_url(url: &str) -> Option<String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    let ok = ["http://", "https://", "mailto:", "ftp://", "tel:"]
        .iter()
        .any(|p| lower.starts_with(p));
    ok.then(|| url.to_string())
}

// ---------------------------------------------------------------------------
// Rendering

fn render_blocks(blocks: &[Block], title: &mut Option<String>) -> String {
    let mut w = BlockWriter::default();
    let mut list = ListState::default();
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            Block::Para { kind, content } => {
                if *kind == ParaKind::Code {
                    let mut lines = Vec::new();
                    while let Some(Block::Para {
                        kind: ParaKind::Code,
                        content,
                    }) = blocks.get(i)
                    {
                        lines.extend(content.plain_lines());
                        i += 1;
                    }
                    while lines.last().is_some_and(|l| l.trim().is_empty()) {
                        lines.pop();
                    }
                    if lines.iter().any(|l| !l.trim().is_empty()) {
                        list.reset();
                        w.block(&md::code_block(&lines));
                    }
                    continue;
                }
                if content.is_blank() {
                    i += 1;
                    continue;
                }
                match kind {
                    ParaKind::Title | ParaKind::Heading(_) => {
                        list.reset();
                        let level = match kind {
                            ParaKind::Heading(n) => *n as usize,
                            _ => 1,
                        };
                        if *kind == ParaKind::Title && title.is_none() {
                            let t = content.plain_line();
                            if !t.is_empty() {
                                *title = Some(t);
                            }
                        }
                        let text = md::heading_text(&content.render(RenderOpts {
                            no_bold: true,
                            ..RenderOpts::default()
                        }));
                        w.block(&format!("{} {}", "#".repeat(level), text));
                    }
                    ParaKind::List {
                        level,
                        ordered,
                        number,
                    } => {
                        let text = md::escape_line_starts(&content.render(RenderOpts::default()));
                        let marker = if *ordered {
                            format!("{number}.")
                        } else {
                            "-".to_string()
                        };
                        w.list_item(&list.item(*level, &marker, &text));
                    }
                    ParaKind::Quote => {
                        list.reset();
                        let text = md::escape_line_starts(&content.render(RenderOpts::default()));
                        let quoted: Vec<String> =
                            text.split('\n').map(|l| format!("> {l}")).collect();
                        w.block(&quoted.join("\n"));
                    }
                    ParaKind::Normal | ParaKind::Code => {
                        list.reset();
                        w.block(&md::escape_line_starts(
                            &content.render(RenderOpts::default()),
                        ));
                    }
                }
            }
            Block::Table(rows) => {
                list.reset();
                if let Some(t) = render_table(rows) {
                    w.block(&t);
                }
            }
            Block::PageBreak => {
                list.reset();
                if !w.out.is_empty() && !w.out.ends_with("---") {
                    w.block("---");
                }
            }
        }
        i += 1;
    }
    let mut out = w.finish();
    while let Some(s) = out.strip_suffix("---") {
        out = s.trim_end().to_string();
    }
    out
}

fn render_table(rows: &[Vec<TableCell>]) -> Option<String> {
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let mut cells = Vec::new();
            for cell in row {
                cells.push(flatten_blocks(&cell.blocks, "<br>"));
                for _ in 1..cell.span {
                    cells.push(String::new());
                }
            }
            cells
        })
        .filter(|row: &Vec<String>| !row.is_empty())
        .collect();
    md::table(&rendered)
}

/// Renders blocks onto a single line for a table cell or footnote.
fn flatten_blocks(blocks: &[Block], sep: &str) -> String {
    let opts = RenderOpts {
        in_table: true,
        ..RenderOpts::default()
    };
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block {
            Block::Para { kind, content } => {
                if content.is_blank() {
                    continue;
                }
                let text = content.render(opts);
                let text = match kind {
                    ParaKind::List {
                        ordered: true,
                        number,
                        ..
                    } => format!("{number}. {text}"),
                    ParaKind::List { .. } => format!("• {text}"),
                    _ => text,
                };
                parts.push(text);
            }
            Block::Table(rows) => {
                for row in rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|c| flatten_blocks(&c.blocks, " "))
                        .filter(|c| !c.is_empty())
                        .collect();
                    if !cells.is_empty() {
                        parts.push(cells.join(" · "));
                    }
                }
            }
            Block::PageBreak => {}
        }
    }
    parts.join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_instructions() {
        assert_eq!(
            hyperlink_from_instr(r#" HYPERLINK "https://example.com/a" \o "tip" "#).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(hyperlink_from_instr(r#"HYPERLINK \l "_Toc1""#), None);
        assert_eq!(hyperlink_from_instr("PAGEREF _Toc1 \\h"), None);
        assert_eq!(
            hyperlink_from_instr(r#"HYPERLINK "javascript:alert(1)""#),
            None
        );
    }

    #[test]
    fn monospace_fonts() {
        assert!(is_monospace("Courier New"));
        assert!(is_monospace("Consolas"));
        assert!(!is_monospace("Calibri"));
    }
}
