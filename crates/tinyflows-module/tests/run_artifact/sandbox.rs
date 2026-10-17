use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use tinybus::{broker::Broker, module::ModuleHost, transport::memory::MemoryTransport};
use tinyflows::model::WorkflowGraph;
use tinyflows_bus::runs::{self, CallbackAddress, SandboxRun};
use tinyflows_catalog::{DraftOrigin, Flow, FlowDraft, FlowRevision, FlowRun, FlowValidation};
use tinyflows_copilot::tools::{DryRunWorkflowTool, ports::*};
use tinytools::Tool;

#[derive(Clone)]
struct Preflight(Arc<AtomicUsize>);

#[tinybus::interface(name = "ai.tinyhumans.tinyflows.CapsHost")]
impl Preflight {
    async fn tool_invoke(
        &self,
        slug: String,
        args: serde_json::Value,
        _connection: Option<String>,
    ) -> tinybus::Result<serde_json::Value> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if args.get("text").is_none_or(serde_json::Value::is_null) {
            return Err(tinybus::Error::failed(
                "required arg 'text' is missing or null",
            ));
        }
        Ok(json!({"host_canary": true, "slug": slug, "args": args}))
    }
}

fn agent_graph(named: bool) -> serde_json::Value {
    let mut config = json!({
        "prompt": "Write a subject",
        "output_parser": {"schema": {"type":"object","properties":{"subject":{"type":"string"}}}}
    });
    if named {
        config["agent_ref"] = json!("named-agent");
    }
    json!({
        "nodes":[
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"agent","name":"Agent","kind":"agent","config":config}
        ],
        "edges":[{"from_node":"trigger","to_node":"agent"}]
    })
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn compiled_module_sandbox_uses_schema_mocks_and_only_the_preflight_callback() {
    assert_eq!(runs::CONTRACT_VERSION, 5);
    assert!(runs::METHODS.contains(&runs::methods::SANDBOX_RUN));
    let artifact = std::env::var_os("TINYFLOWS_MODULE_ARTIFACT")
        .expect("set TINYFLOWS_MODULE_ARTIFACT to the built cdylib");
    let broker = Broker::new();
    let module_host = ModuleHost::new(broker.clone());
    module_host
        .load_file(std::path::PathBuf::from(artifact))
        .unwrap();
    let (client, transport) = MemoryTransport::pair();
    broker.attach(Arc::new(transport));
    let connection = Connection::connect(Box::new(client)).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    connection
        .serve_at(
            "/test/SandboxPreflight".try_into().unwrap(),
            Preflight(calls.clone()),
        )
        .await
        .unwrap();
    connection
        .request_name("test.SandboxPreflight")
        .await
        .unwrap();
    let proxy = connection
        .proxy(tinyflows_bus::BUS_NAME, runs::OBJECT_PATH, runs::INTERFACE)
        .unwrap();
    let callback = CallbackAddress {
        destination: "test.SandboxPreflight".into(),
        object_path: "/test/SandboxPreflight".into(),
    };

    for named in [false, true] {
        let response: runs::SandboxRunResponse = proxy
            .call(
                runs::methods::SANDBOX_RUN,
                (SandboxRun {
                    graph: agent_graph(named),
                    input: json!({}),
                    tool_invoker: callback.clone(),
                    permissions_manifest: vec![json!({"node_id":"approval","permission":"write"})],
                },),
            )
            .await
            .unwrap();
        assert_eq!(response.result["sandbox"], true);
        assert_eq!(response.result["ok"], true, "{response:?}");
        assert_eq!(
            response.result["permissions_manifest"][0]["node_id"],
            "approval"
        );
        assert_eq!(
            response.result["output"]["nodes"]["agent"]["items"][0]["json"]["json"]["subject"], "",
            "{response:?}"
        );
    }

    let tool_graph = json!({
        "nodes":[
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"echo","name":"Echo","kind":"tool_call","config":{"slug":"demo:echo","args":{"text":"hi"}}}
        ],
        "edges":[{"from_node":"trigger","to_node":"echo"}]
    });
    let response: runs::SandboxRunResponse = proxy
        .call(
            runs::methods::SANDBOX_RUN,
            (SandboxRun {
                graph: tool_graph,
                input: json!({}),
                tool_invoker: callback,
                permissions_manifest: vec![],
            },),
        )
        .await
        .unwrap();
    assert_eq!(response.result["ok"], true, "{response:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "agent paths are mock-only");
    let echo = &response.result["output"]["nodes"]["echo"]["items"][0]["json"]["json"];
    assert_eq!(echo["tool"], "demo:echo");
    assert_eq!(echo["args"]["text"], "hi");
    assert_eq!(echo["connection"], serde_json::Value::Null);
    assert_eq!(echo.get("host_canary"), None);

    let null_arg_graph = json!({
        "nodes":[
            {"id":"trigger","name":"Trigger","kind":"trigger","config":{}},
            {"id":"shape","name":"Shape","kind":"transform","config":{"set":{"text":{}}}},
            {"id":"echo","name":"Echo","kind":"tool_call","config":{"slug":"demo:echo","args":{"text":"=nodes.shape.item.text.missing"}}}
        ],
        "edges":[{"from_node":"trigger","to_node":"shape"},{"from_node":"shape","to_node":"echo"}]
    });
    let response: runs::SandboxRunResponse = proxy
        .call(
            runs::methods::SANDBOX_RUN,
            (SandboxRun {
                graph: null_arg_graph,
                input: json!({}),
                tool_invoker: CallbackAddress {
                    destination: "test.SandboxPreflight".into(),
                    object_path: "/test/SandboxPreflight".into(),
                },
                permissions_manifest: vec![],
            },),
        )
        .await
        .unwrap();
    assert_eq!(response.result["sandbox"], true);
    assert_eq!(response.result["ok"], false, "{response:?}");
    assert!(response.result["error"].as_str().unwrap().contains("text"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

struct ToolHost {
    sandbox: BusSandboxRunner,
}

#[async_trait::async_trait]
impl DraftStore for ToolHost {
    async fn get(&self, _: &str) -> PortResult<FlowDraft> {
        Err("no draft".into())
    }
    async fn create(
        &self,
        _: Option<String>,
        _: String,
        _: serde_json::Value,
        _: DraftOrigin,
    ) -> PortResult<FlowDraft> {
        Err("unused".into())
    }
    async fn update(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<serde_json::Value>,
    ) -> PortResult<FlowDraft> {
        Err("unused".into())
    }
}

#[async_trait::async_trait]
impl FlowStore for ToolHost {
    async fn get(&self, _: &str) -> PortResult<Flow> {
        Err("unused".into())
    }
    async fn list(&self) -> PortResult<Vec<Flow>> {
        Ok(vec![])
    }
    async fn load_graph(&self, _: &str) -> PortResult<Option<WorkflowGraph>> {
        Ok(None)
    }
    async fn create(&self, _: String, _: serde_json::Value, _: bool) -> PortResult<Flow> {
        Err("unused".into())
    }
    async fn update(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<serde_json::Value>,
    ) -> PortResult<Written<Flow>> {
        Err("unused".into())
    }
    async fn set_enabled(&self, _: &str, _: bool) -> PortResult<Flow> {
        Err("unused".into())
    }
    async fn duplicate(&self, _: &str) -> PortResult<Flow> {
        Err("unused".into())
    }
    async fn history(&self, _: &str, _: usize) -> PortResult<Vec<FlowRevision>> {
        Ok(vec![])
    }
    async fn list_runs(&self, _: &str, _: usize) -> PortResult<Vec<FlowRun>> {
        Ok(vec![])
    }
    async fn get_run(&self, _: &str) -> PortResult<FlowRun> {
        Err("unused".into())
    }
    async fn resume(
        &self,
        _: &str,
        _: &str,
        _: Vec<String>,
        _: Vec<String>,
    ) -> PortResult<serde_json::Value> {
        Err("unused".into())
    }
    async fn cancel_run(&self, _: &str) -> PortResult<serde_json::Value> {
        Err("unused".into())
    }
}

#[async_trait::async_trait]
impl BuilderGates for ToolHost {
    async fn validate(&self, _: serde_json::Value) -> FlowValidation {
        FlowValidation::default()
    }
    async fn run_gates(&self, _: &WorkflowGraph) -> Vec<String> {
        vec![]
    }
    async fn strict_gate(&self, _: &serde_json::Value) -> PortResult<()> {
        Ok(())
    }
    async fn engine_compatibility_errors(&self, _: &WorkflowGraph) -> Vec<String> {
        vec![]
    }
    async fn wiring_warnings(&self, _: &WorkflowGraph) -> Vec<String> {
        vec![]
    }
    async fn approval_manifest(&self, _: &WorkflowGraph) -> Vec<serde_json::Value> {
        vec![json!({"node_id":"send","permission":"write"})]
    }
    async fn build_proposal(
        &self,
        _: &str,
        _: &str,
        _: &WorkflowGraph,
        _: bool,
        _: bool,
        _: Option<String>,
        _: Option<String>,
        _: Option<String>,
    ) -> Result<serde_json::Value, String> {
        Ok(json!({}))
    }
}

struct BusSandboxRunner {
    connection: Connection,
    callback: CallbackAddress,
}

#[async_trait::async_trait]
impl SandboxRunner for BusSandboxRunner {
    async fn run(&self, request: SandboxRunRequest) -> PortResult<serde_json::Value> {
        let proxy = self
            .connection
            .proxy(tinyflows_bus::BUS_NAME, runs::OBJECT_PATH, runs::INTERFACE)
            .map_err(|error| error.to_string())?;
        let response: runs::SandboxRunResponse = proxy
            .call(
                runs::methods::SANDBOX_RUN,
                (SandboxRun {
                    graph: request.graph,
                    input: request.input,
                    tool_invoker: self.callback.clone(),
                    permissions_manifest: request.permissions_manifest,
                },),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(response.result)
    }
}

#[async_trait::async_trait]
impl SandboxRunner for ToolHost {
    async fn run(&self, request: SandboxRunRequest) -> PortResult<serde_json::Value> {
        self.sandbox.run(request).await
    }
}

#[tokio::test]
#[ignore = "run through scripts/verify-module-artifact.sh"]
async fn advertised_dry_run_tool_executes_through_the_compiled_module_bus_port() {
    let artifact = std::env::var_os("TINYFLOWS_MODULE_ARTIFACT")
        .expect("set TINYFLOWS_MODULE_ARTIFACT to the built cdylib");
    let broker = Broker::new();
    let module_host = ModuleHost::new(broker.clone());
    module_host
        .load_file(std::path::PathBuf::from(artifact))
        .unwrap();
    let (client, transport) = MemoryTransport::pair();
    broker.attach(Arc::new(transport));
    let connection = Connection::connect(Box::new(client)).await.unwrap();
    let callback = CallbackAddress {
        destination: "test.SandboxPreflight".into(),
        object_path: "/test/SandboxPreflight".into(),
    };
    connection
        .serve_at(
            "/test/SandboxPreflight".try_into().unwrap(),
            Preflight(Arc::new(AtomicUsize::new(0))),
        )
        .await
        .unwrap();
    connection
        .request_name("test.SandboxPreflight")
        .await
        .unwrap();

    let host = Arc::new(ToolHost {
        sandbox: BusSandboxRunner {
            connection,
            callback,
        },
    });
    let tool = DryRunWorkflowTool::new(host.clone(), host.clone(), host);
    let result = tool
        .execute(json!({"graph":agent_graph(false),"input":{}}))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.output());
    let report: serde_json::Value = serde_json::from_str(&result.output().to_string()).unwrap();
    assert_eq!(report["sandbox"], true);
    assert_eq!(report["ok"], true, "{report:?}");
    assert_eq!(report["permissions_manifest"][0]["node_id"], "send");
    assert_eq!(
        report["output"]["nodes"]["agent"]["items"][0]["json"]["json"]["subject"],
        ""
    );
}
