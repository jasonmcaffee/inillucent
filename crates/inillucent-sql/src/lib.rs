//! First-party lexer, parser, AST, binder, semantic rewrites, and logical and
//! physical plans.
//!
//! Invariant: the SQL front end is pure. It parses, binds, plans and compiles;
//! it opens no file, reads no page, and holds no connection. Everything it
//! needs to know about a schema arrives through [`catalog_view::CatalogView`],
//! which is a read-only view someone else has already built.
//!
//! That interface is why this crate sits *below* `inillucent-catalog` rather than
//! above it. The catalog has to parse the CREATE text stored in
//! `sqlite_schema` with this parser, and a crate cannot be both above and below
//! another; the parser is the more fundamental half, so it goes underneath and
//! the catalog implements the view.
//!
//! Module map, in the order SQL moves through them:
//!
//! - [`keyword`] - the pinned release's keyword table and its fallback rule;
//! - [`lexer`] - bytes to tokens, zero copy, with spans;
//! - [`precedence`] - the operator table, as data;
//! - [`ast`] - the arena and every node kind;
//! - [`diagnostic`] - syntax failures, with offsets;
//! - [`parser`] - recursive descent for statements, Pratt for expressions;
//! - [`catalog_view`] - what the binder is allowed to know about a schema;
//! - [`bind`] - names to columns, and the bound relational tree;
//! - [`plan`] - the logical and physical plans the compiler walks.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
// Tests assert on exact values and are allowed to fail loudly; the bans above
// exist to keep panics and wrapping out of paths that read caller input.
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

pub mod ast;
pub mod bind;
pub mod catalog_view;
pub mod cost;
pub mod declare;
pub mod diagnostic;
pub mod directive;
pub mod dml;
pub mod foreign_key;
pub mod function;
pub mod keyword;
pub mod lexer;
pub mod parser;
pub mod plan;
pub mod pragma_register;
pub mod precedence;
pub mod rewrite;
pub mod vtab;

pub use ast::{Ast, Statement};
pub use diagnostic::{ParseError, ParseErrorKind};
pub use lexer::{Lexer, Span, Token, TokenKind};
pub use parser::{
    classify_statement, parse_expression, parse_next_statement, ParameterMap, ParsedStatement,
    StatementClass,
};

/// The implementation phase that filled this crate in, as named by the TDD.
pub const IMPLEMENTATION_PHASE: &str = "phase 5: lexer, parser, AST, and syntax parity";
