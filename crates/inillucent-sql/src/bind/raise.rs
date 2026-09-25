//! Binding `RAISE(...)`.
//!
//! Invariant: **a `RAISE` binds only inside a trigger body, and its message is
//! any expression.** A string literal is kept as text, which is what the
//! foreign key bodies the binder synthesises use; anything else is bound over
//! the trigger's row and evaluated when the `RAISE` fires.

use super::{unsupported, Binder, BoundExpr};
use crate::ast::{ExprId, RaiseAction};
use crate::diagnostic::ParseError;
use crate::lexer::Span;

impl Binder<'_> {
    /// Binds `RAISE(action[, message])`.
    ///
    /// **The message is an expression, as SQLite takes it.** `RAISE(ABORT,
    /// 'too big: ' || NEW.n)` names the value that broke the rule, which is
    /// the reason to write a guard trigger at all; it used to be a syntax
    /// error at the `||`.
    ///
    /// @param action - which action was written
    /// @param message - the message expression, when the action takes one
    /// @param span - where the call was written, for a refusal
    pub(super) fn bind_raise(
        &mut self,
        action: RaiseAction,
        message: Option<ExprId>,
        span: Span,
    ) -> Result<BoundExpr, ParseError> {
        // Outside a trigger body there is nothing for it to abandon, so
        // SQLite refuses it there rather than treating it as a no-op.
        if self.row_aliases.is_none() {
            return Err(unsupported("RAISE outside a trigger", span));
        }
        let bound = match message {
            Some(id) => Some(self.bind_expr(id)?),
            None => None,
        };
        let (message, computed) = match bound {
            Some(BoundExpr::Text(text)) => (Some(text), None),
            Some(other) => (None, Some(Box::new(other))),
            None => (None, None),
        };
        Ok(BoundExpr::Raise {
            action,
            message,
            computed,
            foreign_key: false,
        })
    }
}
