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
const UNDOCUMENTED_METHODS: &[&str] = &[];

/// Tools SKILL.md doesn't document yet, as [`UNDOCUMENTED_METHODS`].
const UNDOCUMENTED_TOOLS: &[&str] = &[];

/// The string literals of `pub const METHODS: &[&str] = &[ ... ];` in server.rs.
fn socket_methods() -> Vec<&'static str> {
    const_list("METHODS")
}

/// The string literals of `pub const <name>: &[&str] = &[ ... ];` in server.rs.
fn const_list(name: &str) -> Vec<&'static str> {
    let start = SERVER_RS
        .find(&format!("pub const {name}: &[&str] = &["))
        .unwrap_or_else(|| panic!("twr-hub's server.rs defines {name}"));
    let body = &SERVER_RS[start..];
    let body = &body[body.find('[').unwrap() + 1..];
    let body = &body[body.find('[').unwrap() + 1..];
    let body = &body[..body.find("];").expect("the list ends")];
    body.lines()
        .map(|l| l.split("//").next().unwrap().trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.trim_end_matches(',')
                .strip_prefix('"')
                .and_then(|l| l.strip_suffix('"'))
                .unwrap_or_else(|| panic!("an entry of {name} this test can't read: {l}"))
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

/// The methods SKILL.md names in code between `from` and the first `to` after it: the
/// backticked words that are socket methods (others, such as `refresh`, are left out).
fn methods_between(from: &str, to: &str) -> Vec<&'static str> {
    let start = SKILL
        .find(from)
        .unwrap_or_else(|| panic!("SKILL.md says {from:?}"))
        + from.len();
    let end = SKILL[start..]
        .find(to)
        .unwrap_or_else(|| panic!("SKILL.md says {to:?} after {from:?}"))
        + start;
    let methods = socket_methods();
    let mut named: Vec<&str> = SKILL[start..end]
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|w| methods.contains(w))
        .collect();
    named.sort_unstable();
    named.dedup();
    named
}

/// `want` (a list in server.rs) and `named` (what SKILL.md says of it) hold the same methods.
fn same_methods(what: &str, want: &[&str], named: &[&str]) {
    let missing: Vec<&str> = want
        .iter()
        .copied()
        .filter(|m| !named.contains(m))
        .collect();
    let extra: Vec<&str> = named
        .iter()
        .copied()
        .filter(|m| !want.contains(m))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "SKILL.md's list of the methods {what} may call is not the hub's: it leaves out \
         {missing:?} and names {extra:?}, which the hub refuses"
    );
}

/// SKILL.md's lists of what the orchestrator and an agent may call are the hub's
/// (`ORCHESTRATOR_METHODS`, `AGENT_METHODS`).
#[test]
fn skill_md_lists_what_the_orchestrator_and_agents_may_call() {
    let orchestrator = const_list("ORCHESTRATOR_METHODS");
    assert!(orchestrator.len() > 30, "{orchestrator:?}");
    same_methods(
        "the orchestrator",
        &orchestrator,
        &methods_between("The orchestrator may call only:", "(not `say`"),
    );
    let agent = const_list("AGENT_METHODS");
    assert!(agent.len() > 15, "{agent:?}");
    same_methods(
        "an agent",
        &agent,
        &methods_between("An agent may call\nonly", "Anything else is `unauthorized`"),
    );
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
