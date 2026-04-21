use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{FnArg, ImplItem, ItemImpl, Pat, ReturnType, Token, Type};

/// Parsed `#[gen_server(state = T, args = A)]` attributes.
struct GenServerAttrs {
    state_ty: Option<Type>,
    args_ty: Option<Type>,
}

impl Parse for GenServerAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut state_ty = None;
        let mut args_ty = None;

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            let _: Token![=] = input.parse()?;
            let ty: Type = input.parse()?;

            match ident.to_string().as_str() {
                "state" => state_ty = Some(ty),
                "args" => args_ty = Some(ty),
                other => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown gen_server attribute: {other}"),
                    ))
                }
            }

            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(GenServerAttrs { state_ty, args_ty })
    }
}

/// Parse an impl block, extract async handler methods, and generate:
/// - {Actor}Msg enum with one variant per handler
/// - ractor::Actor impl that dispatches messages
/// - {Actor}Handle struct with typed async send methods
pub fn expand(attr: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let attrs: GenServerAttrs = syn::parse2(attr)?;
    let impl_block: ItemImpl = syn::parse2(input)?;

    let actor_ty = &impl_block.self_ty;
    let actor_name = match actor_ty.as_ref() {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.clone())
            .ok_or_else(|| syn::Error::new_spanned(actor_ty, "expected a named type"))?,
        _ => return Err(syn::Error::new_spanned(actor_ty, "expected a path type")),
    };

    // Strip "Actor" suffix for generated type names: StatsActor → StatsMsg, StatsHandle
    let base_name = actor_name
        .to_string()
        .strip_suffix("Actor")
        .map(|s| s.to_string())
        .unwrap_or_else(|| actor_name.to_string());
    let msg_enum_name = format_ident!("{}Msg", base_name);
    let handle_name = format_ident!("{}Handle", base_name);

    let state_type = attrs
        .state_ty
        .map(|t| quote! { #t })
        .unwrap_or(quote! { () });
    let args_type = attrs
        .args_ty
        .map(|t| quote! { #t })
        .unwrap_or(quote! { () });

    // Collect handler methods, pre_start, and other items
    let mut handlers = Vec::new();
    let mut pre_start_method = None;
    let mut other_items = Vec::new();

    for item in &impl_block.items {
        if let ImplItem::Fn(method) = item {
            let is_async = method.sig.asyncness.is_some();
            let has_self = method
                .sig
                .inputs
                .first()
                .map(|a| matches!(a, FnArg::Receiver(_)))
                .unwrap_or(false);

            if method.sig.ident == "pre_start" && is_async && has_self {
                pre_start_method = Some(method.clone());
            } else if is_async && has_self {
                handlers.push(method.clone());
            } else {
                other_items.push(item.clone());
            }
        } else {
            other_items.push(item.clone());
        }
    }

    // Generate message enum variants, dispatch arms, and handle methods
    let mut enum_variants = Vec::new();
    let mut dispatch_arms = Vec::new();
    let mut handle_methods = Vec::new();

    for handler in &handlers {
        let method_name = &handler.sig.ident;
        let variant_name = format_ident!("{}", to_pascal_case(&method_name.to_string()));

        // Detect if handler takes &mut State as second param (after &self)
        let all_params: Vec<_> = handler.sig.inputs.iter().skip(1).collect();
        let takes_state = all_params.first().is_some_and(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                is_mut_ref_to(&pat_type.ty, &state_type)
            } else {
                false
            }
        });

        // Extract parameter names and types (skip &self, skip &mut State if present)
        let skip_count = if takes_state { 2 } else { 1 };
        let params: Vec<_> = handler
            .sig
            .inputs
            .iter()
            .skip(skip_count)
            .filter_map(|arg| {
                if let FnArg::Typed(pat_type) = arg {
                    if let Pat::Ident(ident) = pat_type.pat.as_ref() {
                        Some((ident.ident.clone(), pat_type.ty.clone()))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        let param_names: Vec<_> = params.iter().map(|(name, _)| name).collect();
        let param_types: Vec<_> = params.iter().map(|(_, ty)| ty).collect();

        // Extract return type
        let ret_type = match &handler.sig.output {
            ReturnType::Default => quote! { () },
            ReturnType::Type(_, ty) => quote! { #ty },
        };

        // Enum variant
        enum_variants.push(quote! {
            #variant_name {
                #( #param_names: #param_types, )*
                reply: tokio::sync::oneshot::Sender<#ret_type>,
            }
        });

        // Dispatch arm — pass state if handler takes it
        let call = if takes_state {
            quote! { self.#method_name(state, #( #param_names ),*).await }
        } else {
            quote! { self.#method_name(#( #param_names ),*).await }
        };

        dispatch_arms.push(quote! {
            #msg_enum_name::#variant_name { #( #param_names, )* reply } => {
                let result = #call;
                let _ = reply.send(result);
            }
        });

        // Handle method with proper error propagation (no unwrap)
        handle_methods.push(quote! {
            pub async fn #method_name(&self, #( #param_names: #param_types ),*) -> #ret_type {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.actor
                    .cast(#msg_enum_name::#variant_name { #( #param_names, )* reply: tx })
                    .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))?;
                rx.await.map_err(|_| anyhow::anyhow!("actor dropped before reply"))?
            }
        });
    }

    // Generate pre_start body
    let pre_start_body = if let Some(method) = &pre_start_method {
        // User-provided pre_start — extract the args parameter name
        let args_param = method
            .sig
            .inputs
            .iter()
            .nth(1) // skip &self
            .and_then(|arg| {
                if let FnArg::Typed(pat_type) = arg {
                    if let Pat::Ident(ident) = pat_type.pat.as_ref() {
                        Some(ident.ident.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

        let args_name = args_param.unwrap_or(format_ident!("_args"));
        let body = &method.block;

        quote! {
            async fn pre_start(
                &self,
                _myself: ractor::ActorRef<Self::Msg>,
                #args_name: Self::Arguments,
            ) -> Result<Self::State, ractor::ActorProcessingErr> {
                #body
            }
        }
    } else {
        // Default pre_start: return Ok(()) for unit state
        quote! {
            async fn pre_start(
                &self,
                _myself: ractor::ActorRef<Self::Msg>,
                _args: Self::Arguments,
            ) -> Result<Self::State, ractor::ActorProcessingErr> {
                Ok(())
            }
        }
    };

    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

    // Build a filtered impl block that excludes pre_start (it goes into Actor::pre_start)
    let filtered_items: Vec<_> = impl_block
        .items
        .iter()
        .filter(|item| {
            if let ImplItem::Fn(method) = item {
                method.sig.ident != "pre_start"
            } else {
                true
            }
        })
        .collect();

    let impl_attrs = &impl_block.attrs;
    let impl_generics_full = &impl_block.generics;

    let output = quote! {
        // Preserve the original impl block (excluding pre_start which is in Actor impl)
        #( #impl_attrs )*
        impl #impl_generics_full #actor_ty {
            #( #filtered_items )*
        }

        // Generated message enum
        pub enum #msg_enum_name {
            #( #enum_variants ),*
        }

        // ractor Actor impl — dispatches messages to handler methods
        #[async_trait::async_trait]
        impl #impl_generics ractor::Actor for #actor_ty #where_clause {
            type Msg = #msg_enum_name;
            type State = #state_type;
            type Arguments = #args_type;

            #pre_start_body

            async fn handle(
                &self,
                _myself: ractor::ActorRef<Self::Msg>,
                msg: Self::Msg,
                state: &mut Self::State,
            ) -> Result<(), ractor::ActorProcessingErr> {
                match msg {
                    #( #dispatch_arms )*
                }
                Ok(())
            }
        }

        // Generated handle struct with typed async methods
        #[derive(Clone)]
        pub struct #handle_name {
            actor: ractor::ActorRef<#msg_enum_name>,
        }

        impl #handle_name {
            pub fn from_ref(actor: ractor::ActorRef<#msg_enum_name>) -> Self {
                Self { actor }
            }

            #( #handle_methods )*
        }
    };

    Ok(output)
}

/// Check if a type is `&mut T` where T matches the given state type tokens.
fn is_mut_ref_to(ty: &Type, state_tokens: &TokenStream) -> bool {
    if let Type::Reference(ref_ty) = ty {
        if ref_ty.mutability.is_some() {
            let inner = &ref_ty.elem;
            let inner_str = quote! { #inner }.to_string();
            let state_str = state_tokens.to_string();
            return inner_str == state_str;
        }
    }
    false
}

fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + &chars.collect::<String>(),
                None => String::new(),
            }
        })
        .collect()
}
