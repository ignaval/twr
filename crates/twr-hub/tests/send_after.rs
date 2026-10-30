//! Prompts that wait for other threads (`hub::after`): sent once each finished its next turn,
//! a card when one did not finish well, cancelled, and kept across a restart.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(30);

fn opts(dir: &Path) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.disk_tick = Some(Duration::ZERO);
    o
}

async fn thread(hub: &Hub, dir: &Path, name: &str) -> String {
    let project = dir.join(name);
    std::fs::create_dir_all(&project).unwrap();
    let t = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
}

fn user() -> Caller {
    Caller::user("test")
}

fn send(hub: &Hub, t: &str, prompt: &str) {
    hub.send_by(t, prompt, &user()).unwrap();
}

/// The prompts turns of `t` started with.
fn prompts(hub: &Hub, t: &str) -> Vec<String> {
    let (_, evs) = hub.thread_events(t, 0, 10_000).unwrap();
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(p.prompt.clone()),
            _ => None,
        })
        .collect()
}

async fn until_prompted(hub: &Hub, t: &str, n: usize) -> Vec<String> {
    timeout(WAIT, async {
        loop {
            let p = prompts(hub, t);
            if p.len() >= n {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("no prompt came")
}

/// The ends recorded on `t` of gated prompts: `(id, outcome)`.
fn ends(hub: &Hub, t: &str) -> Vec<(String, String)> {
    let (_, evs) = hub.thread_events(t, 0, 10_000).unwrap();
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::ConversationUpdate(p) => {
                let end = &p.ext.as_ref()?.get("twr")?.get("send_after_end")?;
                Some((
                    end["id"].as_str()?.to_string(),
                    end["outcome"].as_str()?.to_string(),
                ))
            }
            _ => None,
        })
        .collect()
}

/// The open card about a gated prompt, if any.
fn card(hub: &Hub) -> Option<String> {
    hub.attention_list(true)
        .into_iter()
        .find(|i| i["thread"] == HUB_THREAD && i["options"] == serde_json::json!(["Send", "Drop"]))
        .and_then(|i| i["attn_id"].as_str().map(str::to_string))
}

#[tokio::test]
async fn a_prompt_goes_out_once_the_threads_it_waits_for_finished() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web, db) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
        thread(&hub, dir.path(), "db").await,
    );
    let r = hub
        .send_after_by(
            &api,
            "rebase on twr/web: {report:web}",
            &["web".into(), "db".into()],
            &user(),
        )
        .unwrap();
    assert_eq!(r["after"], serde_json::json!(["web", "db"]));
    let view = hub.get_thread(&api).unwrap();
    assert_eq!(
        view["queued_after"][0]["after"],
        serde_json::json!(["web", "db"])
    );
    assert_eq!(view["activity"], "queued until web, db finish");
    send(&hub, &web, "web is done");
    until_prompted(&hub, &web, 1).await;
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(prompts(&hub, &api).is_empty(), "db has not finished yet");
    send(&hub, &db, "db is done");
    let p = until_prompted(&hub, &api, 1).await;
    assert!(p[0].starts_with("rebase on twr/web: "), "{p:?}");
    assert!(p[0].contains("web is done"), "{p:?}");
    assert_eq!(ends(&hub, &api)[0].1, "sent");
    assert!(hub.get_thread(&api).unwrap().get("queued_after").is_none());
    hub.request_shutdown();
}

#[tokio::test]
async fn a_thread_that_failed_raises_a_card_and_send_sends_anyway() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    hub.send_after_by(&api, "go on", &["web".into()], &user())
        .unwrap();
    send(&hub, &web, "!error boom");
    let attn = timeout(WAIT, async {
        loop {
            if let Some(a) = card(&hub) {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("no card");
    assert!(prompts(&hub, &api).is_empty());
    // Only the user answers it.
    let err = hub
        .respond_by(&attn, Answer::new("Send"), &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(err.code, "relay_forbidden", "{err:?}");
    // Not an answer: the card and the prompt stay.
    let err = hub.respond(&attn, Answer::new("Snd"), "test").unwrap_err();
    assert_eq!(err.code, "bad_request", "{err:?}");
    assert_eq!(card(&hub).as_deref(), Some(attn.as_str()));
    hub.respond(&attn, Answer::new(" send "), "test").unwrap();
    let p = until_prompted(&hub, &api, 1).await;
    assert_eq!(p, ["go on"]);
    assert!(card(&hub).is_none());
    hub.request_shutdown();
}

#[tokio::test]
async fn drop_and_cancel_end_it_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    // Dropped from its card.
    hub.send_after_by(&api, "first", &["web".into()], &user())
        .unwrap();
    send(&hub, &web, "!error boom");
    let attn = timeout(WAIT, async {
        loop {
            if let Some(a) = card(&hub) {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("no card");
    hub.respond(&attn, Answer::new("Drop"), "test").unwrap();
    assert_eq!(ends(&hub, &api)[0].1, "dropped");
    // Cancelled by its id's prefix.
    let r = hub
        .send_after_by(&api, "second", &["web".into()], &user())
        .unwrap();
    let id = r["queued_after"].as_str().unwrap();
    hub.cancel_send_after(&id[..8]).unwrap();
    assert_eq!(
        ends(&hub, &api)[1],
        (id.to_string(), "cancelled".to_string())
    );
    send(&hub, &web, "web is done");
    until_prompted(&hub, &web, 2).await;
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(prompts(&hub, &api).is_empty());
    hub.request_shutdown();
}

#[tokio::test]
async fn a_prompt_waiting_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (api, web) = {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let api = thread(&hub, dir.path(), "api").await;
        let web = thread(&hub, dir.path(), "web").await;
        hub.send_after_by(&api, "after the restart", &["web".into()], &user())
            .unwrap();
        hub.request_shutdown();
        hub.shutdown().await;
        (api, web)
    };
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    assert!(hub.get_thread(&api).unwrap()["queued_after"].is_array());
    send(&hub, &web, "web is done");
    let p = until_prompted(&hub, &api, 1).await;
    assert_eq!(p, ["after the restart"]);
    hub.request_shutdown();
}

#[tokio::test]
async fn bad_requests_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let api = thread(&hub, dir.path(), "api").await;
    thread(&hub, dir.path(), "web").await;
    thread(&hub, dir.path(), "db").await;
    for (after, prompt, why) in [
        (vec!["api"], "x", "cannot wait for itself"),
        (
            vec!["web"],
            "{report:db}",
            "names no thread the prompt waits for",
        ),
        (vec!["nosuch"], "x", ""),
        (vec![], "x", "name 1 to"),
        (vec!["web"], "  ", "prompt is empty"),
    ] {
        let after: Vec<String> = after.into_iter().map(str::to_string).collect();
        let e = hub.send_after_by(&api, prompt, &after, &user());
        let e: twr_core::proto::RpcError = match e {
            Err(e) => e,
            Ok(v) => panic!("{after:?} {prompt:?}: {v}"),
        };
        assert!(e.message.contains(why), "{}", e.message);
    }
    let none: Value = hub.get_thread(&api).unwrap();
    assert!(none.get("queued_after").is_none());
    // Never in a circle, also through a third thread.
    hub.send_after_by(&api, "x", &["web".into()], &user())
        .unwrap();
    hub.send_after_by("web", "y", &["db".into()], &user())
        .unwrap();
    let e = hub
        .send_after_by("db", "z", &["api".into()], &user())
        .unwrap_err();
    assert!(
        e.message.contains("would wait for each other"),
        "{}",
        e.message
    );
    hub.request_shutdown();
}

#[tokio::test]
async fn a_thread_with_a_prompt_held_for_it_is_not_finished() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web, db) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
        thread(&hub, dir.path(), "db").await,
    );
    hub.send_after_by(&web, "web's own next step", &["db".into()], &user())
        .unwrap();
    hub.send_after_by(&api, "after web", &["web".into()], &user())
        .unwrap();
    // web finishes something else meanwhile: its held prompt is still to come.
    send(&hub, &web, "unrelated");
    until_prompted(&hub, &web, 1).await;
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(prompts(&hub, &api).is_empty());
    send(&hub, &db, "db is done");
    until_prompted(&hub, &web, 2).await;
    let p = until_prompted(&hub, &api, 1).await;
    assert_eq!(p, ["after web"]);
    hub.request_shutdown();
}

#[tokio::test]
async fn a_card_comes_for_a_failure_while_another_thread_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    thread(&hub, dir.path(), "db").await;
    hub.send_after_by(&api, "go on", &["web".into(), "db".into()], &user())
        .unwrap();
    // db never runs; web fails.
    send(&hub, &web, "!error boom");
    timeout(WAIT, async {
        while card(&hub).is_none() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("no card");
    hub.request_shutdown();
}

#[tokio::test]
async fn deleting_a_thread_it_waits_for_drops_it_for_good() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let api = thread(&hub, dir.path(), "api").await;
    let web = thread(&hub, dir.path(), "web").await;
    hub.send_after_by(&api, "go on", &["web".into()], &user())
        .unwrap();
    let card = hub
        .delete_threads(&["web".to_string()], &user())
        .await
        .unwrap();
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        card["revision"].as_u64(),
        &user(),
    )
    .unwrap();
    let end = timeout(WAIT, async {
        loop {
            if let Some(e) = ends(&hub, &api).pop() {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("not dropped");
    assert_eq!(end.1, "dropped");
    assert!(hub.get_thread(&api).unwrap().get("queued_after").is_none());
    assert!(hub.thread_view(&web).is_err());
    hub.request_shutdown();
}

#[tokio::test]
async fn reports_go_in_as_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web, db) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
        thread(&hub, dir.path(), "db").await,
    );
    hub.send_after_by(
        &api,
        "web: {report:web} db: {report:db}",
        &["web".into(), "db".into()],
        &user(),
    )
    .unwrap();
    // web's report holds db's placeholder, by db's id: it stays text.
    send(&hub, &web, &format!("see {{report:{db}}} here"));
    send(&hub, &db, "db is done");
    let p = until_prompted(&hub, &api, 1).await;
    assert_eq!(
        p[0],
        format!("web: see {{report:{db}}} here db: db is done"),
        "{p:?}"
    );
    hub.request_shutdown();
}

#[tokio::test]
async fn a_stop_drops_it_and_a_delete_card_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    // A delete card counts it among what goes.
    hub.send_after_by(&api, "first", &["web".into()], &user())
        .unwrap();
    let card = hub
        .delete_threads(&["api".to_string()], &user())
        .await
        .unwrap();
    assert_eq!(card["threads"][0]["pending"], 1, "{card}");
    hub.attention_dismiss(card["attn_id"].as_str().unwrap(), None, &user())
        .ok();
    // A stop drops it with the queue.
    let r = hub.stop_thread_by(&api, &user()).unwrap();
    assert_eq!(r["dropped_prompts"], 1, "{r}");
    assert_eq!(ends(&hub, &api).last().unwrap().1, "dropped");
    send(&hub, &web, "web is done");
    until_prompted(&hub, &web, 1).await;
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(prompts(&hub, &api).is_empty());
    hub.request_shutdown();
}

/// A thread a prompt waits for that is parked once it finished its turn counts at once,
/// although a prompt held for it (waiting for a thread that never runs) would keep a running
/// thread from counting: a turn that finished well sends the prompt, one that did not raises
/// the card.
#[tokio::test]
async fn a_parked_thread_counts_whatever_is_held_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web, ops, ci) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
        thread(&hub, dir.path(), "ops").await,
        thread(&hub, dir.path(), "ci").await,
    );
    thread(&hub, dir.path(), "db").await;
    // web and ci each have a prompt held for them until db finishes, which it never does.
    hub.send_after_by(&web, "web's own next step", &["db".into()], &user())
        .unwrap();
    hub.send_after_by(&ci, "ci's own next step", &["db".into()], &user())
        .unwrap();
    hub.send_after_by(&api, "after web", &["web".into()], &user())
        .unwrap();
    hub.send_after_by(&ops, "after ci", &["ci".into()], &user())
        .unwrap();
    send(&hub, &web, "web is done");
    send(&hub, &ci, "!error boom");
    for t in [&web, &ci] {
        let h = hub.clone();
        let t = t.clone();
        timeout(WAIT, async move {
            loop {
                let v = h.thread_view(&t).unwrap();
                if v.current_turn.is_none() && v.last_turn.is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("the turn never ended");
    }
    hub.park_thread(&web, false, false, &user()).await.unwrap();
    hub.park_thread(&ci, false, false, &user()).await.unwrap();
    // web finished well: api's prompt goes.
    let p = until_prompted(&hub, &api, 1).await;
    assert_eq!(p, ["after web"]);
    // ci failed: a card asks about ops's prompt, which stays held.
    let attn = timeout(WAIT, async {
        loop {
            if let Some(a) = card(&hub) {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("no card");
    let item = hub
        .attention_list(true)
        .into_iter()
        .find(|i| i["attn_id"] == attn.as_str())
        .unwrap();
    assert!(
        item["summary"]
            .as_str()
            .unwrap()
            .starts_with("ci did not finish well"),
        "{item}"
    );
    assert!(prompts(&hub, &ops).is_empty());
    hub.request_shutdown();
}

/// A prompt that waited for other threads is not the user's send when it goes out: queued on a
/// thread whose crash holds its queue, it waits with the rest until the user sends or resumes.
#[tokio::test]
async fn a_prompt_that_waited_does_not_release_a_crash_hold() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    send(&hub, &web, "!sleep 1500\nweb is done");
    hub.send_after_by(&api, "rebase on twr/web", &["web".into()], &user())
        .unwrap();
    send(&hub, &api, "!crash");
    timeout(WAIT, async {
        loop {
            let (_, evs) = hub.thread_events(&api, 0, 10_000).unwrap();
            if evs.iter().any(|e| e.kind() == "thread.crashed") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("api did not crash");
    timeout(WAIT, async {
        while ends(&hub, &api).is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the prompt never went out");
    assert_eq!(ends(&hub, &api)[0].1, "sent");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(prompts(&hub, &api).len(), 1);
    assert_eq!(hub.get_thread(&api).unwrap()["pending"], 1);
    send(&hub, &api, "now");
    let p = until_prompted(&hub, &api, 2).await;
    assert_eq!(p[1], "rebase on twr/web");
    hub.request_shutdown();
}

/// The user's Send on the card of a prompt that waited is the user sending it: queued on a
/// thread whose crash holds its queue, it goes out at once.
#[tokio::test]
async fn the_users_send_on_its_card_releases_a_crash_hold() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    hub.send_after_by(&api, "go on", &["web".into()], &user())
        .unwrap();
    send(&hub, &api, "!crash");
    timeout(WAIT, async {
        loop {
            let (_, evs) = hub.thread_events(&api, 0, 10_000).unwrap();
            if evs.iter().any(|e| e.kind() == "thread.crashed") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("api did not crash");
    send(&hub, &web, "!error boom");
    let attn = timeout(WAIT, async {
        loop {
            if let Some(a) = card(&hub) {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("no card");
    hub.respond(&attn, Answer::new("Send"), "test").unwrap();
    let p = until_prompted(&hub, &api, 2).await;
    assert_eq!(p[1], "go on");
    hub.request_shutdown();
}

/// A prompt that waited, let go into a busy thread's queue, holds the reports agents wrote:
/// `thread.queue` lists it as no send of the user's (`mine` false, never `newest_mine`, which
/// `↑` would put in the input box as theirs to edit), while the user's own send behind it is.
#[tokio::test]
async fn a_prompt_that_waited_is_not_the_users_in_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (api, web) = (
        thread(&hub, dir.path(), "api").await,
        thread(&hub, dir.path(), "web").await,
    );
    send(&hub, &api, "!sleep 60000");
    hub.send_after_by(&api, "web said: {report:web}", &["web".into()], &user())
        .unwrap();
    send(&hub, &web, "web is done");
    let q = timeout(WAIT, async {
        loop {
            let q = hub.thread_queue(&api, &user()).unwrap();
            if q["total"] == 1 {
                return q;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the prompt never went out");
    let item = &q["items"][0];
    assert_eq!(item["prompt"], "web said: web is done", "{q}");
    assert_eq!(
        (item["from"].as_str(), item["mine"].as_bool()),
        (Some("user"), Some(false)),
        "{q}"
    );
    assert!(q["newest_mine"].is_null(), "{q}");
    // The user's own send on the same thread is theirs.
    let mine = hub.send_by(&api, "and then this", &user()).unwrap();
    let q = hub.thread_queue(&api, &user()).unwrap();
    let mine_flags: Vec<Option<bool>> = q["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["mine"].as_bool())
        .collect();
    assert_eq!(mine_flags, [Some(false), Some(true)], "{q}");
    assert_eq!(q["newest_mine"].as_i64(), Some(mine.msg_ref), "{q}");
    hub.stop_thread(&api, "test").unwrap();
    hub.request_shutdown();
}
