//! `sqlite3_create_function` and `sqlite3_create_collation`.
//!
//! Invariant: a function an application registers is called with values it may
//! read for the duration of the call and no longer, and its answer is copied
//! out before the call returns. Nothing the implementation allocates is kept by
//! this library, and nothing this library allocates is handed to it to free -
//! which is what makes the boundary reviewable in one sitting.
//!
//! # Aggregates
//!
//! `xStep` and `xFinal` keep their state in memory `sqlite3_aggregate_context`
//! hands out, and this library must never look inside it. So a group is driven
//! all at once: the engine collects the group's rows, and this module makes one
//! context, calls `xStep` once per row, calls `xFinal`, and frees the context.
//! An implementation sees exactly the sequence it would see from SQLite; what
//! differs is *when*, and no correct implementation can tell.
//!
//! # Destructors
//!
//! `sqlite3_create_function_v2` takes an `xDestroy` for the application pointer
//! and promises to call it exactly once - including when the registration
//! itself fails, which is the case that leaks in a lot of bindings.

use std::os::raw::{c_char, c_int, c_void};
use std::sync::Arc;

use inillucent_legacy::{collation, extensions, DbError, Value};

use crate::codes::{SQLITE_MISUSE, SQLITE_OK};
use crate::handle::{c_str, connection, misuse, sqlite3};
use crate::value::{sqlite3_context, sqlite3_value};

/// The C function a scalar implementation is.
pub type ScalarCallback =
    unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value);
/// The C function an aggregate's step is.
pub type StepCallback = unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value);
/// The C function an aggregate's finish is.
pub type FinalCallback = unsafe extern "C" fn(*mut sqlite3_context);
/// The C function a destructor is.
pub type DestroyCallback = unsafe extern "C" fn(*mut c_void);
/// The C function a collation is.
pub type CompareCallback =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *const c_void) -> c_int;

/// What a connection remembers about one registration.
///
/// It exists so the destructor can be called exactly once, when the
/// registration is replaced or the connection goes.
pub struct Registration {
    /// The application pointer, and what to call to release it.
    pub(crate) destroy: Option<(DestroyCallback, *mut c_void)>,
}

impl Drop for Registration {
    /// Calls the application's destructor, once.
    fn drop(&mut self) {
        if let Some((destroy, data)) = self.destroy.take() {
            // SAFETY: the destructor is the caller's, was given with this
            // pointer, and the `take` is what makes it once.
            unsafe { destroy(data) };
        }
    }
}

/// A raw pointer promised to outlive the registration that carries it.
///
/// The promise is the caller's: SQLite documents that the application pointer
/// stays valid until `xDestroy` is called, and a connection is not shared
/// between threads in this engine.
#[derive(Clone, Copy)]
struct Carried(*mut c_void);

// SAFETY: see the type comment.
unsafe impl Send for Carried {}
// SAFETY: as above.
unsafe impl Sync for Carried {}

/// Registers a scalar or aggregate function.
///
/// # Safety
///
/// The handle must be open and `name` NUL-terminated. Each callback must be
/// callable with the arguments its type names, and `data` must stay valid until
/// the registration is replaced or the connection is closed.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sqlite3_create_function(
    handle: *mut sqlite3,
    name: *const c_char,
    arity: c_int,
    encoding: c_int,
    data: *mut c_void,
    scalar: Option<ScalarCallback>,
    step: Option<StepCallback>,
    finish: Option<FinalCallback>,
) -> c_int {
    sqlite3_create_function_v2(
        handle, name, arity, encoding, data, scalar, step, finish, None,
    )
}

/// Registers a function, with a destructor for the application pointer.
///
/// # Safety
///
/// As [`sqlite3_create_function`]. `destroy`, when given, is called exactly
/// once: on replacement, on close, or immediately if this call is refused.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    handle: *mut sqlite3,
    name: *const c_char,
    arity: c_int,
    encoding: c_int,
    data: *mut c_void,
    scalar: Option<ScalarCallback>,
    step: Option<StepCallback>,
    finish: Option<FinalCallback>,
    destroy: Option<DestroyCallback>,
) -> c_int {
    let Some(database) = connection(handle) else {
        release(destroy, data);
        return misuse();
    };
    let Some(bytes) = c_str(name) else {
        release(destroy, data);
        return database.last.refuse(SQLITE_MISUSE, "no function was named");
    };
    if encoding & 0xf != crate::codes::SQLITE_UTF8 {
        release(destroy, data);
        return database
            .last
            .refuse(SQLITE_MISUSE, "only SQLITE_UTF8 is supported");
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    let flags = flags_of(encoding);
    let owner = handle;
    let carried = Carried(data);
    let outcome = match (scalar, step, finish) {
        (Some(scalar), None, None) => database.connection.create_scalar_function(
            &text,
            arity,
            flags,
            scalar_body(scalar, carried, owner),
        ),
        (None, Some(step), Some(finish)) => database.connection.create_aggregate_function(
            &text,
            arity,
            flags,
            aggregate_body(step, finish, carried, owner),
        ),
        // Removing a registration is how SQLite spells "all three are null",
        // and it is the one case where nothing new is kept.
        (None, None, None) => {
            let _ = database.connection.remove_function(&text, arity);
            database.registered.borrow_mut().remove(&key(&text, arity));
            release(destroy, data);
            return database.succeed();
        }
        _ => {
            release(destroy, data);
            return database
                .last
                .refuse(SQLITE_MISUSE, "a function needs xFunc, or xStep and xFinal");
        }
    };
    match outcome {
        Err(error) => {
            release(destroy, data);
            database.fail(&error)
        }
        Ok(()) => {
            // Replacing a registration drops the old one, which is what calls
            // the previous destructor - exactly once, and here rather than at
            // some later point a caller cannot predict.
            database.registered.borrow_mut().insert(
                key(&text, arity),
                Registration {
                    destroy: destroy.map(|destroy| (destroy, data)),
                },
            );
            database.succeed()
        }
    }
}

/// Returns the map key one registration is kept under.
fn key(name: &str, arity: c_int) -> String {
    format!("{}/{arity}", name.to_ascii_lowercase())
}

/// Calls a destructor the caller supplied, when there is one.
///
/// # Safety
///
/// The destructor must be callable once with `data`.
unsafe fn release(destroy: Option<DestroyCallback>, data: *mut c_void) {
    if let Some(destroy) = destroy {
        destroy(data);
    }
}

/// Turns the encoding argument's flag bits into what the registry records.
fn flags_of(encoding: c_int) -> extensions::FunctionFlags {
    let mut flags = extensions::FunctionFlags::external();
    flags.deterministic = encoding & crate::codes::SQLITE_DETERMINISTIC != 0;
    flags.innocuous = encoding & crate::codes::SQLITE_INNOCUOUS != 0;
    flags.direct_only = encoding & crate::codes::SQLITE_DIRECTONLY != 0 || !flags.innocuous;
    flags
}

/// Wraps a C scalar implementation as something the engine can call.
fn scalar_body(
    scalar: ScalarCallback,
    data: Carried,
    owner: *mut sqlite3,
) -> extensions::ScalarBody {
    let owner = Carried(owner.cast());
    Arc::new(move |arguments: &[Value<'static>]| {
        // Naming both wrappers captures them whole. Capturing `data.0` instead
        // would capture a raw pointer, which is neither `Send` nor `Sync` - the
        // wrapper is what carries that promise.
        let (data, owner) = (data, owner);
        let mut context = context_for(data, owner);
        let mut values = wrap(arguments);
        let mut pointers: Vec<*mut sqlite3_value> =
            values.iter_mut().map(std::ptr::from_mut).collect();
        // SAFETY: the callback is the caller's; the context and the value array
        // live until this closure returns, which is after the call.
        unsafe {
            scalar(
                std::ptr::from_mut(&mut context),
                pointers.len() as c_int,
                pointers.as_mut_ptr(),
            );
        }
        finish_context(context)
    })
}

/// Wraps a C aggregate as something the engine can call once per group.
fn aggregate_body(
    step: StepCallback,
    finish: FinalCallback,
    data: Carried,
    owner: *mut sqlite3,
) -> extensions::AggregateBody {
    let owner = Carried(owner.cast());
    Arc::new(move |rows: &[Vec<Value<'static>>]| {
        // See `scalar_body`.
        let (data, owner) = (data, owner);
        let mut context = context_for(data, owner);
        for row in rows {
            let mut values = wrap(row);
            let mut pointers: Vec<*mut sqlite3_value> =
                values.iter_mut().map(std::ptr::from_mut).collect();
            // SAFETY: as in `scalar_body`; the context is the same one for
            // every row of the group, which is what `sqlite3_aggregate_context`
            // promises an implementation.
            unsafe {
                step(
                    std::ptr::from_mut(&mut context),
                    pointers.len() as c_int,
                    pointers.as_mut_ptr(),
                );
            }
            if context.result.is_err() {
                break;
            }
        }
        // SAFETY: the callback is the caller's and the context is still alive.
        unsafe { finish(std::ptr::from_mut(&mut context)) };
        finish_context(context)
    })
}

/// Builds a fresh call context.
fn context_for(data: Carried, owner: Carried) -> sqlite3_context {
    sqlite3_context {
        result: Ok(Value::Null),
        subtype: 0,
        user_data: data.0,
        owner: owner.0.cast(),
        aggregate: std::ptr::null_mut(),
        aggregate_bytes: 0,
    }
}

/// Takes a context's answer and releases anything it allocated.
fn finish_context(mut context: sqlite3_context) -> Result<Value<'static>, DbError> {
    if !context.aggregate.is_null() {
        // SAFETY: allocated by `sqlite3_aggregate_context` with this library's
        // allocator, and reachable from nowhere else once the group is done.
        unsafe { crate::memory::sqlite3_free(context.aggregate) };
        context.aggregate = std::ptr::null_mut();
    }
    core::mem::replace(&mut context.result, Ok(Value::Null))
}

/// Wraps a row's values so C can read them.
fn wrap(values: &[Value<'static>]) -> Vec<sqlite3_value> {
    values
        .iter()
        .cloned()
        .map(sqlite3_value::new)
        .collect::<Vec<sqlite3_value>>()
}

/// Returns the application pointer a function was registered with.
///
/// # Safety
///
/// The context must be one passed to a function implementation.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_user_data(context: *mut sqlite3_context) -> *mut c_void {
    match context.as_ref() {
        Some(held) => held.user_data,
        None => std::ptr::null_mut(),
    }
}

/// Returns the connection a function call is running on.
///
/// # Safety
///
/// As [`sqlite3_user_data`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_context_db_handle(context: *mut sqlite3_context) -> *mut sqlite3 {
    match context.as_ref() {
        Some(held) => held.owner,
        None => std::ptr::null_mut(),
    }
}

/// Returns an aggregate's per-group memory, zeroed on the first call.
///
/// A first call with `bytes` of zero or less returns null without allocating,
/// which is how `xFinal` asks "was there a first row?" without creating the
/// state it is about to read.
///
/// # Safety
///
/// As [`sqlite3_user_data`]. The pointer is this library's and must not be
/// freed by the caller; it goes when the group does.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_aggregate_context(
    context: *mut sqlite3_context,
    bytes: c_int,
) -> *mut c_void {
    let Some(held) = context.as_mut() else {
        return std::ptr::null_mut();
    };
    if !held.aggregate.is_null() {
        return held.aggregate;
    }
    if bytes <= 0 {
        return std::ptr::null_mut();
    }
    let block = crate::memory::sqlite3_malloc(bytes);
    if block.is_null() {
        return std::ptr::null_mut();
    }
    std::ptr::write_bytes(block.cast::<u8>(), 0, bytes as usize);
    held.aggregate = block;
    held.aggregate_bytes = bytes as usize;
    block
}

/// Defines a collating sequence.
///
/// # Safety
///
/// The handle must be open and `name` NUL-terminated. `compare` must be
/// callable with two counted byte strings, and `data` must stay valid until the
/// collation is replaced or the connection is closed.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation(
    handle: *mut sqlite3,
    name: *const c_char,
    encoding: c_int,
    data: *mut c_void,
    compare: Option<CompareCallback>,
) -> c_int {
    sqlite3_create_collation_v2(handle, name, encoding, data, compare, None)
}

/// Defines a collating sequence, with a destructor for its pointer.
///
/// # Safety
///
/// As [`sqlite3_create_collation`].
#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation_v2(
    handle: *mut sqlite3,
    name: *const c_char,
    encoding: c_int,
    data: *mut c_void,
    compare: Option<CompareCallback>,
    destroy: Option<DestroyCallback>,
) -> c_int {
    let Some(database) = connection(handle) else {
        release(destroy, data);
        return misuse();
    };
    let Some(bytes) = c_str(name) else {
        release(destroy, data);
        return database
            .last
            .refuse(SQLITE_MISUSE, "no collation was named");
    };
    if encoding != crate::codes::SQLITE_UTF8 {
        release(destroy, data);
        return database
            .last
            .refuse(SQLITE_MISUSE, "only SQLITE_UTF8 is supported");
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    let Some(compare) = compare else {
        release(destroy, data);
        return database
            .last
            .refuse(SQLITE_MISUSE, "a collation needs a comparison");
    };
    let carried = Carried(data);
    let comparator: collation::Comparator = Arc::new(move |left: &[u8], right: &[u8]| {
        // The whole wrapper, not its field: see `scalar_body`.
        let carried = carried;
        // SAFETY: the callback is the caller's, and both slices outlive the
        // call - they belong to the values being compared.
        let answer = unsafe {
            compare(
                carried.0,
                left.len() as c_int,
                left.as_ptr().cast(),
                right.len() as c_int,
                right.as_ptr().cast(),
            )
        };
        answer.cmp(&0)
    });
    match database.connection.create_collation(&text, comparator) {
        Err(error) => {
            release(destroy, data);
            database.fail(&error)
        }
        Ok(()) => {
            database.registered.borrow_mut().insert(
                format!("collation/{}", text.to_ascii_lowercase()),
                Registration {
                    destroy: destroy.map(|destroy| (destroy, data)),
                },
            );
            database.succeed()
        }
    }
}

/// Reports that this build has no collation-needed callback.
///
/// SQLite calls it when a statement names a collation nobody defined, so an
/// application can define it on demand. Nothing here calls it, and it is
/// accepted rather than refused so that a caller that installs one - most do,
/// defensively, and never see it fire - is not turned away at startup.
///
/// # Safety
///
/// The handle must be open.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_collation_needed(
    handle: *mut sqlite3,
    _data: *mut c_void,
    _callback: Option<unsafe extern "C" fn(*mut c_void, *mut sqlite3, c_int, *const c_char)>,
) -> c_int {
    if connection(handle).is_none() {
        return misuse();
    }
    SQLITE_OK
}
