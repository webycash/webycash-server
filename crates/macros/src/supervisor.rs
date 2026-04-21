use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{ImplItem, ItemImpl, Token, Type};

/// Parsed `#[supervisor(strategy = one_for_one)]` attributes.
struct SupervisorAttrs {
    #[allow(dead_code)]
    strategy: String,
}

impl Parse for SupervisorAttrs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut strategy = "one_for_one".to_string();

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            let _: Token![=] = input.parse()?;
            let val: syn::Ident = input.parse()?;

            match ident.to_string().as_str() {
                "strategy" => strategy = val.to_string(),
                other => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown supervisor attribute: {other}"),
                    ))
                }
            }

            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(SupervisorAttrs { strategy })
    }
}

/// Information about a child actor extracted from a `spawn_*` method.
struct ChildSpec {
    name: String,            // e.g. "ledger"
    method_name: syn::Ident, // e.g. spawn_ledger
}

pub fn expand(attr: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let _attrs: SupervisorAttrs = syn::parse2(attr)?;
    let impl_block: ItemImpl = syn::parse2(input)?;

    let supervisor_ty = &impl_block.self_ty;
    let supervisor_name = match supervisor_ty.as_ref() {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.clone())
            .ok_or_else(|| syn::Error::new_spanned(supervisor_ty, "expected a named type"))?,
        _ => {
            return Err(syn::Error::new_spanned(
                supervisor_ty,
                "expected a path type",
            ))
        }
    };

    // Strip "Actor" suffix for generated type names
    let base_name = supervisor_name
        .to_string()
        .strip_suffix("Actor")
        .map(|s| s.to_string())
        .unwrap_or_else(|| supervisor_name.to_string());
    let msg_enum_name = format_ident!("{}Msg", base_name);
    let state_name = format_ident!("{}State", base_name);
    let handle_name = format_ident!("{}Handle", base_name);
    let child_handles_name = format_ident!("ChildHandles");

    // Extract spawn_* methods and other items
    let mut children = Vec::new();
    let mut other_items = Vec::new();

    for item in &impl_block.items {
        if let ImplItem::Fn(method) = item {
            let name_str = method.sig.ident.to_string();
            if name_str.starts_with("spawn_") {
                let child_name = name_str.strip_prefix("spawn_").unwrap().to_string();

                children.push(ChildSpec {
                    name: child_name,
                    method_name: method.sig.ident.clone(),
                });
            } else {
                other_items.push(item.clone());
            }
        } else {
            other_items.push(item.clone());
        }
    }

    if children.is_empty() {
        return Err(syn::Error::new_spanned(
            &impl_block,
            "supervisor must have at least one spawn_* method",
        ));
    }

    // For each child, we need to figure out the actor type and msg type
    // We use convention: spawn_ledger returns (LedgerActor, Args) and generates LedgerHandle, LedgerMsg
    let child_method_names: Vec<_> = children.iter().map(|c| &c.method_name).collect();

    // Generate actor ref field names and handle type names
    let ref_field_names: Vec<_> = children
        .iter()
        .map(|c| format_ident!("{}_ref", c.name))
        .collect();
    let handle_type_names: Vec<_> = children
        .iter()
        .map(|c| format_ident!("{}Handle", to_pascal_case(&c.name)))
        .collect();
    let msg_type_names: Vec<_> = children
        .iter()
        .map(|c| format_ident!("{}Msg", to_pascal_case(&c.name)))
        .collect();
    let accessor_names: Vec<_> = children
        .iter()
        .map(|c| format_ident!("{}", c.name))
        .collect();
    let child_name_strs: Vec<_> = children.iter().map(|c| c.name.as_str()).collect();

    let output = quote! {
        // Preserve original impl block with spawn methods and other items
        #impl_block

        /// Messages the supervisor accepts.
        pub enum #msg_enum_name {
            Subscribe {
                reply: tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<#child_handles_name>>,
            },
        }

        /// Snapshot of child actor handles for the API layer.
        #[derive(Clone)]
        pub struct #child_handles_name {
            #( pub #accessor_names: #handle_type_names, )*
        }

        /// Supervisor state: holds child refs and restart context.
        pub struct #state_name {
            #( #ref_field_names: ractor::ActorRef<#msg_type_names>, )*
            handles_tx: tokio::sync::watch::Sender<#child_handles_name>,
        }

        /// Handle used by the API layer to reach the supervisor and its children.
        #[derive(Clone)]
        pub struct #handle_name {
            handles_rx: tokio::sync::watch::Receiver<#child_handles_name>,
        }

        impl #handle_name {
            pub fn children(&self) -> #child_handles_name {
                self.handles_rx.borrow().clone()
            }

            #(
                pub fn #accessor_names(&self) -> #handle_type_names {
                    self.children().#accessor_names
                }
            )*
        }

        impl #supervisor_name {
            pub async fn start_supervisor(self) -> anyhow::Result<#handle_name> {
                let (actor_ref, _) = ractor::Actor::spawn(
                    None::<String>,
                    _SupervisorActorWrapper(self),
                    (),
                )
                .await
                .map_err(|e| anyhow::anyhow!("failed to start supervisor: {e}"))?;

                let (tx, rx) = tokio::sync::oneshot::channel();
                actor_ref
                    .cast(#msg_enum_name::Subscribe { reply: tx })
                    .map_err(|e| anyhow::anyhow!("supervisor send failed: {e}"))?;
                let handles_rx = rx.await?;

                Ok(#handle_name { handles_rx })
            }
        }

        // Internal wrapper to implement Actor on the supervisor
        struct _SupervisorActorWrapper(#supervisor_ty);

        #[async_trait::async_trait]
        impl ractor::Actor for _SupervisorActorWrapper {
            type Msg = #msg_enum_name;
            type State = #state_name;
            type Arguments = ();

            async fn pre_start(
                &self,
                myself: ractor::ActorRef<Self::Msg>,
                _args: Self::Arguments,
            ) -> Result<Self::State, ractor::ActorProcessingErr> {
                let supervisor_cell = myself.get_cell();

                #(
                    let (actor, args) = self.0.#child_method_names();
                    let (#ref_field_names, _) = ractor::Actor::spawn_linked(
                        Some(format!("{}_{}", #child_name_strs, myself.get_id().pid())),
                        actor,
                        args,
                        supervisor_cell.clone(),
                    )
                    .await
                    .map_err(|e| ractor::ActorProcessingErr::from(
                        format!("{} spawn failed: {e}", #child_name_strs)
                    ))?;
                )*

                let handles = #child_handles_name {
                    #( #accessor_names: #handle_type_names::from_ref(#ref_field_names.clone()), )*
                };

                let (handles_tx, _) = tokio::sync::watch::channel(handles);

                tracing::info!("supervisor started with all children linked");

                Ok(#state_name {
                    #( #ref_field_names, )*
                    handles_tx,
                })
            }

            async fn handle(
                &self,
                _myself: ractor::ActorRef<Self::Msg>,
                msg: Self::Msg,
                state: &mut Self::State,
            ) -> Result<(), ractor::ActorProcessingErr> {
                match msg {
                    #msg_enum_name::Subscribe { reply } => {
                        let rx = state.handles_tx.subscribe();
                        let _ = reply.send(rx);
                    }
                }
                Ok(())
            }

            async fn handle_supervisor_evt(
                &self,
                myself: ractor::ActorRef<Self::Msg>,
                message: ractor::SupervisionEvent,
                state: &mut Self::State,
            ) -> Result<(), ractor::ActorProcessingErr> {
                match message {
                    ractor::SupervisionEvent::ActorStarted(_cell) => {
                        tracing::info!("child actor started");
                    }
                    ractor::SupervisionEvent::ActorTerminated(cell, _, _) => {
                        let name = cell.get_name().unwrap_or_default();
                        tracing::warn!(name = name.as_str(), "child actor terminated -- restarting");
                        restart_child(&self.0, &name, &myself, state).await?;
                    }
                    ractor::SupervisionEvent::ActorFailed(cell, err) => {
                        let name = cell.get_name().unwrap_or_default();
                        tracing::error!(name = name.as_str(), error = %err, "child actor failed -- restarting");
                        restart_child(&self.0, &name, &myself, state).await?;
                    }
                    ractor::SupervisionEvent::ProcessGroupChanged(_) => {}
                }
                Ok(())
            }
        }

        async fn restart_child(
            sup: &#supervisor_ty,
            name: &str,
            myself: &ractor::ActorRef<#msg_enum_name>,
            state: &mut #state_name,
        ) -> Result<(), ractor::ActorProcessingErr> {
            let supervisor_cell = myself.get_cell();

            #(
                if name.starts_with(#child_name_strs) {
                    let (actor, args) = sup.#child_method_names();
                    let (ref_new, _) = ractor::Actor::spawn_linked(
                        Some(format!("{}_{}", #child_name_strs, myself.get_id().pid())),
                        actor,
                        args,
                        supervisor_cell,
                    )
                    .await
                    .map_err(|e| ractor::ActorProcessingErr::from(
                        format!("{} restart failed: {e}", #child_name_strs)
                    ))?;
                    state.#ref_field_names = ref_new;
                    tracing::info!("{} actor restarted", #child_name_strs);
                } else
            )*
            {
                tracing::warn!("unknown child actor terminated: {}, not restarting", name);
                return Ok(());
            }

            // Push updated handles to all watchers
            let handles = #child_handles_name {
                #( #accessor_names: #handle_type_names::from_ref(state.#ref_field_names.clone()), )*
            };
            let _ = state.handles_tx.send(handles);

            Ok(())
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
