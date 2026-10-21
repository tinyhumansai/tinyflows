use super::*;

fn js(source: &str) -> bool {
    reads_undefined_item(source, false)
}

fn py(source: &str) -> bool {
    reads_undefined_item(source, true)
}

#[test]
fn a_free_item_read_is_flagged() {
    // The script behind the report: `item` is never defined.
    assert!(js(
        "var txs = txsOf(item, 0);\nreturn { count: txs.length };"
    ));
    assert!(js("return { total: item.a + item.b };"));
    assert!(js("return { item };"));
    assert!(py("return {'total': item['a']}"));
    assert!(py("for x in item:\n    pass"));
    assert!(py("def f(x=item):\n    return x\nreturn f()"));
    // An argument, not a parameter.
    assert!(js("return pick(0, item);"));
    assert!(py("def f(x=g(0, item)):\n    return x"));
}

#[test]
fn a_bound_item_is_not_flagged() {
    for source in [
        "return input.map(item => item.json);",
        "return input.map((item) => item.json);",
        "return input.map((item, i) => ({ i, v: item.json }));",
        "for (const item of input) { total += item.json.a; }",
        "const item = input[0]; return item.json;",
        "let a, item; item = input[0]; return item;",
        "const { item } = input[0].json; return item;",
        "input.forEach(function (item) { out.push(item.json); });",
        "function pick(item) { return item.json; }\nreturn input.map(pick);",
        "try { f(); } catch (item) { return item.message; }",
        "function pick(a = 0, item) { return item.json; }\nreturn pick(0, input[0]);",
        "function pick(a = [1, 2], item) { return item; }",
    ] {
        assert!(!js(source), "{source}");
    }
    for source in [
        "return [item['json'] for item in input]",
        "for i, item in enumerate(input):\n    pass",
        "item = input[0]\nreturn item['json']",
        "def pick(item):\n    return item['json']\nreturn [pick(i) for i in input]",
        "return list(map(lambda item: item['json'], input))",
        "def pick(x: int, item: dict):\n    return item['json']\nreturn pick(0, input[0])",
        "def pick(x: int, item):\n    return item\nreturn pick(0, input[0])",
    ] {
        assert!(!py(source), "{source}");
    }
}

#[test]
fn properties_keys_comments_and_strings_are_not_reads() {
    for source in [
        "return input[0].json.item;",
        "return { item: input[0].json };",
        "// item is not used here\nreturn input;",
        "/* item */ return input;",
        "return 'item' + \"item\" + `item`;",
        "return input; // uses item later",
    ] {
        assert!(!js(source), "{source}");
    }
    assert!(!py("# item\nreturn input"));
    assert!(!py("return 'item'"));
    // A quote inside a comment opens no string, so the read after it counts.
    assert!(js("// don't\nreturn item;"));
    assert!(py("# don't\nreturn item"));
}

#[test]
fn a_source_without_item_is_not_flagged() {
    assert!(!js(""));
    assert!(!js("return input.length;"));
    assert!(!js("return { items: input };"));
    assert!(!py("return len(input)"));
}
