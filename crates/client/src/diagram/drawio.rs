//! Diagrams as draw.io files: an uncompressed `mxfile` holding one `mxGraphModel`.
//! Exporting writes the shapes, colours and connectors draw.io knows; importing
//! reads them back, and turns shapes it doesn't know into rectangles.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use rusttalk_core::diagram::{
    COLORS, Doc, Edge, Item, Kind, MAX_COORD, MIN_SIZE, Op, Shape, new_id,
};

use crate::ui::DIAGRAM;

/// Bigger files aren't read.
pub const MAX_FILE: u64 = 4 * 1024 * 1024;

fn hex(color: eframe::egui::Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r(), color.g(), color.b())
}

/// Text for an XML attribute.
fn attribute(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' => out.push_str("&#10;"),
            c => out.push(c),
        }
    }
    out
}

/// A shape's text as draw.io's HTML labels hold it.
fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\n' => out.push_str("<br>"),
            c => out.push(c),
        }
    }
    out
}

fn style(shape: &Shape) -> String {
    let (fill, stroke) = DIAGRAM[usize::from(shape.color)];
    let base = match shape.kind {
        Kind::Rect => "rounded=0;whiteSpace=wrap;html=1;",
        Kind::Rounded => "rounded=1;whiteSpace=wrap;html=1;",
        Kind::Ellipse => "ellipse;whiteSpace=wrap;html=1;",
        Kind::Diamond => "rhombus;whiteSpace=wrap;html=1;",
        Kind::Text => {
            return format!(
                "text;html=1;align=center;verticalAlign=middle;whiteSpace=wrap;fontColor={};",
                hex(if shape.color == 0 {
                    stroke
                } else {
                    DIAGRAM[usize::from(shape.color)].1
                })
            );
        }
    };
    format!("{base}fillColor={};strokeColor={};", hex(fill), hex(stroke))
}

/// The diagram as a draw.io file.
pub fn export(doc: &Doc) -> String {
    let mut cells = String::new();
    for shape in &doc.shapes {
        cells.push_str(&format!(
            "<mxCell id=\"s{}\" value=\"{}\" style=\"{}\" vertex=\"1\" parent=\"1\">\
             <mxGeometry x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" as=\"geometry\"/></mxCell>\n",
            shape.id,
            attribute(&html(&shape.text)),
            style(shape),
            shape.x,
            shape.y,
            shape.w,
            shape.h,
        ));
    }
    for edge in &doc.edges {
        cells.push_str(&format!(
            "<mxCell id=\"e{}\" style=\"endArrow=classic;html=1;rounded=0;strokeColor={};\" \
             edge=\"1\" parent=\"1\" source=\"s{}\" target=\"s{}\">\
             <mxGeometry relative=\"1\" as=\"geometry\"/></mxCell>\n",
            edge.id,
            hex(DIAGRAM[usize::from(edge.color)].1),
            edge.from,
            edge.to,
        ));
    }
    format!(
        "<mxfile host=\"RustTalk\">\n<diagram id=\"rusttalk\" name=\"Page-1\">\n\
         <mxGraphModel><root>\n<mxCell id=\"0\"/>\n<mxCell id=\"1\" parent=\"0\"/>\n\
         {cells}</root></mxGraphModel>\n</diagram>\n</mxfile>\n"
    )
}

/// `key=value` pairs and bare names (like `ellipse`) of a draw.io style.
fn parse_style(style: &str) -> (Vec<&str>, HashMap<&str, &str>) {
    let mut names = Vec::new();
    let mut values = HashMap::new();
    for part in style.split(';').filter(|part| !part.is_empty()) {
        match part.split_once('=') {
            Some((key, value)) => {
                values.insert(key, value);
            }
            None => names.push(part),
        }
    }
    (names, values)
}

fn kind_of(names: &[&str], values: &HashMap<&str, &str>) -> Kind {
    let shape = values.get("shape").copied();
    if names.contains(&"text") || shape == Some("text") {
        Kind::Text
    } else if names.contains(&"ellipse") || shape == Some("ellipse") {
        Kind::Ellipse
    } else if names.contains(&"rhombus") || shape == Some("rhombus") {
        Kind::Diamond
    } else if values.get("rounded") == Some(&"1") {
        Kind::Rounded
    } else {
        Kind::Rect
    }
}

/// The palette colour nearest to a `#rrggbb` fill (or text colour).
fn nearest(color: Option<&str>, text: bool) -> u8 {
    let Some(rgb) = color
        .and_then(|color| color.strip_prefix('#'))
        .filter(|hex| hex.len() == 6)
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
    else {
        return 0;
    };
    let channels = [
        (rgb >> 16) as i32 & 0xff,
        (rgb >> 8) as i32 & 0xff,
        rgb as i32 & 0xff,
    ];
    (0..COLORS)
        .min_by_key(|color| {
            let (fill, stroke) = DIAGRAM[usize::from(*color)];
            let target = if text { stroke } else { fill };
            let [r, g, b] = channels;
            (r - i32::from(target.r())).pow(2)
                + (g - i32::from(target.g())).pow(2)
                + (b - i32::from(target.b())).pow(2)
        })
        .unwrap_or(0)
}

/// Plain text from a draw.io label: line breaks for `<br>` and blocks, other tags
/// dropped, entities read.
fn plain(value: &str, is_html: bool) -> String {
    if !is_html {
        return value.to_owned();
    }
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        let name = tag.trim_start_matches('/');
        let name = name
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        if name == "br" || (tag.starts_with('/') && matches!(name, "div" | "p")) {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim_end_matches('\n')
        .to_owned()
}

fn geometry<'a, 'i>(cell: &roxmltree::Node<'a, 'i>) -> Option<roxmltree::Node<'a, 'i>> {
    cell.children().find(|node| node.has_tag_name("mxGeometry"))
}

fn number(node: roxmltree::Node, name: &str) -> f32 {
    node.attribute(name)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

/// A diagram from a draw.io file. Only its first page; shapes inside groups or
/// containers keep their place.
pub fn import(text: &str) -> Result<Doc> {
    let xml = roxmltree::Document::parse(text).context("not an XML file")?;
    let root = xml.root_element();
    let model = match root.tag_name().name() {
        "mxGraphModel" => Some(root),
        "mxfile" => (root.children())
            .find(|node| node.has_tag_name("diagram"))
            .and_then(|diagram| {
                diagram
                    .children()
                    .find(|node| node.has_tag_name("mxGraphModel"))
            }),
        _ => bail!("not a draw.io file"),
    };
    let Some(model) = model else {
        bail!("compressed or empty draw.io file");
    };
    let cells: Vec<_> = model
        .descendants()
        .filter(|node| node.has_tag_name("mxCell"))
        .collect();
    let by_id: HashMap<&str, roxmltree::Node> = (cells.iter())
        .filter_map(|cell| Some((cell.attribute("id")?, *cell)))
        .collect();
    // Where a cell's parents put it: draw.io keeps children relative to them.
    let offset = |cell: &roxmltree::Node| {
        let (mut x, mut y) = (0.0, 0.0);
        let mut parent = cell.attribute("parent");
        for _ in 0..32 {
            let Some(node) = parent.and_then(|id| by_id.get(id)) else {
                break;
            };
            if node.attribute("vertex") == Some("1")
                && let Some(geometry) = geometry(node)
            {
                x += number(geometry, "x");
                y += number(geometry, "y");
            }
            parent = node.attribute("parent");
        }
        (x, y)
    };
    let mut doc = Doc::default();
    let mut ids: HashMap<&str, u64> = HashMap::new();
    for cell in cells
        .iter()
        .filter(|cell| cell.attribute("vertex") == Some("1"))
    {
        let Some(geometry) = geometry(cell) else {
            continue;
        };
        let (names, values) = parse_style(cell.attribute("style").unwrap_or(""));
        // Containers and groups only hold other shapes.
        if names.contains(&"group") {
            continue;
        }
        let kind = kind_of(&names, &values);
        let (dx, dy) = offset(cell);
        let clamp = |value: f32| value.clamp(-MAX_COORD, MAX_COORD);
        let size = |value: f32| value.clamp(MIN_SIZE, MAX_COORD);
        let label = plain(
            cell.attribute("value").unwrap_or(""),
            values.get("html") == Some(&"1"),
        );
        let color = match kind {
            Kind::Text => nearest(values.get("fontColor").copied(), true),
            _ => nearest(values.get("fillColor").copied(), false),
        };
        let shape = Shape {
            id: new_id(),
            kind,
            x: clamp(number(geometry, "x") + dx),
            y: clamp(number(geometry, "y") + dy),
            w: size(number(geometry, "width")),
            h: size(number(geometry, "height")),
            text: super::clean_text(&label),
            color,
        };
        if let Some(id) = cell.attribute("id") {
            ids.insert(id, shape.id);
        }
        doc.apply(&Op::Put {
            item: Item::Shape(shape),
        });
    }
    for cell in cells
        .iter()
        .filter(|cell| cell.attribute("edge") == Some("1"))
    {
        let end = |name| cell.attribute(name).and_then(|id| ids.get(id)).copied();
        let (Some(from), Some(to)) = (end("source"), end("target")) else {
            continue;
        };
        let (_, values) = parse_style(cell.attribute("style").unwrap_or(""));
        let edge = Edge {
            id: new_id(),
            from,
            to,
            color: nearest(values.get("strokeColor").copied(), true),
        };
        doc.apply(&Op::Put {
            item: Item::Edge(edge),
        });
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Doc {
        let mut doc = Doc::default();
        for (id, kind, color, text) in [
            (1, Kind::Rect, 0, "Start"),
            (2, Kind::Rounded, 1, "Step <one> & \"two\""),
            (3, Kind::Ellipse, 2, "Two\nlines"),
            (4, Kind::Diamond, 5, "Ready?"),
            (5, Kind::Text, 6, "A note"),
        ] {
            doc.apply(&Op::Put {
                item: Item::Shape(Shape {
                    id,
                    kind,
                    x: id as f32 * 150.0,
                    y: -20.0,
                    w: 120.0,
                    h: 60.0,
                    text: text.into(),
                    color,
                }),
            });
        }
        for (id, from, to, color) in [(10, 1, 2, 0), (11, 2, 4, 3)] {
            doc.apply(&Op::Put {
                item: Item::Edge(Edge {
                    id,
                    from,
                    to,
                    color,
                }),
            });
        }
        doc
    }

    /// The same diagram, with the IDs a fresh import gives it set aside.
    fn shape_of(doc: &Doc) -> Vec<(Kind, f32, f32, f32, f32, String, u8)> {
        (doc.shapes.iter())
            .map(|s| (s.kind, s.x, s.y, s.w, s.h, s.text.clone(), s.color))
            .collect()
    }

    #[test]
    fn export_and_import_round_trip() {
        let doc = sample();
        let file = export(&doc);
        assert!(file.starts_with("<mxfile"));
        let back = import(&file).unwrap();
        assert_eq!(shape_of(&back), shape_of(&doc));
        let index = |doc: &Doc, id| doc.shapes.iter().position(|s| s.id == id).unwrap();
        let links = |doc: &Doc| -> Vec<(usize, usize, u8)> {
            (doc.edges.iter())
                .map(|e| (index(doc, e.from), index(doc, e.to), e.color))
                .collect()
        };
        assert_eq!(links(&back), links(&doc));
    }

    #[test]
    fn imports_what_draw_io_writes() {
        // Saved by draw.io: a group with a child, HTML labels, an image it can't
        // show and a loose edge.
        let file = r##"<mxfile host="app.diagrams.net"><diagram id="x" name="Page-1">
          <mxGraphModel dx="1000" dy="600" grid="1"><root>
            <mxCell id="0"/><mxCell id="1" parent="0"/>
            <mxCell id="g" value="" style="group" vertex="1" connectable="0" parent="1">
              <mxGeometry x="100" y="50" width="200" height="100" as="geometry"/></mxCell>
            <mxCell id="a" value="&lt;b&gt;Hi&lt;/b&gt;&lt;br&gt;there &amp;amp; you" style="ellipse;whiteSpace=wrap;html=1;fillColor=#d5e8d4;strokeColor=#82b366;" vertex="1" parent="g">
              <mxGeometry x="10" y="20" width="80" height="40" as="geometry"/></mxCell>
            <mxCell id="b" value="pic" style="shape=image;image=data:x" vertex="1" parent="1">
              <mxGeometry x="400" y="0" width="50" height="50" as="geometry"/></mxCell>
            <mxCell id="c" style="edgeStyle=orthogonalEdgeStyle;" edge="1" parent="1" source="a" target="b">
              <mxGeometry relative="1" as="geometry"/></mxCell>
            <mxCell id="d" edge="1" parent="1" source="a">
              <mxGeometry relative="1" as="geometry"/></mxCell>
          </root></mxGraphModel></diagram></mxfile>"##;
        let doc = import(file).unwrap();
        assert_eq!(doc.shapes.len(), 2);
        let a = &doc.shapes[0];
        assert_eq!((a.kind, a.x, a.y, a.color), (Kind::Ellipse, 110.0, 70.0, 2));
        assert_eq!(a.text, "Hi\nthere & you");
        assert_eq!(doc.shapes[1].kind, Kind::Rect);
        assert_eq!(doc.edges.len(), 1);
    }

    #[test]
    fn refuses_what_is_not_a_diagram() {
        assert!(import("not xml").is_err());
        assert!(import("<html/>").is_err());
        // Compressed pages hold base64 text instead of a model.
        assert!(import("<mxfile><diagram>7VZNb9swDP01</diagram></mxfile>").is_err());
    }
}
