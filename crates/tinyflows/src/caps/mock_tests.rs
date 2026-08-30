use super::*;

#[tokio::test]
async fn mock_tools_echo() {
    let caps = mock_capabilities();
    let out = caps
        .tools
        .invoke("slack.post", json!({"x": 1}), None)
        .await
        .unwrap();
    assert_eq!(out["tool"], "slack.post");
}

#[tokio::test]
async fn mock_llm_echoes_request_and_threads_connection() {
    let llm = MockLlm;
    let with_conn = llm
        .complete(json!({"prompt": "hi"}), Some("conn_1"))
        .await
        .unwrap();
    assert_eq!(with_conn["completion"], json!({"prompt": "hi"}));
    assert_eq!(with_conn["connection"], "conn_1");

    let without_conn = llm.complete(json!({"prompt": "hi"}), None).await.unwrap();
    assert!(without_conn["connection"].is_null());
}

#[tokio::test]
async fn mock_tools_echoes_slug_args_and_connection() {
    let tools = MockTools;
    let with_conn = tools
        .invoke("gmail.send", json!({"to": "a@b.c"}), Some("conn_2"))
        .await
        .unwrap();
    assert_eq!(with_conn["tool"], "gmail.send");
    assert_eq!(with_conn["args"], json!({"to": "a@b.c"}));
    assert_eq!(with_conn["connection"], "conn_2");

    let without_conn = tools.invoke("gmail.send", json!({}), None).await.unwrap();
    assert!(without_conn["connection"].is_null());
}

#[tokio::test]
async fn mock_http_returns_canned_200_and_threads_connection() {
    let http = MockHttp;
    let with_conn = http
        .request(json!({"method": "GET", "url": "https://x"}), Some("conn_3"))
        .await
        .unwrap();
    assert_eq!(with_conn["status"], 200);
    assert_eq!(with_conn["request"]["method"], "GET");
    assert_eq!(with_conn["connection"], "conn_3");

    let without_conn = http.request(json!({"method": "GET"}), None).await.unwrap();
    assert_eq!(without_conn["status"], 200);
    assert!(without_conn["connection"].is_null());
}

#[tokio::test]
async fn mock_code_returns_input_under_result_key() {
    let code = MockCode;
    let js = code
        .run(CodeLanguage::JavaScript, "return 1;", json!({"n": 7}))
        .await
        .unwrap();
    assert_eq!(js["result"], json!({"n": 7}));

    // The language and source are ignored by the echo runner.
    let py = code
        .run(CodeLanguage::Python, "print(1)", json!([1, 2, 3]))
        .await
        .unwrap();
    assert_eq!(py["result"], json!([1, 2, 3]));
}

#[tokio::test]
async fn mock_memory_recall_returns_shaped_results() {
    let memory = MockMemory;
    let out = memory
        .recall("flow", "budget", json!({ "operation": "recall" }))
        .await
        .unwrap();
    assert_eq!(out["scope"], "flow");
    assert_eq!(out["query"], "budget");
    let results = out["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "mock recall should return shaped results"
    );
    assert!(results[0].get("text").is_some());
}

#[tokio::test]
async fn mock_memory_flavour_returns_shaped_object() {
    let memory = MockMemory;
    let out = memory.flavour("email-tone").await.unwrap();
    assert_eq!(out["slug"], "email-tone");
    assert!(out["traits"].is_object());
}

#[tokio::test]
async fn mock_memory_people_returns_shaped_list() {
    let memory = MockMemory;
    let out = memory.people(Some("cyrus")).await.unwrap();
    assert_eq!(out["query"], "cyrus");
    let people = out["people"].as_array().expect("people array");
    assert!(
        !people.is_empty(),
        "mock people should return a shaped list"
    );

    let out_no_query = memory.people(None).await.unwrap();
    assert!(out_no_query["query"].is_null());
}

#[tokio::test]
async fn mock_memory_remember_and_forget_are_ok() {
    let memory = MockMemory;
    memory
        .remember("flow", "k", json!({ "v": 1 }))
        .await
        .unwrap();
    memory.forget("flow", "k").await.unwrap();
}

#[tokio::test]
async fn mock_state_store_round_trips_and_misses() {
    let store = MockStateStore::default();
    assert!(store.load("missing").await.unwrap().is_none());

    store.store("k", json!({"v": 1})).await.unwrap();
    assert_eq!(store.load("k").await.unwrap(), Some(json!({"v": 1})));

    // Storing again overwrites the previous value.
    store.store("k", json!(2)).await.unwrap();
    assert_eq!(store.load("k").await.unwrap(), Some(json!(2)));
}

#[tokio::test]
async fn mock_capabilities_state_store_round_trips_through_bundle() {
    let caps = mock_capabilities();
    // A missing key reads back as `None`.
    assert!(caps.state.load("k").await.unwrap().is_none());

    // A stored value is readable through the same bundle handle.
    caps.state.store("k", json!({"v": 1})).await.unwrap();
    assert_eq!(caps.state.load("k").await.unwrap(), Some(json!({"v": 1})));
}

#[tokio::test]
async fn mock_capabilities_wires_every_slot() {
    let caps = mock_capabilities();
    assert_eq!(
        caps.llm.complete(json!({"p": 1}), None).await.unwrap()["completion"],
        json!({"p": 1})
    );
    assert_eq!(
        caps.tools.invoke("s", json!({}), None).await.unwrap()["tool"],
        "s"
    );
    assert_eq!(
        caps.http.request(json!({}), None).await.unwrap()["status"],
        200
    );
    assert_eq!(
        caps.code
            .run(CodeLanguage::Python, "", json!("x"))
            .await
            .unwrap()["result"],
        "x"
    );
    assert!(
        caps.memory
            .as_ref()
            .expect("memory wired by default")
            .flavour("f")
            .await
            .unwrap()["slug"]
            == "f"
    );
}
