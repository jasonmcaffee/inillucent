//! `json_each` and `json_tree`, the two table-valued JSON functions.
//!
//! Invariant: the two differ only in how far they walk. `json_each` visits the
//! direct children of one element; `json_tree` visits that element and
//! everything under it. Every column they report - `key`, `value`, `type`,
//! `atom`, `id`, `parent`, `fullkey`, `path` - is the same question asked of
//! the same node, so they are one walk with a depth flag rather than two
//! modules that have to agree with each other.
//!
//! `id` and `parent` are byte offsets into the document's binary form, which is
//! what the pinned release reports and what makes them stable enough to join
//! on. They are carried down the walk rather than measured afterwards: a
//! container knows where its own header ends, and each child's offset is the
//! previous child's plus its encoded length.

use inillucent_base::DbResult;
use inillucent_value::Value;

use crate::json::{binary, node::Node, path, render, Argument};

use super::{
    Context, Declaration, DeclaredColumn, FilterPlan, IndexQuery, Module, ModuleArguments,
    VirtualCursor, VirtualTable,
};

/// Which of the two functions a table is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walk {
    /// `json_each`: the direct children of the element the path names.
    Each,
    /// `json_tree`: that element and everything below it.
    Tree,
}

/// The module for one of the two functions.
pub struct JsonWalkModule {
    walk: Walk,
    name: &'static str,
}

impl JsonWalkModule {
    /// Returns the `json_each` module.
    pub fn each() -> JsonWalkModule {
        JsonWalkModule {
            walk: Walk::Each,
            name: "json_each",
        }
    }

    /// Returns the `json_tree` module.
    pub fn tree() -> JsonWalkModule {
        JsonWalkModule {
            walk: Walk::Tree,
            name: "json_tree",
        }
    }
}

/// The object label or array index of the row's node.
const KEY: usize = 0;
/// The node itself, as JSON when it is a container.
const VALUE: usize = 1;
/// What `json_type()` calls the node.
const TYPE: usize = 2;
/// The node's value when it is a leaf, and NULL when it is not.
const ATOM: usize = 3;
/// The node's offset in the binary encoding.
const ID: usize = 4;
/// The offset of the node's container.
const PARENT: usize = 5;
/// The whole path to the node.
const FULLKEY: usize = 6;
/// The path to the node's container.
const PATH: usize = 7;
/// The hidden column holding the document.
const JSON: usize = 8;
/// The hidden column holding the root path.
const ROOT: usize = 9;

impl Module for JsonWalkModule {
    /// Returns the module's name.
    fn name(&self) -> &str {
        self.name
    }

    /// The two functions are names, not tables: they need no `CREATE`.
    fn eponymous(&self) -> bool {
        true
    }

    /// A `CREATE VIRTUAL TABLE` naming one of them would have no rows of its
    /// own, so it is refused rather than made.
    fn constructible(&self) -> bool {
        false
    }

    /// Connects, which for these two is only declaring the ten columns.
    fn connect(
        &self,
        _arguments: &ModuleArguments,
        _creating: bool,
    ) -> DbResult<Box<dyn VirtualTable>> {
        Ok(Box::new(JsonWalkTable {
            walk: self.walk,
            declaration: declaration(),
        }))
    }
}

/// Returns the schema both functions declare.
fn declaration() -> Declaration {
    Declaration {
        columns: vec![
            DeclaredColumn::visible("key"),
            DeclaredColumn::visible("value"),
            DeclaredColumn::visible("type").typed("TEXT"),
            DeclaredColumn::visible("atom"),
            DeclaredColumn::visible("id").typed("INT"),
            DeclaredColumn::visible("parent").typed("INT"),
            DeclaredColumn::visible("fullkey").typed("TEXT"),
            DeclaredColumn::visible("path").typed("TEXT"),
            DeclaredColumn::hidden("json").typed("JSON"),
            DeclaredColumn::hidden("root").typed("TEXT"),
        ],
        without_rowid: false,
    }
}

/// One connected `json_each` or `json_tree`.
struct JsonWalkTable {
    walk: Walk,
    declaration: Declaration,
}

impl VirtualTable for JsonWalkTable {
    /// Returns the ten columns.
    fn declaration(&self) -> &Declaration {
        &self.declaration
    }

    /// Takes the two hidden columns as arguments and nothing else.
    ///
    /// A plan with no document is priced out of existence rather than refused,
    /// because `SELECT * FROM json_each` is a statement that prepares and then
    /// produces no rows - which is what the pinned release does.
    fn best_index(&self, info: &mut IndexQuery) -> DbResult<()> {
        let mut used_json = false;
        let mut used_root = false;
        for index in 0..info.constraints.len() {
            let Some(constraint) = info.constraints.get(index).copied() else {
                continue;
            };
            if !constraint.usable || constraint.op != super::ConstraintOp::Eq {
                continue;
            }
            match constraint.column as usize {
                JSON if !used_json => {
                    info.use_constraint(index, true);
                    used_json = true;
                }
                ROOT if !used_root => {
                    info.use_constraint(index, true);
                    used_root = true;
                }
                _ => {}
            }
        }
        // The argument order `filter` receives is the order they were claimed
        // in, and the document is always claimed first.
        info.index_number = i32::from(used_json) | (i32::from(used_root) << 1);
        info.estimated_cost = if used_json { 1.0 } else { 1.0e99 };
        info.estimated_rows = 25;
        Ok(())
    }

    /// Opens a cursor, which holds the rows the walk produced.
    fn open(&self) -> DbResult<Box<dyn VirtualCursor>> {
        Ok(Box::new(JsonWalkCursor {
            walk: self.walk,
            rows: Vec::new(),
            at: 0,
        }))
    }
}

/// One row the walk produced.
#[derive(Clone, Debug)]
struct WalkRow {
    /// The object label or array index, when the node has a parent.
    key: Option<Value<'static>>,
    /// The node itself.
    node: Node,
    /// The byte offset of the node - or of its label - in the binary form.
    id: i64,
    /// The offset of the container the node sits in.
    parent: Option<i64>,
    /// The path to the node.
    fullkey: String,
    /// The path to the node's container.
    path: String,
}

/// A cursor over the rows of one walk.
struct JsonWalkCursor {
    walk: Walk,
    rows: Vec<WalkRow>,
    at: usize,
}

impl VirtualCursor for JsonWalkCursor {
    /// Parses the document and produces every row the walk will report.
    ///
    /// The whole walk happens here rather than a row at a time because the
    /// document is already a tree in memory: stepping it lazily would mean
    /// carrying a stack of borrows into a cursor that has to outlive the call,
    /// and would save nothing - the document was parsed in full before the
    /// first row could be answered.
    fn filter(&mut self, _context: &mut Context<'_>, plan: &FilterPlan) -> DbResult<()> {
        self.rows.clear();
        self.at = 0;
        let mut arguments = plan.arguments.iter();
        let document = (plan.index_number & 1 != 0)
            .then(|| arguments.next())
            .flatten();
        let root = (plan.index_number & 2 != 0)
            .then(|| arguments.next())
            .flatten();
        let Some(document) = document else {
            return Ok(());
        };
        if document.is_null() {
            return Ok(());
        }
        let Some(tree) = crate::json::document(&Argument::plain(document))? else {
            return Ok(());
        };
        // The offsets are into the encoding of the *whole* document, so a root
        // path is located inside it rather than encoded on its own.
        let (start, start_id, prefix, root_key, parent_path) = match root {
            None => (&tree, 0i64, "$".to_string(), None, "$".to_string()),
            Some(Value::Null) => return Ok(()),
            Some(value) => {
                let text = text_of(value);
                let steps = path::parse(&text)?;
                let Some((found, offset)) = locate(&tree, 0, &steps) else {
                    return Ok(());
                };
                let (key, parent) = root_position(&text, &steps);
                (found, offset, text, key, parent)
            }
        };
        match self.walk {
            Walk::Each => self.each(start, start_id, &prefix),
            Walk::Tree => {
                self.rows.push(WalkRow {
                    key: root_key,
                    node: start.clone(),
                    id: start_id,
                    parent: None,
                    fullkey: prefix.clone(),
                    path: parent_path,
                });
                self.descend(start, start_id, start_id, &prefix);
            }
        }
        Ok(())
    }

    /// Moves to the next row of the walk.
    fn next(&mut self, _context: &mut Context<'_>) -> DbResult<()> {
        self.at = self.at.saturating_add(1);
        Ok(())
    }

    /// Returns whether the walk is finished.
    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    /// Returns one column of the current row.
    fn column(&mut self, _context: &mut Context<'_>, index: usize) -> DbResult<Value<'static>> {
        let Some(row) = self.rows.get(self.at) else {
            return Ok(Value::Null);
        };
        Ok(match index {
            KEY => row.key.clone().unwrap_or(Value::Null),
            VALUE => crate::json::sql_of(&row.node)?.value,
            TYPE => Value::owned_text(row.node.type_name().as_bytes())?,
            // `atom` is the value for a leaf and NULL for a container, which is
            // the difference between "there is a value here" and "there is more
            // structure here".
            ATOM => {
                if row.node.is_container() {
                    Value::Null
                } else {
                    crate::json::sql_of(&row.node)?.value
                }
            }
            ID => Value::Integer(row.id),
            PARENT => row.parent.map_or(Value::Null, Value::Integer),
            FULLKEY => Value::owned_text(row.fullkey.as_bytes())?,
            PATH => Value::owned_text(row.path.as_bytes())?,
            _ => Value::Null,
        })
    }

    /// Returns the row's position in the walk, which is its rowid.
    fn rowid(&self) -> DbResult<i64> {
        Ok(self.at as i64)
    }
}

impl JsonWalkCursor {
    /// Adds one row per direct child of a node.
    fn each(&mut self, node: &Node, node_id: i64, prefix: &str) {
        for child in children(node, node_id) {
            self.rows.push(WalkRow {
                key: child.key.clone(),
                node: child.node.clone(),
                id: child.id,
                parent: None,
                fullkey: child.fullkey(prefix),
                path: prefix.to_string(),
            });
        }
        if !node.is_container() {
            // A scalar has no children, and `json_each` of one reports the
            // scalar itself with no key - which is what the pinned release does
            // and is why this is not simply "no rows".
            self.rows.push(WalkRow {
                key: None,
                node: node.clone(),
                id: node_id,
                parent: None,
                fullkey: prefix.to_string(),
                path: prefix.to_string(),
            });
        }
    }

    /// Adds one row per descendant of a node, depth first.
    ///
    /// `value_at` is where the node's own encoding begins and decides where its
    /// children are; `reported` is the offset the walk gave the node and is
    /// what its children name as their parent. For an array element the two are
    /// the same, and for an object member they are not: the member is reported
    /// at its label and laid out after it.
    fn descend(&mut self, node: &Node, value_at: i64, reported: i64, prefix: &str) {
        for child in children(node, value_at) {
            let fullkey = child.fullkey(prefix);
            self.rows.push(WalkRow {
                key: child.key.clone(),
                node: child.node.clone(),
                id: child.id,
                parent: Some(reported),
                fullkey: fullkey.clone(),
                path: prefix.to_string(),
            });
            self.descend(child.node, child.value_at, child.id, &fullkey);
        }
    }
}

/// One child of a container, with everything the walk needs about it.
struct Child<'tree> {
    /// The label or index, as the `key` column reports it.
    key: Option<Value<'static>>,
    /// The label as text, for the path.
    label: Option<String>,
    /// The index, for the path.
    index: Option<usize>,
    /// The child node.
    node: &'tree Node,
    /// The child's offset: its label's, in an object, and its own in an array.
    id: i64,
    /// Where the child's own value begins, which is past the label.
    value_at: i64,
}

impl Child<'_> {
    /// Returns the path to this child.
    fn fullkey(&self, prefix: &str) -> String {
        match (&self.label, self.index) {
            (Some(label), _) => join_key(prefix, label),
            (None, Some(index)) => format!("{prefix}[{index}]"),
            _ => prefix.to_string(),
        }
    }
}

/// Returns the direct children of a container, with their binary offsets.
///
/// The offsets follow from the encoding and nothing else: the first child
/// starts where the container's header ends, and each one after it starts where
/// the previous ended. An object's child is reported at its *label's* offset,
/// which is what the pinned release does - the label is the member.
fn children(node: &Node, node_id: i64) -> Vec<Child<'_>> {
    let mut cursor = node_id.saturating_add(header_length(node));
    let mut out = Vec::new();
    match node {
        Node::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                out.push(Child {
                    key: Some(Value::Integer(index as i64)),
                    label: None,
                    index: Some(index),
                    node: item,
                    id: cursor,
                    value_at: cursor,
                });
                cursor = cursor.saturating_add(encoded_length(item));
            }
        }
        Node::Object(members) => {
            for (label, value) in members {
                let name = render::unescape(label);
                let value_at = cursor.saturating_add(encoded_length(label));
                out.push(Child {
                    key: Value::owned_text(name.as_bytes()).ok(),
                    label: Some(name),
                    index: None,
                    node: value,
                    id: cursor,
                    value_at,
                });
                cursor = value_at.saturating_add(encoded_length(value));
            }
        }
        _ => {}
    }
    out
}

/// Resolves a path inside a document, returning the node and its offset.
fn locate<'tree>(node: &'tree Node, at: i64, steps: &[path::Step]) -> Option<(&'tree Node, i64)> {
    let Some((step, rest)) = steps.split_first() else {
        return Some((node, at));
    };
    let members = children(node, at);
    let found = match step {
        path::Step::Key(name) => members
            .iter()
            .find(|child| child.label.as_deref() == Some(name.as_str())),
        path::Step::Index(index) => members.iter().find(|child| child.index == Some(*index)),
        path::Step::FromEnd(back) => members
            .len()
            .checked_sub(*back)
            .and_then(|index| members.get(index)),
        path::Step::Append => None,
    }?;
    locate(found.node, found.value_at, rest)
}

/// Returns how many bytes one node's whole encoding takes.
fn encoded_length(node: &Node) -> i64 {
    let mut out = Vec::new();
    binary::encode(node, &mut out);
    i64::try_from(out.len()).unwrap_or(0)
}

/// Returns how many bytes one node's header takes, payload excluded.
fn header_length(node: &Node) -> i64 {
    encoded_length(node).saturating_sub(payload_length(node))
}

/// Returns how many bytes one node's payload takes, header excluded.
fn payload_length(node: &Node) -> i64 {
    match node {
        Node::Array(items) => items.iter().map(encoded_length).sum(),
        Node::Object(members) => members
            .iter()
            .map(|(label, value)| encoded_length(label).saturating_add(encoded_length(value)))
            .sum(),
        Node::Null | Node::True | Node::False => 0,
        Node::Int(text)
        | Node::Int5(text)
        | Node::Float(text)
        | Node::Float5(text)
        | Node::Text(text)
        | Node::TextJ(text)
        | Node::Text5(text)
        | Node::TextRaw(text) => i64::try_from(text.len()).unwrap_or(0),
    }
}

/// Returns a value as text, whatever storage class it arrived in.
fn text_of(value: &Value<'static>) -> String {
    match value {
        Value::Text(text) => String::from_utf8_lossy(&text.utf8_bytes()).into_owned(),
        Value::Blob(blob) => String::from_utf8_lossy(blob.raw()).into_owned(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => {
            String::from_utf8_lossy(&inillucent_value::numeric::real_to_text(*number)).into_owned()
        }
        Value::Null => String::new(),
    }
}

/// Returns the key and the containing path of the element a root path names.
///
/// `json_tree(x, '$.a')` reports its first row with the key `a` and the path
/// `$`, because the walk is inside the whole document rather than inside one
/// that begins at `$.a`. With no root path there is no key and the path is the
/// root itself.
fn root_position(text: &str, steps: &[path::Step]) -> (Option<Value<'static>>, String) {
    let Some(last) = steps.last() else {
        return (None, "$".to_string());
    };
    let key = match last {
        path::Step::Key(name) => Value::owned_text(name.as_bytes()).ok(),
        path::Step::Index(index) => Some(Value::Integer(*index as i64)),
        path::Step::FromEnd(_) | path::Step::Append => None,
    };
    // The containing path is the written path with its last step removed, which
    // is a suffix trim rather than a re-render: the path was written by the
    // caller and its spelling is what the answer repeats.
    let cut = match last {
        path::Step::Key(_) => text.rfind('.'),
        _ => text.rfind('['),
    };
    let parent = match cut {
        Some(0) | None => "$".to_string(),
        Some(cut) => text.get(..cut).unwrap_or("$").to_string(),
    };
    (key, parent)
}

/// Returns the path to a member, quoting the label when it needs quoting.
fn join_key(prefix: &str, name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        && !name.starts_with(|character: char| character.is_ascii_digit());
    if plain {
        format!("{prefix}.{name}")
    } else {
        format!("{prefix}.\"{name}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    /// Walks a document, returning `(fullkey, id, parent)` per row.
    fn walk(kind: Walk, source: &str) -> Vec<(String, i64, Option<i64>)> {
        let tree = parse::parse(source).expect("parses").node;
        let mut cursor = JsonWalkCursor {
            walk: kind,
            rows: Vec::new(),
            at: 0,
        };
        match kind {
            Walk::Each => cursor.each(&tree, 0, "$"),
            Walk::Tree => {
                cursor.rows.push(WalkRow {
                    key: None,
                    node: tree.clone(),
                    id: 0,
                    parent: None,
                    fullkey: "$".to_string(),
                    path: "$".to_string(),
                });
                cursor.descend(&tree, 0, 0, "$");
            }
        }
        cursor
            .rows
            .iter()
            .map(|row| (row.fullkey.clone(), row.id, row.parent))
            .collect()
    }

    /// `json_each` visits the direct children and reports the label's offset.
    ///
    /// The offsets are the ones the pinned 3.53.4 reports for this document.
    /// Its encoding is `BC 1761 1331 1762 4B 1332 1333`, so the two labels sit
    /// at 1 and 5 and the array's members at 8 and 10.
    #[test]
    fn json_each_visits_the_direct_children() {
        let rows = walk(Walk::Each, r#"{"a":1,"b":[2,3]}"#);
        assert_eq!(
            rows,
            vec![("$.a".to_string(), 1, None), ("$.b".to_string(), 5, None)]
        );
    }

    /// `json_tree` visits the element itself and everything under it.
    #[test]
    fn json_tree_visits_everything() {
        let rows = walk(Walk::Tree, r#"{"a":1,"b":[2,3]}"#);
        assert_eq!(
            rows,
            vec![
                ("$".to_string(), 0, None),
                ("$.a".to_string(), 1, Some(0)),
                ("$.b".to_string(), 5, Some(0)),
                ("$.b[0]".to_string(), 8, Some(5)),
                ("$.b[1]".to_string(), 10, Some(5)),
            ]
        );
    }

    /// A root path is located inside the whole document, so the offsets it
    /// reports are the document's rather than the sub-tree's.
    #[test]
    fn a_root_path_keeps_the_document_offsets() {
        let tree = parse::parse(r#"{"a":1,"b":[2,3]}"#).expect("parses").node;
        let steps = path::parse("$.b").expect("parses");
        let (found, offset) = locate(&tree, 0, &steps).expect("found");
        assert_eq!(offset, 7);
        assert!(matches!(found, Node::Array(_)));
    }

    /// A label that is not a bare word is quoted in the path.
    #[test]
    fn a_label_that_needs_quoting_is_quoted() {
        let rows = walk(Walk::Each, r#"{"a b":1}"#);
        assert_eq!(
            rows.first().map(|row| row.0.clone()),
            Some("$.\"a b\"".to_string())
        );
    }

    /// A scalar reports itself, which is not the same as reporting nothing.
    #[test]
    fn a_scalar_reports_itself() {
        let rows = walk(Walk::Each, "7");
        assert_eq!(rows, vec![("$".to_string(), 0, None)]);
    }
}
