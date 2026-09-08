//! JSON path parsing, lookup, and the four editing operations.
//!
//! Invariant: a path is either well formed or it is an error, and a lookup that
//! finds nothing is not an error. `json_extract(x, '$.missing')` is NULL and
//! `json_extract(x, 'missing')` fails with "bad JSON path"; the two look alike
//! from a distance and applications rely on the difference, because the first
//! is a question about the data and the second is a bug in the query.
//!
//! The editing operations differ only in what they do when the path already
//! exists and when it does not, so they are one walk parameterised by that
//! decision rather than four walks that have to agree with each other.

use inillucent_base::{DbError, DbResult};

use super::node::Node;

/// One step of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `.name` or `."name"`.
    Key(String),
    /// `[n]`, counting from the start.
    Index(usize),
    /// `[#-n]`, counting back from the end. `[#-1]` is the last element.
    FromEnd(usize),
    /// `[#]`, which names the position one past the end.
    Append,
}

/// Parses a path, which must begin with `$`.
pub fn parse(text: &str) -> DbResult<Vec<Step>> {
    let mut characters = text.chars().peekable();
    if characters.next() != Some('$') {
        return Err(bad_path(text));
    }
    let mut steps = Vec::new();
    loop {
        match characters.next() {
            None => return Ok(steps),
            Some('.') => {
                let name = if characters.peek() == Some(&'"') {
                    characters.next();
                    let mut name = String::new();
                    loop {
                        match characters.next() {
                            None => return Err(bad_path(text)),
                            Some('"') => break,
                            Some('\\') => match characters.next() {
                                None => return Err(bad_path(text)),
                                Some(escaped) => name.push(escaped),
                            },
                            Some(character) => name.push(character),
                        }
                    }
                    name
                } else {
                    let mut name = String::new();
                    while let Some(character) = characters.peek() {
                        if *character == '.' || *character == '[' {
                            break;
                        }
                        name.push(*character);
                        characters.next();
                    }
                    if name.is_empty() {
                        return Err(bad_path(text));
                    }
                    name
                };
                steps.push(Step::Key(name));
            }
            Some('[') => {
                let step = if characters.peek() == Some(&'#') {
                    characters.next();
                    if characters.peek() == Some(&'-') {
                        characters.next();
                        let back = read_number(&mut characters).ok_or_else(|| bad_path(text))?;
                        Step::FromEnd(back)
                    } else {
                        Step::Append
                    }
                } else {
                    let index = read_number(&mut characters).ok_or_else(|| bad_path(text))?;
                    Step::Index(index)
                };
                if characters.next() != Some(']') {
                    return Err(bad_path(text));
                }
                steps.push(step);
            }
            Some(_) => return Err(bad_path(text)),
        }
    }
}

/// Reads a run of decimal digits, refusing an empty one.
fn read_number(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<usize> {
    let mut value: usize = 0;
    let mut digits = 0;
    while let Some(digit) = characters
        .peek()
        .and_then(|character| character.to_digit(10))
    {
        characters.next();
        value = value.checked_mul(10)?.checked_add(digit as usize)?;
        digits += 1;
    }
    (digits > 0).then_some(value)
}

/// Returns the error a malformed path reports.
pub fn bad_path(text: &str) -> DbError {
    DbError::primary(inillucent_base::PrimaryCode::Error)
        .with_detail(format!("bad JSON path: '{text}'"))
}

/// Resolves a path against a document, returning the element it names.
pub fn lookup<'tree>(node: &'tree Node, steps: &[Step]) -> Option<&'tree Node> {
    let Some((step, rest)) = steps.split_first() else {
        return Some(node);
    };
    match (step, node) {
        (Step::Key(name), Node::Object(members)) => members
            .iter()
            .find(|(label, _)| label_matches(label, name))
            .and_then(|(_, value)| lookup(value, rest)),
        (Step::Index(index), Node::Array(items)) => {
            items.get(*index).and_then(|item| lookup(item, rest))
        }
        (Step::FromEnd(back), Node::Array(items)) => items
            .len()
            .checked_sub(*back)
            .and_then(|index| items.get(index))
            .and_then(|item| lookup(item, rest)),
        _ => None,
    }
}

/// Returns whether an object label spells a path key.
fn label_matches(label: &Node, name: &str) -> bool {
    super::render::unescape(label) == name
}

/// What an edit does when the path is already there, and when it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// `json_insert`: write only where nothing is.
    Insert,
    /// `json_replace`: write only where something is.
    Replace,
    /// `json_set`: write either way.
    Set,
}

impl Edit {
    /// Returns whether the operation writes over an existing element.
    fn overwrites(self) -> bool {
        matches!(self, Edit::Replace | Edit::Set)
    }

    /// Returns whether the operation creates a missing element.
    fn creates(self) -> bool {
        matches!(self, Edit::Insert | Edit::Set)
    }
}

/// Applies one edit to a document in place.
///
/// A path that does not apply - `$[0]` into an object, `$.a.b` where `a` is a
/// number - changes nothing and is not an error, which is what makes
/// `json_set` safe to run over a column whose rows are not all the same shape.
pub fn apply(node: &mut Node, steps: &[Step], value: Node, edit: Edit) -> DbResult<()> {
    let Some((step, rest)) = steps.split_first() else {
        if edit.overwrites() {
            *node = value;
        }
        return Ok(());
    };
    match (step, node) {
        (Step::Key(name), Node::Object(members)) => {
            if let Some(position) = members
                .iter()
                .position(|(label, _)| label_matches(label, name))
            {
                let Some((_, existing)) = members.get_mut(position) else {
                    return Ok(());
                };
                return apply(existing, rest, value, edit);
            }
            if !edit.creates() {
                return Ok(());
            }
            let Some(mut fresh) = seed(rest) else {
                return Ok(());
            };
            apply(&mut fresh, rest, value, Edit::Set)?;
            members.push((Node::text_raw(name), fresh));
            Ok(())
        }
        (Step::Index(index), Node::Array(items)) => {
            if let Some(existing) = items.get_mut(*index) {
                return apply(existing, rest, value, edit);
            }
            if !edit.creates() || *index != items.len() {
                return Ok(());
            }
            let Some(mut fresh) = seed(rest) else {
                return Ok(());
            };
            apply(&mut fresh, rest, value, Edit::Set)?;
            items.push(fresh);
            Ok(())
        }
        (Step::FromEnd(back), Node::Array(items)) => {
            let Some(index) = items.len().checked_sub(*back) else {
                return Ok(());
            };
            let Some(existing) = items.get_mut(index) else {
                return Ok(());
            };
            apply(existing, rest, value, edit)
        }
        (Step::Append, Node::Array(items)) => {
            if !edit.creates() {
                return Ok(());
            }
            let Some(mut fresh) = seed(rest) else {
                return Ok(());
            };
            apply(&mut fresh, rest, value, Edit::Set)?;
            items.push(fresh);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Inserts a value into an array, shifting whatever follows it along.
///
/// `json_array_insert(X, P, V)`, where the path's last step names a *position*
/// in an array rather than an element to overwrite. The distinction is the
/// whole function: `json_set('[1,2]','$[0]',9)` answers `[9,2]` and this
/// answers `[9,1,2]`.
///
/// A position past the end changes nothing, `[#]` appends, and a path whose
/// last step is a key is a **refusal** rather than a no-op - because a caller
/// who wrote `$.b` asked to insert into something that is not an array, and
/// answering the document back would look like it had worked.
///
/// @param node - the document, edited in place
/// @param steps - the path
/// @param value - what to insert
/// @param written - the path as it was written, for the refusal
pub fn insert_into_array(
    node: &mut Node,
    steps: &[Step],
    value: Node,
    written: &str,
) -> DbResult<()> {
    let Some((last, init)) = steps.split_last() else {
        return Ok(());
    };
    if matches!(last, Step::Key(_)) {
        return Err(inillucent_base::error::misuse(format!(
            "not an array element: '{written}'"
        )));
    }
    let Some(container) = follow(node, init) else {
        return Ok(());
    };
    let Node::Array(items) = container else {
        return Ok(());
    };
    let at = match last {
        Step::Index(index) => *index,
        Step::Append => items.len(),
        Step::FromEnd(back) => match items.len().checked_sub(*back) {
            Some(index) => index,
            None => return Ok(()),
        },
        Step::Key(_) => return Ok(()),
    };
    if at > items.len() {
        return Ok(());
    }
    items.insert(at, value);
    Ok(())
}

/// Returns the node a path names, when the whole path resolves.
///
/// @param node - the document
/// @param steps - the path
fn follow<'n>(node: &'n mut Node, steps: &[Step]) -> Option<&'n mut Node> {
    let Some((step, rest)) = steps.split_first() else {
        return Some(node);
    };
    let next = match (step, node) {
        (Step::Key(name), Node::Object(members)) => members
            .iter_mut()
            .find(|(label, _)| label_matches(label, name))
            .map(|(_, held)| held)?,
        (Step::Index(index), Node::Array(items)) => items.get_mut(*index)?,
        (Step::FromEnd(back), Node::Array(items)) => {
            let index = items.len().checked_sub(*back)?;
            items.get_mut(index)?
        }
        _ => return None,
    };
    follow(next, rest)
}

/// Returns the empty container a missing intermediate step needs.
///
/// The next step decides: a key needs an object to live in and an index needs
/// an array. With no next step there is nothing to create - the value itself
/// is about to be written - so the seed is a placeholder the caller
/// immediately overwrites.
fn seed(rest: &[Step]) -> Option<Node> {
    match rest.first() {
        None => Some(Node::Null),
        Some(Step::Key(_)) => Some(Node::Object(Vec::new())),
        Some(Step::Index(0) | Step::Append) => Some(Node::Array(Vec::new())),
        // `$.a[3]` into a document with no `a` would have to create an array
        // with three holes, and JSON has no hole.
        Some(Step::Index(_) | Step::FromEnd(_)) => None,
    }
}

/// Removes the element a path names, reporting whether one was there.
pub fn remove(node: &mut Node, steps: &[Step]) -> bool {
    let Some((step, rest)) = steps.split_first() else {
        return false;
    };
    match (step, node) {
        (Step::Key(name), Node::Object(members)) => {
            let Some(position) = members
                .iter()
                .position(|(label, _)| label_matches(label, name))
            else {
                return false;
            };
            if rest.is_empty() {
                members.remove(position);
                return true;
            }
            members
                .get_mut(position)
                .is_some_and(|(_, value)| remove(value, rest))
        }
        (Step::Index(index), Node::Array(items)) => {
            let index = *index;
            if index >= items.len() {
                return false;
            }
            if rest.is_empty() {
                items.remove(index);
                return true;
            }
            items.get_mut(index).is_some_and(|item| remove(item, rest))
        }
        (Step::FromEnd(back), Node::Array(items)) => {
            let Some(index) = items.len().checked_sub(*back) else {
                return false;
            };
            if index >= items.len() {
                return false;
            }
            if rest.is_empty() {
                items.remove(index);
                return true;
            }
            items.get_mut(index).is_some_and(|item| remove(item, rest))
        }
        _ => false,
    }
}

/// Applies RFC-7386 merge-patch semantics.
///
/// The rule is short and its consequences are not: a member whose patch value
/// is `null` is deleted rather than set to null, two objects merge recursively,
/// and anything else replaces wholesale. An array is "anything else", which is
/// why a merge patch cannot edit one element of a list.
pub fn patch(target: &mut Node, patch: &Node) {
    let Node::Object(updates) = patch else {
        *target = patch.clone();
        return;
    };
    if !matches!(target, Node::Object(_)) {
        *target = Node::Object(Vec::new());
    }
    let Node::Object(members) = target else {
        return;
    };
    for (label, value) in updates {
        let name = super::render::unescape(label);
        let position = members
            .iter()
            .position(|(existing, _)| label_matches(existing, &name));
        if matches!(value, Node::Null) {
            if let Some(position) = position {
                members.remove(position);
            }
            continue;
        }
        match position {
            Some(position) => {
                if let Some((_, existing)) = members.get_mut(position) {
                    self::patch(existing, value);
                }
            }
            None => {
                let mut fresh = Node::Object(Vec::new());
                self::patch(&mut fresh, value);
                members.push((label.clone(), fresh));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{parse as text, render};

    /// Parses a document for a test.
    fn document(source: &str) -> Node {
        text::parse(source).expect("parses").node
    }

    /// Renders a document for a test.
    fn rendered(node: &Node) -> String {
        render::to_text(node)
    }

    /// A path must start at the root, and anything else is an error.
    #[test]
    fn a_path_must_start_at_the_root() {
        assert!(parse("a").is_err());
        assert!(parse("$a").is_err());
        assert_eq!(parse("$").expect("parses"), Vec::new());
    }

    /// Keys, quoted keys, and indexes all parse.
    #[test]
    fn the_step_forms_parse() {
        assert_eq!(
            parse("$.a[0].\"b.c\"[#-1][#]").expect("parses"),
            vec![
                Step::Key("a".to_string()),
                Step::Index(0),
                Step::Key("b.c".to_string()),
                Step::FromEnd(1),
                Step::Append,
            ]
        );
    }

    /// A lookup that finds nothing answers nothing rather than failing.
    #[test]
    fn a_missing_element_is_not_an_error() {
        let node = document(r#"{"a":1}"#);
        assert!(lookup(&node, &parse("$.a.b").expect("parses")).is_none());
        assert!(lookup(&node, &parse("$[0]").expect("parses")).is_none());
    }

    /// The three edits differ only over an existing and a missing path.
    #[test]
    fn the_edits_differ_over_presence() {
        for (edit, expected) in [
            (Edit::Insert, r#"{"a":1,"b":2}"#),
            (Edit::Replace, r#"{"a":9}"#),
            (Edit::Set, r#"{"a":9,"b":2}"#),
        ] {
            let mut node = document(r#"{"a":1}"#);
            apply(
                &mut node,
                &parse("$.a").expect("parses"),
                Node::Int("9".to_string()),
                edit,
            )
            .expect("edits");
            apply(
                &mut node,
                &parse("$.b").expect("parses"),
                Node::Int("2".to_string()),
                edit,
            )
            .expect("edits");
            assert_eq!(rendered(&node), expected, "{edit:?}");
        }
    }

    /// A missing intermediate step is created from the step that follows it.
    #[test]
    fn intermediate_containers_are_created() {
        let mut node = document("{}");
        apply(
            &mut node,
            &parse("$.a.b").expect("parses"),
            Node::Int("1".to_string()),
            Edit::Set,
        )
        .expect("edits");
        assert_eq!(rendered(&node), r#"{"a":{"b":1}}"#);

        let mut array = document("[]");
        apply(
            &mut array,
            &parse("$[0].a").expect("parses"),
            Node::Int("1".to_string()),
            Edit::Set,
        )
        .expect("edits");
        assert_eq!(rendered(&array), r#"[{"a":1}]"#);
    }

    /// Removal takes the element out and reports whether it was there.
    #[test]
    fn removal_reports_what_it_did() {
        let mut node = document("[1,2,3]");
        assert!(remove(&mut node, &parse("$[0]").expect("parses")));
        assert!(remove(&mut node, &parse("$[0]").expect("parses")));
        assert_eq!(rendered(&node), "[3]");
        assert!(!remove(&mut node, &parse("$.zz").expect("parses")));
    }

    /// A merge patch deletes on null and merges objects recursively.
    #[test]
    fn merge_patch_follows_rfc_7386() {
        let mut node = document(r#"{"a":1,"b":2}"#);
        patch(&mut node, &document(r#"{"b":null,"c":3}"#));
        assert_eq!(rendered(&node), r#"{"a":1,"c":3}"#);

        let mut nested = document(r#"{"a":{"b":1}}"#);
        patch(&mut nested, &document(r#"{"a":{"c":2}}"#));
        assert_eq!(rendered(&nested), r#"{"a":{"b":1,"c":2}}"#);

        let mut array = document("[1,2]");
        patch(&mut array, &document(r#"{"a":1}"#));
        assert_eq!(rendered(&array), r#"{"a":1}"#);
    }
}
