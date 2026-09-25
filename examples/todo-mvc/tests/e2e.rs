//! End to end tests: the real server, the real database file, over HTTP.
//!
//! Each test starts `todo-server serve` on a new database in a folder of its
//! own, on a free port, and calls it the way a browser or a script would.
//! Every assertion checks a value the service returned, and a test that
//! passes removes its folder.
//!
//! Dates that decide "overdue" or "due this week" are fixed with `?today=`
//! and `?from=`, so the tests give the same answer on any day they run. The
//! due dates are years away from today for the same reason.

mod support;

use serde_json::{json, Value};
use support::{finish, id, scratch, titles, Server};

/// The TodoMVC flow: add, filter, complete, toggle all, clear completed,
/// reorder, rename, delete, and the data still there after a restart.
#[test]
fn the_todomvc_flow_works_and_survives_a_restart() {
    let folder = scratch("todomvc");
    let db = folder.join("todo.rdb");
    let server = Server::start(&db);
    assert_eq!(server.get("/health")["ok"], json!(true));

    let ada = server.person("Ada");
    let home = server.list(ada, "Home");
    let milk = server.todo(home, json!({ "title": "Buy milk" }));
    let fence = server.todo(home, json!({ "title": "  Paint the fence  " }));
    let taxes = server.todo(home, json!({ "title": "File taxes" }));

    let all = server.get(&format!("/lists/{home}/todos"));
    assert_eq!(titles(&all), ["Buy milk", "Paint the fence", "File taxes"], "new todos go to the end, and titles are trimmed");
    assert_eq!((all["all"].clone(), all["active"].clone(), all["completed"].clone()), (json!(3), json!(3), json!(0)));
    let positions: Vec<i64> = all["todos"].as_array().expect("todos").iter().map(|t| t["position"].as_i64().unwrap_or(0)).collect();
    assert_eq!(positions, [1, 2, 3]);

    let done = server.patch(&format!("/todos/{fence}"), json!({ "completed": true }));
    assert_eq!(done["completed"], json!(true));
    assert!(done["completed_at"].is_string(), "the todo_completed trigger stamps completed_at: {done}");
    assert_eq!(titles(&server.get(&format!("/lists/{home}/todos?status=active"))), ["Buy milk", "File taxes"]);
    let completed = server.get(&format!("/lists/{home}/todos?status=completed"));
    assert_eq!(titles(&completed), ["Paint the fence"]);
    assert_eq!((completed["active"].clone(), completed["completed"].clone()), (json!(2), json!(1)), "the counts ignore the filter");

    let reopened = server.patch(&format!("/todos/{fence}"), json!({ "completed": false }));
    assert_eq!(reopened["completed_at"], Value::Null, "reopening clears completed_at");

    assert_eq!(server.post(&format!("/lists/{home}/toggle-all"), json!({ "completed": true }))["changed"], json!(3));
    assert_eq!(
        server.post(&format!("/lists/{home}/toggle-all"), json!({ "completed": true }))["changed"],
        json!(0),
        "nothing left to change"
    );
    assert_eq!(server.post(&format!("/lists/{home}/toggle-all"), json!({ "completed": false }))["changed"], json!(3));

    server.patch(&format!("/todos/{milk}"), json!({ "completed": true }));
    assert_eq!(server.delete(&format!("/lists/{home}/completed"))["todos_deleted"], json!(1));
    assert_eq!(titles(&server.get(&format!("/lists/{home}/todos"))), ["Paint the fence", "File taxes"]);
    server.fails(404, "GET", &format!("/todos/{milk}"), None);

    let reordered = server.put(&format!("/lists/{home}/order"), json!({ "todo_ids": [taxes, fence] }));
    let order: Vec<&str> = reordered.as_array().expect("todos").iter().map(|t| t["title"].as_str().unwrap_or("")).collect();
    assert_eq!(order, ["File taxes", "Paint the fence"]);
    server.fails(409, "PUT", &format!("/lists/{home}/order"), Some(json!({ "todo_ids": [taxes] })));
    server.fails(409, "PUT", &format!("/lists/{home}/order"), Some(json!({ "todo_ids": [taxes, fence, 999] })));

    assert_eq!(server.patch(&format!("/lists/{home}"), json!({ "name": "House" }))["name"], json!("House"));
    assert_eq!(server.delete(&format!("/todos/{taxes}"))["todos_deleted"], json!(1));

    drop(server);
    let server = Server::start(&db);
    let after = server.get(&format!("/lists/{home}/todos"));
    assert_eq!(titles(&after), ["Paint the fence"], "the data is in the file, not in the process");
    assert_eq!(after["todos"][0]["list_name"], json!("House"));
    assert_eq!(server.delete(&format!("/lists/{home}"))["todos_deleted"], json!(1));
    server.fails(404, "GET", &format!("/lists/{home}"), None);
    finish(server, &folder);
}

/// Subtasks: the tree, its order, its progress, the breadcrumb, moving a
/// subtree to another list, the checks that keep the tree a tree, and
/// deleting a subtree.
#[test]
fn subtasks_form_a_tree_that_moves_and_deletes_together() {
    let folder = scratch("subtasks");
    let server = Server::start(&folder.join("todo.rdb"));
    let ada = server.person("Ada");
    let home = server.list(ada, "Home");
    let work = server.list(ada, "Work");

    let fence = server.todo(home, json!({ "title": "Paint the fence" }));
    let sand = server.todo(home, json!({ "title": "Sand the boards", "parent_id": fence }));
    let prime = server.todo(home, json!({ "title": "Prime the boards", "parent_id": fence }));
    let paper = server.todo(home, json!({ "title": "Buy sandpaper", "parent_id": sand }));
    let grit = server.todo(home, json!({ "title": "Pick a grit", "parent_id": paper }));
    server.patch(&format!("/todos/{prime}"), json!({ "completed": true }));

    let detail = server.get(&format!("/todos/{fence}"));
    let tree: Vec<(i64, i64)> = detail["subtask_tree"]
        .as_array()
        .expect("a tree")
        .iter()
        .map(|n| (n["id"].as_i64().unwrap_or(0), n["depth"].as_i64().unwrap_or(0)))
        .collect();
    assert_eq!(tree, [(sand, 1), (paper, 2), (grit, 3), (prime, 1)], "each todo comes straight after its parent");
    assert_eq!(detail["progress"], json!({ "total": 4, "done": 1 }));
    assert_eq!((detail["subtasks"].clone(), detail["subtasks_done"].clone()), (json!(2), json!(1)), "the card counts direct subtasks only");

    let leaf = server.get(&format!("/todos/{grit}"));
    let breadcrumb: Vec<&str> =
        leaf["ancestors"].as_array().expect("ancestors").iter().map(|a| a["title"].as_str().unwrap_or("")).collect();
    assert_eq!(breadcrumb, ["Paint the fence", "Sand the boards", "Buy sandpaper"]);

    assert_eq!(titles(&server.get(&format!("/lists/{home}/todos"))), ["Paint the fence"], "subtasks are hidden by default");
    assert_eq!(server.get(&format!("/lists/{home}/todos?subtasks=true"))["todos"].as_array().map(Vec::len), Some(5));

    server.fails(409, "PATCH", &format!("/todos/{fence}"), Some(json!({ "parent_id": grit })));
    server.fails(409, "PATCH", &format!("/todos/{fence}"), Some(json!({ "parent_id": fence })));
    let elsewhere = server.todo(work, json!({ "title": "Write the report" }));
    server.fails(409, "POST", &format!("/lists/{home}/todos"), Some(json!({ "title": "x", "parent_id": elsewhere })));

    let moved = server.patch(&format!("/todos/{sand}"), json!({ "list_id": work }));
    assert_eq!(
        (moved["list_id"].clone(), moved["parent_id"].clone()),
        (json!(work), Value::Null),
        "a moved subtask becomes a top level todo"
    );
    for todo in [paper, grit] {
        assert_eq!(server.get(&format!("/todos/{todo}"))["list_id"], json!(work), "todo {todo} moved with its parent");
    }
    assert_eq!(server.get(&format!("/todos/{fence}"))["progress"], json!({ "total": 1, "done": 1 }));
    assert_eq!(titles(&server.get(&format!("/lists/{work}/todos"))), ["Write the report", "Sand the boards"]);

    let nested = server.patch(&format!("/todos/{sand}"), json!({ "parent_id": elsewhere }));
    assert_eq!(nested["parent_id"], json!(elsewhere));
    assert_eq!(server.delete(&format!("/todos/{elsewhere}"))["todos_deleted"], json!(4));
    for todo in [sand, paper, grit] {
        server.fails(404, "GET", &format!("/todos/{todo}"), None);
    }
    assert_eq!(server.get("/search?q=sandpaper").as_array().map(Vec::len), Some(0), "the search entries of the whole subtree are gone");
    finish(server, &folder);
}

/// The rules the schema declares come back as HTTP errors with the engine's
/// message, and the foreign keys cascade.
#[test]
fn the_schema_enforces_its_rules() {
    let folder = scratch("rules");
    let server = Server::start(&folder.join("todo.rdb"));
    let ada = server.person("Ada");
    let grace = server.person("Grace");
    let home = server.list(ada, "Home");

    let duplicate = server.fails(409, "POST", "/people", Some(json!({ "name": "Ada two", "email": "ADA@example.com" })));
    assert!(duplicate["message"].as_str().is_some_and(|m| m.contains("UNIQUE")), "{duplicate}");
    server.fails(409, "POST", "/lists", Some(json!({ "owner_id": ada, "name": "Home" })));
    server.fails(409, "POST", "/lists", Some(json!({ "owner_id": 999, "name": "Nobody's" })));

    for bad in [
        json!({ "title": "   " }),
        json!({ "title": "x", "priority": 5 }),
        json!({ "title": "x", "due_on": "2026-02-30" }),
        json!({ "title": "x", "due_on": "2026-2-3" }),
        json!({ "title": "x", "assignee_id": 999 }),
    ] {
        let refused = server.fails(409, "POST", &format!("/lists/{home}/todos"), Some(bad.clone()));
        assert!(refused["message"].as_str().is_some_and(|m| m.contains("constraint")), "{bad} was refused by a constraint: {refused}");
    }
    server.fails(400, "POST", &format!("/lists/{home}/todos"), Some(json!({ "notes": "no title" })));
    server.fails(400, "POST", &format!("/lists/{home}/todos"), Some(json!({ "title": "x", "colour": "red" })));
    server.fails(400, "GET", "/todos/abc", None);
    server.fails(400, "GET", &format!("/lists/{home}/todos?status=later"), None);
    server.fails(400, "GET", "/people?today=someday", None);
    server.fails(404, "POST", "/lists/999/todos", Some(json!({ "title": "x" })));
    server.fails(404, "GET", "/nothing/here", None);

    let todo = server.todo(home, json!({ "title": "Paint", "due_on": "2030-01-31", "assignee_id": grace, "priority": 1 }));
    server.fails(409, "POST", &format!("/todos/{todo}/comments"), Some(json!({ "author_id": 999, "body": "who?" })));
    server.post(&format!("/todos/{todo}/comments"), json!({ "author_id": grace, "body": "On it." }));
    assert_eq!(server.patch(&format!("/todos/{todo}"), json!({ "due_on": null }))["due_on"], Value::Null, "null clears a field");

    server.delete(&format!("/people/{grace}"));
    let orphaned = server.get(&format!("/todos/{todo}"));
    assert_eq!(orphaned["assignee_id"], Value::Null, "ON DELETE SET NULL unassigns the todo");
    let comments = server.get(&format!("/todos/{todo}/comments"));
    assert_eq!((comments[0]["author_id"].clone(), comments[0]["body"].clone()), (Value::Null, json!("On it.")));

    server.delete(&format!("/people/{ada}"));
    server.fails(404, "GET", &format!("/lists/{home}"), None);
    server.fails(404, "GET", &format!("/todos/{todo}"), None);
    assert_eq!(server.get("/people"), json!([]));
    finish(server, &folder);
}

/// Tags are shared and case insensitive, and the timeline records every
/// change, including after the todo is deleted.
#[test]
fn tags_comments_and_the_timeline() {
    let folder = scratch("timeline");
    let server = Server::start(&folder.join("todo.rdb"));
    let ada = server.person("Ada");
    let grace = server.person("Grace");
    let home = server.list(ada, "Home");
    let work = server.list(ada, "Work");

    let fence = server.todo(home, json!({ "title": "Paint the fence", "tags": ["Outdoor", "weekend", "outdoor", " "] }));
    let lawn = server.todo(home, json!({ "title": "Mow the lawn", "tags": ["OUTDOOR"] }));
    assert_eq!(server.get(&format!("/todos/{fence}"))["tags"], json!(["Outdoor", "weekend"]), "one tag per name, ignoring case");
    assert_eq!(server.get(&format!("/todos/{lawn}"))["tags"], json!(["Outdoor"]), "the existing tag keeps its first spelling");
    assert_eq!(
        server.get("/tags"),
        json!([{ "id": 1, "name": "Outdoor", "todos": 2, "open": 2 }, { "id": 2, "name": "weekend", "todos": 1, "open": 1 }])
    );
    assert_eq!(titles(&server.get(&format!("/lists/{home}/todos?tag=weekend"))), ["Paint the fence"]);
    assert_eq!(titles(&server.get(&format!("/lists/{home}/todos?tag=OUTDOOR"))), ["Paint the fence", "Mow the lawn"]);
    let latch = server.todo(home, json!({ "title": "Fix the latch", "tags": ["Repairs", "outdoor", "apple"] }));
    assert_eq!(server.get(&format!("/todos/{latch}"))["tags"], json!(["apple", "Outdoor", "Repairs"]), "tags sort ignoring case");
    server.delete(&format!("/todos/{latch}"));

    assert_eq!(server.put(&format!("/todos/{fence}/tags"), json!({ "tags": ["weekend", "urgent"] }))["tags"], json!(["urgent", "weekend"]));
    server.patch(&format!("/todos/{fence}"), json!({ "title": "Paint the whole fence" }));
    server.patch(&format!("/todos/{fence}"), json!({ "assignee_id": grace }));
    server.patch(&format!("/todos/{fence}"), json!({ "completed": true }));
    server.patch(&format!("/todos/{fence}"), json!({ "completed": false }));
    server.post(&format!("/todos/{fence}/comments"), json!({ "author_id": grace, "body": "Green or white?" }));
    server.patch(&format!("/todos/{fence}"), json!({ "list_id": work }));
    assert_eq!(server.get(&format!("/todos/{fence}"))["comments"], json!(1));

    server.delete(&format!("/todos/{fence}"));
    let timeline = server.get(&format!("/todos/{fence}/timeline"));
    let kinds: Vec<&str> = timeline.as_array().expect("entries").iter().map(|e| e["kind"].as_str().unwrap_or("")).collect();
    assert_eq!(kinds, ["created", "tagged", "renamed", "assigned", "completed", "reopened", "comment", "moved", "deleted"]);
    let renamed = &timeline[2]["detail"];
    assert_eq!((renamed["from"].clone(), renamed["to"].clone()), (json!("Paint the fence"), json!("Paint the whole fence")));
    assert_eq!(timeline[1]["detail"], json!({ "tags": ["weekend", "urgent"] }));
    assert_eq!((timeline[6]["author_name"].clone(), timeline[6]["detail"]["body"].clone()), (json!("Grace"), json!("Green or white?")));
    assert_eq!(timeline[7]["detail"], json!({ "from_list": home, "to_list": work }));
    server.fails(404, "GET", "/todos/999/timeline", None);
    finish(server, &folder);
}

/// Search, the list overview, the workload, the agenda and the completion
/// counts, on data whose answers are known.
#[test]
fn reports_answer_across_lists() {
    let folder = scratch("reports");
    let server = Server::start(&folder.join("todo.rdb"));
    let ada = server.person("Ada");
    let grace = server.person("Grace");
    let home = server.list(ada, "Home");
    let work = server.list(ada, "Work");
    let empty = server.list(grace, "Empty");

    let fence = server.todo(
        home,
        json!({ "title": "Paint the fence", "notes": "green paint", "due_on": "2030-01-01", "priority": 1, "assignee_id": ada }),
    );
    let shed = server.todo(
        home,
        json!({ "title": "Tidy the shed", "notes": "the paint tins go on the top shelf", "due_on": "2030-01-03", "assignee_id": ada }),
    );
    let report = server.todo(work, json!({ "title": "Write the report", "notes": "include the painting survey", "due_on": "2030-01-03", "priority": 1, "assignee_id": ada }));
    let offsite = server.todo(work, json!({ "title": "Book the offsite", "priority": 3, "assignee_id": ada }));
    server.todo(work, json!({ "title": "Old task", "due_on": "2029-12-01", "assignee_id": grace }));
    let done = server.todo(home, json!({ "title": "Buy paint", "due_on": "2029-12-31" }));
    server.patch(&format!("/todos/{done}"), json!({ "completed": true }));

    let hits = server.get("/search?q=paint");
    let found: Vec<i64> = hits.as_array().expect("hits").iter().map(id).collect();
    assert_eq!(found[..2], [done, fence], "a match in the title ranks above a match only in the notes: {hits:#}");
    assert_eq!(found.len(), 4, "the porter stemmer matches painting as well as paint: {hits:#}");
    assert!(found.contains(&shed) && found.contains(&report));
    assert_eq!(hits[1]["title_marked"], json!("[Paint] the fence"));
    assert_eq!(hits[1]["notes_snippet"], json!("green [paint]"));
    assert_eq!(
        server.get(&format!("/search?q=paint&list_id={work}")).as_array().map(|h| h.iter().map(id).collect::<Vec<_>>()),
        Some(vec![report])
    );
    assert_eq!(
        server.get("/search?q=fen").as_array().map(|h| h.iter().map(id).collect::<Vec<_>>()),
        Some(vec![fence]),
        "every word is a prefix"
    );
    assert_eq!(server.get("/search?q=\"shed\" -(top) *").as_array().map(|h| h.iter().map(id).collect::<Vec<_>>()), Some(vec![shed]));
    server.fails(400, "GET", "/search?q=--", None);
    server.patch(&format!("/todos/{offsite}"), json!({ "notes": "a paint workshop" }));
    assert_eq!(
        server.get("/search?q=workshop").as_array().map(|h| h.iter().map(id).collect::<Vec<_>>()),
        Some(vec![offsite]),
        "an edit reindexes"
    );

    let lists = server.get("/lists?today=2030-01-02");
    let overview: Vec<(i64, i64, i64, i64, Value, i64)> = lists
        .as_array()
        .expect("lists")
        .iter()
        .map(|l| {
            let n = |k: &str| l[k].as_i64().unwrap_or(-1);
            (n("id"), n("total"), n("open"), n("overdue"), l["percent_done"].clone(), n("busiest_rank"))
        })
        .collect();
    assert_eq!(overview, [(work, 3, 3, 1, json!(0.0), 1), (home, 3, 2, 1, json!(33.3), 2), (empty, 0, 0, 0, Value::Null, 3)]);
    assert_eq!(lists[1]["next_due"], json!("2030-01-01"), "the earliest open due date; the completed todo is left out");
    assert_eq!(server.get(&format!("/lists/{home}?today=2030-01-02"))["busiest_rank"], json!(2), "one list is ranked against all of them");

    let workload = server.get(&format!("/people/{ada}/workload?today=2030-01-02"));
    let ranked: Vec<(i64, i64, Value, bool)> = workload
        .as_array()
        .expect("items")
        .iter()
        .map(|w| {
            (w["rank"].as_i64().unwrap_or(0), w["todo_id"].as_i64().unwrap_or(0), w["due_by_then"].clone(), w["overdue"] == json!(true))
        })
        .collect();
    assert_eq!(
        ranked,
        [(1, fence, json!(1), true), (2, report, json!(3), false), (3, shed, json!(3), false), (4, offsite, Value::Null, false)]
    );
    assert_eq!(workload[0]["in_same_list"], json!(2));
    assert_eq!(server.get(&format!("/people/{ada}?today=2030-01-02"))["overdue_todos"], json!(1));

    let agenda = server.get("/agenda?from=2030-01-01&days=3");
    let days: Vec<(String, Vec<i64>)> = agenda["days"]
        .as_array()
        .expect("days")
        .iter()
        .map(|d| (d["day"].as_str().unwrap_or("").to_string(), d["todos"].as_array().expect("todos").iter().map(id).collect()))
        .collect();
    assert_eq!(
        days,
        [("2030-01-01".to_string(), vec![fence]), ("2030-01-02".to_string(), vec![]), ("2030-01-03".to_string(), vec![report, shed])]
    );
    assert_eq!(
        agenda["overdue"].as_array().map(|o| o.iter().map(|t| t["title"].clone()).collect::<Vec<_>>()),
        Some(vec![json!("Old task")])
    );
    assert_eq!(server.get(&format!("/agenda?from=2030-01-01&days=3&assignee_id={grace}"))["days"][0]["todos"], json!([]));

    server.patch(&format!("/todos/{fence}"), json!({ "completed": true }));
    server.patch(&format!("/todos/{fence}"), json!({ "completed": false }));
    let stats = server.get("/stats/completions?days=3");
    assert_eq!(stats.as_array().map(Vec::len), Some(3));
    let today = server.get("/health")["today"].clone();
    assert_eq!(stats[2]["day"], today);
    assert_eq!(
        (stats[2]["completed"].clone(), stats[2]["reopened"].clone(), stats[2]["running_total"].clone()),
        (json!(2), json!(1), json!(2))
    );
    finish(server, &folder);
}
