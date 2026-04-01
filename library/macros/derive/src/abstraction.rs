//! Implementation of the `#[abstraction]` procedural macro.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{
    Attribute, FnArg, GenericParam, Generics, Ident, ImplItem, ImplItemConst, ImplItemFn,
    ImplItemType, ItemImpl, Pat, Path, Result, Signature, Token, Type, TypeParamBound,
    WherePredicate,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
    spanned::Spanned,
};

/// Represents an associated type with inline bounds (unstable feature).
/// Parses: `type Ident: Bound1 + Bound2 = ConcreteType;`
struct VerbatimAssocType {
    attrs: Vec<Attribute>,
    ident: Ident,
    generics: Generics,
    bounds: Punctuated<TypeParamBound, Token![+]>,
    ty: Type,
}

impl Parse for VerbatimAssocType {
    fn parse(input: ParseStream) -> Result<Self> {
        let attrs = input.call(Attribute::parse_outer)?;
        input.parse::<Token![type]>()?;
        let ident: Ident = input.parse()?;

        // Parse optional generics
        let mut generics: Generics = input.parse()?;

        // Parse bounds after ':'
        let bounds = if input.peek(Token![:]) {
            input.parse::<Token![:]>()?;
            Punctuated::parse_separated_nonempty(input)?
        } else {
            Punctuated::new()
        };

        // Parse where clause if present
        generics.where_clause = input.parse()?;

        // Parse `= Type`
        input.parse::<Token![=]>()?;
        let ty: Type = input.parse()?;
        input.parse::<Token![;]>()?;

        Ok(VerbatimAssocType {
            attrs,
            ident,
            generics,
            bounds,
            ty,
        })
    }
}

/// Parsed arguments from `#[abstraction(TraitName, visibility = "...")]`
struct AbstractionArgs {
    trait_name: Ident,
    visibility: Option<String>,
}

impl Parse for AbstractionArgs {
    fn parse(input: ParseStream) -> Result<Self> {
        let trait_name: Ident = input.parse()?;

        let visibility = if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;

            // Parse `visibility = "..."`
            let key: Ident = input.parse()?;
            if key != "visibility" {
                return Err(syn::Error::new(key.span(), "expected `visibility`"));
            }
            input.parse::<Token![=]>()?;
            let value: syn::LitStr = input.parse()?;
            Some(value.value())
        } else {
            None
        };

        Ok(AbstractionArgs {
            trait_name,
            visibility,
        })
    }
}

/// Parse visibility string into syn::Visibility
fn parse_visibility(vis_str: Option<&str>) -> Result<TokenStream> {
    match vis_str {
        None | Some("private") => Ok(quote! {}),
        Some("public") => Ok(quote! { pub }),
        Some(s) if s.starts_with("public:<") && s.ends_with(">") => {
            let path = &s[8..s.len() - 1]; // Skip "public:<" and ">"
            match path {
                "crate" => Ok(quote! { pub(crate) }),
                "super" => Ok(quote! { pub(super) }),
                "self" => Ok(quote! { pub(self) }),
                other => {
                    let path: Path = syn::parse_str(other)?;
                    Ok(quote! { pub(in #path) })
                }
            }
        }
        Some(other) => Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!("invalid visibility: {}", other),
        )),
    }
}

/// Check if an attribute is `#[default]`
fn is_default_attr(attr: &Attribute) -> bool {
    attr.path().is_ident("default")
}

/// Check if an attribute is an implementation-only attribute that shouldn't
/// appear in trait declarations
fn is_impl_only_attr(attr: &Attribute) -> bool {
    let path = attr.path();
    // These attributes only make sense on implementations, not trait declarations
    path.is_ident("inline")
        || path.is_ident("cold")
        || path.is_ident("track_caller")
        || path.is_ident("no_mangle")
        || path.is_ident("export_name")
        || path.is_ident("link_section")
        || path.is_ident("naked")
        || path.is_ident("instruction_set")
}

/// Remove `#[default]` and `#[abstraction]` attributes from a list
fn filter_attrs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|a| !is_default_attr(a) && !a.path().is_ident("abstraction"))
        .cloned()
        .collect()
}

/// Filter attributes for trait item declarations (removes impl-only attrs like
/// #[inline])
fn filter_trait_attrs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|a| {
            !is_default_attr(a) && !a.path().is_ident("abstraction") && !is_impl_only_attr(a)
        })
        .cloned()
        .collect()
}

/// Strip patterns from function signature for trait declarations.
/// Converts `fn foo(mut self, mut x: i32)` to `fn foo(self, x: i32)`
/// but preserves `&mut self` (only strips mutability on owned self receivers)
fn strip_sig_patterns(sig: &Signature) -> Signature {
    let mut new_sig = sig.clone();
    new_sig.inputs = sig
        .inputs
        .iter()
        .map(|arg| match arg {
            FnArg::Receiver(recv) => {
                // Only remove mutability from owned `mut self`, not from `&mut self`
                // For `&mut self`, reference is Some and mutability should be preserved
                // For `mut self`, reference is None and we should strip mutability
                if recv.reference.is_none() && recv.mutability.is_some() {
                    let mut new_recv = recv.clone();
                    new_recv.mutability = None;
                    FnArg::Receiver(new_recv)
                } else {
                    FnArg::Receiver(recv.clone())
                }
            }
            FnArg::Typed(pat_type) => {
                // For typed args like `mut x: i32`, create a simple pattern `x: i32`
                // Extract the ident from the pattern if it's a simple ident pattern
                let new_pat = match pat_type.pat.as_ref() {
                    Pat::Ident(pat_ident) => {
                        // Create new pattern without mutability
                        let mut new_pat_ident = pat_ident.clone();
                        new_pat_ident.mutability = None;
                        Box::new(Pat::Ident(new_pat_ident))
                    }
                    _ => pat_type.pat.clone(),
                };
                FnArg::Typed(syn::PatType {
                    attrs: pat_type.attrs.clone(),
                    pat: new_pat,
                    colon_token: pat_type.colon_token,
                    ty: pat_type.ty.clone(),
                })
            }
        })
        .collect();
    new_sig
}

/// Extract trait parameters from the impl block.
/// Given `impl<A, B, C, D> Trait<A, B, C> for Struct<D>`,
/// this extracts A, B, C as trait generics.
fn extract_trait_generics(impl_generics: &Generics, trait_path: &Path) -> (Generics, Vec<Ident>) {
    // Get the type arguments used in the trait path
    let trait_type_params: Vec<Ident> = trait_path
        .segments
        .last()
        .and_then(|seg| {
            if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                Some(
                    args.args
                        .iter()
                        .filter_map(|arg| {
                            if let syn::GenericArgument::Type(Type::Path(tp)) = arg {
                                tp.path.get_ident().cloned()
                            } else {
                                None
                            }
                        })
                        .collect(),
                )
            } else {
                None
            }
        })
        .unwrap_or_default();

    // Filter impl generics to only include those used in trait
    let trait_params: Punctuated<GenericParam, Token![,]> = impl_generics
        .params
        .iter()
        .filter(|p| match p {
            GenericParam::Type(tp) => trait_type_params.contains(&tp.ident),
            GenericParam::Lifetime(_) => true, // Keep lifetimes
            GenericParam::Const(_) => false,   // Skip const generics for now
        })
        .cloned()
        .collect();

    let mut trait_generics = impl_generics.clone();
    trait_generics.params = trait_params;
    trait_generics.where_clause = None; // Will be set separately

    (trait_generics, trait_type_params)
}

/// Extract supertraits and trait where clause from impl where clause.
/// Bounds on `Self` become supertraits, bounds on trait generics stay in where
/// clause.
fn extract_trait_bounds(
    impl_where: Option<&syn::WhereClause>,
    trait_type_params: &[Ident],
) -> (Vec<TypeParamBound>, Option<syn::WhereClause>) {
    let Some(where_clause) = impl_where else {
        return (vec![], None);
    };

    let mut supertraits = vec![];
    let mut trait_predicates: Punctuated<WherePredicate, Token![,]> = Punctuated::new();

    for predicate in &where_clause.predicates {
        match predicate {
            WherePredicate::Type(pred_type) => {
                // Check if this is a Self bound (supertrait)
                if let Type::Path(tp) = &pred_type.bounded_ty {
                    if tp.path.is_ident("Self") || tp.path.is_ident("Trait") {
                        // These become supertraits
                        for bound in &pred_type.bounds {
                            supertraits.push(bound.clone());
                        }
                        continue;
                    }

                    // Check if bound is on a trait generic parameter
                    if let Some(ident) = tp.path.get_ident() {
                        if trait_type_params.contains(ident) {
                            trait_predicates.push(predicate.clone());
                            continue;
                        }
                    }
                }
                // Skip struct-specific bounds
            }
            WherePredicate::Lifetime(lt) => {
                trait_predicates.push(WherePredicate::Lifetime(lt.clone()));
            }
            _ => {}
        }
    }

    let trait_where = if trait_predicates.is_empty() {
        None
    } else {
        Some(syn::WhereClause {
            where_token: where_clause.where_token,
            predicates: trait_predicates,
        })
    };

    (supertraits, trait_where)
}

/// Generate a trait const item from impl const
fn generate_trait_const(item: &ImplItemConst, is_default: bool) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let ident = &item.ident;
    let ty = &item.ty;

    if is_default {
        let expr = &item.expr;
        quote! {
            #(#attrs)*
            const #ident: #ty = #expr;
        }
    } else {
        quote! {
            #(#attrs)*
            const #ident: #ty;
        }
    }
}

/// Generate a trait type item from impl type
fn generate_trait_type(item: &ImplItemType, is_default: bool) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let ident = &item.ident;
    let generics = &item.generics;

    // ImplItemType doesn't have inline bounds - those are only in TraitItemType
    // For associated types with bounds in impl blocks (unstable feature),
    // they get parsed as Verbatim and handled separately

    if is_default {
        let ty = &item.ty;
        quote! {
            #(#attrs)*
            type #ident #generics = #ty;
        }
    } else {
        quote! {
            #(#attrs)*
            type #ident #generics;
        }
    }
}

/// Generate a trait fn item from impl fn
fn generate_trait_fn(item: &ImplItemFn, is_default: bool) -> TokenStream {
    let sig = &item.sig;

    if is_default {
        // For default implementations, keep all attrs (except #[default])
        let attrs = filter_attrs(&item.attrs);
        let block = &item.block;
        quote! {
            #(#attrs)*
            #sig #block
        }
    } else {
        // For trait declarations, filter out impl-only attrs like #[inline]
        // and strip patterns like `mut` from arguments
        let attrs = filter_trait_attrs(&item.attrs);
        let clean_sig = strip_sig_patterns(sig);
        quote! {
            #(#attrs)*
            #clean_sig;
        }
    }
}

/// Generate the impl block const item (only if not default)
fn generate_impl_const(item: &ImplItemConst) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let ident = &item.ident;
    let ty = &item.ty;
    let expr = &item.expr;

    quote! {
        #(#attrs)*
        const #ident: #ty = #expr;
    }
}

/// Generate the impl block type item
fn generate_impl_type(item: &ImplItemType) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let ident = &item.ident;
    let generics = &item.generics;
    let ty = &item.ty;

    quote! {
        #(#attrs)*
        type #ident #generics = #ty;
    }
}

/// Generate a trait type item from verbatim associated type (with bounds)
fn generate_trait_type_verbatim(item: &VerbatimAssocType, is_default: bool) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let ident = &item.ident;
    let generics = &item.generics;
    let bounds = &item.bounds;

    let bounds_tokens = if bounds.is_empty() {
        quote! {}
    } else {
        quote! { : #bounds }
    };

    if is_default {
        let ty = &item.ty;
        quote! {
            #(#attrs)*
            type #ident #generics #bounds_tokens = #ty;
        }
    } else {
        quote! {
            #(#attrs)*
            type #ident #generics #bounds_tokens;
        }
    }
}

/// Generate impl type item from verbatim associated type
fn generate_impl_type_verbatim(item: &VerbatimAssocType) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let ident = &item.ident;
    let generics = &item.generics;
    let ty = &item.ty;

    quote! {
        #(#attrs)*
        type #ident #generics = #ty;
    }
}

/// Generate the impl block fn item (only if not default)
fn generate_impl_fn(item: &ImplItemFn) -> TokenStream {
    let attrs = filter_attrs(&item.attrs);
    let sig = &item.sig;
    let block = &item.block;

    quote! {
        #(#attrs)*
        #sig #block
    }
}

/// Main implementation function
pub fn abstraction_impl(attr: TokenStream, item: TokenStream) -> Result<TokenStream> {
    let args: AbstractionArgs = syn::parse2(attr)?;
    let impl_block: ItemImpl = syn::parse2(item)?;

    // Validate that this is a trait impl
    let Some((_, trait_path, _)) = &impl_block.trait_ else {
        return Err(syn::Error::new(
            impl_block.span(),
            "abstraction macro requires a trait impl block",
        ));
    };

    // Parse visibility
    let visibility = parse_visibility(args.visibility.as_deref())?;

    // Extract trait generics
    let (trait_generics, trait_type_params) =
        extract_trait_generics(&impl_block.generics, trait_path);

    // Extract supertraits and where clause
    let (supertraits, trait_where) = extract_trait_bounds(
        impl_block.generics.where_clause.as_ref(),
        &trait_type_params,
    );

    // Collect trait items and impl items
    let mut trait_items = vec![];
    let mut impl_items = vec![];

    for item in &impl_block.items {
        match item {
            ImplItem::Const(c) => {
                let is_default = c.attrs.iter().any(is_default_attr);
                trait_items.push(generate_trait_const(c, is_default));
                if !is_default {
                    impl_items.push(generate_impl_const(c));
                }
            }
            ImplItem::Type(t) => {
                let is_default = t.attrs.iter().any(is_default_attr);
                trait_items.push(generate_trait_type(t, is_default));
                if !is_default {
                    impl_items.push(generate_impl_type(t));
                }
            }
            ImplItem::Fn(f) => {
                let is_default = f.attrs.iter().any(is_default_attr);
                trait_items.push(generate_trait_fn(f, is_default));
                if !is_default {
                    impl_items.push(generate_impl_fn(f));
                }
            }
            ImplItem::Verbatim(tokens) => {
                // Try to parse as associated type with bounds (unstable feature)
                if let Ok(assoc_type) = syn::parse2::<VerbatimAssocType>(tokens.clone()) {
                    let is_default = assoc_type.attrs.iter().any(is_default_attr);
                    trait_items.push(generate_trait_type_verbatim(&assoc_type, is_default));
                    if !is_default {
                        impl_items.push(generate_impl_type_verbatim(&assoc_type));
                    }
                } else {
                    // Pass through unrecognized verbatim items
                    impl_items.push(tokens.clone());
                }
            }
            _ => {
                // Pass through other items
                impl_items.push(item.to_token_stream());
            }
        }
    }

    // Build trait definition
    let trait_name = &args.trait_name;
    let (impl_generics_tokens, _, _) = trait_generics.split_for_impl();

    let supertrait_tokens = if supertraits.is_empty() {
        quote! {}
    } else {
        quote! { : #(#supertraits)+* }
    };

    let trait_def = quote! {
        #visibility trait #trait_name #impl_generics_tokens #supertrait_tokens
        #trait_where
        {
            #(#trait_items)*
        }
    };

    // Build impl block
    let impl_attrs = filter_attrs(&impl_block.attrs);
    let impl_generics = &impl_block.generics;
    let self_ty = &impl_block.self_ty;
    let impl_where = &impl_block.generics.where_clause;

    let impl_def = quote! {
        #(#impl_attrs)*
        impl #impl_generics #trait_path for #self_ty
        #impl_where
        {
            #(#impl_items)*
        }
    };

    Ok(quote! {
        #trait_def
        #impl_def
    })
}
