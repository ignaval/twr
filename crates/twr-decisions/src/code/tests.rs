use twr_core::event::{AttentionReason, Grade, TurnResult};
use twr_core::reduce::ThreadStatus;

use super::route::route;
use super::*;
use crate::context::{AttentionInfo, RouteContext, ThreadInfo, Unresolved};
use crate::ReplyAnswer;

fn thread(id: &str, name: &str) -> ThreadInfo {
    ThreadInfo {
        id: id.into(),
        name: name.into(),
        aliases: Vec::new(),
        former_names: Vec::new(),
        project: Some("proj".into()),
        status: ThreadStatus::Idle,
        runtime: "fake".into(),
        is_orchestrator: false,
        stopped: false,
        top_attention: None,
        last_report: None,
    }
}

fn aliased(id: &str, name: &str, aliases: &[&str]) -> ThreadInfo {
    let mut t = thread(id, name);
    t.aliases = aliases.iter().map(|a| a.to_string()).collect();
    t
}

fn orchestrator() -> ThreadInfo {
    let mut t = aliased("t0", "orchestrator", &["orch"]);
    t.is_orchestrator = true;
    t
}

fn stopped(id: &str, name: &str) -> ThreadInfo {
    let mut t = thread(id, name);
    t.stopped = true;
    t.status = ThreadStatus::Done;
    t
}

fn with_attn(
    mut t: ThreadInfo,
    attn_id: &str,
    reason: AttentionReason,
    opts: &[&str],
) -> ThreadInfo {
    t.top_attention = Some(AttentionInfo {
        attn_id: attn_id.into(),
        reason,
        options: opts.iter().map(|o| o.to_string()).collect(),
        answerable: true,
    });
    t
}

fn ctx(msg: &str, threads: Vec<ThreadInfo>, focused: Option<&str>) -> RouteContext {
    let mut c = RouteContext::new(msg, threads);
    c.focused = focused.map(str::to_string);
    c
}

fn send(thread: &str, message: &str) -> Option<RouteOutcome> {
    Some(RouteOutcome::SendTo {
        thread: thread.into(),
        message: message.into(),
    })
}

fn respond(thread: &str, attn: &str, answer: ReplyAnswer) -> Option<RouteOutcome> {
    Some(RouteOutcome::RespondAttention {
        thread: thread.into(),
        attn_id: attn.into(),
        answer,
    })
}

fn label(s: &str) -> ReplyAnswer {
    ReplyAnswer::Label(s.into())
}

/// Two workers so single-thread routing never kicks in unless a test wants it.
fn two() -> Vec<ThreadInfo> {
    vec![
        orchestrator(),
        aliased("t1", "api", &["backend"]),
        thread("t2", "web"),
    ]
}

// ------------------------------------------------------------------------------------------
// Explicit addressing
// ------------------------------------------------------------------------------------------

#[test]
fn on_forms_strip_the_prefix() {
    let r = |m: &str| route(&ctx(m, two(), None));
    assert_eq!(r("on api, fix it"), send("t1", "fix it"));
    assert_eq!(r("on api: fix it"), send("t1", "fix it"));
    assert_eq!(r("On api,fix it"), send("t1", "fix it"));
    assert_eq!(
        r("on   web ,  run the tests  "),
        send("t2", "run the tests")
    );
    assert_eq!(r("on backend, fix it"), send("t1", "fix it"));
    // The message keeps its own punctuation after the first delimiter.
    assert_eq!(r("on api, fix a, b: c"), send("t1", "fix a, b: c"));
    // Not addressing: unknown target, no delimiter, keyword not followed by space.
    assert_eq!(r("on second thought, stop"), None);
    assert_eq!(r("on api fix it"), None);
    assert_eq!(r("online, check status"), None);
}

#[test]
fn on_versus_a_thread_named_on() {
    let threads = vec![thread("t1", "on"), thread("t2", "api")];
    let r = |m: &str| route(&ctx(m, threads.clone(), None));
    assert_eq!(r("on api, fix it"), send("t2", "fix it"));
    assert_eq!(r("on: fix it"), send("t1", "fix it"));
    assert_eq!(r("on on, fix it"), send("t1", "fix it"));
    assert_eq!(r("@on fix it"), send("t1", "fix it"));
    // "on api, ..." with no thread "api" is not addressing; with a single worker it goes there
    // verbatim.
    let single = vec![thread("t1", "on")];
    assert_eq!(
        route(&ctx("on api, fix it", single, None)),
        send("t1", "on api, fix it")
    );
}

#[test]
fn at_addressing_only_at_start() {
    let r = |m: &str| route(&ctx(m, two(), None));
    assert_eq!(r("@api fix it"), send("t1", "fix it"));
    assert_eq!(r("@api, fix it"), send("t1", "fix it"));
    assert_eq!(r("@api: fix it"), send("t1", "fix it"));
    assert_eq!(r("  @web   deploy "), send("t2", "deploy"));
    assert_eq!(r("@backend fix it"), send("t1", "fix it"));
    // Mid-sentence mention is not addressing.
    assert_eq!(r("tell @api to fix it"), None);
    assert_eq!(r("please ask @web about it"), None);
    // Unknown target: not addressing.
    assert_eq!(r("@nobody fix it"), None);
    // Addressed with nothing to say: abstain.
    assert_eq!(r("@api"), None);
    assert_eq!(r("on api,"), None);
    assert_eq!(r("api:"), None);
}

#[test]
fn colon_addressing_needs_an_exact_target() {
    let r = |m: &str| route(&ctx(m, two(), None));
    assert_eq!(r("api: fix it"), send("t1", "fix it"));
    assert_eq!(r("backend: fix it"), send("t1", "fix it"));
    assert_eq!(r("t2: deploy"), send("t2", "deploy"));
    assert_eq!(r("orch: what's up"), send("t0", "what's up"));
    // Prefix is not enough for the bare colon form, but is for `on` / `@`.
    assert_eq!(r("ap: fix it"), None);
    assert_eq!(r("on ap, fix it"), send("t1", "fix it"));
    assert_eq!(r("@we deploy"), send("t2", "deploy"));
    // Ordinary prose with a colon.
    assert_eq!(r("Note: the build is flaky"), None);
    assert_eq!(r("see https://example.com"), None);
}

#[test]
fn alias_name_prefix_and_ambiguity() {
    let threads = vec![
        aliased("t1", "api", &["be"]),
        aliased("t2", "apps", &["api"]),
        stopped("t3", "web"),
        thread("t4", "web"),
        stopped("t5", "docs"),
        stopped("t6", "docs"),
    ];
    let r = |m: &str| route(&ctx(m, threads.clone(), None));
    // Name beats alias.
    assert_eq!(r("@api fix it"), send("t1", "fix it"));
    assert_eq!(r("@be fix it"), send("t1", "fix it"));
    // Unique prefix.
    assert_eq!(r("@app fix it"), send("t2", "fix it"));
    // Ambiguous prefix: abstain (does not fall through to other rules).
    assert_eq!(r("@a fix it"), None);
    assert_eq!(r("on a, yes"), None);
    // Duplicate name: the only live one wins; all stopped → ambiguous → abstain.
    assert_eq!(r("@web deploy"), send("t4", "deploy"));
    assert_eq!(r("docs: rebuild"), None);
    // Explicit addressing may target a stopped thread (the user asked for it).
    assert_eq!(r("@t5 rebuild"), send("t5", "rebuild"));
}

/// A name a thread had before a rename addresses it after names and aliases, before any
/// prefix: never another thread whose name starts with it. Two threads with it: abstain.
#[test]
fn former_names_address_their_thread() {
    let mut gateway = thread("t1", "gateway");
    gateway.former_names = vec!["api".into()];
    let threads = vec![
        gateway,
        thread("t2", "api-client"),
        aliased("t3", "docs", &["dx"]),
    ];
    let r = |m: &str, threads: &[ThreadInfo]| route(&ctx(m, threads.to_vec(), None));
    for m in ["@api continue", "on api, continue", "api: continue"] {
        assert_eq!(r(m, &threads), send("t1", "continue"), "{m}");
    }
    assert_eq!(r("@api-c continue", &threads), send("t2", "continue"));
    // A name or alias now beats a former name.
    let mut t = threads.clone();
    t[2].former_names = vec!["gateway".into(), "x".into()];
    assert_eq!(r("@gateway go", &t), send("t1", "go"));
    // Two threads that had it: ambiguous, not a prefix match.
    let mut t = threads.clone();
    t[2].former_names = vec!["api".into()];
    assert_eq!(r("@api continue", &t), None);
    let c = ctx("", t, None);
    assert_eq!(c.resolve("api", true), Err(Unresolved::Ambiguous));
}

#[test]
fn new_project_prefix() {
    let r = |m: &str| route(&ctx(m, two(), None));
    let new = |p: &str, m: &str| {
        Some(RouteOutcome::NewThreadRequest {
            project: p.into(),
            message: m.into(),
        })
    };
    assert_eq!(
        r("new billing: add invoices"),
        new("billing", "add invoices")
    );
    assert_eq!(
        r("New billing:add invoices"),
        new("billing", "add invoices")
    );
    assert_eq!(r("new billing:"), new("billing", ""));
    // Not the explicit form.
    assert_eq!(r("new idea for the api"), None);
    assert_eq!(r("new thread for billing: x"), None);
    assert_eq!(r("news: something"), None);
    // A thread literally named "new" is addressed by `new: ...`.
    let threads = vec![thread("t1", "new"), thread("t2", "x")];
    assert_eq!(route(&ctx("new: go", threads, None)), send("t1", "go"));
}

// ------------------------------------------------------------------------------------------
// Reply routing
// ------------------------------------------------------------------------------------------

#[test]
fn reply_goes_to_focused_thread_without_attention() {
    let r = |m: &str| route(&ctx(m, two(), Some("t2")));
    for m in ["Yes!", "go ahead.", "LGTM", "continue", "no", "2", "yes no"] {
        assert_eq!(r(m), send("t2", m), "{m}");
    }
    // A 7-word sentence is not a reply.
    assert_eq!(r("yes go ahead and do it now"), None);
    assert_eq!(r("fix the flaky test"), None);
    // Focus by name works too; no focus → no reply routing.
    assert_eq!(route(&ctx("ok", two(), Some("web"))), send("t2", "ok"));
    assert_eq!(route(&ctx("ok", two(), None)), None);
    // The focused thread may be the orchestrator.
    assert_eq!(route(&ctx("yes", two(), Some("t0"))), send("t0", "yes"));
}

#[test]
fn reply_answers_open_permission() {
    let threads = || {
        vec![
            orchestrator(),
            with_attn(
                thread("t1", "api"),
                "p1",
                AttentionReason::Permission,
                &["allow", "deny"],
            ),
            thread("t2", "web"),
        ]
    };
    let r = |m: &str| route(&ctx(m, threads(), Some("t1")));
    for m in [
        "Yes!",
        "yes",
        "go ahead.",
        "LGTM",
        "approve",
        "do it",
        "ok thanks",
        "allow",
    ] {
        assert_eq!(r(m), respond("t1", "p1", label("allow")), "{m}");
    }
    for m in ["no", "Deny.", "nope", "reject", "don't"] {
        assert_eq!(r(m), respond("t1", "p1", label("deny")), "{m}");
    }
    // Digits are 1-based option numbers.
    assert_eq!(r("1"), respond("t1", "p1", ReplyAnswer::OptionIndex(0)));
    assert_eq!(r("2."), respond("t1", "p1", ReplyAnswer::OptionIndex(1)));
    // Out of range / zero / conflicting: abstain rather than send text to a blocked thread.
    assert_eq!(r("3"), None);
    assert_eq!(r("0"), None);
    assert_eq!(r("yes no"), None);
    // Not reply-shaped: rule 3 does not apply; two workers → abstain.
    assert_eq!(r("why do you need that?"), None);
    // Explicit addressing to the blocked thread with a reply answers it too.
    assert_eq!(
        route(&ctx("on api, yes", threads(), Some("t2"))),
        respond("t1", "p1", label("allow"))
    );
    assert_eq!(
        route(&ctx("@api use sqlite instead", threads(), Some("t2"))),
        send("t1", "use sqlite instead")
    );
}

/// A strict answer answers only an item the user had seen (`say` `seen_gseq`): an item raised
/// after what the client rendered is neither answered nor sent the text (routing abstains).
#[test]
fn a_reply_never_answers_an_item_the_user_had_not_seen() {
    let unseen = |t: ThreadInfo| {
        let mut t = t;
        if let Some(a) = t.top_attention.as_mut() {
            a.answerable = false;
        }
        t
    };
    let threads = || {
        vec![
            orchestrator(),
            unseen(with_attn(
                thread("t1", "api"),
                "p1",
                AttentionReason::Permission,
                &["allow", "deny"],
            )),
            thread("t2", "web"),
        ]
    };
    for m in ["yes", "no", "1", "allow"] {
        assert_eq!(route(&ctx(m, threads(), Some("t1"))), None, "{m}");
        assert_eq!(
            route(&ctx(&format!("@api {m}"), threads(), Some("t2"))),
            None,
            "{m}"
        );
    }
    // The only worker, unseen item: abstain too (answer-shaped).
    let only = vec![unseen(with_attn(
        thread("t1", "api"),
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    ))];
    assert_eq!(route(&ctx("yes", only.clone(), None)), None);
    // Anything that is not an answer still goes as text.
    assert_eq!(
        route(&ctx("@api use sqlite instead", threads(), Some("t2"))),
        send("t1", "use sqlite instead")
    );
    // Contexts from the registry start with nothing answerable; `set_answerable` opts in.
    let mut c = ctx("yes", threads(), Some("t1"));
    c.set_answerable(|id| id == "p1");
    assert_eq!(route(&c), respond("t1", "p1", label("allow")));
    c.set_answerable(|_| false);
    assert_eq!(route(&c), None);
}

#[test]
fn permission_without_listed_options() {
    let threads = vec![with_attn(
        thread("t1", "api"),
        "p1",
        AttentionReason::Permission,
        &[],
    )];
    let r = |m: &str| route(&ctx(m, threads.clone(), Some("t1")));
    assert_eq!(r("yes"), respond("t1", "p1", label("allow")));
    assert_eq!(r("no"), respond("t1", "p1", label("deny")));
    assert_eq!(r("1"), None);
    // Options that do not include allow: abstain.
    let odd = vec![with_attn(
        thread("t1", "api"),
        "p1",
        AttentionReason::Permission,
        &["allow_once"],
    )];
    assert_eq!(route(&ctx("yes", odd, Some("t1"))), None);
}

#[test]
fn reply_answers_open_question() {
    let q = |opts: &[&str]| {
        vec![
            with_attn(thread("t1", "api"), "q1", AttentionReason::Question, opts),
            thread("t2", "web"),
        ]
    };
    let r = |m: &str, opts: &[&str]| route(&ctx(m, q(opts), Some("t1")));
    let db = &["Postgres", "SQLite"];
    // Option label, case/punctuation-insensitive, answered with the label as listed.
    assert_eq!(r("sqlite", db), respond("t1", "q1", label("SQLite")));
    assert_eq!(r("Postgres.", db), respond("t1", "q1", label("Postgres")));
    assert_eq!(r("2", db), respond("t1", "q1", ReplyAnswer::OptionIndex(1)));
    assert_eq!(r("3", db), None);
    // yes/no do not map onto these options: abstain.
    assert_eq!(r("yes", db), None);
    // yes/no map onto the single option that reads that way.
    let yn = &["Yes, proceed", "No"];
    assert_eq!(r("ok", yn), respond("t1", "q1", label("Yes, proceed")));
    assert_eq!(r("nope", yn), respond("t1", "q1", label("No")));
    // Question without options: yes/no abstain.
    assert_eq!(r("yes", &[]), None);
    // Not a reply: rule does not apply (two workers → abstain).
    assert_eq!(r("use mysql", db), None);
}

#[test]
fn reply_with_non_blocking_top_attention_is_sent() {
    let threads = vec![
        with_attn(thread("t1", "api"), "e1", AttentionReason::Error, &[]),
        thread("t2", "web"),
    ];
    assert_eq!(
        route(&ctx("continue", threads, Some("t1"))),
        send("t1", "continue")
    );
}

// ------------------------------------------------------------------------------------------
// Single-thread routing
// ------------------------------------------------------------------------------------------

#[test]
fn single_worker_thread() {
    // The orchestrator and stopped threads are not candidates.
    let threads = vec![orchestrator(), thread("t1", "api"), stopped("t2", "old")];
    assert_eq!(
        route(&ctx("add a health check", threads, None)),
        send("t1", "add a health check")
    );
    // Two live workers: abstain.
    assert_eq!(route(&ctx("add a health check", two(), None)), None);
    // Zero workers: abstain.
    let none = vec![orchestrator(), stopped("t2", "old")];
    assert_eq!(route(&ctx("add a health check", none, None)), None);
    assert_eq!(route(&ctx("add a health check", vec![], None)), None);
    // Empty message: abstain.
    assert_eq!(route(&ctx("   ", vec![thread("t1", "api")], None)), None);
}

#[test]
fn focused_reply_beats_single_thread() {
    let threads = vec![orchestrator(), thread("t1", "api")];
    assert_eq!(
        route(&ctx("yes", threads.clone(), Some("t0"))),
        send("t0", "yes")
    );
    assert_eq!(route(&ctx("yes", threads, None)), send("t1", "yes"));
}

// ------------------------------------------------------------------------------------------
// Provider, grade, block kind
// ------------------------------------------------------------------------------------------

#[test]
fn provider_route_decisions_name_the_target() {
    let p = CodeProvider;
    let d = p
        .decide_now(&DecisionContext::Route(ctx("@api hi", two(), None)))
        .unwrap();
    assert_eq!(d.chosen, "api");
    assert_eq!(d.confidence, 1.0);
    assert_eq!(d.provider, ProviderKind::Code);
    assert_eq!(d.route, send("t1", "hi"));
    let d = p
        .decide_now(&DecisionContext::Route(ctx("new x: y", two(), None)))
        .unwrap();
    assert_eq!(d.chosen, NEW_THREAD);
    assert!(p
        .decide_now(&DecisionContext::Route(ctx("hmm", two(), None)))
        .is_none());
}

fn gctx(result: TurnResult, text: Option<&str>) -> GradeContext {
    GradeContext {
        result,
        final_text: text.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn grading_rule() {
    use TurnResult::*;
    assert_eq!(
        grade(&gctx(Error, Some("done"))),
        Some((Grade::GaveUp, 1.0))
    );
    assert_eq!(grade(&gctx(Interrupted, None)), Some((Grade::Partial, 1.0)));
    assert_eq!(
        grade(&gctx(Completed, Some("Sorry, I couldn't finish the task"))),
        Some((Grade::GaveUp, 0.9))
    );
    assert_eq!(
        grade(&gctx(Completed, Some("I AM UNABLE TO reach the db"))),
        Some((Grade::GaveUp, 0.9))
    );
    assert_eq!(
        grade(&gctx(
            Completed,
            Some("Sorry, I couldn\u{2019}t complete it")
        )),
        Some((Grade::GaveUp, 0.9))
    );
    // Otherwise abstain; the caller's default is success at 0.6.
    assert_eq!(grade(&gctx(Completed, Some("All tests pass."))), None);
    assert_eq!(grade(&gctx(Completed, None)), None);
    let d = CodeProvider
        .decide_now(&DecisionContext::Grade(gctx(Error, None)))
        .unwrap();
    assert_eq!((d.chosen.as_str(), d.confidence), ("gave_up", 1.0));
    assert!(CodeProvider
        .decide_now(&DecisionContext::Grade(gctx(Completed, Some("ok"))))
        .is_none());
}

#[test]
fn block_kind_rule() {
    let transient = BlockContext {
        result: Some(TurnResult::Error),
        transient: true,
        ..Default::default()
    };
    assert_eq!(block_kind(&transient), Some(BlockKind::Transient));
    let d = CodeProvider
        .decide_now(&DecisionContext::BlockKind(transient))
        .unwrap();
    assert_eq!((d.chosen.as_str(), d.confidence), ("transient", 1.0));
    let crash = BlockContext {
        crashed: true,
        exit_code: Some(1),
        ..Default::default()
    };
    assert_eq!(block_kind(&crash), None);
    // A sign-in failure is the user's to fix, even when also flagged transient.
    let auth = BlockContext {
        result: Some(TurnResult::Error),
        transient: true,
        auth_failed: true,
        ..Default::default()
    };
    assert_eq!(block_kind(&auth), Some(BlockKind::UserBlocked));
    assert!(CodeProvider
        .decide_now(&DecisionContext::BlockKind(crash))
        .is_none());
}

fn perm_info() -> AttentionInfo {
    AttentionInfo {
        attn_id: "p1".into(),
        reason: AttentionReason::Permission,
        options: vec!["allow".into(), "deny".into()],
        answerable: true,
    }
}

fn db_info() -> AttentionInfo {
    AttentionInfo {
        attn_id: "q1".into(),
        reason: AttentionReason::Question,
        options: vec!["Postgres".into(), "SQLite".into(), "MySQL".into()],
        answerable: true,
    }
}

/// Messages that must never answer an item (the relay rule): questions, conditions, hedges,
/// quotes, several sentences, mixed polarity, extra words.
const NOT_ANSWERS: &[&str] = &[
    // One of each kind.
    "yes?",
    "approve it?",
    "If tests pass, approve.",
    "approve if tests pass",
    "yes, but not the rm",
    "'yes'",
    "yes. no.",
    "sure, what is it?",
    // Questions.
    "allow?",
    "Yes?!",
    "ok, what is it doing?",
    "should I approve it?",
    "approve it? i am not sure",
    // Conditions and hedges.
    "once CI is green, approve",
    "approve after the build",
    "approve when ready",
    "approve it unless it touches prod",
    "assuming it is safe, yes",
    "yes as long as it is only build/",
    "yes but only in the tests dir",
    "approve it, except the migrations",
    "allow it only for this file",
    "first, allow it",
    "wait. yes",
    "wait, no",
    "maybe yes",
    "maybe. approve",
    "don't approve it",
    "not yet",
    "do not",
    "deny. it should not touch prod",
    // Quotes and punctuation.
    "\"yes\"",
    "\u{201c}approve\u{201d}",
    "`yes`",
    "yes!!",
    "yes...",
    "yes;",
    "yes, go ahead",
    // Several answers or sentences.
    "no. yes",
    "yes no",
    "yes\nno",
    "ok. show me the diff",
    "sure, after lunch",
    "yes. thanks",
    "yes please thanks",
    // Numbers inside other text.
    "bump the version to 1.2",
    "3 tests fail. 2",
    "1, 2",
    "option two",
    "# 2",
    // Not replies.
    "fix the login bug",
    "please",
    "thanks",
    "",
    "   ",
    "@api",
];

#[test]
fn explicit_answer_is_a_strict_whole_message_grammar() {
    use super::route::explicit_answer;
    let (perm, q) = (perm_info(), db_info());
    // Addresses are stripped by the caller (`addressee`, resolved against the registry),
    // never here.
    let addressed = ["@api yes", "on api: allow", "api: yes"];
    for msg in NOT_ANSWERS.iter().chain(&addressed) {
        assert_eq!(explicit_answer(&perm, msg), None, "permission: {msg:?}");
        assert_eq!(explicit_answer(&q, msg), None, "question: {msg:?}");
    }
    for (msg, want) in [
        ("yes", "allow"),
        ("Yes!", "allow"),
        ("y", "allow"),
        ("ok thanks", "allow"),
        ("OK, thanks!", "allow"),
        ("yes please", "allow"),
        ("yes, please.", "allow"),
        ("sure thank you", "allow"),
        ("yep thx", "allow"),
        ("okay ty", "allow"),
        ("Go  ahead.", "allow"),
        ("LGTM", "allow"),
        ("ship it!", "allow"),
        ("do it", "allow"),
        ("approve", "allow"),
        ("Approve it.", "allow"),
        ("approved", "allow"),
        ("allow", "allow"),
        ("deny.", "deny"),
        ("no thanks", "deny"),
        ("No.", "deny"),
        ("n", "deny"),
        ("nope", "deny"),
        ("denied", "deny"),
        ("reject", "deny"),
        ("don't", "deny"),
        ("Don\u{2019}t!", "deny"),
        ("stop", "deny"),
        ("deny it", "deny"),
    ] {
        assert_eq!(explicit_answer(&perm, msg), Some(label(want)), "{msg:?}");
    }
    // Option numbers: `N`, `#N`, `option N`, optionally with courtesy / a terminal mark.
    for (msg, i) in [
        ("1", 0),
        ("2", 1),
        ("2.", 1),
        ("#1", 0),
        ("option 2", 1),
        ("Option 2!", 1),
        ("option 2, thanks", 1),
    ] {
        assert_eq!(
            explicit_answer(&perm, msg),
            Some(ReplyAnswer::OptionIndex(i)),
            "{msg:?}"
        );
    }
    assert_eq!(explicit_answer(&perm, "3"), None);
    assert_eq!(explicit_answer(&perm, "0"), None);
    assert_eq!(explicit_answer(&perm, "99999999999999999999999999"), None);
    // A question: exact labels (case / whitespace), numbers, yes/no only via a label that
    // reads that way.
    assert_eq!(explicit_answer(&q, "SQLite"), Some(label("SQLite")));
    assert_eq!(explicit_answer(&q, "sqlite."), Some(label("SQLite")));
    assert_eq!(explicit_answer(&q, "  mysql please"), Some(label("MySQL")));
    assert_eq!(explicit_answer(&q, "2"), Some(ReplyAnswer::OptionIndex(1)));
    assert_eq!(
        explicit_answer(&q, "option 3"),
        Some(ReplyAnswer::OptionIndex(2))
    );
    assert_eq!(explicit_answer(&q, "4"), None);
    assert_eq!(explicit_answer(&q, "yes"), None);
    assert_eq!(explicit_answer(&q, "sqlite?"), None);
    assert_eq!(explicit_answer(&q, "sqlite, then postgres"), None);
    assert_eq!(explicit_answer(&q, "use sqlite"), None);
    assert_eq!(explicit_answer(&q, "postgres, sqlite"), None);
    let yn = AttentionInfo {
        attn_id: "q2".into(),
        reason: AttentionReason::Question,
        options: vec!["Yes, proceed".into(), "No".into()],
        answerable: true,
    };
    assert_eq!(explicit_answer(&yn, "ok"), Some(label("Yes, proceed")));
    assert_eq!(
        explicit_answer(&yn, "yes, proceed"),
        Some(label("Yes, proceed"))
    );
    assert_eq!(explicit_answer(&yn, "no thanks"), Some(label("No")));
    // An option label typed in full counts whatever its words; a `?` the label does not have
    // does not.
    let odd = AttentionInfo {
        attn_id: "q3".into(),
        reason: AttentionReason::Question,
        options: vec!["Yes, but only staging".into(), "Is it safe?".into()],
        answerable: true,
    };
    assert_eq!(
        explicit_answer(&odd, "yes, but only staging"),
        Some(label("Yes, but only staging"))
    );
    assert_eq!(explicit_answer(&odd, "yes, but only staging?"), None);
    assert_eq!(
        explicit_answer(&odd, "is it safe?"),
        Some(label("Is it safe?"))
    );
    assert_eq!(explicit_answer(&odd, "no but ask me later"), None);
    // A permission without options: yes/no only; one listing other options: exact labels.
    let bare = AttentionInfo {
        options: vec![],
        ..perm_info()
    };
    assert_eq!(explicit_answer(&bare, "yes"), Some(label("allow")));
    assert_eq!(explicit_answer(&bare, "1"), None);
    let once = AttentionInfo {
        options: vec!["allow_once".into(), "deny".into()],
        ..perm_info()
    };
    assert_eq!(explicit_answer(&once, "yes"), None);
    assert_eq!(
        explicit_answer(&once, "allow_once"),
        Some(label("allow_once"))
    );
}

#[test]
fn say_never_answers_without_a_strict_answer() {
    // api is focused and blocked on a permission; web is the other worker.
    let blocked = || {
        vec![
            orchestrator(),
            with_attn(
                thread("t1", "api"),
                "p1",
                AttentionReason::Permission,
                &["allow", "deny"],
            ),
            thread("t2", "web"),
        ]
    };
    for msg in NOT_ANSWERS {
        let got = route(&ctx(msg, blocked(), Some("t1")));
        assert!(
            !matches!(got, Some(RouteOutcome::RespondAttention { .. })),
            "{msg:?}: {got:?}"
        );
    }
    // Reply-shaped but not strict: abstain (neither an answer nor text to the blocked thread),
    // focused, addressed, or as the only worker.
    let only = || {
        vec![
            orchestrator(),
            with_attn(
                thread("t1", "api"),
                "p1",
                AttentionReason::Permission,
                &["allow", "deny"],
            ),
        ]
    };
    for msg in [
        "yes?",
        "approve it?",
        "If tests pass, approve.",
        "approve if tests pass",
        "yes, but not the rm",
        "'yes'",
        "yes. no.",
        "sure, what is it?",
    ] {
        assert_eq!(route(&ctx(msg, blocked(), Some("t1"))), None, "{msg:?}");
        assert_eq!(route(&ctx(msg, only(), Some("t1"))), None, "{msg:?}");
        assert_eq!(route(&ctx(msg, only(), None)), None, "{msg:?}");
        let addressed = format!("@api {msg}");
        assert_eq!(
            route(&ctx(&addressed, blocked(), Some("t2"))),
            None,
            "{addressed:?}"
        );
    }
    // Strict answers.
    let r = |m: &str, focus: &str| route(&ctx(m, blocked(), Some(focus)));
    assert_eq!(r("ok thanks", "t1"), respond("t1", "p1", label("allow")));
    assert_eq!(
        r("@api yes please", "t2"),
        respond("t1", "p1", label("allow"))
    );
    assert_eq!(r("on api, yes.", "t2"), respond("t1", "p1", label("allow")));
    assert_eq!(
        r("api: approve it", "t2"),
        respond("t1", "p1", label("allow"))
    );
    assert_eq!(r("deny.", "t1"), respond("t1", "p1", label("deny")));
    assert_eq!(r("no thanks", "t1"), respond("t1", "p1", label("deny")));
    assert_eq!(
        r("2", "t1"),
        respond("t1", "p1", ReplyAnswer::OptionIndex(1))
    );
    assert_eq!(
        r("option 2", "t1"),
        respond("t1", "p1", ReplyAnswer::OptionIndex(1))
    );
    // An unresolved address is part of the message: no answer.
    assert_eq!(r("@nobody yes", "t1"), None);
    // Instructions that do not look like answers keep their routes: to the addressed thread
    // as text, or (unfocused, single worker) to that worker; a long one even if it says yes.
    assert_eq!(
        r("@api use the staging db instead", "t2"),
        send("t1", "use the staging db instead")
    );
    assert_eq!(
        route(&ctx("look at the flaky test", only(), None)),
        send("t1", "look at the flaky test")
    );
    let long = "yes and then after that please also rebuild the docs and rerun every test";
    assert_eq!(route(&ctx(long, only(), None)), send("t1", long));
    // A question's labels.
    let q = || {
        vec![
            with_attn(
                thread("t1", "api"),
                "q1",
                AttentionReason::Question,
                &["Postgres", "SQLite"],
            ),
            thread("t2", "web"),
        ]
    };
    let rq = |m: &str| route(&ctx(m, q(), Some("t1")));
    assert_eq!(rq("SQLite"), respond("t1", "q1", label("SQLite")));
    assert_eq!(rq("sqlite."), respond("t1", "q1", label("SQLite")));
    assert_eq!(rq("2"), respond("t1", "q1", ReplyAnswer::OptionIndex(1)));
    assert_eq!(rq("sqlite?"), None);
    assert_eq!(rq("sqlite, if it is installed"), None);
    // Without an open item, reply-shaped messages still go to the focused thread as text.
    assert_eq!(route(&ctx("yes?", two(), Some("t2"))), send("t2", "yes?"));
}

/// `twr say` (nothing on screen) with an answer-shaped message while a thread waits on a
/// permission/question: the message is an answer attempt (never routed), whether strict,
/// answer-shaped or addressed; anything else routes as usual.
#[test]
fn unseen_answers_are_recognized() {
    use super::route::unseen_answer;
    let threads = || {
        vec![
            orchestrator(),
            with_attn(
                thread("t1", "api"),
                "p1",
                AttentionReason::Permission,
                &["allow", "deny"],
            ),
            thread("t2", "web"),
        ]
    };
    for m in ["yes", "no", "allow", "1", "yes?", "ok, if tests pass"] {
        assert_eq!(
            unseen_answer(&ctx(m, threads(), None), m),
            Some(None),
            "{m}"
        );
    }
    assert_eq!(
        unseen_answer(&ctx("@api yes", threads(), None), "@api yes"),
        Some(Some("t1".into()))
    );
    // Addressed to a thread with nothing pending, a new thread, or plain instructions: routed.
    for m in [
        "@web yes",
        "new proj: yes",
        "refactor the auth module so that sessions expire after an hour of inactivity please",
        "deploy",
    ] {
        assert_eq!(unseen_answer(&ctx(m, threads(), None), m), None, "{m}");
    }
    // Nothing pending anywhere: routed.
    let calm = vec![thread("t1", "api"), thread("t2", "web")];
    assert_eq!(unseen_answer(&ctx("yes", calm, None), "yes"), None);
    // A review (not blocking) does not count.
    let review = vec![with_attn(
        thread("t1", "api"),
        "r1",
        AttentionReason::Review,
        &["dismiss"],
    )];
    assert_eq!(unseen_answer(&ctx("yes", review, None), "yes"), None);
}

/// Quantified and compound answers are answer attempts too (`twr say "allow both"` answers
/// nothing and lists every pending item), whatever their length.
#[test]
fn unseen_compound_answers_are_recognized() {
    use super::route::unseen_answer;
    let threads = || {
        vec![
            orchestrator(),
            with_attn(
                thread("t1", "api"),
                "p1",
                AttentionReason::Permission,
                &["allow", "deny"],
            ),
            with_attn(
                thread("t2", "web"),
                "p2",
                AttentionReason::Permission,
                &["allow", "deny"],
            ),
        ]
    };
    for m in [
        "allow both",
        "Allow both, and set them both to audo mode",
        "approve all permissions",
        "no to all",
        "@api yes, @web no",
        "allow @api and @web",
        // The relay refuses bare custom names, but `say` still sees an attempt at answering.
        "allow api and web",
        "allow api, and then please write a short summary of what each of the two threads changed",
        "allow both, and then please write a short summary of what each of the two threads changed",
    ] {
        assert_eq!(
            unseen_answer(&ctx(m, threads(), None), m),
            Some(None),
            "{m}"
        );
    }
    // Nothing pending: routed.
    let calm = vec![thread("t1", "api"), thread("t2", "web")];
    assert_eq!(
        unseen_answer(&ctx("allow both", calm, None), "allow both"),
        None
    );
}

/// A completed turn whose every tool call failed or was denied (and none succeeded) is
/// `partial` (0.8), not the default success; a turn with one success, or no tool calls, is not.
#[test]
fn a_turn_whose_every_tool_call_failed_is_partial() {
    use TurnResult::*;
    let with_tools = |result, calls, failures| GradeContext {
        result,
        final_text: Some("Done.".into()),
        tool_calls: calls,
        tool_failures: failures,
        ..Default::default()
    };
    assert_eq!(
        grade(&with_tools(Completed, 1, 1)),
        Some((Grade::Partial, 0.8))
    );
    assert_eq!(
        grade(&with_tools(Completed, 3, 3)),
        Some((Grade::Partial, 0.8))
    );
    assert_eq!(grade(&with_tools(Completed, 3, 2)), None);
    assert_eq!(grade(&with_tools(Completed, 0, 0)), None);
    // Other rules keep their precedence.
    assert_eq!(grade(&with_tools(Error, 1, 1)), Some((Grade::GaveUp, 1.0)));
    let mut gave_up = with_tools(Completed, 1, 1);
    gave_up.final_text = Some("I was unable to write the file".into());
    assert_eq!(grade(&gave_up), Some((Grade::GaveUp, 0.9)));
    let d = CodeProvider
        .decide_now(&DecisionContext::Grade(with_tools(Completed, 2, 2)))
        .unwrap();
    assert_eq!((d.chosen.as_str(), d.confidence), ("partial", 0.8));
}
