//! A small set of demo data, so every endpoint has something to show.
//!
//! It goes through the same [`Store`] methods the HTTP handlers call, so the
//! triggers, the search index and the constraints all see it the way they see
//! a request.

use crate::error::ApiResult;
use crate::store::{NewComment, NewList, NewPerson, NewTodo, Store, TodoPatch};

/// What `seed` added.
#[derive(Debug, serde::Serialize)]
pub struct Seeded {
    /// People added.
    pub people: usize,
    /// Lists added.
    pub lists: usize,
    /// Todos added, subtasks included.
    pub todos: usize,
}

/// Adds two people, three lists, and a dozen todos with subtasks, tags and comments.
///
/// Due dates are counted from `today`, so the agenda and the overdue counts
/// have something in them whatever day the seed runs.
///
/// @param store - the database, which should be empty
pub fn seed(store: &Store) -> ApiResult<Seeded> {
    let today = store.today(None)?;
    let day = |offset: i64| store.day_after(&today, offset);
    let ada = store.create_person(&NewPerson { name: "Ada".into(), email: "ada@example.com".into() })?.id;
    let grace = store.create_person(&NewPerson { name: "Grace".into(), email: "grace@example.com".into() })?.id;
    let home = store.create_list(&NewList { owner_id: ada, name: "Home".into() })?.id;
    let work = store.create_list(&NewList { owner_id: ada, name: "Work".into() })?.id;
    let garden = store.create_list(&NewList { owner_id: grace, name: "Garden".into() })?.id;

    let todo = |list: i64, title: &str, notes: &str, priority: i64, due: Option<String>, assignee: Option<i64>, tags: &[&str]| {
        store.create_todo(
            list,
            &NewTodo {
                title: title.into(),
                notes: Some(notes.into()),
                priority: Some(priority),
                due_on: due,
                assignee_id: assignee,
                tags: tags.iter().map(|tag| tag.to_string()).collect(),
                ..NewTodo::default()
            },
        )
    };
    let subtask = |list: i64, parent: i64, title: &str| {
        store.create_todo(list, &NewTodo { title: title.into(), parent_id: Some(parent), ..NewTodo::default() })
    };

    let fence =
        todo(home, "Paint the fence", "Two coats of the green paint from the shed", 1, Some(day(-2)?), Some(ada), &["outdoor", "weekend"])?
            .id;
    let sand = subtask(home, fence, "Sand the boards")?.id;
    subtask(home, sand, "Buy sandpaper")?;
    subtask(home, fence, "Prime the boards")?;
    todo(home, "Buy groceries", "Milk, eggs, bread, coffee", 2, Some(day(0)?), Some(ada), &["shopping"])?;
    let taxes = todo(home, "File the tax return", "The forms are in the blue folder", 1, Some(day(5)?), Some(ada), &["paperwork"])?.id;
    todo(home, "Call the plumber", "The kitchen tap drips", 3, None, None, &[])?;
    todo(work, "Write the quarterly report", "Include the new retention numbers", 1, Some(day(1)?), Some(ada), &["writing"])?;
    let review = todo(work, "Review Grace's design document", "Focus on the storage section", 2, Some(day(3)?), Some(ada), &[])?.id;
    todo(work, "Book the team offsite", "", 3, Some(day(14)?), Some(grace), &["planning"])?;
    todo(garden, "Plant the tomatoes", "After the last frost", 2, Some(day(2)?), Some(grace), &["outdoor"])?;
    let compost = todo(garden, "Turn the compost", "", 3, Some(day(-1)?), Some(grace), &["outdoor"])?.id;

    store.update_todo(compost, &TodoPatch { completed: Some(true), ..TodoPatch::default() })?;
    store.update_todo(
        taxes,
        &TodoPatch { notes: Some("The forms are in the blue folder. Deadline is firm.".into()), ..TodoPatch::default() },
    )?;
    store.add_comment(review, &NewComment { author_id: Some(grace), body: "The storage section changed yesterday.".into() })?;
    store.add_comment(review, &NewComment { author_id: Some(ada), body: "Thanks, I will read the new version.".into() })?;
    Ok(Seeded { people: 2, lists: 3, todos: 13 })
}
