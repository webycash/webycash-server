mod gen_server;

use proc_macro::TokenStream;

/// Generates ractor Actor boilerplate from a handler impl block.
///
/// Transforms an impl block with async handler methods into:
/// 1. A Message enum with one variant per handler (carrying args + oneshot reply)
/// 2. A ractor Actor impl that dispatches messages to the original methods
/// 3. A Handle struct with typed async methods that send messages via oneshot
///
/// # Example
///
/// ```ignore
/// #[gen_server]
/// impl MyActor {
///     async fn get_value(&self, key: String) -> Result<Option<String>> {
///         self.store.get(&key)
///     }
///     async fn set_value(&self, key: String, val: String) -> Result<()> {
///         self.store.set(&key, &val)
///     }
/// }
/// ```
///
/// Generates `MyActorMsg` enum, `Actor for MyActor` impl, and `MyActorHandle`.
#[proc_macro_attribute]
pub fn gen_server(_attr: TokenStream, item: TokenStream) -> TokenStream {
    gen_server::expand(item.into())
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
