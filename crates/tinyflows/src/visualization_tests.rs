use super::*;
use crate::model::{Edge, Node};

fn graph() -> WorkflowGraph {
    WorkflowGraph {
        name: "branching demo".into(),
        nodes: vec![
            Node {
                id: "start".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "Start".into(),
                config: serde_json::Value::Null,
                ports: vec![],
                position: None,
            },
            Node {
                id: "check".into(),
                kind: NodeKind::Condition,
                type_version: 1,
                name: "Check input".into(),
                config: serde_json::Value::Null,
                ports: vec![],
                position: None,
            },
        ],
        edges: vec![
            Edge {
                from_node: "start".into(),
                from_port: "main".into(),
                to_node: "check".into(),
                to_port: "main".into(),
            },
            Edge {
                from_node: "check".into(),
                from_port: "error".into(),
                to_node: "missing".into(),
                to_port: "main".into(),
            },
        ],
        ..WorkflowGraph::default()
    }
}

#[test]
fn renders_png_and_dangling_edge_endpoint() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("graph.PNG");
    render_graph(&graph(), &path).expect("render graph");
    let bytes = std::fs::read(&path).expect("read image");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let image = image::open(path).expect("decode image");
    assert!(image.width() >= 640);
    assert!(image.height() >= 240);
}

#[test]
fn renders_jpeg_from_jpg_or_jpeg_extension() {
    let directory = tempfile::tempdir().expect("temporary directory");
    for extension in ["jpg", "jpeg"] {
        let path = directory.path().join(format!("graph.{extension}"));
        render_graph(&graph(), &path).expect("render graph");
        let bytes = std::fs::read(path).expect("read image");
        assert_eq!(&bytes[..2], b"\xff\xd8");
    }
}

#[test]
fn rejects_an_unknown_output_format_before_writing() {
    let path = Path::new("graph.gif");
    let error = render_graph(&graph(), path).expect_err("unsupported format");
    assert!(matches!(error, GraphRenderError::UnsupportedExtension(_)));
    assert!(error.to_string().contains(".png, .jpg, or .jpeg"));
}

#[test]
fn cycle_layout_terminates_and_places_every_node() {
    let nodes = visual_nodes(&graph());
    let mut edges = graph().edges;
    edges.push(Edge {
        from_node: "missing".into(),
        from_port: "main".into(),
        to_node: "check".into(),
        to_port: "main".into(),
    });
    let layers = assign_layers(&nodes, &edges);
    assert_eq!(layers.len(), nodes.len());
}
