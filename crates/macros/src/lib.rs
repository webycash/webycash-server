mod gen_server;
mod supervisor;

use proc_macro::TokenStream;

/// Generates ractor Actor boilerplate from a handler impl block.
///
/// Transforms an impl block with async handler methods into:
/// 1. A Message enum with one variant per handler (carrying args + oneshot reply)
/// 2. A ractor Actor impl that dispatches messages to the original methods
/// 3. A Handle struct with typed async methods that send messages via oneshot
///
/// # Attributes
///
/// - `#[gen_server]` — state = (), args = ()
/// - `#[gen_server(state = MyState)]` — custom state type, args = ()
/// - `#[gen_server(state = MyState, args = MyArgs)]` — custom state and arguments
///
/// # Special Methods
///
/// - `pre_start`: If present, used as the Actor::pre_start hook. Required when
///   state is not `()`. Receives `self` and the arguments, must return `Result<State, ...>`.
///
/// - Handler methods taking `&mut State` as second parameter will receive the
///   actor state in the dispatch. Others receive `&self` only.
///
/// # Example
///
/// ```ignore
/// #[gen_server]
/// impl MyActor {
///     async fn get_value(&self, key: String) -> Result<Option<String>> {
///         self.store.get(&key)
///     }
/// }
/// ```
///
/// Generates `MyActorMsg` enum, `Actor for MyActor` impl, and `MyActorHandle`.
#[proc_macro_attribute]
pub fn gen_server(attr: TokenStream, item: TokenStream) -> TokenStream {
    gen_server::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Generates ractor Supervisor boilerplate from a spawn-spec impl block.
///
/// Transforms an impl block with `spawn_*` methods into a complete one-for-one
/// (or configurable strategy) supervisor actor with:
/// 1. `ChildHandles` struct with typed handles for each child
/// 2. `SupervisorHandle` with accessor methods and watch channel subscription
/// 3. Actor impl with pre_start (spawns children), handle (subscribe), and
///    handle_supervisor_evt (one-for-one restart)
///
/// # Attributes
///
/// - `#[supervisor(strategy = one_for_one)]` — restart strategy
///
/// # Spawn Methods
///
/// Each method named `spawn_{name}` defines a child actor:
/// - Returns `(ActorImpl, Args)` tuple used for `Actor::spawn_linked`
/// - The method name suffix becomes the child's registered name
///
/// # Example
///
/// ```ignore
/// #[supervisor(strategy = one_for_one)]
/// impl MySupervisor {
///     fn spawn_worker(&self) -> (WorkerActor, ()) {
///         (WorkerActor::new(self.store.clone()), ())
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn supervisor(attr: TokenStream, item: TokenStream) -> TokenStream {
    supervisor::expand(attr.into(), item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
