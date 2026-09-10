//! inillucent and the pinned SQLite 3.53.4, asked the same JSON questions.
//!
//! Invariant: every claim here is a comparison against a live SQLite process.
//! Nothing in this file writes down what the answer should be, because the
//! answers are the part that is easy to get almost right - `json('{"a":1.50}')`
//! keeps the trailing zero, `json_object('a','[1]')` quotes the string it was
//! given while `json_object('a',json('[1]'))` embeds the array, and
//! `jsonb('{a:0x10}')` stores the four bytes `0x10` rather than the two bytes
//! `16`. A test that asserted a remembered value would be asserting my memory.
//!
//! The binary format is compared through `hex()`, byte for byte. It is not an
//! internal representation: applications store `jsonb()` output in columns and
//! open those files with either engine, so a blob that is merely equivalent is
//! a bug that surfaces the first time the other engine reads it.

use inillucent_compat::differential::{compare_queries, Step};

/// Where this suite's scratch databases live.
const AREA: &str = "json";

/// Runs a list of queries against both engines, failing on any difference.
fn check(name: &str, queries: &[&'static str]) {
    let compared = compare_queries(AREA, name, queries);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, queries.len(), "every query was compared");
}

/// The minified text form keeps whatever spelling it was given.
#[test]
fn json_text_round_trips() {
    check(
        "text",
        &[
            "SELECT json(' { \"a\" : 1.50 } ')",
            "SELECT json('[1,2,3]')",
            "SELECT json('[]'), json('{}')",
            "SELECT json('\"a\\u0041b\"')",
            "SELECT json(1), json(1.5), json(null)",
            "SELECT json('  [1]  ')",
            "SELECT json_pretty('{\"a\":[1,{\"b\":2}],\"c\":\"d\"}')",
            "SELECT json_pretty('[]'), json_pretty('{\"a\":{}}')",
        ],
    );
}

/// Every JSON5 spelling the pinned release accepts, and how it renders.
#[test]
fn json5_input_is_accepted_and_converted() {
    check(
        "json5",
        &[
            "SELECT json('{a:1, b:0x10, c:.5, d:+Infinity, e:NaN, f:''sq'', g:[1,2,],}')",
            // Aliased because the protocol is line-based and an unaliased
            // column would be named after source text containing a newline.
            "SELECT json('[1,/*c*/2, // trailing\n3]') AS commented",
            "SELECT json('[0x10, 0xFF, -0x1f, .5, 5., -.25, +7, 1.]')",
            "SELECT json('[\"a\\x41b\", \"a\\''b\", \"a\\0b\", \"a\\vb\", \"q\\\"q\"]')",
            "SELECT json('[''a\"b'', ''c'']')",
            "SELECT json_valid('{a:1}'), json_valid('{a:1}',2), json_valid('{a:1}',4), json_valid('{a:1}',6)",
            "SELECT json_valid('[1]'), json_valid('x'), json_valid(''), json_valid(null)",
            "SELECT json_error_position('[1,2'), json_error_position('{\"a\":}'), json_error_position('[1,2,,]')",
            "SELECT json_error_position('{\"a\":1}'), json_error_position('{a:1}')",
        ],
    );
}

/// The binary format, compared byte for byte through `hex()`.
#[test]
fn jsonb_blobs_are_byte_identical() {
    check(
        "jsonb",
        &[
            "SELECT hex(jsonb('null')), hex(jsonb('true')), hex(jsonb('false'))",
            "SELECT hex(jsonb('1')), hex(jsonb('1.5')), hex(jsonb('\"ab\"'))",
            "SELECT hex(jsonb('[1,2]')), hex(jsonb('{\"a\":1}')), hex(jsonb('[]')), hex(jsonb('{}'))",
            "SELECT hex(jsonb('{a:1, b:0x10, c:.5, d:+Infinity, e:NaN, f:''sq''}'))",
            "SELECT hex(jsonb('\"a\\nb\"')), hex(jsonb('\"a''b\"')), hex(jsonb('\"b\\u0041c\"'))",
            "SELECT hex(jsonb_object('a', 'b\"c')), hex(jsonb_array('b\"c'))",
            "SELECT hex(jsonb_insert('{}','$.k','b\"c')), hex(jsonb_insert('{}','$.k','v'))",
            "SELECT hex(jsonb_set('{}','$.k','v')), hex(jsonb_replace('{\"k\":1}','$.k','v'))",
            "SELECT hex(jsonb_insert('{}','$.k',1)), hex(jsonb_insert('{}','$.k',1.5)), hex(jsonb_insert('{}','$.k',null))",
            "SELECT hex(jsonb_object('a','')), hex(jsonb_array(''))",
            "SELECT hex(jsonb_object('k\"1', 1))",
            "SELECT hex(jsonb('\"' || replace(hex(zeroblob(20)),'0','x') || '\"'))",
            "SELECT json(x'4C17611331'), json(jsonb('{\"a\":[1,2]}'))",
            "SELECT typeof(jsonb('1')), typeof(json('1'))",
        ],
    );
}

/// Building documents out of SQL values, and the mark that says which is which.
#[test]
fn construction_follows_the_json_mark() {
    check(
        "construct",
        &[
            "SELECT json_array(1,2.0,2.5,1e300,'x',null)",
            "SELECT json_object('a',1.0,'b',1e300)",
            "SELECT json_object('a', json('[1]')), json_object('a','[1]'), json_object('a', jsonb('[1]'))",
            "SELECT json_array(json('[1]')), json_array('[1]')",
            "SELECT json_object('a',1,'a',2)",
            "SELECT json_quote('ab'), json_quote(3.0), json_quote(1), json_quote(null)",
            "SELECT json_quote('a\"b'), json_quote(json('[1]'))",
            "SELECT json_group_array(v), json_group_object(k,v) FROM (SELECT 'a' k, 1 v UNION ALL SELECT 'b', 2)",
            "SELECT json_group_array(v) FROM (SELECT 1 v WHERE 0)",
            "SELECT json_group_object(k,v) FROM (SELECT 'a' k,1 v WHERE 0)",
            "SELECT json_group_array(v) FROM (SELECT null v UNION ALL SELECT 1)",
            "SELECT hex(jsonb_group_array(v)) FROM (SELECT 'b\"c' v)",
            "SELECT hex(jsonb_group_object(k,v)) FROM (SELECT 'a\"' k, 'b' v)",
        ],
    );
}

/// Paths: what they name, what they refuse, and what a miss answers.
#[test]
fn paths_resolve_the_way_the_pinned_release_resolves_them() {
    check(
        "paths",
        &[
            "SELECT json_extract('{\"a\":{\"b\":[7,8]}}','$.a.b[1]')",
            "SELECT json_extract('{\"a b\":1}','$.\"a b\"'), json_extract('{\"a.b\":1}','$.\"a.b\"')",
            "SELECT json_extract('{\"a\":1}','$.a.b')",
            "SELECT json_extract('[[1,2]]','$[0][1]')",
            "SELECT json_extract('[1,2,3]','$[#-1]'), json_extract('[1,2,3]','$[#-3]'), json_extract('[1,2,3]','$[#-4]')",
            "SELECT json_extract('{\"a\":1,\"b\":2}','$.a','$.b')",
            "SELECT json_extract('[1]','$'), json_extract('\"x\"','$'), json_extract('{\"a\":1}','$')",
            "SELECT json_extract('{\"a\":null}','$.a'), typeof(json_extract('{\"a\":null}','$.a'))",
            "SELECT json_extract('[1,2.5,\"x\",null,true,{}]','$[4]'), json_extract('[9e999]','$[0]')",
            "SELECT json_extract('[0x10]','$[0]'), typeof(json_extract('[0x10]','$[0]'))",
            "SELECT '{\"a\":1}'->'$.a', '{\"a\":1}'->>'$.a'",
            "SELECT '{\"a\":[1,2]}'->'$.a', '{\"a\":[1,2]}'->>'$.a'",
            "SELECT '[1,2]'->0, '[1,2]'->>1, '{\"a\":3}'->'a', '{\"a\":3}'->>'a'",
            "SELECT json_type('[1,2.5,\"x\",null,true,{}]','$[0]'), json_type('[1,2.5,\"x\",null,true,{}]','$[1]')",
            "SELECT json_type('[1,2.5,\"x\",null,true,{}]','$[2]'), json_type('[1,2.5,\"x\",null,true,{}]','$[3]')",
            "SELECT json_type('[1,2.5,\"x\",null,true,{}]','$[4]'), json_type('[1,2.5,\"x\",null,true,{}]','$[5]')",
            "SELECT json_type('{\"a\":1}','$.b'), json_type('{\"a\":1}')",
            "SELECT json_array_length('{\"a\":1}'), json_array_length('[1,2]','$'), json_array_length('[1,2]','$.a')",
        ],
    );
}

/// A bad path is an error, and a missing one is not.
#[test]
fn a_bad_path_is_an_error() {
    check(
        "bad-paths",
        &[
            "SELECT json_extract('{\"a\":1}','a')",
            "SELECT json_extract('{\"a\":1}','$a')",
            "SELECT json_extract('[1,2,3]','$[-1]')",
            "SELECT json('x')",
            "SELECT json('')",
            "SELECT json_array(x'6162')",
            "SELECT json_insert('{}','$.a',x'6162')",
            "SELECT json_object('a')",
            "SELECT json_insert('{}','$.a')",
        ],
    );
}

/// Editing: insert, replace, set, remove, and the merge patch.
#[test]
fn editing_follows_presence() {
    check(
        "editing",
        &[
            "SELECT json_insert('{\"a\":1}','$.b',2), json_replace('{\"a\":1}','$.a',2)",
            "SELECT json_set('{\"a\":1}','$.a',2,'$.c',3)",
            "SELECT json_insert('[1]','$[0]',9), json_set('[1]','$[0]',9)",
            "SELECT json_replace('{\"a\":1}','$.b',2), json_set('{\"a\":1}','$',5)",
            "SELECT json_set('{}','$.a.b',1), json_set('{}','$[0]',1), json_set('[]','$[0].a',1)",
            "SELECT json_set('{\"a\":1}','$.a.b',2), json_set('{\"a\":1}','$.b[0]',7)",
            "SELECT json_insert('{\"a\":[1]}','$.a[#]',2), json_insert('{\"a\":[1,2,3]}','$.a[#-1]',9)",
            "SELECT json_insert('[]','$[#]',1,'$[#]',2)",
            "SELECT json_remove('[1,2,3]','$[0]'), json_remove('[1,2,3]','$[0]','$[0]')",
            "SELECT json_remove('{\"a\":1}','$'), json_remove('{\"a\":1}','$.zz')",
            "SELECT json_patch('{\"a\":1,\"b\":2}','{\"b\":null,\"c\":3}')",
            "SELECT json_patch('[1,2]','{\"a\":1}'), json_patch('{\"a\":{\"b\":1}}','{\"a\":{\"c\":2}}')",
            "SELECT json_set('{\"a\":1}','$.b',json('[1,2]'))",
        ],
    );
}

/// JSON stored in and read back out of a table, which is where the mark ends.
#[test]
fn documents_survive_a_round_trip_through_a_table() {
    let steps = [
        Step::Exec("CREATE TABLE d(id INTEGER PRIMARY KEY, t TEXT, b BLOB)"),
        Step::Exec(
            "INSERT INTO d VALUES (1, json('{\"a\":[1,2],\"b\":\"x\"}'), jsonb('{\"a\":[1,2]}'))",
        ),
        Step::Query("SELECT t, hex(b) FROM d"),
        Step::Query("SELECT json_extract(t,'$.a[1]'), json_extract(b,'$.a[0]') FROM d"),
        Step::Query("SELECT json_type(b), json_array_length(b,'$.a') FROM d"),
        Step::Query("SELECT json_valid(b,8), json_valid(b,4), json_valid(b,1) FROM d"),
        Step::Query("SELECT json(b) FROM d"),
        Step::Exec("UPDATE d SET t = json_set(t,'$.c',3)"),
        Step::Query("SELECT t FROM d"),
        Step::Query("SELECT json_group_array(json_extract(t,'$.b')) FROM d"),
    ];
    let compared = inillucent_compat::differential::compare(AREA, "table", &steps);
    if compared == 0 {
        return;
    }
    assert_eq!(compared, steps.len(), "every step was compared");
}
