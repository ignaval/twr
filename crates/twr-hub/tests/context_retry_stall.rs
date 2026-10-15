//! Hub behaviors: standing project context, the block_kind decision with its one automatic
//! retry, and the stall timer.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::blob::blob_ref;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::{injection, ThreadSubscription, RETRY_PROMPT};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// Prompts a [`Recorder`] runtime received, in order.
type Prompts = Arc<Mutex<Vec<String>>>;

/// Runtime `rec`: records every prompt and completes each turn at once.
struct Recorder(Prompts);

impl Adapter for Recorder {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "rec",
            tier: Tier::Headless,
            description: "records prompts",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        let prompts = Arc::clone(&self.0);
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        tokio::spawn(serve(end, prompts, first));
        Ok(h)
    }
}

async fn serve(mut end: AdapterEnd, prompts: Prompts, first: Option<(String, u64)>) {
    end.emit(
        EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-rec".into()),
            ..Default::default()
        }),
        None,
    );
    let turn = |text: String, turn: u64, end: &AdapterEnd| {
        prompts.lock().unwrap().push(text);
        end.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
    };
    if let Some((p, t)) = first {
        turn(p, t, &end);
    }
    while let Some(cmd) = end.cmds.recv().await {
        match cmd {
            AdapterCmd::Send { text, turn: t } => turn(text, t, &end),
            AdapterCmd::Stop | AdapterCmd::Interrupt => break,
            _ => {}
        }
    }
    end.exited(Some(0), "");
}

fn opts(dir: &Path, prompts: &Prompts) -> HubOptions {
    let mut config = Config::default();
    config.orchestrator.runtime = Some("rec".into());
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    let rec: Arc<dyn Adapter> = Arc::new(Recorder(Arc::clone(prompts)));
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "rec" => Some(Arc::clone(&rec)),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

async fn new_thread(hub: &Hub, project: &Path, name: &str, runtime: &str) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some(runtime.into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn send(hub: &Hub, thread: &str, prompt: &str) {
    hub.send(thread, prompt, TurnFrom::User, Source::User("test".into()))
        .unwrap();
}

async fn until(sub: &mut ThreadSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            let done = pred(&ev);
            seen.push((*ev).clone());
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {:?}", kinds(&seen)));
    seen
}

fn kinds(evs: &[Event]) -> Vec<String> {
    evs.iter().map(|e| e.kind().to_string()).collect()
}

fn ended_turn(n: u64) -> impl Fn(&Event) -> bool {
    move |e| e.kind() == "turn.ended" && e.turn == Some(n)
}

/// Waits until `f` holds (polling).
async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// `context.updated` events (on the `hub` thread, via the control stream).
fn context_updates(hub: &Hub) -> Vec<Event> {
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    items
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| e.kind() == "context.updated")
        .inspect(|e| assert_eq!(e.thread, HUB_THREAD))
        .collect()
}

fn turn_started(ev: &Event) -> &TurnStarted {
    match &ev.body {
        EventBody::TurnStarted(p) => p,
        other => panic!("not turn.started: {other:?}"),
    }
}

fn project_name(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------------------------
// Standing context
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn standing_context_is_injected_and_manual_edits_are_logged() {
    let dir = tempfile::tempdir().unwrap();
    let prompts = Prompts::default();
    let hub = Hub::open(opts(dir.path(), &prompts)).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "rec").await;
    let project = project_name(dir.path());
    let file = dir.path().join(".twr").join("context.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "Use pnpm, never npm.\n").unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();

    send(&hub, &t, "build it");
    let evs = until(&mut sub, ended_turn(1)).await;
    let expected = injection(&project, "Use pnpm, never npm.\n", None);
    assert!(expected.starts_with(&format!("<twr-context project=\"{project}\">\n")));
    assert_eq!(
        prompts.lock().unwrap().clone(),
        [format!("{expected}build it")]
    );
    // The log keeps the prompt as given plus the hash of the context that was prepended.
    let started = evs.iter().find(|e| e.kind() == "turn.started").unwrap();
    let p = turn_started(started);
    assert_eq!(p.prompt, "build it");
    let hash = blob_ref(b"Use pnpm, never npm.\n");
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["context_hash"], json!(hash));
    // The manual edit was captured first (on the hub thread, bytes in the blob store).
    let updates = context_updates(&hub);
    assert_eq!(updates.len(), 1);
    let EventBody::ContextUpdated(u) = &updates[0].body else {
        panic!()
    };
    assert_eq!(u.hash, hash);
    let twr = &u.ext.as_ref().unwrap()["twr"];
    assert_eq!(twr["project"], json!(project));
    assert_eq!(twr["origin"], "file");
    assert_eq!(updates[0].source, Source::Hub);
    assert_eq!(hub.blob(&hash).unwrap().unwrap(), b"Use pnpm, never npm.\n");
    assert!(updates[0].ts <= started.ts);

    // Unchanged: nothing new is logged. Edited by hand: logged again before the turn.
    send(&hub, &t, "again");
    until(&mut sub, ended_turn(2)).await;
    assert_eq!(context_updates(&hub).len(), 1);
    std::fs::write(&file, "Use yarn.").unwrap();
    send(&hub, &t, "third");
    let evs = until(&mut sub, ended_turn(3)).await;
    let updates = context_updates(&hub);
    assert_eq!(updates.len(), 2);
    let EventBody::ContextUpdated(u) = &updates[1].body else {
        panic!()
    };
    assert_eq!(u.hash, blob_ref(b"Use yarn."));
    let p = turn_started(evs.iter().find(|e| e.kind() == "turn.started").unwrap());
    assert_eq!(
        p.ext.as_ref().unwrap()["twr"]["context_hash"],
        json!(u.hash)
    );
    assert_eq!(
        prompts.lock().unwrap()[2],
        format!("{}third", injection(&project, "Use yarn.", None))
    );

    // Too big: cut to 64 KiB with a note, flagged on the turn.
    std::fs::write(&file, "z".repeat(70 * 1024)).unwrap();
    send(&hub, &t, "fourth");
    let evs = until(&mut sub, ended_turn(4)).await;
    let got = prompts.lock().unwrap()[3].clone();
    assert!(got.len() < 66 * 1024, "{}", got.len());
    assert!(got.contains("was cut to its first 65536 of 71680 bytes"));
    assert!(got.ends_with("</twr-context>\n\nfourth"));
    let p = turn_started(evs.iter().find(|e| e.kind() == "turn.started").unwrap());
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["context_truncated"], true);

    // Emptied: logged (the file changed), nothing injected, no hash on the turn.
    std::fs::write(&file, "").unwrap();
    send(&hub, &t, "fifth");
    let evs = until(&mut sub, ended_turn(5)).await;
    assert_eq!(prompts.lock().unwrap()[4], "fifth");
    let p = turn_started(evs.iter().find(|e| e.kind() == "turn.started").unwrap());
    // Only the message's ref.
    let ext = serde_json::to_value(p.ext.clone().unwrap()).unwrap();
    assert_eq!(ext["twr"].as_object().unwrap().len(), 1, "{ext}");
    assert!(ext["twr"]["ref"].is_i64(), "{ext}");
    assert_eq!(context_updates(&hub).len(), 4);
    hub.shutdown().await;
}

#[tokio::test]
async fn context_set_logs_then_writes_and_read_returns_it() {
    let dir = tempfile::tempdir().unwrap();
    let prompts = Prompts::default();
    let hub = Hub::open(opts(dir.path(), &prompts)).await.unwrap();
    let project = project_name(dir.path());
    // Unknown until a thread (or the config) names the project.
    let e = hub.context_read(&project).await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    let t = new_thread(&hub, dir.path(), "api", "rec").await;

    let empty = hub.context_read(&project).await.unwrap();
    assert_eq!(empty["content"], "");
    assert_eq!(empty["hash"], json!(blob_ref(b"")));

    let caller = Caller::user("cli");
    let r = hub
        .context_set(&project, "Deploys go through CI.\n", None, &caller)
        .await
        .unwrap();
    let hash = blob_ref(b"Deploys go through CI.\n");
    assert_eq!(r["changed"], true);
    assert_eq!(r["hash"], json!(hash));
    let file = dir.path().join(".twr").join("context.md");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "Deploys go through CI.\n"
    );
    let updates = context_updates(&hub);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].source, Source::User("cli".into()));
    let EventBody::ContextUpdated(u) = &updates[0].body else {
        panic!()
    };
    assert_eq!(u.hash, hash);
    assert_eq!(u.ext.as_ref().unwrap()["twr"]["origin"], "set");
    assert_eq!(
        hub.blob(&hash).unwrap().unwrap(),
        b"Deploys go through CI.\n"
    );
    let read = hub.context_read(&project).await.unwrap();
    assert_eq!(read["content"], "Deploys go through CI.\n");
    assert_eq!(read["hash"], json!(hash));
    assert_eq!(read["path"], json!(file));

    // Same content again: nothing to do. The next turn does not log it again either.
    let r = hub
        .context_set(&project, "Deploys go through CI.\n", None, &caller)
        .await
        .unwrap();
    assert_eq!(r["changed"], false);
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, ended_turn(1)).await;
    assert_eq!(context_updates(&hub).len(), 1);
    assert!(prompts.lock().unwrap()[0].starts_with("<twr-context"));

    // The log survives a restart: the file matches the last logged hash.
    hub.shutdown().await;
    drop((sub, hub));
    let hub = Hub::open(opts(dir.path(), &prompts)).await.unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go again");
    until(&mut sub, ended_turn(2)).await;
    assert_eq!(context_updates(&hub).len(), 1);
    let big = "x".repeat(twr_hub::hub::INJECT_MAX * 20);
    assert_eq!(
        hub.context_set(&project, &big, None, &caller)
            .await
            .unwrap_err()
            .code,
        codes::BAD_REQUEST
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn the_orchestrator_gets_no_standing_context() {
    let dir = tempfile::tempdir().unwrap();
    let prompts = Prompts::default();
    let hub = Hub::open(opts(dir.path(), &prompts)).await.unwrap();
    new_thread(&hub, dir.path(), "api", "rec").await;
    new_thread(&hub, dir.path(), "web", "rec").await;
    let project = project_name(dir.path());
    hub.context_set(&project, "Standing rules.", None, &Caller::user("cli"))
        .await
        .unwrap();
    // Two workers and an unaddressed message: the orchestrator gets it, verbatim.
    let r = hub
        .say("coordinate the release", None, &Caller::user("cli"))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    eventually("the orchestrator's prompt", || {
        !prompts.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(prompts.lock().unwrap().clone(), ["coordinate the release"]);
    let orch = hub.orchestrator_thread().unwrap();
    let (_, evs) = hub.thread_events(&orch, 0, 100).unwrap();
    let p = turn_started(evs.iter().find(|e| e.kind() == "turn.started").unwrap());
    // No context; only when the message arrived (the relay guard's `said_after`), and the
    // message's ref.
    let ext = serde_json::to_value(p.ext.clone().unwrap()).unwrap();
    assert_eq!(ext["twr"].as_object().unwrap().len(), 3, "{ext}");
    assert!(ext["twr"]["prompt_fingerprint"].is_string(), "{ext}");
    assert!(ext["twr"]["said_after"].is_u64(), "{ext}");
    assert!(ext["twr"]["ref"].is_i64(), "{ext}");
    // A worker of the project does get it.
    let mut sub = hub.subscribe_thread("api", 0).unwrap();
    send(&hub, "api", "ship it");
    until(&mut sub, ended_turn(1)).await;
    assert_eq!(
        prompts.lock().unwrap()[1],
        format!("{}ship it", injection(&project, "Standing rules.", None))
    );
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Block kind and the automatic retry
// ---------------------------------------------------------------------------------------------

fn decisions(evs: &[Event], site: DecisionSite) -> Vec<(Option<u64>, DecisionMade)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::DecisionMade(d) if d.site == site => Some((e.turn, d.clone())),
            _ => None,
        })
        .collect()
}

fn raised(evs: &[Event]) -> Vec<(Option<u64>, AttentionRaised)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some((e.turn, a.clone())),
            _ => None,
        })
        .collect()
}

fn retries(evs: &[Event]) -> Vec<(u64, u64)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => {
                let of = p.ext.as_ref()?.get("twr")?.get("retry_of")?.as_u64()?;
                Some((e.turn?, of))
            }
            _ => None,
        })
        .collect()
}

fn fake_opts(dir: &Path) -> HubOptions {
    opts(dir, &Prompts::default())
}

#[tokio::test]
async fn a_transient_failure_is_retried_once() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(fake_opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "working\n!transient");
    let evs = until(&mut sub, ended_turn(2)).await;
    let d = decisions(&evs, DecisionSite::BlockKind);
    assert_eq!(d.len(), 1);
    assert_eq!(
        (d[0].0, d[0].1.provider, d[0].1.chosen.as_str()),
        (Some(1), DecisionProvider::Code, "transient")
    );
    let retry = evs
        .iter()
        .find(|e| e.kind() == "turn.started" && e.turn == Some(2))
        .unwrap();
    assert_eq!(retry.source, Source::Hub);
    let p = turn_started(retry);
    assert_eq!(p.prompt, RETRY_PROMPT);
    assert_eq!(p.from, TurnFrom::User);
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["retry_of"], 1);
    let EventBody::TurnEnded(e) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Completed);
    // No error or review item for a failure that is being retried.
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert!(raised(&all).is_empty(), "{:?}", raised(&all));
    assert_eq!(retries(&all), [(2, 1)]);
    // The retry is the one exception to "one grade per turn": both turns are graded.
    assert_eq!(decisions(&all, DecisionSite::Grade).len(), 2);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_retry_that_fails_again_raises_an_error_item() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(fake_opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    // The first turn and its retry both fail transiently.
    send(&hub, &t, "!transient 2");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let d = decisions(&evs, DecisionSite::BlockKind);
    assert_eq!(
        d.iter()
            .map(|(t, d)| (t.unwrap(), d.chosen.as_str()))
            .collect::<Vec<_>>(),
        [(1, "transient"), (2, "transient")]
    );
    let r = raised(&evs);
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].0, r[0].1.reason), (Some(2), AttentionReason::Error));
    assert_eq!(
        r[0].1.summary,
        "api: turn 2 failed again after an automatic retry: fake transient error (again)"
    );
    // Nothing else runs; the thread waits for the user.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.turns, v.current_turn), (2, None));
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(retries(&all), [(2, 1)]);
    // A new prompt from the user is not preceded by another retry.
    send(&hub, &t, "try something else");
    until(&mut sub, ended_turn(3)).await;
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(retries(&all), [(2, 1)]);
    assert_eq!(
        turn_started(all.iter().rfind(|e| e.kind() == "turn.started").unwrap()).prompt,
        "try something else"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_user_blocked_failure_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(fake_opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!error the schema is wrong");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let d = decisions(&evs, DecisionSite::BlockKind);
    assert_eq!(d.len(), 1);
    assert_eq!(
        (d[0].1.provider, d[0].1.chosen.as_str()),
        (DecisionProvider::Code, "user_blocked")
    );
    let r = raised(&evs);
    assert_eq!((r[0].0, r[0].1.reason), (Some(1), AttentionReason::Error));
    assert_eq!(r[0].1.summary, "api: turn 1 failed: the schema is wrong");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert!(retries(&all).is_empty());
    assert_eq!(raised(&all).len(), 1, "an error item, no review");
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_retry_due_across_a_restart_starts_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let (one, two);
    {
        let hub = Hub::open(fake_opts(dir.path())).await.unwrap();
        one = new_thread(&hub, dir.path(), "one", "fake").await;
        two = new_thread(&hub, dir.path(), "two", "fake").await;
        let fake = Source::Adapter("fake".into());
        let failed = EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Error,
            ext: json!({"fake": {"transient": true, "message": "overloaded"}})
                .as_object()
                .cloned(),
            ..Default::default()
        });
        for t in [&one, &two] {
            hub.append(
                t,
                Some(1),
                Source::User("test".into()),
                EventBody::TurnStarted(TurnStarted {
                    prompt: "go".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
            hub.append(t, Some(1), fake.clone(), failed.clone(), None)
                .unwrap();
        }
        // Thread two got as far as its block_kind decision; thread one did not.
        hub.append(
            &two,
            Some(1),
            Source::Decision("code".into()),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::BlockKind,
                provider: DecisionProvider::Code,
                chosen: "transient".into(),
                confidence: 1.0,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.shutdown().await;
    }
    for round in 0..2 {
        let hub = Hub::open(fake_opts(dir.path())).await.unwrap();
        for t in [&one, &two] {
            if round == 0 {
                let mut sub = hub.subscribe_thread(t, 0).unwrap();
                until(&mut sub, ended_turn(2)).await;
            }
            let (_, all) = hub.thread_events(t, 0, 1000).unwrap();
            assert_eq!(retries(&all), [(2, 1)], "round {round}");
            assert_eq!(
                decisions(&all, DecisionSite::BlockKind).len(),
                1,
                "round {round}"
            );
            assert!(raised(&all).is_empty(), "round {round}");
        }
        hub.shutdown().await;
    }
}

// ---------------------------------------------------------------------------------------------
// Stall timer
// ---------------------------------------------------------------------------------------------

fn stall_opts(dir: &Path) -> HubOptions {
    let mut o = fake_opts(dir);
    o.stall_after = Some(Duration::from_millis(300));
    o
}

#[tokio::test]
async fn a_silent_turn_is_stalled_until_activity_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(stall_opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 1200\ndone");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let (turn, a) = raised(&evs).pop().unwrap();
    assert_eq!(turn, Some(1));
    assert_eq!(a.reason, AttentionReason::Stalled);
    assert_eq!(a.summary, "No activity for 1 s");
    assert_eq!(evs.last().unwrap().source, Source::Hub);
    // One item per thread, however long the silence lasts; it closes when output arrives.
    let evs = until(&mut sub, |e| e.kind() == "attention.resolved").await;
    assert!(raised(&evs).is_empty());
    let EventBody::AttentionResolved(r) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(
        (r.attn_id.as_str(), r.by),
        (a.attn_id.as_str(), ResolvedBy::Timeout)
    );
    assert_eq!(
        evs.len(),
        2,
        "the first new event, then the resolution: {:?}",
        kinds(&evs)
    );
    until(&mut sub, ended_turn(1)).await;
    assert!(hub.attention_list(true).is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn a_turn_waiting_on_the_user_is_not_stalled() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(stall_opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!perm Bash ls\ndone");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let (_, perm) = raised(&evs).pop().unwrap();
    assert_eq!(perm.reason, AttentionReason::Permission);
    tokio::time::sleep(Duration::from_millis(900)).await;
    let items = hub.attention_list(true);
    assert_eq!(items.len(), 1, "{items:?}");
    hub.respond(&perm.attn_id, Answer::new("allow"), "test")
        .unwrap();
    let evs = until(&mut sub, ended_turn(1)).await;
    assert!(raised(&evs).is_empty());
    hub.shutdown().await;
}
