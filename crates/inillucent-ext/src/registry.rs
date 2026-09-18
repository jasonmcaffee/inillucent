//! What a connection knows how to reach: modules, collations, functions, and
//! the policy that decides which of them a schema may name.
//!
//! Invariant: a schema is data, and data does not get to choose what code runs.
//! That sentence is the whole of `PRAGMA trusted_schema`, and it is why this
//! file exists as something other than three maps. A `CREATE TABLE` with a
//! `DEFAULT` calling a function, a virtual table naming a module, an index on
//! an expression - each is a string in a file somebody else may have written,
//! and each is a place where opening a database could run code chosen by the
//! file. SQLite draws the line with three flags, and so does this: a function
//! is `direct-only` unless it says otherwise, `innocuous` if it is safe for a
//! schema to call, and the whole distinction is switched off for schemas the
//! connection has said it trusts.

use std::collections::BTreeMap;
use std::sync::Arc;

use inillucent_base::{DbError, DbResult};
use inillucent_value::Value;

use crate::vtab::{
    fts5::Fts5Module, json_each::JsonWalkModule, rtree::RTreeModule, series::SeriesModule, Module,
};

/// What a registered function promises about itself, and where a name was
/// written.
///
/// **Defined in `inillucent-sql` and re-exported here (task-1972).** The binder
/// is the layer that enforces the promise and it sits below this crate, so the
/// type has to live below it too. Every path an application already writes -
/// `inillucent_ext::registry::FunctionFlags`, `::CallSite` - resolves to the
/// same type it always did.
pub use inillucent_sql::function::{CallSite, FunctionFlags};

/// The policy flags a connection carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Whether the schema may name anything that is not innocuous.
    ///
    /// SQLite's default is on, and it is on here, because turning it off is a
    /// promise about every database the application will ever open rather than
    /// about the one in front of it.
    pub trusted_schema: bool,
    /// Whether the connection refuses the operations that can rewrite the
    /// schema out from under itself.
    ///
    /// `PRAGMA defensive` forbids `writable_schema`, `PRAGMA journal_mode=off`
    /// and writes to shadow tables. It is what an application sets when it is
    /// about to open a file it did not write.
    pub defensive: bool,
    /// Whether the schema table may be written directly.
    pub writable_schema: bool,
    /// Whether an extension may be loaded at all.
    pub load_extension: bool,
}

impl Default for Policy {
    /// Returns the defaults, which are SQLite's.
    fn default() -> Policy {
        Policy {
            trusted_schema: true,
            defensive: false,
            writable_schema: false,
            load_extension: false,
        }
    }
}

/// What an application-defined scalar function does.
pub type ScalarBody = Arc<dyn Fn(&[Value<'static>]) -> DbResult<Value<'static>> + Send + Sync>;

/// What an application-defined aggregate does with a whole group.
///
/// It is handed every row the group collected, in order, and returns the one
/// value the group reduces to. Rows rather than a running accumulator is a
/// deliberate choice: an implementation written in C keeps its state in memory
/// this engine must not look inside, and driving `xStep` and `xFinal` at the
/// end of the group is how that state stays entirely on the other side of the
/// boundary.
pub type AggregateBody =
    Arc<dyn Fn(&[Vec<Value<'static>>]) -> DbResult<Value<'static>> + Send + Sync>;

/// What a registered function is.
#[derive(Clone)]
pub enum UserBody {
    /// One value per row.
    Scalar(ScalarBody),
    /// One value per group.
    Aggregate(AggregateBody),
}

/// One function an application registered.
#[derive(Clone)]
pub struct UserFunction {
    /// The name as registered, in its original case.
    pub name: String,
    /// How many arguments it takes, or -1 for any number.
    pub arity: i32,
    /// What it promises about itself.
    pub flags: FunctionFlags,
    /// What it does.
    pub body: UserBody,
}

impl UserFunction {
    /// Returns a scalar function registered from outside, with the flags such a
    /// function carries.
    ///
    /// **The constructor exists because the `Default` derive was the trap
    /// (task-1969, 7.4).** `FunctionFlags` derives `Default`, so
    /// `FunctionFlags { deterministic: true, ..Default::default() }` is
    /// `direct_only: false` - a registration that reads as "I set one flag and
    /// took the defaults for the rest" and in fact says "a schema may name
    /// this". `inillucent-search`'s `embed` was registered that way while its
    /// own doc comment said "It stays `direct_only`", so with a trusted schema
    /// a 275 MB model load was callable from a `CHECK` constraint or an index
    /// expression.
    ///
    /// `FunctionFlags::default()` remains, because [`FunctionFlags::builtin`]
    /// needs a `Default` to exist for the struct-update syntax the built-ins
    /// use. What changed is that a registrant has a name to reach for that
    /// means what the two doc comments above already claim the default means.
    ///
    /// @param name - the name as it should be registered
    /// @param arity - how many arguments it takes, or -1 for any number
    /// @param body - what it does
    pub fn external(name: &str, arity: i32, body: UserBody) -> UserFunction {
        UserFunction {
            name: name.to_string(),
            arity,
            flags: FunctionFlags::external(),
            body,
        }
    }

    /// Returns whether the function reduces a group rather than a row.
    pub fn is_aggregate(&self) -> bool {
        matches!(self.body, UserBody::Aggregate(_))
    }

    /// Returns whether the function accepts a call with this many arguments.
    pub fn accepts(&self, argc: usize) -> bool {
        self.arity < 0 || self.arity as usize == argc
    }
}

/// Everything a connection can reach by name.
#[derive(Clone, Default)]
pub struct Registry {
    modules: BTreeMap<String, Arc<dyn Module>>,
    function_flags: BTreeMap<String, FunctionFlags>,
    /// Application functions, keyed by folded name and then by arity.
    ///
    /// Two arities of one name are two functions - `overlay(a,b,c)` and
    /// `overlay(a,b,c,d)` are separate registrations in SQLite too - and the
    /// any-arity form is kept under -1 and consulted when no exact match is
    /// there, which is the order SQLite resolves in.
    functions: BTreeMap<(String, i32), Arc<UserFunction>>,
    /// The extensions an application has explicitly allowed to be loaded.
    ///
    /// An allow-list rather than a search path: "load whatever is at this path"
    /// is the vulnerability, and a list of exactly what may be loaded is the
    /// only version of the feature that can be reasoned about.
    allowed_extensions: Vec<String>,
    policy: Policy,
}

impl std::fmt::Debug for Registry {
    /// Reports what is registered, since none of the values can print itself.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registry")
            .field("modules", &self.modules.keys().collect::<Vec<_>>())
            .field("policy", &self.policy)
            .finish()
    }
}

impl Registry {
    /// Returns a registry holding every first-party module.
    pub fn with_builtins() -> Registry {
        let mut registry = Registry::default();
        registry.register_module(Arc::new(JsonWalkModule::each()));
        registry.register_module(Arc::new(JsonWalkModule::tree()));
        registry.register_module(Arc::new(SeriesModule));
        registry.register_module(Arc::new(RTreeModule::float()));
        registry.register_module(Arc::new(RTreeModule::integer()));
        registry.register_module(Arc::new(RTreeModule::geopoly()));
        registry.register_module(Arc::new(Fts5Module));
        registry.register_module(Arc::new(crate::vtab::fts5::Fts3Module::three()));
        registry.register_module(Arc::new(crate::vtab::fts5::Fts3Module::four()));
        registry.register_module(Arc::new(crate::vtab::fts5::vocab::Fts5VocabModule));
        // pgvector's other index type, which is k-means centroids and an
        // inverted list per centroid rather than a graph - see
        // `crate::vtab::ivfflat`.
        registry.register_module(Arc::new(crate::vtab::ivfflat::IvfFlatModule));
        registry
    }

    /// Registers a function, replacing one of the same name and arity.
    pub fn register_function(&mut self, function: UserFunction) {
        let key = (function.name.to_ascii_lowercase(), function.arity);
        self.function_flags.insert(key.0.clone(), function.flags);
        self.functions.insert(key, Arc::new(function));
    }

    /// Removes a function by name and arity, reporting whether one went.
    pub fn unregister_function(&mut self, name: &str, arity: i32) -> bool {
        self.functions
            .remove(&(name.to_ascii_lowercase(), arity))
            .is_some()
    }

    /// Looks up a function by name and how many arguments a call passes.
    ///
    /// An exact arity wins over the any-arity registration, which is how an
    /// application can define both a fast two-argument form and a general one.
    pub fn function(&self, name: &[u8], argc: usize) -> Option<Arc<UserFunction>> {
        let folded = String::from_utf8_lossy(name).to_ascii_lowercase();
        if let Ok(arity) = i32::try_from(argc) {
            if let Some(found) = self.functions.get(&(folded.clone(), arity)) {
                return Some(Arc::clone(found));
            }
        }
        self.functions.get(&(folded, -1)).map(Arc::clone)
    }

    /// Returns every registered function, for the binder and `function_list`.
    pub fn functions(&self) -> Vec<Arc<UserFunction>> {
        self.functions.values().map(Arc::clone).collect()
    }

    /// Registers one module, replacing any module of the same name.
    pub fn register_module(&mut self, module: Arc<dyn Module>) {
        self.modules
            .insert(module.name().to_ascii_lowercase(), module);
    }

    /// Returns one module by name.
    pub fn module(&self, name: &[u8]) -> Option<Arc<dyn Module>> {
        let folded = String::from_utf8_lossy(name).to_ascii_lowercase();
        self.modules.get(&folded).cloned()
    }

    /// Returns every registered module name, in order.
    pub fn module_names(&self) -> Vec<String> {
        self.modules.keys().cloned().collect()
    }

    /// Returns whether a name is an eponymous module, usable with no `CREATE`.
    pub fn eponymous(&self, name: &[u8]) -> Option<Arc<dyn Module>> {
        self.module(name).filter(|module| module.eponymous())
    }

    /// Records what a function promises about itself.
    pub fn set_function_flags(&mut self, name: &str, flags: FunctionFlags) {
        self.function_flags.insert(name.to_ascii_lowercase(), flags);
    }

    /// Returns what a function promises, defaulting to a built-in's promise.
    pub fn function_flags(&self, name: &[u8]) -> FunctionFlags {
        let folded = String::from_utf8_lossy(name).to_ascii_lowercase();
        self.function_flags
            .get(&folded)
            .copied()
            .unwrap_or_else(FunctionFlags::builtin)
    }

    /// Returns the policy flags.
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Returns the policy flags, to be changed.
    pub fn policy_mut(&mut self) -> &mut Policy {
        &mut self.policy
    }

    /// Allows one extension to be loaded, by its exact path.
    pub fn allow_extension(&mut self, path: &str) {
        let path = path.to_string();
        if !self.allowed_extensions.contains(&path) {
            self.allowed_extensions.push(path);
        }
    }

    /// Returns the extensions that may be loaded.
    pub fn allowed_extensions(&self) -> &[String] {
        &self.allowed_extensions
    }

    /// Decides whether a name may be called from where it was written.
    ///
    /// The rule itself is [`inillucent_sql::function::schema_refusal`], because
    /// the binder enforces the same rule on the same flags and two copies of it
    /// would eventually be two rules. This is the entry point an application
    /// that holds a registry asks through; the binder asks through the flags it
    /// was handed with the name.
    ///
    /// @param name - the function the schema or statement wrote
    /// @param site - where the call was written
    pub fn authorize_function(&self, name: &[u8], site: CallSite) -> DbResult<()> {
        match inillucent_sql::function::schema_refusal(
            self.function_flags(name),
            site,
            self.policy.trusted_schema,
        ) {
            Some(why) => Err(refused(name, why)),
            None => Ok(()),
        }
    }

    /// Decides whether an extension at a path may be loaded.
    ///
    /// **It has no caller, and that is not the defect task-1972 fixed in
    /// `authorize_function`.** There is nothing here that loads an extension:
    /// `load_extension(path)` refuses every path in
    /// `inillucent_scalar::builtin::refusal_for`, and the shell's `.load`
    /// refuses every path in `inillucent_cli::dot`. Both refuse in the words
    /// the platform uses for a library it cannot open, and a build that forbids
    /// `unsafe` cannot call `LoadLibrary` anyway. This is the policy the day
    /// something can load one, and the two refusals above are what
    /// `crates/inillucent-compat/tests/schema_function_policy.rs` checks,
    /// because those are the guarantee a caller actually has.
    ///
    /// There is no `authorize_module` at all, so nothing there is inert either:
    /// `CREATE VIRTUAL TABLE` resolves a name through [`Registry::module`] and
    /// a name that is not registered is refused by the resolution.
    ///
    /// @param path - the library a caller asked for
    pub fn authorize_extension(&self, path: &str) -> DbResult<()> {
        if !self.policy.load_extension {
            return Err(refused(
                path.as_bytes(),
                "cannot be loaded: extension loading is off",
            ));
        }
        if !self
            .allowed_extensions
            .iter()
            .any(|allowed| allowed == path)
        {
            return Err(refused(
                path.as_bytes(),
                "cannot be loaded: it is not on the allow-list",
            ));
        }
        Ok(())
    }

    /// Decides whether a statement may write a shadow table.
    ///
    /// A shadow table is a module's private storage. Writing one directly is
    /// how a hostile file gets a module to read structures no module ever
    /// wrote, which is the class of bug `PRAGMA defensive` exists to close.
    ///
    /// **Which table is a shadow is decided by the caller**, because only the
    /// engine knows what each connected module was handed:
    /// `ImportedDatabase::is_shadow_table` derives the names from the roots the
    /// modules were connected with, and `refuse_shadow_write` asks this once a
    /// write has been bound. Until task-1972 nothing asked, and nothing set
    /// [`Policy::defensive`] either, so `.dbconfig defensive on` refused a
    /// `journal_mode=off` and nothing else.
    ///
    /// @param name - the shadow table a statement is about to write
    pub fn authorize_shadow_write(&self, name: &[u8]) -> DbResult<()> {
        if !self.policy.defensive {
            return Ok(());
        }
        Err(refused(
            name,
            "is a shadow table and the connection is defensive",
        ))
    }
}

/// Returns the refusal a policy check reports.
///
/// **The sentence is the message, not the detail (task-1969, 7.4).** It was
/// `with_detail`, which `inillucent-base` documents as internal diagnostic text
/// a caller sees only with diagnostics on - so `DbError::message()` answered the
/// primary code's manifest text and a schema refused for naming a direct-only
/// function said `SQL logic error`. That is the same shape task-1952 fixed in
/// `inillucent-search`'s missing-model refusal: a sentence written for a person
/// that never reached one.
///
/// The name is the only thing in it that came from the database, and it is a
/// function or table name rather than a value, so there is nothing here to keep
/// inside the process.
///
/// @param name - what was refused
/// @param why - the rest of the sentence, which follows the name
fn refused(name: &[u8], why: &str) -> DbError {
    DbError::primary(inillucent_base::PrimaryCode::Error)
        .with_message(format!("{} {why}", String::from_utf8_lossy(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first-party modules are there without anything registering them.
    #[test]
    fn the_builtin_modules_are_registered() {
        let registry = Registry::with_builtins();
        assert!(registry.module(b"json_each").is_some());
        assert!(registry.module(b"JSON_TREE").is_some());
        assert!(registry.module(b"generate_series").is_some());
        assert!(registry.module(b"nope").is_none());
    }

    /// A statement may call anything; a schema may not call a direct-only
    /// function even when the schema is trusted.
    #[test]
    fn direct_only_beats_a_trusted_schema() {
        let mut registry = Registry::with_builtins();
        registry.set_function_flags("risky", FunctionFlags::external());
        assert!(registry
            .authorize_function(b"risky", CallSite::Statement)
            .is_ok());
        assert!(registry.policy().trusted_schema);
        assert!(registry
            .authorize_function(b"risky", CallSite::Schema)
            .is_err());
    }

    /// An untrusted schema may still call an innocuous function.
    #[test]
    fn an_untrusted_schema_keeps_the_innocuous_ones() {
        let mut registry = Registry::with_builtins();
        registry.policy_mut().trusted_schema = false;
        registry.set_function_flags(
            "harmless",
            FunctionFlags {
                direct_only: false,
                innocuous: true,
                deterministic: true,
            },
        );
        registry.set_function_flags(
            "opaque",
            FunctionFlags {
                direct_only: false,
                innocuous: false,
                deterministic: true,
            },
        );
        assert!(registry
            .authorize_function(b"harmless", CallSite::Schema)
            .is_ok());
        assert!(registry
            .authorize_function(b"opaque", CallSite::Schema)
            .is_err());
    }

    /// An extension is refused unless loading is on *and* it is on the list.
    #[test]
    fn an_extension_needs_both_the_switch_and_the_list() {
        let mut registry = Registry::with_builtins();
        assert!(registry.authorize_extension("/tmp/x.so").is_err());
        registry.policy_mut().load_extension = true;
        assert!(registry.authorize_extension("/tmp/x.so").is_err());
        registry.allow_extension("/tmp/x.so");
        assert!(registry.authorize_extension("/tmp/x.so").is_ok());
        assert!(registry.authorize_extension("/tmp/y.so").is_err());
    }

    /// A defensive connection refuses a shadow-table write outright.
    #[test]
    fn defensive_refuses_a_shadow_write() {
        let mut registry = Registry::with_builtins();
        assert!(registry.authorize_shadow_write(b"t_data").is_ok());
        registry.policy_mut().defensive = true;
        assert!(registry.authorize_shadow_write(b"t_data").is_err());
    }
}
