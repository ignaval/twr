use super::*;
use twr_core::reduce::ThreadStatus;

use Coverage::{Conflict, Direct, Lone, Uncovered};

fn th(id: &str, name: &str) -> ThreadInfo {
    ThreadInfo {
        id: id.into(),
        name: name.into(),
        aliases: Vec::new(),
        former_names: Vec::new(),
        project: None,
        status: ThreadStatus::Idle,
        runtime: "fake".into(),
        is_orchestrator: false,
        stopped: false,
        top_attention: None,
        last_report: None,
    }
}

fn orch() -> ThreadInfo {
    let mut t = th("t0", "orchestrator");
    t.aliases = vec!["orch".into()];
    t.is_orchestrator = true;
    t
}

fn perm(attn: &str, thread: &str) -> Waiting {
    Waiting {
        attn_id: attn.into(),
        thread: thread.into(),
        reason: AttentionReason::Permission,
        options: vec!["allow".into(), "deny".into()],
    }
}

fn question(attn: &str, thread: &str, options: &[&str]) -> Waiting {
    Waiting {
        attn_id: attn.into(),
        thread: thread.into(),
        reason: AttentionReason::Question,
        options: options.iter().map(|o| o.to_string()).collect(),
    }
}

type World = (Vec<ThreadInfo>, Vec<Waiting>);

/// The user's session: atlas-1 and orbit-webapp-1, each with a permission waiting.
fn session() -> World {
    (
        vec![orch(), th("td", "atlas-1"), th("tw", "orbit-webapp-1")],
        vec![perm("pd", "td"), perm("pw", "tw")],
    )
}

/// Workers api and web, each with a permission waiting.
fn api_web() -> World {
    (
        vec![orch(), th("ta", "api"), th("tb", "web")],
        vec![perm("pa", "ta"), perm("pb", "tb")],
    )
}

/// Workers api and web, only api's permission waiting.
fn api_only() -> World {
    let w = api_web();
    (w.0, vec![perm("pa", "ta")])
}

fn scene<'a>(world: &'a World, msg: &'a str) -> Scene<'a> {
    Scene {
        message: msg,
        threads: &world.0,
        waiting: &world.1,
    }
}

fn answer(world: &World, msg: &str, attn: &str, label: &str) -> Coverage {
    answer_coverage(&scene(world, msg), attn, label)
}

fn mode(world: &World, msg: &str, thread: &str, m: &str) -> Coverage {
    mode_coverage(&scene(world, msg), thread, m)
}

/// `m` is no command for `world`: every answer and mode call is uncovered.
fn no_command(world: &World, m: &str) {
    assert_eq!(read(&scene(world, m)), None, "{m:?}");
    for w in &world.1 {
        let mut options = w.options.clone();
        options.extend(["allow".to_string(), "deny".to_string()]);
        for l in &options {
            assert_eq!(
                answer(world, m, &w.attn_id, l),
                Uncovered,
                "{m:?} {}",
                w.attn_id
            );
        }
    }
    for t in world.0.iter().filter(|t| !t.is_orchestrator) {
        for md in ["auto", "plan", "bypassPermissions"] {
            assert_eq!(mode(world, m, &t.id, md), Uncovered, "{m:?} {} {md}", t.id);
        }
    }
}

/// Every item of `world` answered `label` directly by `m`, and the other answer a conflict.
fn both_answered(world: &World, m: &str, label: &str) {
    let other = if label == "allow" { "deny" } else { "allow" };
    for w in &world.1 {
        assert_eq!(
            answer(world, m, &w.attn_id, label),
            Direct,
            "{m:?} {}",
            w.attn_id
        );
        assert_eq!(
            answer(world, m, &w.attn_id, other),
            Conflict,
            "{m:?} {}",
            w.attn_id
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The direct forms
// ---------------------------------------------------------------------------------------------

#[test]
fn the_short_commands_apply_directly() {
    let s = session();
    for m in [
        "allow both",
        "approve all",
        "Allow both.",
        "approve all!",
        "dale a ambos",
        "aprueba todos",
        "yes to all",
        "allow atlas and webapp",
        "allow atlas, webapp",
        "allow atlas & the webapp one",
        "allow them both",
        "allow every",
        "allow everything",
    ] {
        both_answered(&s, m, "allow");
    }
    for m in ["deny both", "reject all", "rechaza ambos", "block them all"] {
        both_answered(&s, m, "deny");
    }
    let aw = api_web();
    both_answered(&aw, "allow api and web", "allow");
    both_answered(&aw, "ALLOW API AND WEB", "allow");
    both_answered(&aw, "allow api,web", "allow");
    both_answered(&aw, "approve api, web, and the web one", "allow");

    let m = "allow on the webapp";
    assert_eq!(answer(&s, m, "pw", "allow"), Direct);
    assert_eq!(answer(&s, m, "pd", "allow"), Uncovered);

    let m = "approve atlas, deny the webapp one";
    assert_eq!(answer(&s, m, "pd", "allow"), Direct);
    assert_eq!(answer(&s, m, "pw", "deny"), Direct);
    assert_eq!(answer(&s, m, "pw", "allow"), Conflict);

    let m = "Allow both, and set them both to audo mode";
    for (a, t) in [("pd", "td"), ("pw", "tw")] {
        assert_eq!(answer(&s, m, a, "allow"), Direct, "{a}");
        assert_eq!(mode(&s, m, t, "auto"), Direct, "{t}");
        assert_eq!(mode(&s, m, t, "plan"), Conflict, "{t}");
    }

    for t in ["td", "tw"] {
        assert_eq!(mode(&s, "set both to audo mode", t, "auto"), Direct, "{t}");
    }
    assert_eq!(mode(&aw, "set api to plan mode", "ta", "plan"), Direct);
    assert_eq!(mode(&aw, "set api to plan mode", "tb", "plan"), Uncovered);

    let one = api_only();
    for m in [
        "allow it",
        "yes",
        "yep",
        "Yes.",
        "ok!",
        "go ahead",
        "approve it",
    ] {
        // A lone answer covers the only item, but goes on the card: it names nothing, and
        // may answer the orchestrator's own question.
        assert_eq!(answer(&one, m, "pa", "allow"), Lone, "{m}");
        assert_eq!(answer(&one, m, "pa", "deny"), Conflict, "{m}");
        assert_eq!(direct_actions(&scene(&one, m)), None, "{m}");
    }
    for m in ["no", "nope", "deny", "deny it", "No."] {
        assert_eq!(answer(&one, m, "pa", "deny"), Lone, "{m}");
        assert_eq!(direct_actions(&scene(&one, m)), None, "{m}");
    }
    // Named, the same answer applies directly.
    assert_eq!(answer(&one, "allow api", "pa", "allow"), Direct);
    assert!(direct_actions(&scene(&one, "allow api")).is_some());
}

// ---------------------------------------------------------------------------------------------
// Everything else goes on the card
// ---------------------------------------------------------------------------------------------

#[test]
fn anything_outside_the_grammar_goes_on_the_card() {
    let s = session();
    // The user's message: the mode change goes on the card (the question still reaches the
    // orchestrator as a message).
    no_command(
        &s,
        "Set both agents to auto mode. What were the latest changes on the webapp?",
    );
    for m in [
        "set both to audo mode and approve bothrequests",
        "approve both requests",
        "approve the request for atlas",
        "allow teh webapp",
        "allow atlas only if the tests pass",
        "approve all, except webapp",
        "don't allow atlas",
        "please allow both",
        "allow both, thanks",
        "allow both please",
        "si",
        "si, allow both",
        "allow them",
        "allow these",
        "allow those",
        "allow both of them",
        "allow one of them",
        "allow each",
        "allow both agents",
        "approve all permissions",
        "allow 'atlas' and 'webapp'",
        "allow (atlas)",
        "allow *both*",
        "allow atlas webapp",
        "allow atlas and and webapp",
        "allow atlas,, webapp",
        "allow atlas, and",
        "and allow both",
        "allow both and",
        "allow both, set",
        "both: allow",
        "both allow",
        "atlas: yes",
        "@atlas: yes",
        "allow atlas then webapp",
        "allow atlas plus webapp",
        "allow atlas, then deny webapp",
        "set both to auto",
        "set both in auto mode",
        "use both in auto mode",
        "make both auto mode",
        "auto mode for both",
        "set it to auto mode",
        "set both to automatic mode",
        "set both to auth mode",
    ] {
        no_command(&s, m);
    }
    // Two sentences, a question, other marks, a line break.
    let aw = api_web();
    for m in [
        "Allow api. Only if tests pass?",
        "Set api to auto mode. Or keep it manual?",
        "allow api. deny web",
        "allow api! deny web",
        "allow api?",
        "¿allow api",
        "allow api？",
        "allow api..",
        "allow api.!",
        "allow api; deny web",
        "allow api: yes",
        "allow api…",
        "allow api。",
        "allow api\ndeny web",
        "allow api\r\ndeny web",
        "allow api\u{2028}deny web",
        "allow\u{0}api",
        "allow api\n\nThanks",
        "Thanks. allow api",
        "allow api. Thanks!",
    ] {
        no_command(&aw, m);
    }
    // Option labels and numbers are no answer words.
    let q = (
        aw.0.clone(),
        vec![
            perm("pa", "ta"),
            question("qb", "tb", &["Use SQLite", "Use Postgres"]),
        ],
    );
    for m in [
        "1",
        "option 1",
        "#1",
        "option 1 for web",
        "Use SQLite",
        "use sqlite for web",
    ] {
        no_command(&q, m);
    }
}

#[test]
fn the_message_is_one_trimmed_sentence() {
    let one = api_only();
    for m in [
        "allow api",
        "  allow api  ",
        "allow api.",
        "allow api!",
        "\n allow api.\n",
        "allow api .",
        "allow\tapi",
        "Allow   API",
    ] {
        assert_eq!(answer(&one, m, "pa", "allow"), Direct, "{m:?}");
    }
    for m in ["", " ", ".", "!", "allow. api", "allow api. ."] {
        assert_ne!(answer(&one, m, "pa", "allow"), Direct, "{m:?}");
    }
    // Curly apostrophes are straightened.
    let aw = api_web();
    for m in ["set api to don't ask", "set api to don\u{2019}t ask mode"] {
        assert_eq!(mode(&aw, m, "ta", "dontAsk"), Direct, "{m:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------------------------

#[test]
fn every_answer_word_answers() {
    let one = api_only();
    let two = api_web();
    for w in APPROVE {
        for m in [w.to_string(), format!("{w} it")] {
            assert_eq!(answer(&one, &m, "pa", "allow"), Lone, "{m}");
        }
        let m = format!("{w} api");
        assert_eq!(answer(&one, &m, "pa", "allow"), Direct, "{m}");
        let m = format!("{w} api and web");
        both_answered(&two, &m, "allow");
    }
    for w in DENY {
        assert_eq!(answer(&one, w, "pa", "deny"), Lone, "{w}");
        if LONE_ONLY.contains(w) {
            for m in [format!("{w} it"), format!("{w} api"), format!("{w} to all")] {
                no_command(&one, &m);
            }
        } else {
            assert_eq!(answer(&one, &format!("{w} it"), "pa", "deny"), Lone, "{w}");
            let m = format!("{w} api");
            assert_eq!(answer(&one, &m, "pa", "deny"), Direct, "{m}");
        }
    }
    // The unaccented "si" is no answer word; "sí" is.
    for m in ["si", "si api", "si, allow api", "allow si"] {
        no_command(&one, m);
    }
    assert_eq!(answer(&one, "sí", "pa", "allow"), Lone);
    assert_eq!(answer(&one, "sí api", "pa", "allow"), Direct);
    // Prepositions only before targets.
    for m in [
        "allow on api",
        "allow for api",
        "allow to api",
        "dale a api",
    ] {
        assert_eq!(answer(&one, m, "pa", "allow"), Direct, "{m}");
    }
    for m in [
        "allow on",
        "allow on on api",
        "allow with api",
        "allow on it",
        "no to api",
    ] {
        no_command(&one, m);
    }
    // Two answer words never share a clause; "it" only in a lone answer.
    for m in [
        "yes allow api",
        "allow approve api",
        "allow it and web",
        "allow api and it",
        "allow it, set api to auto mode",
        "set api to auto mode, allow it",
    ] {
        no_command(&two, m);
    }
}

#[test]
fn a_lone_answer_needs_exactly_one_candidate() {
    let aw = api_web();
    for m in ["yes", "allow it", "no", "deny it", "go ahead"] {
        no_command(&aw, m);
    }
    let none = (api_web().0, Vec::new());
    for m in ["yes", "allow it", "no"] {
        no_command(&none, m);
    }
    // An orchestrator's item is no candidate.
    let mut with_orch = api_only();
    with_orch.1.push(perm("po", "t0"));
    assert_eq!(answer(&with_orch, "yes", "pa", "allow"), Lone);
    assert_eq!(answer(&with_orch, "yes", "po", "allow"), Uncovered);
    assert_eq!(answer(&with_orch, "allow both", "pa", "allow"), Uncovered);
    assert_eq!(answer(&with_orch, "approve all", "po", "allow"), Uncovered);
    // A lone answer is the whole message.
    for m in ["yes, yes", "yes and set api to auto mode", "allow api, yes"] {
        no_command(&with_orch, m);
    }
}

#[test]
fn only_a_permission_takes_an_answer() {
    let (threads, _) = api_web();
    // Questions are never answered directly: covered, a conflict.
    let q = (
        threads.clone(),
        vec![question("qa", "ta", &["Yes", "No"]), perm("pb", "tb")],
    );
    assert_eq!(answer(&q, "approve all", "qa", "Yes"), Conflict);
    assert_eq!(answer(&q, "approve all", "pb", "allow"), Direct);
    assert_eq!(answer(&q, "yes api", "qa", "Yes"), Conflict);
    assert_eq!(answer(&q, "no", "qa", "No"), Uncovered);
    // "approve all api permissions" for api's Yes/No question.
    no_command(&q, "approve all api permissions");
    let lone = (threads.clone(), vec![question("qa", "ta", &["Yes", "No"])]);
    assert_eq!(answer(&lone, "yes", "qa", "Yes"), Conflict);
    no_command(&lone, "approve all api permissions");
    // A deny-only permission is never allowed.
    let deny_only = Waiting {
        options: vec!["deny".into()],
        ..perm("pa", "ta")
    };
    let d = (threads.clone(), vec![deny_only]);
    assert_eq!(answer(&d, "allow api", "pa", "deny"), Conflict);
    assert_eq!(answer(&d, "deny api", "pa", "deny"), Direct);
    // A permission listing no options takes allow / deny.
    let bare = Waiting {
        options: Vec::new(),
        ..perm("pa", "ta")
    };
    let b = (threads.clone(), vec![bare]);
    assert_eq!(answer(&b, "yes", "pa", "allow"), Lone);
    assert_eq!(answer(&b, "allow api", "pa", "allow"), Direct);
    assert_eq!(answer(&b, "block api", "pa", "deny"), Direct);
    // Labels: spelled as the word, else the only one of its polarity.
    let yn = Waiting {
        options: vec!["Yes".into(), "Yes, and don't ask again".into(), "No".into()],
        ..perm("pa", "ta")
    };
    let y = (threads.clone(), vec![yn]);
    assert_eq!(answer(&y, "yes", "pa", "Yes"), Lone);
    assert_eq!(answer(&y, "no", "pa", "No"), Lone);
    assert_eq!(answer(&y, "yes api", "pa", "Yes"), Direct);
    assert_eq!(answer(&y, "deny api", "pa", "No"), Direct);
    assert_eq!(
        answer(&y, "yes", "pa", "Yes, and don't ask again"),
        Conflict
    );
    let ab = Waiting {
        options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
        ..perm("pa", "ta")
    };
    let a = (threads, vec![ab]);
    assert_eq!(answer(&a, "deny", "pa", "Deny"), Lone);
    assert_eq!(answer(&a, "deny api", "pa", "Deny"), Direct);
}

#[test]
fn conflicting_actions_are_not_direct() {
    let aw = api_web();
    let m = "approve api, deny api";
    assert_eq!(answer(&aw, m, "pa", "allow"), Conflict);
    assert_eq!(answer(&aw, m, "pa", "deny"), Conflict);
    let m = "approve all, deny web";
    assert_eq!(answer(&aw, m, "pa", "allow"), Direct);
    assert_eq!(answer(&aw, m, "pb", "deny"), Conflict);
    let m = "set api to auto mode, set api to plan mode";
    assert_eq!(mode(&aw, m, "ta", "auto"), Conflict);
    let m = "set all to auto mode and web to plan mode";
    assert_eq!(mode(&aw, m, "ta", "auto"), Direct);
    assert_eq!(mode(&aw, m, "tb", "plan"), Conflict);
    let m = "allow api and approve api";
    assert_eq!(answer(&aw, m, "pa", "allow"), Direct);
}

// ---------------------------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------------------------

#[test]
fn every_group_must_pick_something_out() {
    // "both" needs exactly two candidates; the whole message is no command otherwise.
    let mut three = api_web();
    three.0.push(th("tc", "docs"));
    three.1.push(perm("pc", "tc"));
    for m in [
        "allow both",
        "dale a ambos",
        "allow api, deny both",
        "set both to auto mode",
        "allow all and set both to auto mode",
    ] {
        no_command(&three, m);
    }
    let m = "approve all";
    for a in ["pa", "pb", "pc"] {
        assert_eq!(answer(&three, m, a, "allow"), Direct, "{a}");
    }
    // A reference to a thread with two items, or none, picks out nothing.
    let crowded = (
        api_web().0,
        vec![perm("pa", "ta"), perm("pa2", "ta"), perm("pb", "tb")],
    );
    for m in [
        "allow api",
        "allow web and api",
        "approve all, deny api",
        "allow all api",
    ] {
        no_command(&crowded, m);
    }
    assert_eq!(answer(&crowded, "approve all", "pa2", "allow"), Direct);
    assert_eq!(answer(&crowded, "allow web", "pb", "allow"), Direct);
    let only = api_only();
    no_command(&only, "allow api and web");
    no_command(&only, "allow web");
    no_command(&(api_web().0, Vec::new()), "approve all");
    // A mode REF names a worker, even stopped; "all" / "both" count the running ones.
    let mut stopped = api_web();
    stopped.0[2].stopped = true;
    assert_eq!(mode(&stopped, "set web to plan mode", "tb", "plan"), Direct);
    assert_eq!(mode(&stopped, "set all to plan mode", "ta", "plan"), Direct);
    assert_eq!(
        mode(&stopped, "set all to plan mode", "tb", "plan"),
        Uncovered
    );
    no_command(&stopped, "set both to plan mode");
    // The hub thread is no worker.
    let mut hub = api_web();
    hub.0.push(th(HUB_THREAD, "hubby"));
    assert_eq!(mode(&hub, "set both to plan mode", "ta", "plan"), Direct);
    no_command(&hub, "set hubby to plan mode");
}

#[test]
fn a_target_list_has_its_own_joiners() {
    let s = session();
    for m in [
        "allow atlas and webapp",
        "allow atlas y webapp",
        "allow atlas & webapp",
        "allow atlas, webapp",
        "allow atlas, and webapp",
        "allow atlas, y webapp",
        "allow atlas, & webapp",
        "allow the atlas one and the webapp one",
        "allow el atlas y la webapp",
    ] {
        both_answered(&s, m, "allow");
    }
    for m in [
        "allow atlas and, webapp",
        "allow atlas & & webapp",
        "allow atlas or webapp",
        "allow atlas plus webapp",
        "allow the the atlas",
        "allow atlas one one",
        "allow the both",
        "allow los atlas",
        "allow them",
        "allow them atlas",
    ] {
        no_command(&s, m);
    }
}

#[test]
fn references_name_one_thread_exactly() {
    let mut threads = vec![
        orch(),
        th("t1", "orbit-webapp-1"),
        th("t2", "atlas-1"),
        th("t3", "web-admin_2"),
        th("t4", "Api"),
    ];
    threads[2].aliases = vec!["dp".into()];
    threads[1].project = Some("/home/me/code/orbit".into());
    let waiting = vec![
        perm("p1", "t1"),
        perm("p2", "t2"),
        perm("p3", "t3"),
        perm("p4", "t4"),
    ];
    let w: World = (threads, waiting);
    for (word, attn) in [
        ("orbit-webapp-1", "p1"),
        ("ORBIT-WEBAPP-1", "p1"),
        ("webapp", "p1"),
        ("orbit", "p1"),
        ("@webapp", "p1"),
        ("t1", "p1"),
        ("atlas", "p2"),
        ("dp", "p2"),
        ("@dp", "p2"),
        ("atlas-1", "p2"),
        ("admin", "p3"),
        ("web", "p3"),
        ("web-admin_2", "p3"),
        ("api", "p4"),
        ("@API", "p4"),
    ] {
        let m = format!("allow {word}");
        assert_eq!(answer(&w, &m, attn, "allow"), Direct, "{word}");
    }
    for word in [
        "webap",
        "atla",
        "webapp-1",
        "orbit-webapp",
        "app",
        "1",
        "2",
        "admin_2",
        "atlass",
        "@nobody",
        "@",
        "orbit/webapp",
        "code",
        "-web",
        "web!!",
        "w\u{200B}eb",
    ] {
        no_command(&w, &format!("allow {word}"));
    }
    // Two threads matching at the first step that matches: no reference.
    let amb: World = (
        vec![
            orch(),
            th("ta", "api"),
            th("tb", "API"),
            th("tc", "api-web"),
            th("td", "web-x"),
        ],
        vec![perm("pc", "tc"), perm("pd", "td")],
    );
    no_command(&amb, "allow api");
    no_command(&amb, "allow web");
    // One thread's exact name that is another's name part names neither (a thread made or
    // renamed `x-api-y` beside `api` takes the word from both).
    let part: World = (
        vec![orch(), th("ta", "api"), th("tb", "x-api-y")],
        vec![perm("pa", "ta"), perm("pb", "tb")],
    );
    no_command(&part, "allow api");
    no_command(&part, "set api to plan mode");
    assert_eq!(answer(&part, "allow @x-api-y", "pb", "allow"), Direct);
    assert_eq!(answer(&amb, "allow api-web", "pc", "allow"), Direct);
    assert_eq!(answer(&amb, "allow x", "pd", "allow"), Uncovered);
}

/// A name a thread had before a rename names it: an answer, a mode change, `@x`. Two threads
/// with it, or another with it as a name in any case or as a name part: no reference.
#[test]
fn former_names_name_their_thread() {
    let mut gateway = th("ta", "gateway");
    gateway.former_names = vec!["api".into(), "Legacy-Svc".into()];
    let w: World = (
        vec![orch(), gateway, th("tb", "svc-client"), th("tc", "beta")],
        vec![perm("pa", "ta"), perm("pb", "tb")],
    );
    for word in ["api", "@api", "API", "legacy-svc", "gateway"] {
        let m = format!("allow {word}");
        assert_eq!(answer(&w, &m, "pa", "allow"), Direct, "{word}");
        assert_eq!(answer(&w, &m, "pb", "allow"), Uncovered, "{word}");
        let m = format!("set {word} to plan mode");
        assert_eq!(mode(&w, &m, "ta", "plan"), Direct, "{word}");
        assert_eq!(mode(&w, &m, "tb", "plan"), Uncovered, "{word}");
    }
    assert_eq!(answer(&w, "allow client", "pb", "allow"), Direct);
    // A former name is no source of name parts.
    no_command(&w, "allow legacy");
    assert_eq!(mode(&w, "set beta to plan mode", "tc", "plan"), Direct);
    // One thread's former name that is another's name part names neither.
    let mut w5 = w.clone();
    w5.0[2].name = "api-client".into();
    no_command(&w5, "allow api");
    no_command(&w5, "set api to plan mode");
    assert_eq!(answer(&w5, "allow gateway", "pa", "allow"), Direct);
    // Two threads that had it: no reference.
    let mut w2 = w.clone();
    w2.0[3].former_names = vec!["api".into()];
    no_command(&w2, "allow api");
    no_command(&w2, "set api to plan mode");
    // A word that is one thread's name and another's former name names neither, in any case
    // (`API` made beside `api` before names were reserved without case).
    let mut w3 = w.clone();
    w3.0[3].former_names = vec!["gateway".into()];
    no_command(&w3, "allow gateway");
    let mut w4 = w.clone();
    w4.0[3].name = "API".into();
    w4.1.push(perm("pc", "tc"));
    for m in [
        "allow api",
        "allow @api",
        "allow @API",
        "set api to plan mode",
    ] {
        no_command(&w4, m);
    }
    assert_eq!(answer(&w4, "allow gateway", "pa", "allow"), Direct);
}

#[test]
fn stoplist_reserved_and_numeric_words_never_name_a_thread_bare() {
    for name in [
        "never-1", "no-one-1", "nobody-1", "none", "2", "1-2", "only-api", "agents", "each-x",
        "now",
    ] {
        let w: World = (
            vec![orch(), th("tx", name), th("ty", "web")],
            vec![perm("px", "tx"), perm("py", "ty")],
        );
        let bare = name.split('-').next().unwrap();
        for m in [
            format!("allow {name}"),
            format!("allow {bare}"),
            format!("set {name} to plan mode"),
        ] {
            assert_ne!(answer(&w, &m, "px", "allow"), Direct, "{m}");
            assert_ne!(mode(&w, &m, "tx", "plan"), Direct, "{m}");
        }
        let m = format!("allow @{name}");
        assert_eq!(answer(&w, &m, "px", "allow"), Direct, "{m}");
        let m = format!("set @{name} to plan mode");
        assert_eq!(mode(&w, &m, "tx", "plan"), Direct, "{m}");
    }
    for w in [
        "never",
        "nobody",
        "2",
        "1-2",
        "no-one",
        "never-1",
        "agents",
        "si",
        "orchestrator",
    ] {
        assert!(never_bare(w), "{w}");
    }
    for w in ["api", "orbit-webapp-1", "planner-2", "atlas"] {
        assert!(!never_bare(w), "{w}");
    }
}

/// "allow api, cheers" approved a worker named `cheers` too, though the user approved api and
/// said thanks. Courtesy words and closing remarks never name a thread bare, whether a thread's
/// name, alias or name part; `@x` still does.
#[test]
fn courtesy_words_never_name_a_thread_bare() {
    let w: World = (
        vec![orch(), th("ta", "api"), th("tc", "cheers")],
        vec![perm("pa", "ta"), perm("pc", "tc")],
    );
    let m = "allow api, cheers";
    assert_eq!(read(&scene(&w, m)), None, "{m}");
    assert_eq!(answer(&w, m, "pa", "allow"), Uncovered);
    assert_eq!(answer(&w, m, "pc", "allow"), Uncovered);
    for m in ["allow api, @cheers", "allow api and @cheers"] {
        both_answered(&w, m, "allow");
    }
    // Exact names, aliases and name parts.
    let words = [
        "please", "pls", "plz", "thanks", "thank", "thx", "ty", "tyvm", "cheers", "gracias",
        "merci", "danke", "ta", "cool", "great", "perfect", "nice", "awesome", "done", "kudos",
    ];
    for word in words {
        let worlds: [(&str, ThreadInfo); 3] = [
            ("name", th("tx", word)),
            ("alias", {
                let mut t = th("tx", "zeta-7");
                t.aliases = vec![word.into()];
                t
            }),
            ("part", th("tx", &format!("{word}-bot"))),
        ];
        for (how, t) in worlds {
            let w: World = (
                // Ids no courtesy word spells ("ta" would be one).
                vec![orch(), th("id-api", "api"), t],
                vec![perm("pa", "id-api"), perm("px", "tx")],
            );
            for m in [
                format!("allow api, {word}"),
                format!("allow api and {word}"),
                format!("allow {word}"),
                format!("set api, {word} to plan mode"),
            ] {
                assert_ne!(answer(&w, &m, "px", "allow"), Direct, "{how} {m}");
                assert_ne!(mode(&w, &m, "tx", "plan"), Direct, "{how} {m}");
            }
            if how != "part" {
                let m = format!("allow api, @{word}");
                assert_eq!(answer(&w, &m, "px", "allow"), Direct, "{how} {m}");
                let m = format!("set @{word} to plan mode");
                assert_eq!(mode(&w, &m, "tx", "plan"), Direct, "{how} {m}");
            }
        }
        assert!(never_bare(word), "{word}");
    }
    // A compound is caught by its parts.
    for w in [
        "okay-thanks",
        "thank-you",
        "thanks_a_lot",
        "cheers-mate",
        "ty-1",
    ] {
        assert!(never_bare(w), "{w}");
    }
}

/// With api and `conditionally-1` each holding a seen permission, "allow api, conditionally"
/// approved both ("tentatively" too), and "set either to bypass mode" changed `either-1`.
/// Hedging, conditional and temporal qualifiers and indefinite choices never name a thread bare,
/// whether a thread's name, alias or name part; `@x` still does.
#[test]
fn qualifiers_never_name_a_thread_bare() {
    let w: World = (
        vec![
            orch(),
            th("ta", "api"),
            th("tc", "conditionally-1"),
            th("te", "either-1"),
        ],
        vec![perm("pa", "ta"), perm("pc", "tc"), perm("pe", "te")],
    );
    for m in ["allow api, conditionally", "allow api, tentatively"] {
        assert_eq!(read(&scene(&w, m)), None, "{m}");
        assert_eq!(answer(&w, m, "pa", "allow"), Uncovered, "{m}");
        assert_eq!(answer(&w, m, "pc", "allow"), Uncovered, "{m}");
    }
    for m in ["set either to bypass mode", "set whichever to bypass mode"] {
        assert_eq!(read(&scene(&w, m)), None, "{m}");
        assert_ne!(mode(&w, m, "te", "bypassPermissions"), Direct, "{m}");
    }
    assert_eq!(
        answer(&w, "allow api, @conditionally-1", "pc", "allow"),
        Direct
    );
    assert_eq!(
        mode(
            &w,
            "set @either-1 to bypass mode",
            "te",
            "bypassPermissions"
        ),
        Direct
    );
    // Exact names, aliases and name parts.
    let words = [
        "conditionally",
        "tentatively",
        "either",
        "whichever",
        "maybe",
        "provisionally",
        "temporarily",
        "later",
        "sometime",
        "next",
        "last",
        "whatever",
        "another",
        "several",
        "quizás",
        "luego",
        "cualquiera",
        "otro",
        "vez",
    ];
    for word in words {
        let worlds: [(&str, ThreadInfo); 3] = [
            ("name", th("tx", word)),
            ("alias", {
                let mut t = th("tx", "zeta-7");
                t.aliases = vec![word.into()];
                t
            }),
            ("part", th("tx", &format!("{word}-1"))),
        ];
        for (how, t) in worlds {
            let w: World = (
                vec![orch(), th("id-api", "api"), t],
                vec![perm("pa", "id-api"), perm("px", "tx")],
            );
            for m in [
                format!("allow api, {word}"),
                format!("allow api and {word}"),
                format!("allow {word}"),
                format!("set {word} to plan mode"),
                format!("set api, {word} to plan mode"),
            ] {
                assert_ne!(answer(&w, &m, "px", "allow"), Direct, "{how} {m}");
                assert_ne!(mode(&w, &m, "tx", "plan"), Direct, "{how} {m}");
            }
            let at = if how == "part" {
                format!("{word}-1")
            } else {
                word.to_string()
            };
            let m = format!("allow api, @{at}");
            assert_eq!(answer(&w, &m, "px", "allow"), Direct, "{how} {m}");
            let m = format!("set @{at} to plan mode");
            assert_eq!(mode(&w, &m, "tx", "plan"), Direct, "{how} {m}");
        }
        assert!(never_bare(word), "{word}");
    }
    for w in [
        "maybe-later",
        "either_one",
        "tal-vez",
        "conditionally-1",
        "whichever-bot",
    ] {
        assert!(never_bare(w), "{w}");
    }
}

#[test]
fn an_orchestrator_match_blocks_a_reference() {
    let mut retired = th("t9", "planner");
    retired.is_orchestrator = true;
    retired.stopped = true;
    let w: World = (
        vec![
            orch(),
            retired,
            th("ta", "orchestrator-worker"),
            th("tb", "orch-api"),
            th("tc", "planner-2"),
        ],
        vec![perm("pa", "ta"), perm("pb", "tb"), perm("pc", "tc")],
    );
    for m in [
        "allow orchestrator",
        "allow orch",
        "allow planner",
        "allow @orchestrator",
        "allow @orch",
        "set orch to plan mode",
    ] {
        no_command(&w, m);
    }
    // "orchestrator" and "worker" are never bare names, so only `@x` names this worker.
    no_command(&w, "allow orchestrator-worker");
    no_command(&w, "allow worker");
    assert_eq!(
        answer(&w, "allow @orchestrator-worker", "pa", "allow"),
        Direct
    );
    assert_eq!(answer(&w, "allow orch-api", "pb", "allow"), Direct);
    assert_eq!(answer(&w, "allow api", "pb", "allow"), Direct);
    assert_eq!(answer(&w, "allow planner-2", "pc", "allow"), Direct);
    // The orchestrator itself is never a mode target.
    assert_eq!(mode(&w, "set all to plan mode", "t0", "plan"), Uncovered);
}

// ---------------------------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------------------------

#[test]
fn mode_shapes() {
    let aw = api_web();
    for (m, id) in [
        ("set api to auto mode", "auto"),
        ("switch api to the plan mode", "plan"),
        ("change api to manual modo", "manual"),
        ("put api to default mode", "default"),
        ("turn api to plan mode", "plan"),
        ("api to plan mode", "plan"),
        ("set api to read-only", "read-only"),
        ("set api to read only", "read-only"),
        ("set api to read_only mode", "read-only"),
        ("set api to the readonly", "read-only"),
        ("set api to dontAsk", "dontAsk"),
        ("set api to don't ask", "dontAsk"),
        ("set api to dont ask mode", "dontAsk"),
        ("set api to accept edits", "acceptEdits"),
        ("set api to acceptEdits mode", "acceptEdits"),
        ("set api to bypass permissions", "bypassPermissions"),
        ("set api to bypass", "bypassPermissions"),
        ("set api to bypassPermissions mode", "bypassPermissions"),
        ("set api to audo mode", "auto"),
        ("set api to paln mode", "plan"),
        ("set api to manul mode", "manual"),
        ("set api to bypas mode", "bypassPermissions"),
        ("pon api en modo auto", "auto"),
        ("pasa api a modo plan", "plan"),
        ("cambia api a read-only", "read-only"),
        ("pon api en modo read only", "read-only"),
        ("pon api en modo bypass permissions", "bypassPermissions"),
        ("pon api en modo bypass", "bypassPermissions"),
        ("pon api en auto modo", "auto"),
        ("pon api en audo modo", "auto"),
    ] {
        assert_eq!(mode(&aw, m, "ta", id), Direct, "{m}");
        assert_eq!(mode(&aw, m, "tb", id), Uncovered, "{m}");
    }
    for m in [
        "set api to auto",
        "set api to plan",
        "set api to the plan",
        "set api in plan mode",
        "set api into plan mode",
        "put api in plan mode",
        "use api in plan mode",
        "make api plan mode",
        "move api to plan mode",
        "set api plan mode",
        "plan mode for api",
        "set api to audo",
        "set api to mode plan",
        "set api to auto mode mode",
        "set api to modo auto",
        "pon api en auto",
        "pon api en modo audo",
        "pon api al modo auto",
        "pon api to auto mode",
        "set api a auto mode",
        "api a modo auto",
        "set api to plain mode",
        "set api to auth mode",
        "set api to a plan mode",
        "set to auto mode",
        "set api to auto mode and",
        "set api, web to",
    ] {
        no_command(&aw, m);
    }
    // Clauses and groups together.
    let m = "set api to auto mode and web to plan mode";
    assert_eq!(mode(&aw, m, "ta", "auto"), Direct);
    assert_eq!(mode(&aw, m, "tb", "plan"), Direct);
    let m = "set api and web to bypass permissions, allow api";
    assert_eq!(mode(&aw, m, "tb", "bypassPermissions"), Direct);
    assert_eq!(answer(&aw, m, "pa", "allow"), Direct);
    assert_eq!(answer(&aw, m, "pb", "allow"), Uncovered);
}

#[test]
fn a_call_mode_counts_only_as_the_hub_spells_the_command_mode() {
    // "set api to plan mode" covered set_mode("api", "p'lan").
    let aw = api_web();
    let m = "set api to plan mode";
    assert_eq!(mode(&aw, m, "ta", "plan"), Direct);
    for call in [
        "p'lan",
        "p\u{2019}lan",
        "Plan",
        "PLAN",
        "pl-an",
        "plan ",
        "plan\u{200b}",
        "planmode",
    ] {
        assert_eq!(mode(&aw, m, "ta", call), Conflict, "{call:?}");
    }
    for (m, id, tricks) in [
        (
            "set api to read only",
            "read-only",
            &["readonly", "read_only", "Read-Only", "read only"][..],
        ),
        (
            "set api to don't ask mode",
            "dontAsk",
            &["dont'Ask", "dontask", "dont-ask"][..],
        ),
        (
            "set api to bypass",
            "bypassPermissions",
            &["bypass", "bypass-permissions"][..],
        ),
    ] {
        assert_eq!(mode(&aw, m, "ta", id), Direct, "{m}");
        for call in tricks {
            assert_eq!(mode(&aw, m, "ta", call), Conflict, "{m} {call:?}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------------------------

#[test]
fn a_command_is_read_only_within_its_limits() {
    let (mut threads, mut waiting) = api_web();
    for i in 0..20 {
        threads.push(th(&format!("tx{i}"), &format!("w{i}x")));
        waiting.push(perm(&format!("px{i}"), &format!("tx{i}")));
    }
    let w: World = (threads, waiting);
    let names = |n: usize| (0..n).map(|i| format!("w{i}x")).collect::<Vec<_>>();
    // MAX_GROUPS groups in a list, not one more.
    let m = format!("allow {}", names(MAX_GROUPS).join(", "));
    assert_eq!(answer(&w, &m, "px0", "allow"), Direct);
    no_command(&w, &format!("allow {}", names(MAX_GROUPS + 1).join(", ")));
    no_command(&w, &format!("allow {}", names(MAX_GROUPS + 1).join(" & ")));
    // The cap drops only the oversized reading. With MAX_GROUPS answer targets and a mode
    // clause after them, reading the mode clause's first target as answer target 17 must not
    // discard the one valid parse (37 tokens).
    let m = format!("allow {}, w0x to plan mode", names(MAX_GROUPS).join(" & "));
    assert_eq!(tokens(&m).map(|t| t.len()), Some(37));
    for i in 0..MAX_GROUPS {
        assert_eq!(answer(&w, &m, &format!("px{i}"), "allow"), Direct, "px{i}");
    }
    assert_eq!(
        answer(&w, &m, &format!("px{MAX_GROUPS}"), "allow"),
        Uncovered
    );
    assert_eq!(mode(&w, &m, "tx0", "plan"), Direct);
    assert_eq!(mode(&w, &m, "tx1", "plan"), Uncovered);
    // MAX_CLAUSES clauses, not one more.
    let clauses = |n: usize| {
        names(n)
            .iter()
            .map(|x| format!("allow {x}"))
            .collect::<Vec<_>>()
    };
    let m = clauses(MAX_CLAUSES).join(", ");
    assert_eq!(answer(&w, &m, "px0", "allow"), Direct);
    no_command(&w, &clauses(MAX_CLAUSES + 1).join(", "));
    // MAX_TOKENS tokens, not one more.
    let m = format!("allow api{}", " and api".repeat((MAX_TOKENS - 2) / 2));
    assert_eq!(tokens(&m).map(|t| t.len()), Some(MAX_TOKENS));
    assert_eq!(tokens(&format!("{m},")), None);
    // MAX_MESSAGE_BYTES.
    let long = format!("allow api{}", " ".repeat(MAX_MESSAGE_BYTES));
    assert_eq!(tokens(&long), None);
}

// ---------------------------------------------------------------------------------------------
// Canonical labels, the loose reading
// ---------------------------------------------------------------------------------------------

#[test]
fn canonical_labels() {
    let q = question("q", "t", &["Yes", "yes", "No"]);
    assert_eq!(canonical_label(&q, "Yes"), Some(("Yes".into(), Some(0))));
    assert_eq!(canonical_label(&q, "YES"), None);
    assert_eq!(canonical_label(&q, " No "), Some(("No".into(), Some(2))));
    let p = Waiting {
        options: Vec::new(),
        ..perm("p", "t")
    };
    assert_eq!(canonical_label(&p, "Allow"), Some(("allow".into(), None)));
    assert_eq!(canonical_label(&p, "maybe"), None);
}

#[test]
fn answer_attempts_for_say() {
    let threads = api_web().0;
    let ctx = RouteContext::new("", threads);
    for m in [
        "allow both",
        "Allow both, and set them both to audo mode",
        "approve all permissions",
        "approve allrequests",
        "no to all",
        "@api yes, @web no",
        "allow @api and @web",
        "allow api and web",
        "allow api, and then please write a short summary of what each thread changed",
        "dale a ambos",
        "si, api",
    ] {
        assert!(answer_attempt(&ctx, m), "{m}");
    }
    for m in [
        "yes",
        "@web yes",
        "deploy the api",
        "set both to auto mode",
        "what changed on web?",
    ] {
        assert!(!answer_attempt(&ctx, m), "{m}");
    }
}

/// Every thread a word could name, alone or not: the hub keeps a command off the direct path
/// when one of them was made or renamed after what the user saw.
#[test]
fn named_threads_lists_every_thread_a_word_matches() {
    let mut gateway = th("tg", "gateway");
    gateway.former_names = vec!["web".into()];
    let w: World = (
        vec![
            orch(),
            th("ta", "api"),
            th("tb", "x-api-y"),
            gateway,
            th("tc", "cli"),
        ],
        Vec::new(),
    );
    let named = |m: &str| named_threads(&scene(&w, m));
    let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(named("allow api"), ids(&["ta", "tb"]));
    assert_eq!(
        named("allow web, set @cli to plan mode"),
        ids(&["tg", "tc"])
    );
    // Quantifiers, stop words and the orchestrator name nothing; nor does no command.
    assert_eq!(named("allow all"), ids(&[]));
    assert_eq!(named("allow orchestrator"), ids(&[]));
    assert_eq!(named("allow api. Then web?"), ids(&[]));
}
