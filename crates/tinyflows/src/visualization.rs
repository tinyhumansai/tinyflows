//! Lightweight, self-contained workflow graph rendering.
//!
//! This module is behind the `graph-debug` feature because it writes files and
//! pulls in raster encoders that the workflow engine itself does not need.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use image::{ImageError, ImageFormat, Rgb, RgbImage};

use crate::model::{Edge, NodeKind, WorkflowGraph};

const MARGIN: i32 = 48;
const NODE_WIDTH: i32 = 240;
const NODE_HEIGHT: i32 = 84;
const COLUMN_GAP: i32 = 112;
const ROW_GAP: i32 = 64;
const TEXT_SCALE: i32 = 2;

const WHITE: Rgb<u8> = Rgb([250, 251, 253]);
const INK: Rgb<u8> = Rgb([29, 37, 51]);
const MUTED: Rgb<u8> = Rgb([92, 103, 122]);
const BORDER: Rgb<u8> = Rgb([62, 74, 94]);
const DANGLING: Rgb<u8> = Rgb([254, 226, 226]);

/// Errors returned while rendering a workflow graph image.
#[derive(Debug)]
pub enum GraphRenderError {
    /// The output name does not end in `.png`, `.jpg`, or `.jpeg`.
    UnsupportedExtension(PathBuf),
    /// The encoded image could not be written.
    Image(ImageError),
}

impl fmt::Display for GraphRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedExtension(path) => write!(
                formatter,
                "unsupported graph image extension for {}; use .png, .jpg, or .jpeg",
                path.display()
            ),
            Self::Image(error) => write!(formatter, "could not write graph image: {error}"),
        }
    }
}

impl std::error::Error for GraphRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnsupportedExtension(_) => None,
            Self::Image(error) => Some(error),
        }
    }
}

impl From<ImageError> for GraphRenderError {
    fn from(error: ImageError) -> Self {
        Self::Image(error)
    }
}

#[derive(Clone)]
struct VisualNode {
    id: String,
    name: String,
    kind: Option<NodeKind>,
    dangling: bool,
}

/// Renders `graph` to a PNG or JPEG selected from `path`'s extension.
///
/// Nodes are arranged from left to right by flow distance. Edges carry arrows
/// and `from_port -> to_port` labels; common branch ports use distinct colors.
/// Edges that refer to missing nodes are retained and point to red placeholder
/// nodes, which makes malformed graphs useful to inspect too.
///
/// Parent directories are not created automatically. Existing files are
/// replaced by the image encoder.
///
/// # Examples
///
/// ```no_run
/// use tinyflows::model::WorkflowGraph;
/// use tinyflows::visualization::render_graph;
///
/// let graph = WorkflowGraph::default();
/// render_graph(&graph, "workflow.png")?;
/// # Ok::<(), tinyflows::visualization::GraphRenderError>(())
/// ```
pub fn render_graph(graph: &WorkflowGraph, path: impl AsRef<Path>) -> Result<(), GraphRenderError> {
    let path = path.as_ref();
    let format = image_format(path)?;
    let nodes = visual_nodes(graph);
    let layers = assign_layers(&nodes, &graph.edges);
    let (positions, width, height) = place_nodes(&nodes, &layers);
    let mut image = RgbImage::from_pixel(width, height, WHITE);

    let title = if graph.name.trim().is_empty() {
        "Workflow graph"
    } else {
        graph.name.trim()
    };
    draw_text(&mut image, MARGIN, 16, &truncate(title, 34), 2, INK);

    for edge in &graph.edges {
        draw_edge(&mut image, edge, &positions);
    }
    for node in &nodes {
        if let Some(&(x, y)) = positions.get(node.id.as_str()) {
            draw_node(&mut image, node, x, y);
        }
    }

    image.save_with_format(path, format)?;
    Ok(())
}

fn image_format(path: &Path) -> Result<ImageFormat, GraphRenderError> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok(ImageFormat::Png),
        Some("jpg" | "jpeg") => Ok(ImageFormat::Jpeg),
        _ => Err(GraphRenderError::UnsupportedExtension(path.to_path_buf())),
    }
}

fn visual_nodes(graph: &WorkflowGraph) -> Vec<VisualNode> {
    let mut nodes = graph
        .nodes
        .iter()
        .map(|node| VisualNode {
            id: node.id.clone(),
            name: node.name.clone(),
            kind: Some(node.kind.clone()),
            dangling: false,
        })
        .collect::<Vec<_>>();
    let mut known = graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();

    for id in graph
        .edges
        .iter()
        .flat_map(|edge| [edge.from_node.as_str(), edge.to_node.as_str()])
    {
        if known.insert(id) {
            nodes.push(VisualNode {
                id: id.to_owned(),
                name: format!("missing: {id}"),
                kind: None,
                dangling: true,
            });
        }
    }
    nodes
}

fn assign_layers(nodes: &[VisualNode], edges: &[Edge]) -> HashMap<String, usize> {
    let ids = nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let mut incoming = nodes
        .iter()
        .map(|node| (node.id.as_str(), 0usize))
        .collect::<HashMap<_, _>>();
    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges {
        if ids.contains(edge.from_node.as_str()) && ids.contains(edge.to_node.as_str()) {
            *incoming.entry(edge.to_node.as_str()).or_default() += 1;
            outgoing
                .entry(edge.from_node.as_str())
                .or_default()
                .push(edge.to_node.as_str());
        }
    }

    let mut starts = nodes
        .iter()
        .filter(|node| {
            node.kind == Some(NodeKind::Trigger)
                || incoming.get(node.id.as_str()).copied().unwrap_or_default() == 0
        })
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    if starts.is_empty() && !nodes.is_empty() {
        starts.push(nodes[0].id.as_str());
    }

    let mut layers = HashMap::new();
    let mut queue = VecDeque::new();
    for start in starts {
        if layers.insert(start.to_owned(), 0).is_none() {
            queue.push_back(start);
        }
    }
    while let Some(id) = queue.pop_front() {
        let next_layer = layers[id] + 1;
        for &next in outgoing.get(id).into_iter().flatten() {
            if !layers.contains_key(next) {
                layers.insert(next.to_owned(), next_layer);
                queue.push_back(next);
            }
        }
    }

    let fallback_layer =
        layers.values().copied().max().unwrap_or(0) + usize::from(!layers.is_empty());
    for node in nodes {
        layers.entry(node.id.clone()).or_insert(fallback_layer);
    }
    layers
}

fn place_nodes(
    nodes: &[VisualNode],
    layers: &HashMap<String, usize>,
) -> (HashMap<String, (i32, i32)>, u32, u32) {
    if nodes.is_empty() {
        return (HashMap::new(), 640, 240);
    }
    let max_layer = layers.values().copied().max().unwrap_or(0);
    let mut rows = vec![0usize; max_layer + 1];
    let mut positions = HashMap::new();
    for node in nodes {
        let layer = layers[node.id.as_str()];
        let row = rows[layer];
        rows[layer] += 1;
        positions.insert(
            node.id.clone(),
            (
                MARGIN + layer as i32 * (NODE_WIDTH + COLUMN_GAP),
                MARGIN + row as i32 * (NODE_HEIGHT + ROW_GAP),
            ),
        );
    }
    let width = (MARGIN * 2 + NODE_WIDTH + max_layer as i32 * (NODE_WIDTH + COLUMN_GAP)) as u32;
    let max_rows = rows.into_iter().max().unwrap_or(1).max(1);
    let height =
        (MARGIN * 2 + NODE_HEIGHT + (max_rows - 1) as i32 * (NODE_HEIGHT + ROW_GAP)) as u32;
    (positions, width.max(640), height.max(240))
}

fn draw_edge(image: &mut RgbImage, edge: &Edge, positions: &HashMap<String, (i32, i32)>) {
    let (Some(&(from_x, from_y)), Some(&(to_x, to_y))) = (
        positions.get(edge.from_node.as_str()),
        positions.get(edge.to_node.as_str()),
    ) else {
        return;
    };
    let start = (from_x + NODE_WIDTH, from_y + NODE_HEIGHT / 2);
    let end = (to_x, to_y + NODE_HEIGHT / 2);
    let color = port_color(&edge.from_port);
    let bend_x = if end.0 > start.0 {
        (start.0 + end.0) / 2
    } else {
        start.0 + 32
    };
    draw_line(image, start.0, start.1, bend_x, start.1, color);
    draw_line(image, bend_x, start.1, bend_x, end.1, color);
    draw_line(image, bend_x, end.1, end.0, end.1, color);
    draw_line(image, end.0, end.1, end.0 - 9, end.1 - 6, color);
    draw_line(image, end.0, end.1, end.0 - 9, end.1 + 6, color);

    let label = truncate(&format!("{} -> {}", edge.from_port, edge.to_port), 18);
    let label_width = text_width(&label, 1) + 8;
    let label_x = bend_x - label_width / 2;
    let label_y = (start.1.min(end.1) + (start.1 - end.1).abs() / 2) - 7;
    fill_rect(image, label_x, label_y, label_width, 15, WHITE);
    draw_text(image, label_x + 4, label_y + 4, &label, 1, color);
}

fn draw_node(image: &mut RgbImage, node: &VisualNode, x: i32, y: i32) {
    let fill = if node.dangling {
        DANGLING
    } else {
        node_fill(node.kind.as_ref())
    };
    fill_rect(image, x, y, NODE_WIDTH, NODE_HEIGHT, fill);
    stroke_rect(image, x, y, NODE_WIDTH, NODE_HEIGHT, BORDER);
    fill_rect(image, x, y, 8, NODE_HEIGHT, node_accent(node.kind.as_ref()));
    draw_text(
        image,
        x + 20,
        y + 16,
        &truncate(&node.name, 25),
        TEXT_SCALE,
        INK,
    );
    let kind = node
        .kind
        .as_ref()
        .map(kind_name)
        .unwrap_or("dangling reference");
    draw_text(image, x + 20, y + 48, kind, 1, MUTED);
    draw_text(
        image,
        x + NODE_WIDTH - text_width(&truncate(&node.id, 12), 1) - 12,
        y + NODE_HEIGHT - 16,
        &truncate(&node.id, 12),
        1,
        MUTED,
    );
}

fn kind_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Trigger => "trigger",
        NodeKind::Agent => "agent",
        NodeKind::ToolCall => "tool_call",
        NodeKind::HttpRequest => "http_request",
        NodeKind::Code => "code",
        NodeKind::Shell => "shell",
        NodeKind::Condition => "condition",
        NodeKind::Switch => "switch",
        NodeKind::Merge => "merge",
        NodeKind::SplitOut => "split_out",
        NodeKind::Loop => "loop",
        NodeKind::Transform => "transform",
        NodeKind::OutputParser => "output_parser",
        NodeKind::SubWorkflow => "sub_workflow",
        NodeKind::Memory => "memory",
        NodeKind::Dedup => "dedup",
        NodeKind::Spawn => "spawn",
        NodeKind::Scatter => "scatter",
        NodeKind::Gather => "gather",
        NodeKind::Gate => "gate",
        NodeKind::Approval => "approval",
        NodeKind::Void => "void",
    }
}

fn node_fill(kind: Option<&NodeKind>) -> Rgb<u8> {
    match kind {
        Some(NodeKind::Trigger) => Rgb([224, 242, 254]),
        Some(NodeKind::Condition | NodeKind::Switch | NodeKind::Loop) => Rgb([254, 249, 195]),
        Some(NodeKind::Merge | NodeKind::SplitOut) => Rgb([237, 233, 254]),
        Some(NodeKind::Agent | NodeKind::ToolCall | NodeKind::HttpRequest | NodeKind::Memory) => {
            Rgb([220, 252, 231])
        }
        // A human review reads as a decision point, not as work the machine
        // does, so it takes the branching palette rather than the capability one.
        Some(NodeKind::Approval) => Rgb([254, 226, 226]),
        _ => Rgb([241, 245, 249]),
    }
}

fn node_accent(kind: Option<&NodeKind>) -> Rgb<u8> {
    match kind {
        Some(NodeKind::Trigger) => Rgb([2, 132, 199]),
        Some(NodeKind::Condition | NodeKind::Switch | NodeKind::Loop) => Rgb([202, 138, 4]),
        Some(NodeKind::Merge | NodeKind::SplitOut) => Rgb([124, 58, 237]),
        Some(NodeKind::Agent | NodeKind::ToolCall | NodeKind::HttpRequest | NodeKind::Memory) => {
            Rgb([22, 163, 74])
        }
        Some(NodeKind::Approval) => Rgb([220, 38, 38]),
        None => Rgb([220, 38, 38]),
        _ => Rgb([71, 85, 105]),
    }
}

fn port_color(port: &str) -> Rgb<u8> {
    match port {
        "true" => Rgb([22, 163, 74]),
        "false" => Rgb([217, 119, 6]),
        "error" => Rgb([220, 38, 38]),
        "body" => Rgb([124, 58, 237]),
        "done" => Rgb([13, 148, 136]),
        _ => Rgb([37, 99, 235]),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!(
            "{}...",
            prefix
                .chars()
                .take(max_chars.saturating_sub(3))
                .collect::<String>()
        )
    } else {
        prefix
    }
}

fn text_width(text: &str, scale: i32) -> i32 {
    text.chars().count() as i32 * 8 * scale
}

fn draw_text(image: &mut RgbImage, x: i32, y: i32, text: &str, scale: i32, color: Rgb<u8>) {
    for (index, character) in text.chars().enumerate() {
        let glyph = BASIC_FONTS.get(character).or_else(|| BASIC_FONTS.get('?'));
        let Some(glyph) = glyph else { continue };
        for (row, bits) in glyph.into_iter().enumerate() {
            for column in 0..8 {
                if bits & (1 << column) != 0 {
                    fill_rect(
                        image,
                        x + (index as i32 * 8 + column) * scale,
                        y + row as i32 * scale,
                        scale,
                        scale,
                        color,
                    );
                }
            }
        }
    }
}

fn fill_rect(image: &mut RgbImage, x: i32, y: i32, width: i32, height: i32, color: Rgb<u8>) {
    for pixel_y in y.max(0)..(y + height).min(image.height() as i32) {
        for pixel_x in x.max(0)..(x + width).min(image.width() as i32) {
            image.put_pixel(pixel_x as u32, pixel_y as u32, color);
        }
    }
}

fn stroke_rect(image: &mut RgbImage, x: i32, y: i32, width: i32, height: i32, color: Rgb<u8>) {
    draw_line(image, x, y, x + width, y, color);
    draw_line(image, x + width, y, x + width, y + height, color);
    draw_line(image, x + width, y + height, x, y + height, color);
    draw_line(image, x, y + height, x, y, color);
}

fn draw_line(image: &mut RgbImage, mut x0: i32, mut y0: i32, x1: i32, y1: i32, color: Rgb<u8>) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 && x0 < image.width() as i32 && y0 < image.height() as i32 {
            image.put_pixel(x0 as u32, y0 as u32, color);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let doubled = 2 * error;
        if doubled >= dy {
            error += dy;
            x0 += sx;
        }
        if doubled <= dx {
            error += dx;
            y0 += sy;
        }
    }
}

#[cfg(test)]
#[path = "visualization_tests.rs"]
mod tests;
