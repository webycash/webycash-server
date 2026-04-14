use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{parse2, FnArg, ImplItem, ItemImpl, Pat, ReturnType, Type};

/// Parse an impl block, extract async handler methods, and generate:
/// - {Actor}Msg enum with one variant per handler
/// - ractor::Actor impl that dispatches messages
/// - {Actor}Handle struct with typed async send methods
pub fn expand(input: TokenStream) -> syn::Result<TokenStream> {
    let impl_block: ItemImpl = parse2(input)?;

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

    let msg_enum_name = format_ident!("{}Msg", actor_name);
    let handle_name = format_ident!("{}Handle", actor_name);

    // Collect handler methods (async fn with &self)
    let mut handlers = Vec::new();
    let mut original_methods = Vec::new();

    for item in &impl_block.items {
        if let ImplItem::Fn(method) = item {
            let is_async = method.sig.asyncness.is_some();
            let has_self = method
                .sig
                .inputs
                .first()
                .map(|a| matches!(a, FnArg::Receiver(_)))
                .unwrap_or(false);

            if is_async && has_self {
                handlers.push(method.clone());
            } else {
                original_methods.push(item.clone());
            }
        } else {
            original_methods.push(item.clone());
        }
    }

    // Generate message enum variants
    let mut enum_variants = Vec::new();
    let mut dispatch_arms = Vec::new();
    let mut handle_methods = Vec::new();

    for handler in &handlers {
        let method_name = &handler.sig.ident;
        let variant_name = format_ident!(
            "{}",
            to_pascal_case(&method_name.to_string())
        );

        // Extract parameter names and types (skip &self)
        let params: Vec<_> = handler
            .sig
            .inputs
            .iter()
            .skip(1) // skip &self
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

        // Enum variant: MethodName { param1: Type1, reply: oneshot::Sender<RetType> }
        enum_variants.push(quote! {
            #variant_name {
                #( #param_names: #param_types, )*
                reply: tokio::sync::oneshot::Sender<#ret_type>,
            }
        });

        // Dispatch arm in Actor::handle
        dispatch_arms.push(quote! {
            #msg_enum_name::#variant_name { #( #param_names, )* reply } => {
                let result = self.#method_name(#( #param_names ),*).await;
                let _ = reply.send(result);
            }
        });

        // Handle method: pub async fn method_name(&self, params) -> RetType
        handle_methods.push(quote! {
            pub async fn #method_name(&self, #( #param_names: #param_types ),*) -> #ret_type {
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.actor
                    .cast(#msg_enum_name::#variant_name { #( #param_names, )* reply: tx })
                    .map_err(|e| anyhow::anyhow!("actor send failed: {}", e))
                    .unwrap();
                rx.await.unwrap()
            }
        });
    }

    let (impl_generics, _, where_clause) = impl_block.generics.split_for_impl();

    let output = quote! {
        // Original impl block with non-handler methods + handler methods preserved
        #impl_block

        // Generated message enum
        pub enum #msg_enum_name {
            #( #enum_variants ),*
        }

        // ractor Actor impl — dispatches messages to handler methods
        #[async_trait::async_trait]
        impl #impl_generics ractor::Actor for #actor_ty #where_clause {
            type Msg = #msg_enum_name;
            type State = ();
            type Arguments = ();

            async fn pre_start(
                &self,
                _myself: ractor::ActorRef<Self::Msg>,
                _args: Self::Arguments,
            ) -> Result<Self::State, ractor::ActorProcessingErr> {
                Ok(())
            }

            async fn handle(
                &self,
                _myself: ractor::ActorRef<Self::Msg>,
                msg: Self::Msg,
                _state: &mut Self::State,
            ) -> Result<(), ractor::ActorProcessingErr> {
                match msg {
                    #( #dispatch_arms )*
                }
                Ok(())
            }
        }

        // Generated handle struct
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
