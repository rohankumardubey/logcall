// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

#![doc = include_str!("../README.md")]
#![recursion_limit = "256"]
// Instrumenting the async fn is not as straight forward as expected because `async_trait` rewrites `async fn`
// into a normal fn which returns `Box<impl Future>`, and this stops the macro from distinguishing `async fn` from `fn`.
// The following code reused the `async_trait` probes from [tokio-tracing](https://github.com/tokio-rs/tracing/blob/6a61897a5e834988ad9ac709e28c93c4dbf29116/tracing-attributes/src/expand.rs).

extern crate proc_macro;

#[macro_use]
extern crate proc_macro_error;

use proc_macro2::Span;
use quote::quote_spanned;
use syn::spanned::Spanned;
use syn::Ident;
use syn::*;

enum Args {
    Simple {
        level: String,
    },
    Result {
        ok_level: Option<String>,
        err_level: Option<String>,
    },
}

impl Args {
    fn parse(input: AttributeArgs) -> Args {
        match input.as_slice() {
            [NestedMeta::Lit(Lit::Str(s))] => Args::Simple { level: s.value() },
            [NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                path,
                lit: Lit::Str(s),
                ..
            }))] if path.is_ident("ok") => Args::Result {
                ok_level: Some(s.value()),
                err_level: None,
            },
            [NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                path,
                lit: Lit::Str(s),
                ..
            }))] if path.is_ident("err") => Args::Result {
                ok_level: None,
                err_level: Some(s.value()),
            },
            [NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                path,
                lit: Lit::Str(s),
                ..
            })), NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                path: path2,
                lit: Lit::Str(s2),
                ..
            }))]
            | [NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                path: path2,
                lit: Lit::Str(s2),
                ..
            })), NestedMeta::Meta(Meta::NameValue(MetaNameValue {
                path,
                lit: Lit::Str(s),
                ..
            }))] if path.is_ident("ok") && path2.is_ident("err") => Args::Result {
                ok_level: Some(s.value()),
                err_level: Some(s2.value()),
            },
            [] => abort_call_site!("missing arguments"),
            _ => abort_call_site!("invalid arguments"),
        }
    }
}

/// An attribute macro that logs the return values of functions.
#[proc_macro_attribute]
#[proc_macro_error]
pub fn logcall(
    args: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(item as ItemFn);
    let args = Args::parse(syn::parse_macro_input!(args as AttributeArgs));

    // check for async_trait-like patterns in the block, and instrument
    // the future instead of the wrapper
    let func_body = if let Some(internal_fun) =
        get_async_trait_info(&input.block, input.sig.asyncness.is_some())
    {
        // let's rewrite some statements!
        match internal_fun.kind {
            // async-trait <= 0.1.43
            AsyncTraitKind::Function(_) => {
                unimplemented!(
                    "Please upgrade the crate `async-trait` to a version higher than 0.1.44"
                )
            }
            // async-trait >= 0.1.44
            AsyncTraitKind::Async(async_expr) => {
                // fallback if we couldn't find the '__async_trait' binding, might be
                // useful for crates exhibiting the same behaviors as async-trait
                let instrumented_block = gen_block(
                    &async_expr.block,
                    true,
                    false,
                    &input.sig.ident.to_string(),
                    args,
                );
                let async_attrs = &async_expr.attrs;
                quote! {
                    Box::pin(#(#async_attrs) * #instrumented_block )
                }
            }
        }
    } else {
        gen_block(
            &input.block,
            input.sig.asyncness.is_some(),
            input.sig.asyncness.is_some(),
            &input.sig.ident.to_string(),
            args,
        )
    };

    let ItemFn {
        attrs, vis, sig, ..
    } = input;

    let Signature {
        output: return_type,
        inputs: params,
        unsafety,
        constness,
        abi,
        ident,
        asyncness,
        generics:
            Generics {
                params: gen_params,
                where_clause,
                ..
            },
        ..
    } = sig;

    quote::quote!(
        #(#attrs) *
        #vis #constness #unsafety #asyncness #abi fn #ident<#gen_params>(#params) #return_type
        #where_clause
        {
            #func_body
        }
    )
    .into()
}

/// Instrument a block
fn gen_block(
    block: &Block,
    async_context: bool,
    async_keyword: bool,
    fn_name: &str,
    args: Args,
) -> proc_macro2::TokenStream {
    match args {
        Args::Simple { level } => {
            // Generate the instrumented function body.
            // If the function is an `async fn`, this will wrap it in an async block.
            if async_context {
                let log = gen_log(&level, fn_name, "__ret_value");
                let block = quote_spanned!(block.span()=>
                    async move {
                        let __ret_value = async move { #block }.await;
                        #log;
                        __ret_value
                    }
                );

                if async_keyword {
                    quote_spanned!(block.span()=>
                        #block.await
                    )
                } else {
                    block
                }
            } else {
                let log = gen_log(&level, fn_name, "__ret_value");
                quote_spanned!(block.span()=>
                    #[allow(unknown_lints)]
                    #[allow(clippy::redundant_closure_call)]
                    let __ret_value = (move || #block)();
                    #log;
                    __ret_value
                )
            }
        }
        Args::Result {
            ok_level,
            err_level,
        } => {
            let ok_arm = if let Some(ok_level) = ok_level {
                let log_ok = gen_log(&ok_level, fn_name, "__ret_value");
                quote_spanned!(block.span()=>
                    __ret_value@Ok(_) => {
                        #log_ok;
                        __ret_value
                    }
                )
            } else {
                quote_spanned!(block.span()=>
                    Ok(__ret_value) => Ok(__ret_value),
                )
            };
            let err_arm = if let Some(err_level) = err_level {
                let log_err = gen_log(&err_level, fn_name, "__ret_value");
                quote_spanned!(block.span()=>
                    __ret_value@Err(_) => {
                        #log_err;
                        __ret_value
                    }
                )
            } else {
                quote_spanned!(block.span()=>
                    Err(__ret_value) => Err(__ret_value),
                )
            };

            // Generate the instrumented function body.
            // If the function is an `async fn`, this will wrap it in an async block.
            if async_context {
                let block = quote_spanned!(block.span()=>
                    async move {
                        let __ret_value = async move { #block }.await;
                        match __ret_value {
                            #ok_arm
                            #err_arm
                        }
                    }
                );

                if async_keyword {
                    quote_spanned!(block.span()=>
                        #block.await
                    )
                } else {
                    block
                }
            } else {
                quote_spanned!(block.span()=>
                    #[allow(unknown_lints)]
                    #[allow(clippy::redundant_closure_call)]
                    let __ret_value = (move || #block)();
                    match __ret_value {
                        #ok_arm
                        #err_arm
                    }
                )
            }
        }
    }
}

fn gen_log(level: &str, fn_name: &str, return_value: &str) -> proc_macro2::TokenStream {
    let level = level.to_lowercase();
    if !["error", "warn", "info", "debug", "trace"].contains(&level.as_str()) {
        abort_call_site!("unknown log level");
    }
    let level: Ident = Ident::new(&level, Span::call_site());
    let return_value: Ident = Ident::new(return_value, Span::call_site());
    quote!(
        log::#level! ("{}() => {:?}", #fn_name, #return_value)
    )
}

enum AsyncTraitKind<'a> {
    // old construction. Contains the function
    Function(&'a ItemFn),
    // new construction. Contains a reference to the async block
    Async(&'a ExprAsync),
}

struct AsyncTraitInfo<'a> {
    // statement that must be patched
    _source_stmt: &'a Stmt,
    kind: AsyncTraitKind<'a>,
}

// Get the AST of the inner function we need to hook, if it was generated
// by async-trait.
// When we are given a function annotated by async-trait, that function
// is only a placeholder that returns a pinned future containing the
// user logic, and it is that pinned future that needs to be instrumented.
// Were we to instrument its parent, we would only collect information
// regarding the allocation of that future, and not its own span of execution.
// Depending on the version of async-trait, we inspect the block of the function
// to find if it matches the pattern
// `async fn foo<...>(...) {...}; Box::pin(foo<...>(...))` (<=0.1.43), or if
// it matches `Box::pin(async move { ... }) (>=0.1.44). We the return the
// statement that must be instrumented, along with some other informations.
// 'gen_body' will then be able to use that information to instrument the
// proper function/future.
// (this follows the approach suggested in
// https://github.com/dtolnay/async-trait/issues/45#issuecomment-571245673)
fn get_async_trait_info(block: &Block, block_is_async: bool) -> Option<AsyncTraitInfo<'_>> {
    // are we in an async context? If yes, this isn't a async_trait-like pattern
    if block_is_async {
        return None;
    }

    // list of async functions declared inside the block
    let inside_funs = block.stmts.iter().filter_map(|stmt| {
        if let Stmt::Item(Item::Fn(fun)) = &stmt {
            // If the function is async, this is a candidate
            if fun.sig.asyncness.is_some() {
                return Some((stmt, fun));
            }
        }
        None
    });

    // last expression of the block (it determines the return value
    // of the block, so that if we are working on a function whose
    // `trait` or `impl` declaration is annotated by async_trait,
    // this is quite likely the point where the future is pinned)
    let (last_expr_stmt, last_expr) = block.stmts.iter().rev().find_map(|stmt| {
        if let Stmt::Expr(expr) = stmt {
            Some((stmt, expr))
        } else {
            None
        }
    })?;

    // is the last expression a function call?
    let (outside_func, outside_args) = match last_expr {
        Expr::Call(ExprCall { func, args, .. }) => (func, args),
        _ => return None,
    };

    // is it a call to `Box::pin()`?
    let path = match outside_func.as_ref() {
        Expr::Path(path) => &path.path,
        _ => return None,
    };
    if !path_to_string(path).ends_with("Box::pin") {
        return None;
    }

    // Does the call take an argument? If it doesn't,
    // it's not gonna compile anyway, but that's no reason
    // to (try to) perform an out of bounds access
    if outside_args.is_empty() {
        return None;
    }

    // Is the argument to Box::pin an async block that
    // captures its arguments?
    if let Expr::Async(async_expr) = &outside_args[0] {
        // check that the move 'keyword' is present
        async_expr.capture?;

        return Some(AsyncTraitInfo {
            _source_stmt: last_expr_stmt,
            kind: AsyncTraitKind::Async(async_expr),
        });
    }

    // Is the argument to Box::pin a function call itself?
    let func = match &outside_args[0] {
        Expr::Call(ExprCall { func, .. }) => func,
        _ => return None,
    };

    // "stringify" the path of the function called
    let func_name = match **func {
        Expr::Path(ref func_path) => path_to_string(&func_path.path),
        _ => return None,
    };

    // Was that function defined inside of the current block?
    // If so, retrieve the statement where it was declared and the function itself
    let (stmt_func_declaration, func) = inside_funs
        .into_iter()
        .find(|(_, fun)| fun.sig.ident == func_name)?;

    Some(AsyncTraitInfo {
        _source_stmt: stmt_func_declaration,
        kind: AsyncTraitKind::Function(func),
    })
}

// Return a path as a String
fn path_to_string(path: &Path) -> String {
    use std::fmt::Write;
    // some heuristic to prevent too many allocations
    let mut res = String::with_capacity(path.segments.len() * 5);
    for i in 0..path.segments.len() {
        write!(res, "{}", path.segments[i].ident).expect("writing to a String should never fail");
        if i < path.segments.len() - 1 {
            res.push_str("::");
        }
    }
    res
}
