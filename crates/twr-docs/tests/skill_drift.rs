//! SKILL.md, the reference for the socket API and the MCP tools, names every socket method the
//! hub dispatches and every MCP tool the orchestrator's server offers.
//!
//! The lists are read from the sources they live in (`twr_hub::server::METHODS`, the
//! `#[tool]` functions of `twr_mcp`), not through those crates: both depend on this one, so
//! depending on them here would make a cycle. A name counts as documented when SKILL.md has it
//! in code, a backtick right before it (`` `thread.upload {thread, ...}` `` does); a tool, in
//! its "MCP tools" section.

/// The hub's dispatch list, as written in its source.
const SERVER_RS: &str = include_str!("../../twr-hub/src/server.rs");
/// The orchestrator's MCP server, as written in its source.
const MCP_RS: &str = include_str!("../../twr-mcp/src/lib.rs");
/// SKILL.md as compiled into this crate.
const SKILL: &str = include_str!("../../../SKILL.md");

/// Methods SKILL.md doesn't document yet: each needs more than a line, so it is listed here
/// until it is written up. Remove a name when SKILL.md documents it (the test fails while a
/// listed name is documented, or no longer a method).
const UNDOCUMENTED_METHODS: &[&str] = &[
    // `{thread, models?, every_secs, retry?}` (users only): the TUI's one-line summary of a
    // worker thread, with its own reasons, caching and rate limits (`twr_hub::hub::summary`).
    "summary.thread",
];

/// Tools SKILL.md doesn't document yet, as [`UNDOCUMENTED_METHODS`].
const UNDOCUMENTED_TOOLS: &[&str] = &[];

/// The string literals of `pub const METHODS: &[&str] = &[ ... ];` in server.rs.
fn socket_methods() -> Vec<&'static str> {
    let start = SERVER_RS
        .find("pub const METHODS: &[&str] = &[")
        .expect("twr-hub's server.rs defines METHODS");
    let body = &SERVER_RS[start..];
    let body = &body[body.find('[').unwrap() + 1..];
    let body = &body[body.find('[').unwrap() + 1..];
    let body = &body[..body.find("];").expect("METHODS ends")];
    body.lines()
        .map(|l| l.split("//").next().unwrap().trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.trim_end_matches(',')
                .strip_prefix('"')
                .and_then(|l| l.strip_suffix('"'))
                .unwrap_or_else(|| panic!("an entry of METHODS this test can't read: {l}"))
        })
        .collect()
}

/// The names of the `#[tool(...)]` functions between `#[tool_router]` and `#[tool_handler`
/// in twr-mcp's lib.rs: each tool is named after its function.
fn mcp_tools() -> Vec<&'static str> {
    let start = MCP_RS
        .find("#[tool_router]")
        .expect("twr-mcp has a #[tool_router] impl");
    let end = MCP_RS[start..]
        .find("#[tool_handler")
        .expect("twr-mcp has a #[tool_handler] impl")
        + start;
    let router = &MCP_RS[start..end];
    let mut tools = Vec::new();
    let mut rest = router;
    while let Some(at) = rest.find("#[tool(") {
        rest = &rest[at..];
        let f = rest
            .find("async fn ")
            .expect("a #[tool] attribute on an async fn")
            + 9;
        rest = &rest[f..];
        let name = &rest[..rest.find('(').unwrap()];
        assert!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "a tool function name this test can't read: {name:?}"
        );
        tools.push(name);
    }
    tools
}

/// Whether `text` has `name` in code: a backtick right before it, and no name character
/// right after (`thread.get` doesn't count for `thread.get_x`).
fn documented(text: &str, name: &str) -> bool {
    let needle = format!("`{name}");
    text.match_indices(&needle).any(|(at, _)| {
        text[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
    })
}

/// SKILL.md's "MCP tools" section, up to the next `## ` heading.
fn mcp_section() -> &'static str {
    let start = SKILL
        .find("\n## MCP tools\n")
        .expect("SKILL.md has an MCP tools section");
    let rest = &SKILL[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |e| e + 3);
    &rest[..end]
}

fn check(kind: &str, names: &[&str], text: &str, allowed: &[&str]) {
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !documented(text, n) && !allowed.contains(n))
        .collect();
    assert!(
        missing.is_empty(),
        "SKILL.md doesn't document these {kind}s: {missing:?}. Document each where the others \
         are (or, for one that needs more than a line, add it to this test's list of \
         undocumented {kind}s, saying why)."
    );
    for n in allowed {
        assert!(
            names.contains(n),
            "{n} is listed as an undocumented {kind}, but is no {kind} any more: remove it"
        );
        assert!(
            !documented(text, n),
            "SKILL.md documents {n} now: remove it from this test's undocumented {kind}s"
        );
    }
}

#[test]
fn skill_md_names_every_socket_method() {
    let methods = socket_methods();
    // The reader still finds the list: not just a few names out of a changed declaration.
    assert!(methods.len() > 80, "{methods:?}");
    for m in ["hello", "thread.new", "attention.respond", "job.log"] {
        assert!(methods.contains(&m), "{m} is not in {methods:?}");
    }
    check("socket method", &methods, SKILL, UNDOCUMENTED_METHODS);
}

#[test]
fn skill_md_names_every_mcp_tool() {
    let tools = mcp_tools();
    assert!(tools.len() > 25, "{tools:?}");
    for t in [
        "list_threads",
        "spawn_thread",
        "respond_attention",
        "read_docs",
    ] {
        assert!(tools.contains(&t), "{t} is not in {tools:?}");
    }
    check("MCP tool", &tools, mcp_section(), UNDOCUMENTED_TOOLS);
}

#[test]
fn documented_needs_the_whole_name_in_code() {
    assert!(documented("`thread.get` reads", "thread.get"));
    assert!(documented("- `thread.upload {thread}`", "thread.upload"));
    assert!(!documented("`thread.get_x`", "thread.get"));
    assert!(!documented("`thread.getx`", "thread.get"));
    assert!(!documented("`hub.info.more`", "hub.info"));
    assert!(!documented("thread.get without code", "thread.get"));
}
